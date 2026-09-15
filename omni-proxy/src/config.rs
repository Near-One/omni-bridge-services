use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use base64::{Engine, engine::general_purpose};
use serde::{Deserialize, de};
use url::Url;

use crate::errors::ConfigError;
use crate::types::{HttpStatusCode, Prefix, RpcCode};

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Testnet,
    Mainnet,
}

impl FromStr for Network {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "testnet" => Self::Testnet,
            "mainnet" => Self::Mainnet,
            _ => return Err(format!("Unsupported network: {s}")),
        })
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

#[derive(Debug)]
pub(crate) struct Upstream {
    url: url::Url,
    timeout_ms: Option<u32>,
    sni: Arc<str>,
}

impl<'de> Deserialize<'de> for Upstream {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(deserialize_with = "de_inject_secrets_into_url")]
            url: url::Url,
            timeout_ms: Option<u32>,
        }
        let Raw { url, timeout_ms } = Raw::deserialize(deserializer)?;
        let sni: Arc<str> = url.host_str().unwrap_or("").into();
        Ok(Self {
            url,
            timeout_ms,
            sni,
        })
    }
}

impl Upstream {
    pub(crate) fn addr(&self) -> String {
        let host = self.url.host_str().unwrap_or("localhost");
        let port = self.url.port_or_known_default().unwrap_or(80);
        format!("{host}:{port}")
    }

    pub(crate) fn is_tls(&self) -> bool {
        matches!(self.url.scheme(), "https" | "wss")
    }

    pub(crate) fn sni(&self) -> Arc<str> {
        Arc::clone(&self.sni)
    }

    pub(crate) fn authority(&self) -> String {
        let host = self.url.host_str().unwrap_or("");
        match self.url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        }
    }

    pub(crate) fn timeout(&self) -> Option<Duration> {
        self.timeout_ms
            .map(|ms| Duration::from_millis(u64::from(ms)))
    }

    pub(crate) fn url_path(&self) -> &str {
        self.url.path()
    }

    pub(crate) fn url_query(&self) -> Option<&str> {
        self.url.query()
    }

    pub(crate) fn is_websocket(&self) -> bool {
        matches!(self.url.scheme(), "ws" | "wss")
    }

    pub(crate) fn auth_header(&self) -> Option<String> {
        let user = self.url.username();
        if user.is_empty() {
            return None;
        }
        let pass = self.url.password().unwrap_or("");
        Some(format!(
            "Basic {}",
            general_purpose::STANDARD.encode(format!("{user}:{pass}"))
        ))
    }
}

#[derive(Deserialize, Debug)]
pub(crate) struct Failover {
    #[serde(default)]
    status_codes: Vec<HttpStatusCode>,
    #[serde(default)]
    rpc_codes: Vec<RpcCode>,
    #[serde(default = "Failover::default_threshold")]
    failure_threshold: u32,
    #[serde(default = "Failover::default_window_secs")]
    window_secs: u64,
}

impl Default for Failover {
    fn default() -> Self {
        Self {
            status_codes: Vec::new(),
            rpc_codes: Vec::new(),
            failure_threshold: Self::default_threshold(),
            window_secs: Self::default_window_secs(),
        }
    }
}

impl Failover {
    fn default_threshold() -> u32 {
        3
    }
    fn default_window_secs() -> u64 {
        60
    }

    pub(crate) fn is_failure_status(&self, status: u16) -> bool {
        self.status_codes.iter().any(|s| s.value() == status)
    }

    pub(crate) fn is_failure_rpc_code(&self, code: i32) -> bool {
        self.rpc_codes.contains(&code)
    }

    pub(crate) fn failure_threshold(&self) -> usize {
        usize::try_from(self.failure_threshold).unwrap_or(usize::MAX)
    }

    pub(crate) fn window(&self) -> Duration {
        Duration::from_secs(self.window_secs)
    }

    pub(crate) fn rpc_codes(&self) -> &[RpcCode] {
        &self.rpc_codes
    }
}

#[derive(Deserialize, Debug)]
pub struct Route {
    prefix: Prefix,
    upstreams: Vec<Upstream>,
    #[serde(default)]
    failover: Failover,
}

impl Route {
    pub(crate) fn prefix(&self) -> &Prefix {
        &self.prefix
    }

    pub(crate) fn upstreams(&self) -> &[Upstream] {
        &self.upstreams
    }

    pub(crate) fn failover(&self) -> &Failover {
        &self.failover
    }
}

#[derive(Deserialize, Debug)]
pub struct Config {
    pub routes: Vec<Route>,
}

impl Config {
    pub fn load_config(config_path: PathBuf) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(&std::fs::read_to_string(config_path)?)?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn from_dynamic_value(value: serde_json::Value) -> Result<Self, ConfigError> {
        let config: Self = serde_json::from_value(value)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let mut seen = HashSet::new();
        for route in &self.routes {
            if route.upstreams.is_empty() {
                return Err(ConfigError::EmptyUpstreams(
                    route.prefix.as_str().to_owned(),
                ));
            }
            if !seen.insert(route.prefix.as_str()) {
                return Err(ConfigError::DuplicatePrefix(
                    route.prefix.as_str().to_owned(),
                ));
            }
        }
        Ok(())
    }
}

