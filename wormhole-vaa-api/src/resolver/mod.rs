//! txHash → `(chain, emitter, sequence)` resolution.
//!
//! A VAA encodes its emitter/sequence but not the source tx hash, and the spy stream
//! carries no tx hash either — so endpoint (2) (`?txHash=`) resolves the hash on demand
//! against the chain RPCs we already operate (via omni-proxy). EVM and SVM candidate
//! chains are probed in parallel; a hash lives on exactly one chain.

pub mod evm;
pub mod svm;

use futures::future::join_all;
use serde_json::json;

use crate::config::{Config, Family};
use crate::errors::ConfigError;
use crate::proxy_client::ProxyClient;

/// One resolved Wormhole message identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub chain: u16,
    pub emitter: [u8; 32],
    pub sequence: u64,
}

#[derive(Clone)]
struct EvmTarget {
    prefix: String,
    core_hex: String,
    wh_chain_id: u16,
}

#[derive(Clone)]
struct SvmTarget {
    prefix: String,
    emitter: [u8; 32],
    wh_chain_id: u16,
    /// Base58 bridge program id; only `Sequence:` logs emitted within this program's
    /// invocation are attributed to our emitter.
    program_id: String,
}

#[derive(Clone)]
pub struct Resolver {
    proxy: ProxyClient,
    evm: Vec<EvmTarget>,
    svm: Vec<SvmTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashShape {
    Evm,
    Svm,
    Unknown,
}

impl Resolver {
    pub fn from_config(config: &Config, proxy: ProxyClient) -> Result<Self, ConfigError> {
        let mut evm = Vec::new();
        for chain in config.chains_of(Family::Evm) {
            evm.push(EvmTarget {
                prefix: chain.proxy_prefix.clone(),
                core_hex: chain
                    .core_hex()?
                    .ok_or_else(|| ConfigError::Chain(chain.name.clone(), "missing core".into()))?,
                wh_chain_id: chain.wh_chain_id,
            });
        }

        let mut svm = Vec::new();
        for chain in config.chains_of(Family::Svm) {
            svm.push(SvmTarget {
                prefix: chain.proxy_prefix.clone(),
                emitter: chain.emitter_bytes()?,
                wh_chain_id: chain.wh_chain_id,
                program_id: chain.program_id.clone().ok_or_else(|| {
                    ConfigError::Chain(chain.name.clone(), "missing program_id".into())
                })?,
            });
        }

        Ok(Self { proxy, evm, svm })
    }

    /// Resolve a txHash to zero or more `(chain, emitter, sequence)` identities.
    ///
    /// Returns `Ok([])` when the hash is well-formed but no matching message was found
    /// (caller → 404). Returns `Err` only when every candidate probe failed (→ 5xx).
    pub async fn resolve(&self, hash: &str) -> anyhow::Result<Vec<Resolution>> {
        match classify_hash(hash) {
            HashShape::Evm => self.resolve_evm(hash).await,
            HashShape::Svm => self.resolve_svm(hash).await,
            HashShape::Unknown => Ok(Vec::new()),
        }
    }

    async fn resolve_evm(&self, hash: &str) -> anyhow::Result<Vec<Resolution>> {
        // No EVM chains configured → not our hash, not an error: 404 (proxy fails over).
        if self.evm.is_empty() {
            return Ok(Vec::new());
        }
        let params = json!([hash]);
        let probes = self.evm.iter().map(|t| {
            let proxy = self.proxy.clone();
            let params = params.clone();
            async move {
                (
                    t,
                    proxy
                        .rpc(&t.prefix, "eth_getTransactionReceipt", params)
                        .await,
                )
            }
        });

        let mut any_err = false;
        for (target, result) in join_all(probes).await {
            match result {
                Ok(receipt) => {
                    if !receipt.is_null() {
                        let resolutions =
                            evm::parse_receipt(&receipt, &target.core_hex, target.wh_chain_id);
                        if !resolutions.is_empty() {
                            return Ok(resolutions);
                        }
                    }
                }
                Err(e) => {
                    any_err = true;
                    tracing::debug!(prefix = %target.prefix, "evm receipt probe failed: {e}");
                }
            }
        }

        // Nothing matched. If a probe errored, we can't assert a definitive miss — the
        // chain that actually held the tx may be the one that failed — so 5xx (the proxy
        // fails over / the relayer retries) rather than a 404 that masks a transient outage.
        if any_err {
            anyhow::bail!("EVM receipt probe(s) failed and no match found for {hash}");
        }
        Ok(Vec::new())
    }

