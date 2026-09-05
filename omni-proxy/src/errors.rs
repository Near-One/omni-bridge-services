use thiserror::Error;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to open file: {0}")]
    FailedToOpenFile(#[from] std::io::Error),
    #[error("Failed to parse config: {0}")]
    FailedToParseConfig(#[from] toml::de::Error),
    #[error("Failed to parse dynamic config: {0}")]
    FailedToParseDynamicConfig(#[from] serde_json::Error),
    #[error("route `{0}` has no upstreams")]
    EmptyUpstreams(String),
    #[error("duplicate route prefix `{0}`")]
    DuplicatePrefix(String),
}

#[derive(Error, Debug)]
pub enum DynamicConfigError {
    #[error("config service request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid config fetched from config service: {0}")]
    Config(#[from] ConfigError),
}

#[derive(Error, Debug)]
pub enum LoggerError {
    #[error("Failed to parse URL: {0}")]
    FailedToParseUrl(#[from] url::ParseError),
    #[error("Error while setting up Loki: {0:?}")]
    TracingLokiError(#[from] tracing_loki::Error),
    #[error("Error while setting up tracing subscriber: {0:?}")]
    TracingSubscriberInitError(#[from] tracing_subscriber::util::TryInitError),
}
