use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;

use clap::Parser;
use serde::{Deserialize, de};

use crate::address::{normalize_emitter, normalize_evm_address};
use crate::errors::ConfigError;

#[derive(Parser, Clone, Debug)]
#[command(about = "Self-hosted, wormholescan-compatible Wormhole VAA API")]
pub struct CliArgs {
    #[clap(long)]
    pub network: Network,
    #[clap(long)]
    pub config: PathBuf,
    #[clap(long, default_value = "8080")]
    pub port: u16,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Testnet,
    Mainnet,
}

impl FromStr for Network {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "testnet" => Ok(Self::Testnet),
            "mainnet" => Ok(Self::Mainnet),
            _ => Err(format!("unsupported network: {s}")),
        }
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Network::Testnet => write!(f, "testnet"),
            Network::Mainnet => write!(f, "mainnet"),
        }
    }
}

/// Chain family — decides how a txHash is resolved.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    /// EVM: resolve via `eth_getTransactionReceipt` + `LogMessagePublished`.
    Evm,
    /// Solana VM (Solana, Fogo): resolve via `getTransaction` + sequence log.
    Svm,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ChainConfig {
    /// Human-readable name (e.g. "arb"); used in logs/metrics.
    pub name: String,
    pub family: Family,
    /// Canonical Wormhole chain id (network-agnostic, e.g. arb=23, hyperevm=47).
    pub wh_chain_id: u16,
    /// Our omni-bridge emitter, hex (±`0x`, any left-padding) or 32-byte hex.
    pub emitter: String,
    /// Wormhole core bridge contract — required for EVM chains, ignored for SVM.
    #[serde(default)]
    pub core: Option<String>,
    /// Base58 bridge program id — required for SVM chains (scopes `Sequence:` log
    /// attribution to our program's invocation), ignored for EVM.
    #[serde(default)]
    pub program_id: Option<String>,
    /// omni-proxy route prefix used to reach this chain's RPC (e.g. "/arb").
    pub proxy_prefix: String,
}

impl ChainConfig {
    /// Our emitter normalized to a canonical 32-byte address.
    pub fn emitter_bytes(&self) -> Result<[u8; 32], ConfigError> {
        normalize_emitter(&self.emitter)
            .map_err(|e| ConfigError::Chain(self.name.clone(), format!("invalid emitter: {e}")))
    }

