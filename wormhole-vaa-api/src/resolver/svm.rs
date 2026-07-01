//! Extract Wormhole sequence(s) from a Solana/Fogo transaction's logs.
//!
//! The omni-bridge Solana program's emitter is a fixed `config` PDA (verified equal to
//! the on-chain VAA emitter even under the post-message shim), so it comes from config
//! rather than the tx. We only need the sequence(s): the Wormhole core program emits a
//! `Program log: Sequence: <N>` line per message.
//!
//! A `Sequence:` line is only attributed to our emitter when it is emitted *within our
//! bridge program's invocation subtree*. We track the program-invocation stack from the
//! runtime's `Program <id> invoke [d]` / `Program <id> success|failed` log lines, and
//! only count a sequence when our `program_id` is on the stack at that point. This binds
//! each sequence to the program that produced it, so a foreign integrator's Wormhole
//! message in the same tx (or a tx that never touched our bridge) is not mis-attributed
//! to our emitter.

use serde_json::Value;

use crate::resolver::Resolution;

const SEQUENCE_MARKER: &str = "Sequence:";

/// Parse all of our Wormhole sequences from a `getTransaction` result, in log order.
///
/// `program_id` is the base58 bridge program id; only sequences logged within its
/// invocation are returned.
pub fn parse_tx(
    tx: &Value,
    wh_chain_id: u16,
    emitter: [u8; 32],
    program_id: &str,
) -> Vec<Resolution> {
    let Some(logs) = tx.pointer("/meta/logMessages").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut stack: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for line in logs.iter().filter_map(Value::as_str) {
        if let Some(invoked) = invoked_program(line) {
            stack.push(invoked);
        } else if is_program_end(line) {
            stack.pop();
        } else if let Some(sequence) = sequence_from_log(line) {
            // Attribute only when our bridge program is an ancestor of this log.
            if stack.contains(&program_id) {
                out.push(Resolution {
                    chain: wh_chain_id,
                    emitter,
                    sequence,
                });
            }
        }
    }
    out
}

/// `Program <id> invoke [<depth>]` → the invoked program id.
fn invoked_program(line: &str) -> Option<&str> {
    let mut parts = line.split_ascii_whitespace();
    if parts.next()? != "Program" {
        return None;
    }
    let id = parts.next()?;
    (parts.next()? == "invoke").then_some(id)
}

/// `Program <id> success` / `Program <id> failed: …` → end of the top invocation.
fn is_program_end(line: &str) -> bool {
    let mut parts = line.split_ascii_whitespace();
    if parts.next() != Some("Program") || parts.next().is_none() {
        return false;
    }
    matches!(parts.next(), Some(third) if third == "success" || third.starts_with("failed"))
}

/// Extract the sequence from a `…Sequence: <N>…` log line.
///
/// Anchors on the `Sequence:` marker and reads the run of digits that follows,
/// tolerating trailing punctuation or extra tokens.
fn sequence_from_log(line: &str) -> Option<u64> {
    let idx = line.find(SEQUENCE_MARKER)?;
    let after = &line[idx + SEQUENCE_MARKER.len()..];
    let digits: String = after
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BRIDGE: &str = "dahPEoZGXfyV58JqqH85okdHmpN8U2q8owgPUXSCPxe";
    const CORE: &str = "worm2ZoG2kUd4vFXhvjh93UUH596ayRfgQ2MgjNMTth";

    #[test]
    fn parses_real_shim_sequence_log() {
        // Log shape captured from a real mainnet bridge tx (under the shim): the core
        // program is invoked as a CPI inside our bridge program.
        let tx = json!({
            "meta": { "logMessages": [
                format!("Program {BRIDGE} invoke [1]"),
                "Program log: Instruction: InitTransferSol",
                format!("Program {CORE} invoke [3]"),
                "Program log: Sequence: 42597",
                format!("Program {CORE} success"),
                format!("Program {BRIDGE} success"),
            ]}
        });
        let emitter = [0xAB; 32];
        let out = parse_tx(&tx, 1, emitter, BRIDGE);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chain, 1);
        assert_eq!(out[0].sequence, 42597);
        assert_eq!(out[0].emitter, emitter);
    }

    #[test]
    fn ignores_sequence_outside_our_program() {
        // A foreign program emits a Wormhole message; our bridge is never invoked.
        let tx = json!({
            "meta": { "logMessages": [
                "Program FoReIgnProgram1111111111111111111111111111 invoke [1]",
                format!("Program {CORE} invoke [2]"),
                "Program log: Sequence: 999",
                format!("Program {CORE} success"),
                "Program FoReIgnProgram1111111111111111111111111111 success",
            ]}
        });
        assert!(parse_tx(&tx, 1, [0u8; 32], BRIDGE).is_empty());
    }

    #[test]
    fn attributes_only_our_subtree_in_mixed_tx() {
        // Foreign message (seq 999) then our message (seq 42597) in the same tx.
        let tx = json!({
            "meta": { "logMessages": [
                "Program FoReIgnProgram1111111111111111111111111111 invoke [1]",
                format!("Program {CORE} invoke [2]"),
                "Program log: Sequence: 999",
                format!("Program {CORE} success"),
                "Program FoReIgnProgram1111111111111111111111111111 success",
                format!("Program {BRIDGE} invoke [1]"),
                format!("Program {CORE} invoke [2]"),
                "Program log: Sequence: 42597",
                format!("Program {CORE} success"),
                format!("Program {BRIDGE} success"),
            ]}
        });
        let out = parse_tx(&tx, 1, [0u8; 32], BRIDGE);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].sequence, 42597);
    }

    #[test]
    fn multiple_messages_under_our_program_in_log_order() {
        let tx = json!({
            "meta": { "logMessages": [
                format!("Program {BRIDGE} invoke [1]"),
                "Program log: Sequence: 10",
                "Program log: Sequence: 11",
                format!("Program {BRIDGE} success"),
            ]}
        });
        let out = parse_tx(&tx, 51, [0u8; 32], BRIDGE);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].sequence, 10);
        assert_eq!(out[1].sequence, 11);
    }

    #[test]
    fn tolerates_trailing_punctuation_and_extra_tokens() {
        assert_eq!(sequence_from_log("Program log: Sequence: 123."), Some(123));
        assert_eq!(sequence_from_log("Sequence: 456 extra"), Some(456));
        assert_eq!(sequence_from_log("...Sequence:789"), Some(789));
    }

    #[test]
    fn ignores_lines_without_sequence() {
        assert_eq!(
            sequence_from_log("Program log: Initializing transfer"),
            None
        );
        assert_eq!(sequence_from_log("Sequence: notanumber"), None);
    }

    #[test]
    fn no_logs_yields_nothing() {
        assert!(parse_tx(&json!({}), 1, [0u8; 32], BRIDGE).is_empty());
        assert!(parse_tx(&json!({"meta": {}}), 1, [0u8; 32], BRIDGE).is_empty());
    }
}
