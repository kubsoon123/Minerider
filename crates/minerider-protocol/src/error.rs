//! The error type of the protocol crate.
//!
//! Deliberately independent of the client crate: the protocol layer is a
//! standalone library and must not depend on game-level error types.

/// Every fallible operation in `minerider-protocol` returns this error.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// A VarInt used more than five bytes.
    #[error("varint is too long (more than 5 bytes)")]
    VarIntTooLong,

    /// A VarLong used more than ten bytes.
    #[error("varlong is too long (more than 10 bytes)")]
    VarLongTooLong,

    /// A read ran past the end of the buffer.
    #[error("unexpected end of buffer: need {needed} bytes, {remaining} remain")]
    BufferUnderflow {
        /// Bytes the reader asked for.
        needed: usize,
        /// Bytes actually left.
        remaining: usize,
    },

    /// A length prefix was negative.
    #[error("negative length {0}")]
    NegativeLength(i32),

    /// A string violated protocol constraints (too long or byte length out
    /// of bounds).
    #[error("invalid string: {0}")]
    InvalidString(String),

    /// String bytes were not valid UTF-8.
    #[error("invalid UTF-8 in string")]
    InvalidUtf8,

    /// An identifier violated vanilla charset rules.
    #[error("invalid identifier: {0}")]
    InvalidIdentifier(String),

    /// A frame declared more bytes than the codec accepts.
    #[error("frame length {length} exceeds maximum {max}")]
    FrameTooLarge {
        /// Declared frame length.
        length: usize,
        /// Configured maximum.
        max: usize,
    },

    /// zlib compression or decompression failed.
    #[error("compression error: {0}")]
    Compression(String),

    /// RSA or AES operation failed.
    #[error("crypto error: {0}")]
    Crypto(String),
}

/// Convenience alias for protocol results.
pub type Result<T> = std::result::Result<T, ProtocolError>;
