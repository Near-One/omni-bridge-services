//! Address / emitter normalization helpers.
//!
//! Wormhole emitter addresses are 32 bytes. EVM integrator addresses are 20-byte
//! values left-zero-padded to 32; Solana emitters are already 32 bytes. Callers
//! (config, the spy filter, the REST path param, the resolver) all funnel through
//! [`normalize_emitter`] so a `(chain, emitter, sequence)` key is computed the same
//! way everywhere.

use crate::errors::AddressError;

/// Parse an emitter given as hex (with or without `0x`, any left-padding) into a
/// canonical 32-byte array.
pub fn normalize_emitter(input: &str) -> Result<[u8; 32], AddressError> {
    let trimmed = strip_0x(input.trim());
    if trimmed.len() > 64 {
        return Err(AddressError::TooLong(trimmed.len(), 64));
    }
    // Left-pad to 64 hex chars (32 bytes).
    let padded = format!("{trimmed:0>64}");
    let mut out = [0u8; 32];
    hex::decode_to_slice(&padded, &mut out).map_err(|e| AddressError::Hex(e.to_string()))?;
    Ok(out)
}

/// Lowercase hex (no `0x`) of an emitter, the canonical form used in Redis keys.
pub fn emitter_hex(emitter: &[u8; 32]) -> String {
    hex::encode(emitter)
}

/// Normalize an EVM 20-byte contract address (e.g. a Wormhole core address) into a
/// lowercase 40-char hex string without `0x`, for case-insensitive log comparison.
pub fn normalize_evm_address(input: &str) -> Result<String, AddressError> {
    let trimmed = strip_0x(input.trim());
    if trimmed.len() > 40 {
        return Err(AddressError::TooLong(trimmed.len(), 40));
    }
    let padded = format!("{trimmed:0>40}");
    let mut out = [0u8; 20];
    hex::decode_to_slice(&padded, &mut out).map_err(|e| AddressError::Hex(e.to_string()))?;
    Ok(hex::encode(out))
}

fn strip_0x(s: &str) -> &str {
    s.strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s)
        .trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_emitter_pads_evm_address() {
        let got = normalize_emitter("0xd025b38762B4A4E36F0Cde483b86CB13ea00D989").unwrap();
        assert_eq!(
            emitter_hex(&got),
            "000000000000000000000000d025b38762b4a4e36f0cde483b86cb13ea00d989"
        );
    }

    #[test]
    fn normalize_emitter_accepts_full_32_byte_hex() {
        let s = "19671a08a9cef6f3a04314ed478fc332a4966f41ad3e6fea76933dede9c6cdfe";
        let got = normalize_emitter(s).unwrap();
        assert_eq!(emitter_hex(&got), s);
    }

    #[test]
    fn normalize_emitter_is_case_insensitive_and_strips_prefix() {
        let lower = normalize_emitter("0xDEADBEEF").unwrap();
        let upper = normalize_emitter("deadbeef").unwrap();
        assert_eq!(lower, upper);
        assert_eq!(
            emitter_hex(&lower),
            "00000000000000000000000000000000000000000000000000000000deadbeef"
        );
    }

    #[test]
    fn normalize_emitter_rejects_overlong() {
        let long = "0".repeat(65);
        assert!(matches!(
            normalize_emitter(&long),
            Err(AddressError::TooLong(65, 64))
        ));
    }

    #[test]
    fn normalize_evm_address_lowercases_and_pads() {
        let got = normalize_evm_address("0x98f3c9e6E3fAce36bAAd05FE09d375Ef1464288B").unwrap();
        assert_eq!(got, "98f3c9e6e3face36baad05fe09d375ef1464288b");
    }
}
