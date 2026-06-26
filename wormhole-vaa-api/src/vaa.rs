//! Minimal Wormhole VAA (v1) header parser.
//!
//! We only need to extract the identifying triple `(emitter_chain, emitter_address,
//! sequence)` from the raw signed VAA bytes the spy streams; the raw bytes are stored
//! and served verbatim. Layout (all multi-byte integers big-endian), verified against
//! the Wormhole Go SDK `sdk/vaa/structs.go` and the official docs:
//!
//! ```text
//! header:  version:u8 | guardian_set_index:u32 | len_signatures:u8
//!          | signatures[len_signatures] (each = guardian_index:u8 + signature:[65] = 66 bytes)
//! body:    timestamp:u32 | nonce:u32 | emitter_chain:u16 | emitter_address:[32]
//!          | sequence:u64 | consistency_level:u8 | payload:...
//! ```

use crate::errors::VaaError;

const HEADER_PREFIX: usize = 6; // version(1) + guardian_set_index(4) + len_signatures(1)
const SIGNATURE_RECORD: usize = 66; // guardian_index(1) + signature(65)
const BODY_MIN: usize = 51; // timestamp..consistency_level inclusive

/// Identifying fields parsed from a VAA header + body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaaId {
    pub emitter_chain: u16,
    pub emitter_address: [u8; 32],
    pub sequence: u64,
}

/// Parse `(emitter_chain, emitter_address, sequence)` from raw VAA bytes.
pub fn parse_vaa_id(data: &[u8]) -> Result<VaaId, VaaError> {
    if data.len() < HEADER_PREFIX {
        return Err(VaaError::TooShort {
            got: data.len(),
            need: HEADER_PREFIX,
        });
    }

    let len_signatures = usize::from(data[5]);
    let body_off = len_signatures
        .checked_mul(SIGNATURE_RECORD)
        .and_then(|sigs| sigs.checked_add(HEADER_PREFIX))
        .ok_or(VaaError::Overflow)?;

    let need = body_off.checked_add(BODY_MIN).ok_or(VaaError::Overflow)?;
    if data.len() < need {
        return Err(VaaError::TooShort {
            got: data.len(),
            need,
        });
    }

    // Offsets within the body: timestamp @ +0 (4), nonce @ +4 (4), then:
    let chain_off = body_off + 8;
    let addr_off = body_off + 10;
    let seq_off = body_off + 42;

    let emitter_chain = u16::from_be_bytes([data[chain_off], data[chain_off + 1]]);

    let mut emitter_address = [0u8; 32];
    emitter_address.copy_from_slice(&data[addr_off..addr_off + 32]);

    let mut seq_bytes = [0u8; 8];
    seq_bytes.copy_from_slice(&data[seq_off..seq_off + 8]);
    let sequence = u64::from_be_bytes(seq_bytes);

    Ok(VaaId {
        emitter_chain,
        emitter_address,
        sequence,
    })
}

