//! Extract `(emitter, sequence)` from an EVM transaction receipt's
//! `LogMessagePublished` logs.
//!
//! For each receipt log whose `address` is the chain's Wormhole core bridge and whose
//! `topics[0]` is the `LogMessagePublished` signature hash:
//!
//! - emitter = `topics[1]`, the indexed `sender` (a 20-byte address left-padded to 32 bytes = the VAA emitter form).
//! - sequence = the first 32-byte word of `data` (a big-endian `uint64` in the low bytes).
//!
//! Logs are returned in receipt order (= Wormhole's stable per-tx ordering).

use serde_json::Value;

use crate::address::normalize_emitter;
use crate::resolver::Resolution;

/// keccak256("LogMessagePublished(address,uint64,uint32,bytes,uint8)").
pub const LOG_MESSAGE_PUBLISHED_TOPIC0: &str =
    "6eb224fb001ed210e379b335e35efe88672a8ce935d981a6896b27ffdf52a3b2";

/// Parse all of our `LogMessagePublished` logs from a receipt.
///
/// `core_hex` is the lowercase 40-char (no `0x`) core bridge address for the chain.
pub fn parse_receipt(receipt: &Value, core_hex: &str, wh_chain_id: u16) -> Vec<Resolution> {
    let mut out = Vec::new();
    let Some(logs) = receipt.get("logs").and_then(Value::as_array) else {
        return out;
    };

    for log in logs {
        if !log_address_matches(log, core_hex) {
            continue;
        }
        let Some(topics) = log.get("topics").and_then(Value::as_array) else {
            continue;
        };
        // A valid LogMessagePublished log has exactly [topic0, indexed sender].
        if topics.len() != 2 {
            continue;
        }
        if !topic_matches(&topics[0], LOG_MESSAGE_PUBLISHED_TOPIC0) {
            continue;
        }
        let Some(emitter) = topics[1].as_str().and_then(|t| normalize_emitter(t).ok()) else {
            continue;
        };
        let Some(sequence) = log
            .get("data")
            .and_then(Value::as_str)
            .and_then(sequence_from_data)
        else {
            continue;
        };
        out.push(Resolution {
            chain: wh_chain_id,
            emitter,
            sequence,
        });
    }
    out
}

fn log_address_matches(log: &Value, core_hex: &str) -> bool {
    log.get("address")
        .and_then(Value::as_str)
        .is_some_and(|a| strip_0x_lower(a) == core_hex)
}

fn topic_matches(topic: &Value, expected_lower: &str) -> bool {
    topic
        .as_str()
        .is_some_and(|t| strip_0x_lower(t) == expected_lower)
}

/// `sequence` is the first 32-byte ABI word of `data`; the `uint64` value sits in the
/// low 8 bytes (bytes 24..32) big-endian.
fn sequence_from_data(data: &str) -> Option<u64> {
    let hex_str = strip_0x(data);
    let bytes = hex::decode(hex_str).ok()?;
    if bytes.len() < 32 {
        return None;
    }
    let mut seq = [0u8; 8];
    seq.copy_from_slice(&bytes[24..32]);
    Some(u64::from_be_bytes(seq))
}

fn strip_0x(s: &str) -> &str {
    s.strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s)
}

fn strip_0x_lower(s: &str) -> String {
    strip_0x(s.trim()).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data_with_sequence(seq: u64) -> String {
        // ABI head: word0 = sequence (uint64 in low 8 bytes), word1 = nonce,
        // word2 = payload offset, word3 = consistencyLevel.
        let mut word0 = [0u8; 32];
        word0[24..32].copy_from_slice(&seq.to_be_bytes());
        let tail = [0u8; 96]; // three more head words
        format!("0x{}{}", hex::encode(word0), hex::encode(tail))
    }

    #[test]
    fn parses_matching_log() {
        let core = "98f3c9e6e3face36baad05fe09d375ef1464288b";
        let emitter = "3ee18b2214aff97000d974cf647e7c347e8fa585";
        let receipt = json!({
            "logs": [{
                "address": format!("0x{core}"),
                "topics": [
                    format!("0x{LOG_MESSAGE_PUBLISHED_TOPIC0}"),
                    format!("0x000000000000000000000000{emitter}"),
                ],
                "data": data_with_sequence(578818),
            }]
        });
        let out = parse_receipt(&receipt, core, 2);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chain, 2);
        assert_eq!(out[0].sequence, 578818);
        assert_eq!(
            hex::encode(out[0].emitter),
            format!("000000000000000000000000{emitter}")
        );
    }

    #[test]
    fn ignores_logs_from_other_contracts() {
        let receipt = json!({
            "logs": [{
                "address": "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                "topics": [format!("0x{LOG_MESSAGE_PUBLISHED_TOPIC0}"), "0x00"],
                "data": data_with_sequence(1),
            }]
        });
        assert!(parse_receipt(&receipt, "98f3c9e6e3face36baad05fe09d375ef1464288b", 2).is_empty());
    }

    #[test]
    fn ignores_non_wormhole_topic() {
        let core = "98f3c9e6e3face36baad05fe09d375ef1464288b";
        let receipt = json!({
            "logs": [{
                "address": format!("0x{core}"),
                "topics": ["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef", "0x00"],
                "data": data_with_sequence(1),
            }]
        });
        assert!(parse_receipt(&receipt, core, 2).is_empty());
    }

    #[test]
    fn returns_multiple_in_receipt_order() {
        let core = "98f3c9e6e3face36baad05fe09d375ef1464288b";
        let mk = |emitter: &str, seq: u64| {
            json!({
                "address": format!("0x{core}"),
                "topics": [
                    format!("0x{LOG_MESSAGE_PUBLISHED_TOPIC0}"),
                    format!("0x000000000000000000000000{emitter}"),
                ],
                "data": data_with_sequence(seq),
            })
        };
        let receipt = json!({"logs": [
            mk("1111111111111111111111111111111111111111", 84902),
            mk("2222222222222222222222222222222222222222", 657990),
        ]});
        let out = parse_receipt(&receipt, core, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].sequence, 84902);
        assert_eq!(out[1].sequence, 657990);
    }

    #[test]
    fn empty_receipt_yields_nothing() {
        assert!(parse_receipt(&json!({}), "00", 2).is_empty());
        assert!(parse_receipt(&json!({"logs": []}), "00", 2).is_empty());
    }
}