    /// EVM core address normalized to lowercase 40-char hex (no `0x`).
    pub fn core_hex(&self) -> Result<Option<String>, ConfigError> {
        match &self.core {
            Some(c) => normalize_evm_address(c)
                .map(Some)
                .map_err(|e| ConfigError::Chain(self.name.clone(), format!("invalid core: {e}"))),
            None => Ok(None),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    /// Wormhole spy gRPC address, e.g. "wormhole-spy.ns.svc.cluster.local:7073".
    #[serde(deserialize_with = "de_env")]
    pub spy_addr: String,
    /// Redis connection URL.
    #[serde(deserialize_with = "de_env")]
    pub redis_url: String,
    /// omni-proxy base URL, e.g. `http://omni-proxy.omni-proxy-mainnet.svc.cluster.local`.
    #[serde(deserialize_with = "de_env")]
    pub proxy_base_url: String,
    /// Public wormholescan base URL for per-request fallback, e.g.
    /// `https://api.wormholescan.io` (testnet: `https://api.testnet.wormholescan.io`).
    /// When unset, the service serves only from its own store (no fallback).
    #[serde(default)]
    pub wormholescan_base_url: Option<String>,
    /// TTL applied to stored VAAs (default 15 days).
    #[serde(default = "default_vaa_ttl_secs")]
    pub vaa_ttl_secs: u64,
    /// TTL applied to cached txHash→resolution entries (default 1 day).
    #[serde(default = "default_txres_ttl_secs")]
    pub txres_ttl_secs: u64,
    pub chains: Vec<ChainConfig>,
}

fn default_vaa_ttl_secs() -> u64 {
    15 * 24 * 60 * 60
}

fn default_txres_ttl_secs() -> u64 {
    24 * 60 * 60
}

impl Config {
    pub fn load(path: PathBuf) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.chains.is_empty() {
            return Err(ConfigError::NoChains);
        }
        let mut seen = HashSet::new();
        // (wh_chain_id, emitter) is the identity that backs the Redis VAA key, so a
        // collision on that pair would let one chain's VAAs satisfy another's lookups.
        let mut seen_keys = HashSet::new();
        for chain in &self.chains {
            if !seen.insert(chain.name.as_str()) {
                return Err(ConfigError::DuplicateChain(chain.name.clone()));
            }
            if !chain.proxy_prefix.starts_with('/') {
                return Err(ConfigError::Chain(
                    chain.name.clone(),
                    "proxy_prefix must start with '/'".to_owned(),
                ));
            }
            let emitter = chain.emitter_bytes()?;
            if !seen_keys.insert((chain.wh_chain_id, emitter)) {
                return Err(ConfigError::Chain(
                    chain.name.clone(),
                    format!(
                        "duplicate (wh_chain_id={}, emitter) collides with another chain",
                        chain.wh_chain_id
                    ),
                ));
            }
            let core = chain.core_hex()?;
            if chain.family == Family::Evm && core.is_none() {
                return Err(ConfigError::Chain(
                    chain.name.clone(),
                    "EVM chains require a `core` address".to_owned(),
                ));
            }
            if chain.family == Family::Svm {
                match &chain.program_id {
                    None => {
                        return Err(ConfigError::Chain(
                            chain.name.clone(),
                            "SVM chains require a base58 `program_id`".to_owned(),
                        ));
                    }
                    Some(p)
                        if !bs58::decode(p)
                            .into_vec()
                            .is_ok_and(|bytes| bytes.len() == 32) =>
                    {
                        return Err(ConfigError::Chain(
                            chain.name.clone(),
                            "program_id must be a base58 32-byte pubkey".to_owned(),
                        ));
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(())
    }

    /// Chains of a given family, in config order.
    pub fn chains_of(&self, family: Family) -> impl Iterator<Item = &ChainConfig> {
        self.chains.iter().filter(move |c| c.family == family)
    }
}

/// Expand `${VAR}` references against the process environment (same scheme as
/// omni-proxy), allowing secrets/hosts to come from env at parse time.
fn de_env<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    expand_env(&raw).map_err(de::Error::custom)
}

fn expand_env(raw: &str) -> Result<String, ConfigError> {
    let mut expanded = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        expanded.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| ConfigError::UnterminatedEnv(raw.to_owned()))?;
        let var = &after[..end];
        let val = std::env::var(var).map_err(|_| ConfigError::MissingEnv(var.to_owned()))?;
        expanded.push_str(&val);
        rest = &after[end + 1..];
    }
    expanded.push_str(rest);
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
spy_addr = "spy.local:7073"
redis_url = "redis://localhost:6379"
proxy_base_url = "http://proxy.local"

[[chains]]
name = "arb"
family = "evm"
wh_chain_id = 23
emitter = "0xd025b38762B4A4E36F0Cde483b86CB13ea00D989"
core = "0xa5f208e072434bC67592E4C49C1B991BA79BCA46"
proxy_prefix = "/arb"

[[chains]]
name = "solana"
family = "svm"
wh_chain_id = 1
emitter = "19671a08a9cef6f3a04314ed478fc332a4966f41ad3e6fea76933dede9c6cdfe"
program_id = "dahPEoZGXfyV58JqqH85okdHmpN8U2q8owgPUXSCPxe"
proxy_prefix = "/solana"
"#;

    #[test]
    fn parses_and_validates_sample() {
        let cfg: Config = toml::from_str(SAMPLE).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.chains.len(), 2);
        assert_eq!(cfg.vaa_ttl_secs, default_vaa_ttl_secs());
        assert_eq!(cfg.chains_of(Family::Evm).count(), 1);
        assert_eq!(cfg.chains_of(Family::Svm).count(), 1);
    }

    #[test]
    fn evm_chain_requires_core() {
        let bad = SAMPLE.replace(
            "core = \"0xa5f208e072434bC67592E4C49C1B991BA79BCA46\"\n",
            "",
        );
        let cfg: Config = toml::from_str(&bad).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Chain(_, _))));
    }

    #[test]
    fn rejects_duplicate_chain_names() {
        let dup = format!(
            "{SAMPLE}\n[[chains]]\nname = \"arb\"\nfamily = \"evm\"\nwh_chain_id = 23\nemitter = \"0x00\"\ncore = \"0x00\"\nproxy_prefix = \"/arb\"\n"
        );
        let cfg: Config = toml::from_str(&dup).unwrap();
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::DuplicateChain(_))
        ));
    }

    #[test]
    fn rejects_duplicate_chain_id_and_emitter() {
        // Two differently-named chains with the SAME (wh_chain_id, emitter) collide in
        // the Redis keyspace.
        let toml_str = r#"
spy_addr = "spy.local:7073"
redis_url = "redis://localhost:6379"
proxy_base_url = "http://proxy.local"
[[chains]]
name = "arb"
family = "evm"
wh_chain_id = 23
emitter = "0xd025b38762B4A4E36F0Cde483b86CB13ea00D989"
core = "0xa5f208e072434bC67592E4C49C1B991BA79BCA46"
proxy_prefix = "/arb"
[[chains]]
name = "arb-dup"
family = "evm"
wh_chain_id = 23
emitter = "0xd025b38762B4A4E36F0Cde483b86CB13ea00D989"
core = "0xa5f208e072434bC67592E4C49C1B991BA79BCA46"
proxy_prefix = "/arb2"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(matches!(cfg.validate(), Err(ConfigError::Chain(_, _))));
    }

    #[test]
    fn allows_same_emitter_across_different_chain_ids() {
        // arb/base/pol legitimately share one emitter but differ in wh_chain_id.
        let toml_str = r#"
spy_addr = "spy.local:7073"
redis_url = "redis://localhost:6379"
proxy_base_url = "http://proxy.local"
[[chains]]
name = "arb"
family = "evm"
wh_chain_id = 23
emitter = "0xd025b38762B4A4E36F0Cde483b86CB13ea00D989"
core = "0xa5f208e072434bC67592E4C49C1B991BA79BCA46"
proxy_prefix = "/arb"
[[chains]]
name = "base"
family = "evm"
wh_chain_id = 30
emitter = "0xd025b38762B4A4E36F0Cde483b86CB13ea00D989"
core = "0xbebdb6C8ddC678FfA9f8748f85C815C556Dd8ac6"
proxy_prefix = "/base"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn example_configs_load_and_validate() {
        for example in ["example-mainnet-config.toml", "example-testnet-config.toml"] {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(example);
            Config::load(path).unwrap_or_else(|e| panic!("{example} should validate: {e}"));
        }
    }

    #[test]
    fn expands_env_vars() {
        // `set_var` is sound here despite the multi-threaded test harness: this var name
        // is unique to this test and no other test in the crate reads or writes the
        // environment, so there is no concurrent env access to race with.
        unsafe {
            std::env::set_var("WVAA_TEST_REDIS", "redis://secret-host:6379");
        }
        let toml_str = r#"
spy_addr = "spy.local:7073"
redis_url = "${WVAA_TEST_REDIS}"
proxy_base_url = "http://proxy.local"
[[chains]]
name = "arb"
family = "evm"
wh_chain_id = 23
emitter = "0xd025b38762B4A4E36F0Cde483b86CB13ea00D989"
core = "0xa5f208e072434bC67592E4C49C1B991BA79BCA46"
proxy_prefix = "/arb"
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.redis_url, "redis://secret-host:6379");
    }
}