fn de_inject_secrets_into_url<'de, D>(deserializer: D) -> Result<Url, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;

    let mut expanded = String::with_capacity(raw.len());
    let mut rest = raw.as_str();
    while let Some(start) = rest.find("${") {
        expanded.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| de::Error::custom(format!("unterminated `${{` in: {raw}")))?;
        let var = &after[..end];
        let val = std::env::var(var)
            .map_err(|_| de::Error::custom(format!("env var `{var}` not set")))?;
        expanded.push_str(&val);
        rest = &after[end + 1..];
    }
    expanded.push_str(rest);

    expanded
        .parse()
        .map_err(|_| de::Error::custom(format!("failed to parse url from `{raw}`")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_parsing() {
        for env_var in [
            "ANKR_API_KEY",
            "INFURA_API_KEY",
            "TATUM_API_KEY",
            "FASTNEAR_API_KEY",
            "QUICKNODE_API_KEY",
        ] {
            // Safe: tests run single-threaded (no other threads reading env at this point).
            unsafe {
                std::env::set_var(env_var, "test");
            }
        }

        for example in ["example-mainnet-config.toml", "example-testnet-config.toml"] {
            let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(example);
            Config::load_config(config_path)
                .unwrap_or_else(|e| panic!("{example} should load and validate: {e}"));
        }
    }

    #[test]
    fn test_multiple_secrets_expanded_in_one_url() {
        // Safe: tests run single-threaded; these vars are unique to this test.
        unsafe {
            std::env::set_var("OP_TEST_USER", "alice");
            std::env::set_var("OP_TEST_KEY", "secret123");
        }
        let config: Config = toml::from_str(
            r#"
[[routes]]
prefix = "/x"
upstreams = [{ url = "https://${OP_TEST_USER}:pw@host.example/path/${OP_TEST_KEY}" }]
"#,
        )
        .expect("config should parse");
        let url = &config.routes[0].upstreams[0].url;
        assert_eq!(url.username(), "alice");
        assert!(
            url.path().ends_with("/secret123"),
            "second `${{}}` should be expanded, got path {}",
            url.path()
        );
    }

    #[test]
    fn test_validate_rejects_empty_upstreams() {
        let config: Config = toml::from_str(
            r#"
[[routes]]
prefix = "/x"
upstreams = []
"#,
        )
        .unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::EmptyUpstreams(_))
        ));
    }

    #[test]
    fn test_validate_rejects_duplicate_prefix() {
        let config: Config = toml::from_str(
            r#"
[[routes]]
prefix = "/eth"
upstreams = [{ url = "https://a.example" }]

[[routes]]
prefix = "/eth"
upstreams = [{ url = "https://b.example" }]
"#,
        )
        .unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::DuplicatePrefix(_))
        ));
    }

    #[test]
    fn test_from_dynamic_value_parses_like_toml() {
        // Safe: tests run single-threaded; unique var for this test.
        unsafe {
            std::env::set_var("OP_DYNAMIC_TEST_KEY", "secret456");
        }
        let value = serde_json::json!({
            "routes": [{
                "prefix": "/x",
                "upstreams": [{ "url": "https://host.example/${OP_DYNAMIC_TEST_KEY}", "timeout_ms": 500 }],
                "failover": { "status_codes": [500], "rpc_codes": [-32000], "failure_threshold": 2, "window_secs": 30 },
            }]
        });
        let config = Config::from_dynamic_value(value).expect("should parse and validate");
        assert_eq!(config.routes.len(), 1);
        let route = &config.routes[0];
        assert!(route.upstreams()[0].url_path().ends_with("/secret456"));
        assert_eq!(
            route.upstreams()[0].timeout(),
            Some(Duration::from_millis(500))
        );
        assert!(route.failover().is_failure_status(500));
        assert!(route.failover().is_failure_rpc_code(-32000));
    }

    #[test]
    fn test_from_dynamic_value_rejects_empty_upstreams() {
        let value = serde_json::json!({
            "routes": [{ "prefix": "/x", "upstreams": [] }]
        });
        assert!(matches!(
            Config::from_dynamic_value(value),
            Err(ConfigError::EmptyUpstreams(_))
        ));
    }

    #[test]
    fn test_from_dynamic_value_rejects_duplicate_prefix() {
        let value = serde_json::json!({
            "routes": [
                { "prefix": "/x", "upstreams": [{ "url": "https://a.example" }] },
                { "prefix": "/x", "upstreams": [{ "url": "https://b.example" }] },
            ]
        });
        assert!(matches!(
            Config::from_dynamic_value(value),
            Err(ConfigError::DuplicatePrefix(_))
        ));
    }

    #[test]
    fn test_from_dynamic_value_rejects_invalid_prefix() {
        let value = serde_json::json!({
            "routes": [{ "prefix": "not-slash-prefixed", "upstreams": [{ "url": "https://a.example" }] }]
        });
        assert!(Config::from_dynamic_value(value).is_err());
    }

    #[test]
    fn test_from_dynamic_value_rejects_malformed_shape() {
        let value = serde_json::json!({ "routes": "not-an-array" });
        assert!(matches!(
            Config::from_dynamic_value(value),
            Err(ConfigError::FailedToParseDynamicConfig(_))
        ));
    }

    #[test]
    fn test_authority_includes_only_non_default_port() {
        let config: Config = toml::from_str(
            r#"
[[routes]]
prefix = "/a"
upstreams = [
  { url = "https://host.example" },
  { url = "http://host.example:8545" },
]
"#,
        )
        .unwrap();
        let upstreams = &config.routes[0].upstreams;
        assert_eq!(upstreams[0].authority(), "host.example");
        assert_eq!(upstreams[1].authority(), "host.example:8545");
    }
}
