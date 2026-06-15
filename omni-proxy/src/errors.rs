use thiserror::Error;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to open file: {0}")]
    FailedToOpenFile(#[from] std::io::Error),
    #[error("Failed to parse config: {0}")]
    FailedToParseConfig(#[from] toml::de::Error),
    #[error("route `{0}` has no upstreams")]
    EmptyUpstreams(String),
    #[error("duplicate route prefix `{0}`")]
    DuplicatePrefix(String),
}

#[derive(Error, Debug)]
pub enum LoggerError {
    #[error("Failed to parse URL: {0}")]
    FailedToParseUrl(#[from] url::ParseError),
    #[error("Error while setting up Loki: {0:?}")]
    TracingLokiError(#[from] tracing_loki::Error),
    #[error("Error while setting up Loki subscriber: {0:?}")]
    TracingSubscriberLokiError(#[from] tracing_subscriber::util::TryInitError),
}
