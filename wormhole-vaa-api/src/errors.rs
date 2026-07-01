use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("environment variable `{0}` is not set")]
    MissingEnv(String),
    #[error("unterminated `${{` in config value: {0}")]
    UnterminatedEnv(String),
    #[error("chain `{0}`: {1}")]
    Chain(String, String),
    #[error("duplicate chain name `{0}`")]
    DuplicateChain(String),
    #[error("no chains configured")]
    NoChains,
}

/// Error decoding a VAA's binary header.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum VaaError {
    #[error("vaa too short: {got} bytes, need at least {need}")]
    TooShort { got: usize, need: usize },
    #[error("vaa header arithmetic overflowed")]
    Overflow,
}

/// Error normalizing an emitter or address string.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AddressError {
    #[error("invalid hex: {0}")]
    Hex(String),
    #[error("address too long: {0} hex chars, max {1}")]
    TooLong(usize, usize),
}
