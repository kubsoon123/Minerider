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

    /// Microsoft/Xbox Live/Minecraft Services authentication failure (device
    /// code flow, XSTS, session-server join, ...).
    #[error("authentication error: {0}")]
    Auth(String),

    /// Transport failure talking to a Microsoft/Xbox/Mojang HTTP endpoint.
    #[error("authentication request failed: {0}")]
    AuthTransport(#[from] reqwest::Error),

    /// SOCKS5 proxy connect/negotiation failure (see
    /// [`crate::network::socks5`]). Never carries a username or password.
    #[error("proxy error: {0}")]
    Proxy(#[from] crate::network::socks5::ProxySocks5Error),
}

pub type Result<T> = std::result::Result<T, MineRiderError>;

/// How [`crate::core::supervisor::ClientSupervisor`] should treat a failure:
/// centralized here (one method, matched over the error's own variants)
/// rather than left to callers pattern-matching strings, so the policy in
/// [`crate::core::supervisor::ReconnectPolicy`] has one place to reason
/// about and one place to test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// A network-level hiccup: a read/write/connect timeout, a plain I/O
    /// error (reset, broken pipe, ...), the peer closing without a stated
    /// reason, or an HTTP transport failure talking to a Microsoft/Xbox/
    /// Mojang endpoint. Worth another attempt.
    Transient,
    /// The server explicitly disconnected us with a stated reason (a kick,
    /// a ban message, a maintenance notice, ...). Ambiguous enough — and
    /// ban-adjacent often enough — that the safe default is to *not*
    /// silently retry; see [`crate::core::supervisor::ReconnectPolicy`].
    ServerRejected,
    /// Authentication failed in a way retrying won't fix: bad/expired
    /// credentials, a declined sign-in, a server requiring online-mode with
    /// no premium session attached, an oversized/undersized RSA key, a
    /// decrypt failure, ...
    AuthFailure,
    /// A wire/game-level protocol violation: an unexpected packet, a
    /// malformed frame, a version mismatch. The exact same server will
    /// produce the exact same failure on the next attempt.
    ProtocolIncompatible,
}

impl MineRiderError {
    /// Classifies this error for reconnect-policy purposes. See
    /// [`RetryClass`] for what each class means and why.
    pub fn retry_class(&self) -> RetryClass {
        match self {
            MineRiderError::Io(_) => RetryClass::Transient,
            MineRiderError::Timeout(_) => RetryClass::Transient,
            MineRiderError::ConnectionClosed => RetryClass::Transient,
            MineRiderError::AuthTransport(_) => RetryClass::Transient,
            MineRiderError::Disconnected(_) => RetryClass::ServerRejected,
            MineRiderError::Auth(_) => RetryClass::AuthFailure,
            MineRiderError::Wire(_) => RetryClass::ProtocolIncompatible,
            MineRiderError::Protocol(_) => RetryClass::ProtocolIncompatible,
            MineRiderError::Crypto(_) => RetryClass::ProtocolIncompatible,
            MineRiderError::Proxy(inner) => inner.retry_class(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_every_variant() {
        assert_eq!(
            MineRiderError::Io(std::io::Error::other("x")).retry_class(),
            RetryClass::Transient
        );
        assert_eq!(
            MineRiderError::Timeout("x".into()).retry_class(),
            RetryClass::Transient
        );
        assert_eq!(
            MineRiderError::ConnectionClosed.retry_class(),
            RetryClass::Transient
        );
        assert_eq!(
            MineRiderError::Disconnected("kicked".into()).retry_class(),
            RetryClass::ServerRejected
        );
        assert_eq!(
            MineRiderError::Auth("bad token".into()).retry_class(),
            RetryClass::AuthFailure
        );
        assert_eq!(
            MineRiderError::Protocol("unexpected packet".into()).retry_class(),
            RetryClass::ProtocolIncompatible
        );
        assert_eq!(
            MineRiderError::Crypto("bad key".into()).retry_class(),
            RetryClass::ProtocolIncompatible
        );
    }
}
