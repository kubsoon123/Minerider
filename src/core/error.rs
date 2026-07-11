//! The single error type used across the engine.

/// Every fallible operation in MineRider returns this error.
#[derive(Debug, thiserror::Error)]
pub enum MineRiderError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("connection closed by peer")]
    ConnectionClosed,

    #[error("disconnected by server: {0}")]
    Disconnected(String),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("compression error: {0}")]
    Compression(String),

    #[error("timed out: {0}")]
    Timeout(String),
}

pub type Result<T> = std::result::Result<T, MineRiderError>;