/// Number of guardian signatures declared in the VAA header. A real, fully-signed VAA
/// always has ≥1 (quorum, ~13); `0` indicates an unsigned/degenerate payload.
pub fn signature_count(data: &[u8]) -> u8 {
    data.get(5).copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};

    /// Build a synthetic VAA with the given signature count, chain, emitter, sequence.
    fn synth(num_sigs: u8, chain: u16, emitter: [u8; 32], seq: u64) -> Vec<u8> {
        let mut v = Vec::new();
        v.push(1); // version
        v.extend_from_slice(&7u32.to_be_bytes()); // guardian_set_index
        v.push(num_sigs);
        for i in 0..num_sigs {
            v.push(i); // guardian index
            v.extend_from_slice(&[0u8; 65]); // signature
        }
        v.extend_from_slice(&1_700_000_000u32.to_be_bytes()); // timestamp
        v.extend_from_slice(&0u32.to_be_bytes()); // nonce
        v.extend_from_slice(&chain.to_be_bytes()); // emitter_chain
        v.extend_from_slice(&emitter); // emitter_address
        v.extend_from_slice(&seq.to_be_bytes()); // sequence
        v.push(1); // consistency level
        v.extend_from_slice(b"payload-bytes"); // payload
        v
    }

    #[test]
    fn parses_synthetic_vaa_with_signatures() {
        let mut emitter = [0u8; 32];
        emitter[31] = 0xAB;
        let bytes = synth(13, 1, emitter, 42597);
        let id = parse_vaa_id(&bytes).unwrap();
        assert_eq!(id.emitter_chain, 1);
        assert_eq!(id.emitter_address, emitter);
        assert_eq!(id.sequence, 42597);
    }

    #[test]
    fn parses_vaa_with_zero_signatures() {
        let bytes = synth(0, 30, [0xCD; 32], 1);
        let id = parse_vaa_id(&bytes).unwrap();
        assert_eq!(id.emitter_chain, 30);
        assert_eq!(id.emitter_address, [0xCD; 32]);
        assert_eq!(id.sequence, 1);
    }

    #[test]
    fn rejects_truncated_vaa() {
        assert!(matches!(
            parse_vaa_id(&[1, 0, 0]),
            Err(VaaError::TooShort { .. })
        ));
        // Header claims 13 signatures but body bytes are missing.
        let mut v = vec![1, 0, 0, 0, 7, 13];
        v.extend_from_slice(&[0u8; 10]);
        assert!(matches!(parse_vaa_id(&v), Err(VaaError::TooShort { .. })));
    }

    #[test]
    fn parses_real_mainnet_solana_vaa_header() {
        // Full real VAA (1127 bytes) captured from api.wormholescan.io for our mainnet
        // Solana emitter: chain 1, emitter 19671a08…cdfe, sequence 42597, 13 signatures.
        let b64 = "AQAAAAcNAB5IBuD9qSNlWhtVVu03Cxb0mtDrwPSCy/90KpnqpOZbcGvg/E+OmwxtIxJJq6r9MwQXAPgvGEYOOYiUIioWwRgAAZIIbvIxSijNJHczZ0CLGwuLWeV3wFohep84dvg8PL1ta9L0L+jcQfr3JKLfKnGueahSs/Fz3CDPG/vcnWZIp/wAAsfcpySiUhsg7Fh32Jivmljk1/jEXdWRkdJ2D0yPFhDfHni64gLaeUNw82nPGwQxBGuQb4DT5YuWeqkMxMEKIEABAx4LtDpEM/AlCKj2ymi5SksJ9yFrHnDkyt+m+XjsODgUYeFUF1CjaANDaaRGpL7XLhavOTSOBMfIz3Cinhw3HLMABCafVxK39cJ+h6XZsolpWAapZ4m/8V4E3uILijyJGTFxfMeKNfHXn3Uenc3AuibaontZ9qIVooXRkSruL0FM4sYABbxy8+ok2WaxQUC2oK8dMLYNEP2mlXIACWcnEiP5D+hHGZUPzv0VwgFWqEwX8daIB6HgfidnIS/8W3g4DzwGkXwABupgwZ1XGy9XffGJ+iPTVicSdHOCZGsSaPDmZ/RgIzAUBtZD263ioic9PQiBcE64w7Pp0OGZOKGE1fw+YKljJ8cACAxwVcenbOy0A2v3QT3br/MDbe7HbOGtKfMCkntGVfg4ayf+vk3l5MHvbgRhDtil89YGARaKrmkSCRsKSh+tRgUACW3ndp7WFMDNaZJOoonDrlzaoZ3urBB7j4IlWMj6E8Z3ecyMjAB3To25MFa64xoICIzkenHkRfLAY0DRVTt0FpsADCZORLJE8Ml9fVRrsnzbNN9dh2BCOkojvH6lOolWAJkNNS+SH9KhFlp/9ynuJSdpjNwPoJAnjFWTpswtNPoRu7gBDSRa6emcDTkENeLnwXImAe1Gio/EfyjGihIRzDfA07+raeUS/xm8P4n7XYtFELgO8em/z4ppcsqsBr7J95Ed0KABD4mlhWh0EdNpQsH46TxHxKwzX0hXOM7TTE7f8X2Q6m3VVpzFCq8dunklQkrgFfWaMGC0cq1MOwonYosAkNtHkhAAEAHQfsANv1NPWDobjPzeUugWdwBYkZ4fyl8vejATWCq/BTyB6BYhBt0XsT/g4hUltvDuLBw/qRTNOqoO56YEuCwBaj6g7AAAAAAAARlnGgipzvbzoEMU7UePwzKklm9BrT5v6naTPe3pxs3+AAAAAAAApmUBAAL3CN4qdixI1FDv9/4jdwbE0s11LbT97qkfv9L5btKwTwIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAGWmAAAAAAAAQBReAgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAADgYgIAAAAAAAAAAAAAAAAAEQAAAG5lYXI6aW50ZW50cy5uZWFyQAAAADQyNjlhOTg0YmIzMGE0MDNiMmQ4N2NhOGU5NGQ2ZDUwYzJiYTdlZTNjMGFjNmJkMDg1YzQ0NzExMWQxYTUzODc=";
        let bytes = STANDARD.decode(b64).expect("valid base64");
        let id = parse_vaa_id(&bytes).expect("parses real VAA");
        assert_eq!(id.emitter_chain, 1);
        assert_eq!(
            hex::encode(id.emitter_address),
            "19671a08a9cef6f3a04314ed478fc332a4966f41ad3e6fea76933dede9c6cdfe"
        );
        assert_eq!(id.sequence, 42597);
    }
}
