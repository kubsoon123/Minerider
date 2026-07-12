//! The single error type used across the engine.

/// Every fallible operation in MineRider returns this error.
#[derive(Debug, thiserror::Error)]
pub enum MineRiderError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Wire-level protocol failure from `minerider-protocol` (framing,
    /// encoding, compression, crypto).
    #[error("protocol error: {0}")]
    Wire(#[from] minerider_protocol::ProtocolError),

    /// Game-level protocol violation (unexpected packet ids, broken state
    /// machine sequences).
    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("connection closed by peer")]
    ConnectionClosed,

    #[error("disconnected by server: {0}")]
    Disconnected(String),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("timed out: {0}")]
    Timeout(String),
}

pub type Result<T> = std::result::Result<T, MineRiderError>;