    async fn resolve_svm(&self, hash: &str) -> anyhow::Result<Vec<Resolution>> {
        // No SVM chains configured → not our hash, not an error: 404 (proxy fails over).
        if self.svm.is_empty() {
            return Ok(Vec::new());
        }
        let params = json!([
            hash,
            { "encoding": "json", "commitment": "confirmed", "maxSupportedTransactionVersion": 0 }
        ]);
        let probes = self.svm.iter().map(|t| {
            let proxy = self.proxy.clone();
            let params = params.clone();
            async move { (t, proxy.rpc(&t.prefix, "getTransaction", params).await) }
        });

        let mut any_err = false;
        for (target, result) in join_all(probes).await {
            match result {
                Ok(tx) => {
                    if !tx.is_null() {
                        let resolutions = svm::parse_tx(
                            &tx,
                            target.wh_chain_id,
                            target.emitter,
                            &target.program_id,
                        );
                        if !resolutions.is_empty() {
                            return Ok(resolutions);
                        }
                    }
                }
                Err(e) => {
                    any_err = true;
                    tracing::debug!(prefix = %target.prefix, "svm tx probe failed: {e}");
                }
            }
        }

        if any_err {
            anyhow::bail!("SVM tx probe(s) failed and no match found for {hash}");
        }
        Ok(Vec::new())
    }
}

/// Classify a txHash by shape: EVM (`0x`+64 hex) vs Solana/Fogo (base58 → 64 bytes).
pub fn classify_hash(hash: &str) -> HashShape {
    let h = hash.trim();
    let no0x = h
        .strip_prefix("0x")
        .or_else(|| h.strip_prefix("0X"))
        .unwrap_or(h);
    if no0x.len() == 64 && no0x.bytes().all(|b| b.is_ascii_hexdigit()) {
        return HashShape::Evm;
    }
    if (40..=100).contains(&h.len()) && bs58::decode(h).into_vec().is_ok_and(|b| b.len() == 64) {
        return HashShape::Svm;
    }
    HashShape::Unknown
}

/// Canonicalize a hash for use as a cache key (lowercase EVM hashes; leave
/// case-sensitive base58 untouched).
pub fn normalize_hash(hash: &str) -> String {
    let h = hash.trim();
    match classify_hash(h) {
        HashShape::Evm => {
            let no0x = h
                .strip_prefix("0x")
                .or_else(|| h.strip_prefix("0X"))
                .unwrap_or(h);
            no0x.to_ascii_lowercase()
        }
        HashShape::Svm | HashShape::Unknown => h.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_evm_hash() {
        let h = "0x2b1d4733e591483ff22641c448017dbdae5d49c76eb40b9ccc101edc852f4a34";
        assert_eq!(classify_hash(h), HashShape::Evm);
        assert_eq!(classify_hash(&h[2..]), HashShape::Evm);
        assert_eq!(
            classify_hash("2B1D4733E591483FF22641C448017DBDAE5D49C76EB40B9CCC101EDC852F4A34"),
            HashShape::Evm
        );
    }

    #[test]
    fn classifies_solana_signature() {
        let sig = "5aHknzwSmjtN6HNETazyXPE6qTWKV9aWX4YmQDUuaJf5p7mSgHknPyYkZs8vArXoHYbA36WK6FoxP22FyNJSdiQ2";
        assert_eq!(classify_hash(sig), HashShape::Svm);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(classify_hash("hello"), HashShape::Unknown);
        assert_eq!(classify_hash("0x1234"), HashShape::Unknown);
    }

    #[tokio::test]
    async fn resolve_with_no_matching_family_is_empty_not_error() {
        use crate::proxy_client::ProxyClient;
        use std::time::Duration;

        let proxy = ProxyClient::new("http://unused.invalid", Duration::from_secs(1)).unwrap();
        let resolver = Resolver {
            proxy,
            evm: vec![],
            svm: vec![],
        };
        // Short-circuits before any network call when the family has no chains.
        let evm = resolver
            .resolve("0x2b1d4733e591483ff22641c448017dbdae5d49c76eb40b9ccc101edc852f4a34")
            .await
            .expect("empty family is Ok([]), not Err");
        assert!(evm.is_empty());
        let svm = resolver
            .resolve("5aHknzwSmjtN6HNETazyXPE6qTWKV9aWX4YmQDUuaJf5p7mSgHknPyYkZs8vArXoHYbA36WK6FoxP22FyNJSdiQ2")
            .await
            .expect("empty family is Ok([]), not Err");
        assert!(svm.is_empty());
    }

    #[test]
    fn normalize_hash_lowercases_evm_strips_0x_keeps_base58() {
        assert_eq!(
            normalize_hash("0xABCDEF1234567890123456789012345678901234567890123456789012345678"),
            "abcdef1234567890123456789012345678901234567890123456789012345678"
        );
        let sig = "5aHknzwSmjtN6HNETazyXPE6qTWKV9aWX4YmQDUuaJf5p7mSgHknPyYkZs8vArXoHYbA36WK6FoxP22FyNJSdiQ2";
        assert_eq!(normalize_hash(sig), sig);
    }
}
