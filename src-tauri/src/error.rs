use serde::Serialize;
use thiserror::Error;

#[derive(Error, Debug, Serialize)]
pub enum TitanError {
    #[error("I/O error: {0}")]
    Io(String),

    #[error("Serialization error: {0}")]
    Serde(String),

    #[error("TOML deserialization error: {0}")]
    TomlDeser(String),

    #[error("HTTP request error: {0}")]
    Http(String),

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("Config validation: {0}")]
    Config(String),

    #[error("Validation: {0}")]
    Validation(String),

    #[error("Market data: {0}")]
    MarketData(String),

    #[error("Execution: {0}")]
    Execution(String),

    #[error("Network: {0}")]
    Network(String),

    #[error("Broker: {0}")]
    Broker(String),

    #[error("Internal: {0}")]
    Internal(String),

    #[error("State lock poisoned: {0}")]
    LockPoisoned(String),
}

impl From<std::io::Error> for TitanError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<serde_json::Error> for TitanError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serde(e.to_string())
    }
}

impl From<toml::de::Error> for TitanError {
    fn from(e: toml::de::Error) -> Self {
        Self::TomlDeser(e.to_string())
    }
}

impl From<reqwest::Error> for TitanError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e.to_string())
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for TitanError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(e.to_string())
    }
}

impl From<TitanError> for String {
    fn from(e: TitanError) -> Self {
        e.to_string()
    }
}

pub type TitanResult<T> = Result<T, TitanError>;
