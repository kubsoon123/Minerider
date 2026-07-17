//! SOCKS5 (RFC 1928) `CONNECT` support, used to route the Minecraft TCP
//! connection through a proxy before the vanilla handshake begins.
//!
//! Deliberately hand-rolled rather than built on an existing SOCKS5 crate:
//! this codebase already hand-rolls every other wire protocol it depends on
//! (the Minecraft frame codec, the AES-128-CFB8 stream cipher wrapper, the
//! RSA encrypt wrapper, NBT) specifically to keep byte-level control and
//! direct test coverage of every edge case, and the mission's own test list
//! (every reply code, truncated/malformed responses, invalid version/address
//! type, credential/domain length limits, ...) is exactly the kind of
//! protocol-conformance surface this project already tests its other
//! hand-rolled codecs against. A third-party crate's own internal parsing
//! would sit between those tests and the code they're meant to verify.
//!
//! Only `CONNECT` (RFC 1928 §4, `CMD = 0x01`) is implemented — the only
//! command a Minecraft client needs. `UDP ASSOCIATE`/`BIND` are out of
//! scope.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;

use crate::core::error::RetryClass;

const VERSION: u8 = 0x05;

const METHOD_NO_AUTH: u8 = 0x00;
const METHOD_USER_PASS: u8 = 0x02;
const METHOD_NO_ACCEPTABLE: u8 = 0xFF;

const USER_PASS_VERSION: u8 = 0x01;
const USER_PASS_SUCCESS: u8 = 0x00;

const CMD_CONNECT: u8 = 0x01;

const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

/// RFC 1928/1929 length-prefix fields are one byte: 255 is the largest value
/// that fits.
const MAX_FIELD_LEN: usize = 255;

/// A proxy password that is never printed. There is deliberately no
/// [`fmt::Display`] impl and [`fmt::Debug`] always prints a fixed redacted
/// placeholder, so a `ClientConfig` (or anything containing one) accidentally
/// logged, traced, or included in a panic/snapshot never leaks it — the only
/// way to read the plaintext is [`ProxyPassword::expose_secret`], named to
/// make every call site visibly opt in.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyPassword(String);

impl ProxyPassword {
    pub fn new(password: impl Into<String>) -> Self {
        Self(password.into())
    }

    /// The plaintext password. Named loudly on purpose: every call site is
    /// an explicit admission that this value is about to leave the type
    /// that protects it (here, only to be written to the proxy socket).
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProxyPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProxyPassword(<redacted>)")
    }
}

/// Username/password credentials for the SOCKS5 `USERNAME/PASSWORD` method
/// (RFC 1929). The username is not a secret in the same sense as the
/// password, but is still excluded from [`fmt::Debug`] — proxy usernames are
/// often account identifiers worth keeping out of logs by default.
#[derive(Clone, PartialEq, Eq)]
pub struct Socks5Credentials {
    pub username: String,
    pub password: ProxyPassword,
}

impl Socks5Credentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: ProxyPassword::new(password),
        }
    }
}

impl fmt::Debug for Socks5Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Socks5Credentials")
            .field("username", &"<redacted>")
            .field("password", &self.password)
            .finish()
    }
}

/// Immutable SOCKS5 proxy configuration. Intended to be wrapped in an `Arc`
/// (see [`crate::core::client::ClientConfig::proxy`]) so many bot configs can
/// cheaply share one instance, or each hold their own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Socks5ProxyConfig {
    pub host: String,
    pub port: u16,
    pub credentials: Option<Socks5Credentials>,
}

impl Socks5ProxyConfig {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            credentials: None,
        }
    }

    pub fn with_credentials(mut self, credentials: Socks5Credentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Reads `{prefix}_HOST`, `{prefix}_PORT`, and optionally
    /// `{prefix}_USERNAME`/`{prefix}_PASSWORD`, from the process environment.
    /// Returns `Ok(None)` when `{prefix}_HOST` is simply unset (no proxy
    /// configured); `Err` when it *is* set but the rest of the configuration
    /// is invalid. Never reads command-line arguments — env vars are not
    /// visible in `ps`/Task Manager process-argument listings the way CLI
    /// flags are, which is why this is the supported non-programmatic path
    /// for a proxy password.
    pub fn from_env(prefix: &str) -> Result<Option<Self>, EnvConfigError> {
        let Ok(host) = std::env::var(format!("{prefix}_HOST")) else {
            return Ok(None);
        };
        let port_var = format!("{prefix}_PORT");
        let port: u16 = std::env::var(&port_var)
            .map_err(|_| EnvConfigError::Missing(port_var.clone()))?
            .parse()
            .map_err(|_| EnvConfigError::InvalidPort(port_var))?;
        let mut config = Socks5ProxyConfig::new(host, port);

        let username = std::env::var(format!("{prefix}_USERNAME")).ok();
        let password = std::env::var(format!("{prefix}_PASSWORD")).ok();
        match (username, password) {
            (Some(username), Some(password)) => {
                config = config.with_credentials(Socks5Credentials::new(username, password));
            }
            (None, None) => {}
            _ => return Err(EnvConfigError::PartialCredentials),
        }
        Ok(Some(config))
    }
}

/// [`Socks5ProxyConfig::from_env`] failures. Never carries the actual
/// password value.
#[derive(Debug, thiserror::Error)]
pub enum EnvConfigError {
    #[error("environment variable {0} is required when the host variable is set")]
    Missing(String),
    #[error("environment variable {0} is not a valid port number")]
    InvalidPort(String),
    #[error(
        "only one of the username/password environment variables was set; provide both or neither"
    )]
    PartialCredentials,
}

/// The `REP` byte of a SOCKS5 reply (RFC 1928 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Socks5Reply {
    Succeeded,
    GeneralFailure,
    NotAllowedByRuleset,
    NetworkUnreachable,
    HostUnreachable,
    ConnectionRefused,
    TtlExpired,
    CommandNotSupported,
    AddressTypeNotSupported,
    /// A reply code outside 0x00-0x08; still a well-formed reply frame.
    Unknown(u8),
}

impl Socks5Reply {
    fn from_code(code: u8) -> Self {
        match code {
            0x00 => Self::Succeeded,
            0x01 => Self::GeneralFailure,
            0x02 => Self::NotAllowedByRuleset,
            0x03 => Self::NetworkUnreachable,
            0x04 => Self::HostUnreachable,
            0x05 => Self::ConnectionRefused,
            0x06 => Self::TtlExpired,
            0x07 => Self::CommandNotSupported,
            0x08 => Self::AddressTypeNotSupported,
            other => Self::Unknown(other),
        }
    }

    /// How [`crate::core::supervisor::ClientSupervisor`] should treat this
    /// specific reply if reconnection is enabled — see
    /// [`crate::core::error::RetryClass`]. Ruleset denial is a stated
    /// decision by the proxy (mirrors [`RetryClass::ServerRejected`] for a
    /// Minecraft-server kick); network/host/connection/TTL failures are
    /// conditions that can change between attempts; unsupported
    /// command/address-type are fixed proxy capability limits that retrying
    /// cannot change.
    fn retry_class(self) -> RetryClass {
        match self {
            Self::Succeeded => RetryClass::Transient, // unreachable in error paths
            Self::NotAllowedByRuleset => RetryClass::ServerRejected,
            Self::GeneralFailure
            | Self::NetworkUnreachable
            | Self::HostUnreachable
            | Self::ConnectionRefused
            | Self::TtlExpired => RetryClass::Transient,
            Self::CommandNotSupported | Self::AddressTypeNotSupported | Self::Unknown(_) => {
                RetryClass::ProtocolIncompatible
            }
        }
    }
}

impl fmt::Display for Socks5Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Succeeded => "succeeded",
            Self::GeneralFailure => "general SOCKS server failure",
            Self::NotAllowedByRuleset => "connection not allowed by ruleset",
            Self::NetworkUnreachable => "network unreachable",
            Self::HostUnreachable => "host unreachable",
            Self::ConnectionRefused => "connection refused",
            Self::TtlExpired => "TTL expired",
            Self::CommandNotSupported => "command not supported",
            Self::AddressTypeNotSupported => "address type not supported",
            Self::Unknown(code) => return write!(f, "unknown reply code 0x{code:02x}"),
        };
        f.write_str(text)
    }
}

/// Every way [`connect`] can fail. Endpoints (proxy and target `host:port`)
/// are safe to include — they're configuration, not secrets. No variant
/// ever carries a username or password; see [`ProxyPassword`].
#[derive(Debug, thiserror::Error)]
pub enum ProxySocks5Error {
    #[error("connecting to SOCKS5 proxy {endpoint}: {source}")]
    ProxyConnect {
        endpoint: String,
        source: std::io::Error,
    },
    #[error("io error talking to SOCKS5 proxy {endpoint} during {phase}: {source}")]
    Io {
        endpoint: String,
        phase: &'static str,
        source: std::io::Error,
    },
    #[error("timed out talking to SOCKS5 proxy {endpoint} during {phase}")]
    Timeout {
        endpoint: String,
        phase: &'static str,
    },
    #[error("SOCKS5 proxy {endpoint} rejected every offered authentication method")]
    NoAcceptableAuthMethod { endpoint: String },
    #[error("SOCKS5 proxy {endpoint} rejected the supplied username/password")]
    AuthenticationRejected { endpoint: String },
    #[error("SOCKS5 proxy {endpoint} refused CONNECT to {target}: {reply}")]
    ConnectRefused {
        endpoint: String,
        target: String,
        reply: Socks5Reply,
    },
    #[error("SOCKS5 proxy {endpoint} sent a malformed {phase}: {reason}")]
    Protocol {
        endpoint: String,
        phase: &'static str,
        reason: String,
    },
    #[error("proxy username exceeds the SOCKS5 255-byte length limit")]
    UsernameTooLong,
    #[error("proxy password exceeds the SOCKS5 255-byte length limit")]
    PasswordTooLong,
    #[error("target hostname {target:?} exceeds the SOCKS5 255-byte domain length limit")]
    TargetTooLong { target: String },
}

impl ProxySocks5Error {
    /// See [`crate::core::error::RetryClass`]. Config-shaped failures
    /// (bad/rejected credentials, oversized fields, a fixed proxy
    /// capability mismatch) are never worth silently retrying; network
    /// conditions and a stated ruleset denial follow the same reasoning as
    /// [`crate::core::error::MineRiderError`]'s other variants.
    pub fn retry_class(&self) -> RetryClass {
        match self {
            Self::ProxyConnect { .. } | Self::Io { .. } | Self::Timeout { .. } => {
                RetryClass::Transient
            }
            Self::NoAcceptableAuthMethod { .. } | Self::AuthenticationRejected { .. } => {
                RetryClass::AuthFailure
            }
            Self::UsernameTooLong | Self::PasswordTooLong => RetryClass::AuthFailure,
            Self::TargetTooLong { .. } | Self::Protocol { .. } => RetryClass::ProtocolIncompatible,
            Self::ConnectRefused { reply, .. } => reply.retry_class(),
        }
    }
}

/// Connects to `proxy`, negotiates SOCKS5, and issues `CONNECT target_host:
/// target_port`. On success, returns the raw, already-tunneled `TcpStream`:
/// every byte written after this point goes straight to `target_host:
/// target_port` unmodified, so the caller's Minecraft handshake reaches the
/// proxy as an opaque payload and reaches the real server exactly as sent.
///
/// `budget` bounds the whole operation (proxy TCP connect through the
/// CONNECT reply) with one shared deadline computed once, the same pattern
/// [`crate::network::connection::write_frame_with_timeout`] uses — not one
/// timeout reset at every phase — so a peer trickling bytes just before each
/// individual read timeout still can't stall the caller indefinitely.
pub async fn connect(
    proxy: &Socks5ProxyConfig,
    target_host: &str,
    target_port: u16,
    budget: Duration,
) -> Result<TcpStream, ProxySocks5Error> {
    let endpoint = proxy.endpoint();
    let deadline = Instant::now() + budget;

    let stream = tokio::time::timeout_at(deadline, TcpStream::connect((&*proxy.host, proxy.port)))
        .await
        .map_err(|_| ProxySocks5Error::Timeout {
            endpoint: endpoint.clone(),
            phase: "proxy TCP connect",
        })?
        .map_err(|source| ProxySocks5Error::ProxyConnect {
            endpoint: endpoint.clone(),
            source,
        })?;
    stream
        .set_nodelay(true)
        .map_err(|source| ProxySocks5Error::Io {
            endpoint: endpoint.clone(),
            phase: "proxy TCP connect",
            source,
        })?;

    let mut stream = stream;
    negotiate(
        &mut stream,
        &endpoint,
        proxy.credentials.as_ref(),
        target_host,
        target_port,
        deadline,
    )
    .await?;
    Ok(stream)
}

/// The negotiation itself, generic over the stream so tests can exercise
/// every edge case against `tokio::io::duplex` fakes without a real socket
/// or timing flakiness — the same rationale
/// [`crate::network::connection::write_frame_with_timeout`] uses for its own
/// generic `AsyncWrite` parameter.
async fn negotiate<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    endpoint: &str,
    credentials: Option<&Socks5Credentials>,
    target_host: &str,
    target_port: u16,
    deadline: Instant,
) -> Result<(), ProxySocks5Error> {
    if let Some(creds) = credentials {
        if creds.username.len() > MAX_FIELD_LEN {
            return Err(ProxySocks5Error::UsernameTooLong);
        }
        if creds.password.expose_secret().len() > MAX_FIELD_LEN {
            return Err(ProxySocks5Error::PasswordTooLong);
        }
    }
    let target = encode_target(target_host)?;

    // Method negotiation (RFC 1928 §3). Offer exactly one method: never
    // offer NO AUTHENTICATION as a fallback when credentials are
    // configured — that would let a proxy silently skip authentication the
    // caller explicitly asked for.
    let offered = if credentials.is_some() {
        METHOD_USER_PASS
    } else {
        METHOD_NO_AUTH
    };
    write_all(
        stream,
        &[VERSION, 1, offered],
        endpoint,
        "method negotiation",
        deadline,
    )
    .await?;
    let mut method_reply = [0u8; 2];
    read_exact(
        stream,
        &mut method_reply,
        endpoint,
        "method negotiation",
        deadline,
    )
    .await?;
    if method_reply[0] != VERSION {
        return Err(ProxySocks5Error::Protocol {
            endpoint: endpoint.to_string(),
            phase: "method negotiation reply",
            reason: format!("unexpected SOCKS version 0x{:02x}", method_reply[0]),
        });
    }
    match method_reply[1] {
        m if m == offered => {}
        METHOD_NO_ACCEPTABLE => {
            return Err(ProxySocks5Error::NoAcceptableAuthMethod {
                endpoint: endpoint.to_string(),
            })
        }
        other => {
            return Err(ProxySocks5Error::Protocol {
                endpoint: endpoint.to_string(),
                phase: "method negotiation reply",
                reason: format!("proxy selected unoffered method 0x{other:02x}"),
            })
        }
    }

    if let Some(creds) = credentials {
        authenticate(stream, endpoint, creds, deadline).await?;
    }

    // CONNECT request (RFC 1928 §4).
    let mut request = vec![VERSION, CMD_CONNECT, 0x00];
    request.extend_from_slice(&target);
    request.extend_from_slice(&target_port.to_be_bytes());
    write_all(stream, &request, endpoint, "CONNECT request", deadline).await?;

    // CONNECT reply (RFC 1928 §6): fixed 4-byte header, then a
    // variable-length BND.ADDR/BND.PORT this client never uses but must
    // still consume to stay frame-aligned (irrelevant on this stream since
    // nothing else is read from it afterward, but consuming it here — rather
    // than leaving unread bytes for the Minecraft codec to trip over — keeps
    // the contract simple regardless of future changes).
    let mut header = [0u8; 4];
    read_exact(stream, &mut header, endpoint, "CONNECT reply", deadline).await?;
    if header[0] != VERSION {
        return Err(ProxySocks5Error::Protocol {
            endpoint: endpoint.to_string(),
            phase: "CONNECT reply",
            reason: format!("unexpected SOCKS version 0x{:02x}", header[0]),
        });
    }
    let bound_addr_len = match header[3] {
        ATYP_IPV4 => 4,
        ATYP_IPV6 => 16,
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            read_exact(stream, &mut len, endpoint, "CONNECT reply", deadline).await?;
            usize::from(len[0])
        }
        other => {
            return Err(ProxySocks5Error::Protocol {
                endpoint: endpoint.to_string(),
                phase: "CONNECT reply",
                reason: format!("unsupported bound-address type 0x{other:02x}"),
            })
        }
    };
    let mut bound = vec![0u8; bound_addr_len + 2]; // + BND.PORT
    read_exact(stream, &mut bound, endpoint, "CONNECT reply", deadline).await?;

    let reply = Socks5Reply::from_code(header[1]);
    if reply != Socks5Reply::Succeeded {
        return Err(ProxySocks5Error::ConnectRefused {
            endpoint: endpoint.to_string(),
            target: format!("{target_host}:{target_port}"),
            reply,
        });
    }
    Ok(())
}

async fn authenticate<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    endpoint: &str,
    creds: &Socks5Credentials,
    deadline: Instant,
) -> Result<(), ProxySocks5Error> {
    let mut request = vec![USER_PASS_VERSION, creds.username.len() as u8];
    request.extend_from_slice(creds.username.as_bytes());
    request.push(creds.password.expose_secret().len() as u8);
    request.extend_from_slice(creds.password.expose_secret().as_bytes());
    write_all(stream, &request, endpoint, "authentication", deadline).await?;

    let mut reply = [0u8; 2];
    read_exact(stream, &mut reply, endpoint, "authentication", deadline).await?;
    if reply[0] != USER_PASS_VERSION {
        return Err(ProxySocks5Error::Protocol {
            endpoint: endpoint.to_string(),
            phase: "authentication reply",
            reason: format!("unexpected sub-negotiation version 0x{:02x}", reply[0]),
        });
    }
    if reply[1] != USER_PASS_SUCCESS {
        return Err(ProxySocks5Error::AuthenticationRejected {
            endpoint: endpoint.to_string(),
        });
    }
    Ok(())
}

/// Encodes `host` as a SOCKS5 `ATYP + DST.ADDR` field: a literal IPv4/IPv6
/// address is sent as that address type (no DNS involved); anything else is
/// sent as `ATYP_DOMAIN` with the hostname bytes verbatim, so the *proxy*
/// resolves it — proxy-side DNS resolution, not this client's.
fn encode_target(host: &str) -> Result<Vec<u8>, ProxySocks5Error> {
    if let Ok(addr) = host.parse::<IpAddr>() {
        return Ok(match addr {
            IpAddr::V4(v4) => encode_ipv4(v4),
            IpAddr::V6(v6) => encode_ipv6(v6),
        });
    }
    if host.len() > MAX_FIELD_LEN {
        return Err(ProxySocks5Error::TargetTooLong {
            target: host.to_string(),
        });
    }
    let mut out = vec![ATYP_DOMAIN, host.len() as u8];
    out.extend_from_slice(host.as_bytes());
    Ok(out)
}

fn encode_ipv4(addr: Ipv4Addr) -> Vec<u8> {
    let mut out = vec![ATYP_IPV4];
    out.extend_from_slice(&addr.octets());
    out
}

fn encode_ipv6(addr: Ipv6Addr) -> Vec<u8> {
    let mut out = vec![ATYP_IPV6];
    out.extend_from_slice(&addr.octets());
    out
}

async fn write_all<S: AsyncWrite + Unpin>(
    stream: &mut S,
    buf: &[u8],
    endpoint: &str,
    phase: &'static str,
    deadline: Instant,
) -> Result<(), ProxySocks5Error> {
    tokio::time::timeout_at(deadline, stream.write_all(buf))
        .await
        .map_err(|_| ProxySocks5Error::Timeout {
            endpoint: endpoint.to_string(),
            phase,
        })?
        .map_err(|source| ProxySocks5Error::Io {
            endpoint: endpoint.to_string(),
            phase,
            source,
        })
}

async fn read_exact<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut [u8],
    endpoint: &str,
    phase: &'static str,
    deadline: Instant,
) -> Result<(), ProxySocks5Error> {
    tokio::time::timeout_at(deadline, stream.read_exact(buf))
        .await
        .map_err(|_| ProxySocks5Error::Timeout {
            endpoint: endpoint.to_string(),
            phase,
        })?
        .map(|_bytes_read| ())
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::UnexpectedEof {
                ProxySocks5Error::Protocol {
                    endpoint: endpoint.to_string(),
                    phase,
                    reason: "connection closed with a truncated reply".to_string(),
                }
            } else {
                ProxySocks5Error::Io {
                    endpoint: endpoint.to_string(),
                    phase,
                    source,
                }
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::DuplexStream;

    const T: Duration = Duration::from_secs(5);

    fn deadline() -> Instant {
        Instant::now() + T
    }

    fn pair() -> (DuplexStream, DuplexStream) {
        tokio::io::duplex(4096)
    }

    #[test]
    fn password_debug_is_redacted() {
        let secret = ProxyPassword::new("hunter2");
        assert_eq!(format!("{secret:?}"), "ProxyPassword(<redacted>)");
    }

    #[test]
    fn credentials_debug_redacts_username_and_password() {
        let creds = Socks5Credentials::new("alice", "hunter2");
        let text = format!("{creds:?}");
        assert!(!text.contains("alice"));
        assert!(!text.contains("hunter2"));
    }

    #[test]
    fn proxy_config_debug_never_contains_password() {
        let cfg = Socks5ProxyConfig::new("proxy.example", 1080)
            .with_credentials(Socks5Credentials::new("alice", "hunter2"));
        let text = format!("{cfg:?}");
        assert!(!text.contains("hunter2"));
        assert!(text.contains("proxy.example"));
    }

    #[test]
    fn errors_never_contain_a_planted_password() {
        // No `ProxySocks5Error` variant has a field that could even hold a
        // password (only `endpoint`/`target`/`reply`/`reason` strings, none
        // ever built from credential bytes) — this exercises every
        // auth-adjacent variant end to end and checks the rendered text
        // against a distinctive marker as a regression guard, in case a
        // future edit ever threads a credential into one of them.
        const PLANTED: &str = "s3cr3t-marker";
        let endpoint = "proxy.example:1080".to_string();
        let errors: Vec<ProxySocks5Error> = vec![
            ProxySocks5Error::NoAcceptableAuthMethod {
                endpoint: endpoint.clone(),
            },
            ProxySocks5Error::AuthenticationRejected {
                endpoint: endpoint.clone(),
            },
            ProxySocks5Error::ConnectRefused {
                endpoint: endpoint.clone(),
                target: "mc.example:25565".to_string(),
                reply: Socks5Reply::ConnectionRefused,
            },
        ];
        for error in errors {
            assert!(!format!("{error}").contains(PLANTED));
            assert!(!format!("{error:?}").contains(PLANTED));
        }
    }

    #[tokio::test]
    async fn no_auth_success_full_roundtrip() {
        let (mut client, mut server) = pair();
        let client_fut = negotiate(&mut client, "p:1", None, "mc.example", 25565, deadline());
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [VERSION, 1, METHOD_NO_AUTH]);
            server.write_all(&[VERSION, METHOD_NO_AUTH]).await.unwrap();

            let mut req_head = [0u8; 3];
            server.read_exact(&mut req_head).await.unwrap();
            assert_eq!(req_head, [VERSION, CMD_CONNECT, 0x00]);
            let mut atyp = [0u8; 1];
            server.read_exact(&mut atyp).await.unwrap();
            assert_eq!(atyp[0], ATYP_DOMAIN);
            let mut len = [0u8; 1];
            server.read_exact(&mut len).await.unwrap();
            let mut domain = vec![0u8; len[0] as usize];
            server.read_exact(&mut domain).await.unwrap();
            assert_eq!(domain, b"mc.example");
            let mut port = [0u8; 2];
            server.read_exact(&mut port).await.unwrap();
            assert_eq!(u16::from_be_bytes(port), 25565);

            server
                .write_all(&[VERSION, 0x00, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
        };
        let (result, _) = tokio::join!(client_fut, server_fut);
        result.expect("no-auth CONNECT should succeed");
    }

    #[tokio::test]
    async fn authenticated_success() {
        let (mut client, mut server) = pair();
        let creds = Socks5Credentials::new("alice", "hunter2");
        let client_fut = negotiate(
            &mut client,
            "p:1",
            Some(&creds),
            "127.0.0.1",
            25565,
            deadline(),
        );
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [VERSION, 1, METHOD_USER_PASS]);
            server
                .write_all(&[VERSION, METHOD_USER_PASS])
                .await
                .unwrap();

            let mut head = [0u8; 2];
            server.read_exact(&mut head).await.unwrap();
            assert_eq!(head[0], USER_PASS_VERSION);
            let mut uname = vec![0u8; head[1] as usize];
            server.read_exact(&mut uname).await.unwrap();
            assert_eq!(uname, b"alice");
            let mut plen = [0u8; 1];
            server.read_exact(&mut plen).await.unwrap();
            let mut pass = vec![0u8; plen[0] as usize];
            server.read_exact(&mut pass).await.unwrap();
            assert_eq!(pass, b"hunter2");
            server
                .write_all(&[USER_PASS_VERSION, USER_PASS_SUCCESS])
                .await
                .unwrap();

            let mut req_head = [0u8; 4];
            server.read_exact(&mut req_head).await.unwrap();
            assert_eq!(req_head[3], ATYP_IPV4);
            let mut rest = [0u8; 6];
            server.read_exact(&mut rest).await.unwrap();
            server
                .write_all(&[VERSION, 0x00, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
        };
        let (result, _) = tokio::join!(client_fut, server_fut);
        result.expect("authenticated CONNECT should succeed");
    }

    #[tokio::test]
    async fn rejected_authentication_surfaces_typed_error() {
        let (mut client, mut server) = pair();
        let creds = Socks5Credentials::new("alice", "wrong");
        let client_fut = negotiate(&mut client, "p:1", Some(&creds), "h", 1, deadline());
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            server
                .write_all(&[VERSION, METHOD_USER_PASS])
                .await
                .unwrap();
            let mut buf = [0u8; 64];
            let _ = server.read(&mut buf).await.unwrap();
            server
                .write_all(&[USER_PASS_VERSION, 0x01]) // any non-zero status
                .await
                .unwrap();
        };
        let (result, _) = tokio::join!(client_fut, server_fut);
        match result {
            Err(ProxySocks5Error::AuthenticationRejected { .. }) => {}
            other => panic!("expected AuthenticationRejected, got {other:?}"),
        }
        assert_eq!(
            ProxySocks5Error::AuthenticationRejected {
                endpoint: "p:1".into()
            }
            .retry_class(),
            RetryClass::AuthFailure
        );
    }

    #[tokio::test]
    async fn no_acceptable_method_surfaces_typed_error() {
        let (mut client, mut server) = pair();
        let client_fut = negotiate(&mut client, "p:1", None, "h", 1, deadline());
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            server
                .write_all(&[VERSION, METHOD_NO_ACCEPTABLE])
                .await
                .unwrap();
        };
        let (result, _) = tokio::join!(client_fut, server_fut);
        match result {
            Err(ProxySocks5Error::NoAcceptableAuthMethod { .. }) => {}
            other => panic!("expected NoAcceptableAuthMethod, got {other:?}"),
        }
    }

    async fn connect_reply_case(reply_byte: u8) -> Result<(), ProxySocks5Error> {
        let (mut client, mut server) = pair();
        let client_fut = negotiate(&mut client, "p:1", None, "h", 1, deadline());
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            server.write_all(&[VERSION, METHOD_NO_AUTH]).await.unwrap();
            let mut buf = [0u8; 64];
            let mut total = 0;
            // domain "h" request: 3 + 1(atyp) + 1(len) + 1(host) + 2(port) = 8
            while total < 8 {
                total += server.read(&mut buf[total..]).await.unwrap();
            }
            server
                .write_all(&[VERSION, reply_byte, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
        };
        let (result, _) = tokio::join!(client_fut, server_fut);
        result
    }

    #[tokio::test]
    async fn every_documented_reply_code_is_classified() {
        let cases: &[(u8, Socks5Reply, RetryClass)] = &[
            (0x01, Socks5Reply::GeneralFailure, RetryClass::Transient),
            (
                0x02,
                Socks5Reply::NotAllowedByRuleset,
                RetryClass::ServerRejected,
            ),
            (0x03, Socks5Reply::NetworkUnreachable, RetryClass::Transient),
            (0x04, Socks5Reply::HostUnreachable, RetryClass::Transient),
            (0x05, Socks5Reply::ConnectionRefused, RetryClass::Transient),
            (0x06, Socks5Reply::TtlExpired, RetryClass::Transient),
            (
                0x07,
                Socks5Reply::CommandNotSupported,
                RetryClass::ProtocolIncompatible,
            ),
            (
                0x08,
                Socks5Reply::AddressTypeNotSupported,
                RetryClass::ProtocolIncompatible,
            ),
            (
                0x09,
                Socks5Reply::Unknown(0x09),
                RetryClass::ProtocolIncompatible,
            ),
        ];
        for &(code, expected_reply, expected_class) in cases {
            let result = connect_reply_case(code).await;
            match result {
                Err(ProxySocks5Error::ConnectRefused { reply, .. }) => {
                    assert_eq!(reply, expected_reply, "code 0x{code:02x}");
                    assert_eq!(reply.retry_class(), expected_class, "code 0x{code:02x}");
                }
                other => panic!("code 0x{code:02x}: expected ConnectRefused, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn succeeded_reply_with_ipv6_bound_address_is_accepted() {
        let (mut client, mut server) = pair();
        let client_fut = negotiate(&mut client, "p:1", None, "h", 1, deadline());
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            server.write_all(&[VERSION, METHOD_NO_AUTH]).await.unwrap();
            let mut buf = [0u8; 64];
            let mut total = 0;
            while total < 8 {
                total += server.read(&mut buf[total..]).await.unwrap();
            }
            let mut reply = vec![VERSION, 0x00, 0x00, ATYP_IPV6];
            reply.extend_from_slice(&[0u8; 16]);
            reply.extend_from_slice(&[0u8; 2]);
            server.write_all(&reply).await.unwrap();
        };
        let (result, _) = tokio::join!(client_fut, server_fut);
        result.expect("IPv6 bound address in the reply must still parse");
    }

    #[tokio::test]
    async fn domain_targets_send_no_local_dns_only_the_hostname() {
        let (mut client, mut server) = pair();
        let client_fut = negotiate(
            &mut client,
            "p:1",
            None,
            "sub.minecraft.example",
            25565,
            deadline(),
        );
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            server.write_all(&[VERSION, METHOD_NO_AUTH]).await.unwrap();
            let mut head = [0u8; 5];
            server.read_exact(&mut head).await.unwrap();
            assert_eq!(head[3], ATYP_DOMAIN);
            let len = head[4] as usize;
            let mut domain = vec![0u8; len];
            server.read_exact(&mut domain).await.unwrap();
            assert_eq!(domain, b"sub.minecraft.example");
            let mut port = [0u8; 2];
            server.read_exact(&mut port).await.unwrap();
            assert_eq!(u16::from_be_bytes(port), 25565);
            server
                .write_all(&[VERSION, 0x00, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
        };
        let (result, _) = tokio::join!(client_fut, server_fut);
        result.unwrap();
    }

    #[tokio::test]
    async fn ipv4_target_is_sent_as_atyp_ipv4_not_domain() {
        let (mut client, mut server) = pair();
        let client_fut = negotiate(&mut client, "p:1", None, "203.0.113.7", 25565, deadline());
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            server.write_all(&[VERSION, METHOD_NO_AUTH]).await.unwrap();
            let mut head = [0u8; 4];
            server.read_exact(&mut head).await.unwrap();
            assert_eq!(head[3], ATYP_IPV4);
            let mut addr = [0u8; 4];
            server.read_exact(&mut addr).await.unwrap();
            assert_eq!(addr, [203, 0, 113, 7]);
            server
                .write_all(&[VERSION, 0x00, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
        };
        let (result, _) = tokio::join!(client_fut, server_fut);
        result.unwrap();
    }

    #[tokio::test]
    async fn ipv6_target_is_sent_as_atyp_ipv6() {
        let (mut client, mut server) = pair();
        let client_fut = negotiate(&mut client, "p:1", None, "::1", 25565, deadline());
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            server.write_all(&[VERSION, METHOD_NO_AUTH]).await.unwrap();
            let mut head = [0u8; 4];
            server.read_exact(&mut head).await.unwrap();
            assert_eq!(head[3], ATYP_IPV6);
            let mut addr = [0u8; 16];
            server.read_exact(&mut addr).await.unwrap();
            assert_eq!(
                IpAddr::V6(Ipv6Addr::from(addr)),
                "::1".parse::<IpAddr>().unwrap()
            );
            server
                .write_all(&[VERSION, 0x00, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
        };
        let (result, _) = tokio::join!(client_fut, server_fut);
        result.unwrap();
    }

    #[tokio::test]
    async fn truncated_method_reply_is_a_protocol_error_not_a_hang() {
        let (mut client, mut server) = pair();
        let client_fut = negotiate(&mut client, "p:1", None, "h", 1, deadline());
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            server.write_all(&[VERSION]).await.unwrap(); // one byte, then close
            drop(server);
        };
        let (result, _) = tokio::join!(client_fut, server_fut);
        match result {
            Err(ProxySocks5Error::Protocol { phase, .. }) => {
                assert_eq!(phase, "method negotiation")
            }
            other => panic!("expected Protocol(truncated), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn truncated_connect_reply_is_a_protocol_error() {
        let (mut client, mut server) = pair();
        let client_fut = negotiate(&mut client, "p:1", None, "h", 1, deadline());
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            server.write_all(&[VERSION, METHOD_NO_AUTH]).await.unwrap();
            let mut buf = [0u8; 64];
            let mut total = 0;
            while total < 8 {
                total += server.read(&mut buf[total..]).await.unwrap();
            }
            server.write_all(&[VERSION, 0x00]).await.unwrap(); // truncated reply
            drop(server);
        };
        let (result, _) = tokio::join!(client_fut, server_fut);
        match result {
            Err(ProxySocks5Error::Protocol { phase, .. }) => {
                assert_eq!(phase, "CONNECT reply")
            }
            other => panic!("expected Protocol(truncated), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_socks_version_in_method_reply_is_rejected() {
        let (mut client, mut server) = pair();
        let client_fut = negotiate(&mut client, "p:1", None, "h", 1, deadline());
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            server.write_all(&[0x04, METHOD_NO_AUTH]).await.unwrap();
        };
        let (result, _) = tokio::join!(client_fut, server_fut);
        match result {
            Err(ProxySocks5Error::Protocol { reason, .. }) => {
                assert!(reason.contains("version"))
            }
            other => panic!("expected Protocol(bad version), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_address_type_in_connect_reply_is_rejected() {
        let (mut client, mut server) = pair();
        let client_fut = negotiate(&mut client, "p:1", None, "h", 1, deadline());
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            server.write_all(&[VERSION, METHOD_NO_AUTH]).await.unwrap();
            let mut buf = [0u8; 64];
            let mut total = 0;
            while total < 8 {
                total += server.read(&mut buf[total..]).await.unwrap();
            }
            server
                .write_all(&[VERSION, 0x00, 0x00, 0x7F]) // bogus ATYP
                .await
                .unwrap();
        };
        let (result, _) = tokio::join!(client_fut, server_fut);
        match result {
            Err(ProxySocks5Error::Protocol { reason, .. }) => {
                assert!(reason.contains("address type"))
            }
            other => panic!("expected Protocol(bad atyp), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn username_over_255_bytes_is_rejected_before_any_write() {
        let (mut client, mut _server) = pair();
        let creds = Socks5Credentials::new("a".repeat(256), "p");
        let result = negotiate(&mut client, "p:1", Some(&creds), "h", 1, deadline()).await;
        assert!(matches!(result, Err(ProxySocks5Error::UsernameTooLong)));
    }

    #[tokio::test]
    async fn password_over_255_bytes_is_rejected_before_any_write() {
        let (mut client, mut _server) = pair();
        let creds = Socks5Credentials::new("u", "p".repeat(256));
        let result = negotiate(&mut client, "p:1", Some(&creds), "h", 1, deadline()).await;
        assert!(matches!(result, Err(ProxySocks5Error::PasswordTooLong)));
    }

    #[tokio::test]
    async fn domain_over_255_bytes_is_rejected_before_any_write() {
        let (mut client, mut _server) = pair();
        let long_host = "a".repeat(256);
        let result = negotiate(&mut client, "p:1", None, &long_host, 1, deadline()).await;
        assert!(matches!(
            result,
            Err(ProxySocks5Error::TargetTooLong { .. })
        ));
    }

    #[tokio::test]
    async fn timeout_during_method_negotiation() {
        let (mut client, server) = pair();
        // Server never reads or replies; hold the handle open so the pipe
        // doesn't look closed, just silent.
        let _server = server;
        let short_deadline = Instant::now() + Duration::from_millis(50);
        let result = negotiate(&mut client, "p:1", None, "h", 1, short_deadline).await;
        match result {
            Err(ProxySocks5Error::Timeout { phase, .. }) => {
                assert_eq!(phase, "method negotiation")
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_during_authentication() {
        let (mut client, mut server) = pair();
        let creds = Socks5Credentials::new("u", "p");
        let short_deadline = Instant::now() + Duration::from_millis(80);
        let client_fut = negotiate(&mut client, "p:1", Some(&creds), "h", 1, short_deadline);
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            server
                .write_all(&[VERSION, METHOD_USER_PASS])
                .await
                .unwrap();
            // Then go silent forever during the auth sub-negotiation.
            std::future::pending::<()>().await;
        };
        let result = tokio::select! {
            r = client_fut => r,
            _ = server_fut => unreachable!(),
        };
        match result {
            Err(ProxySocks5Error::Timeout { phase, .. }) => assert_eq!(phase, "authentication"),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_waiting_for_connect_reply() {
        let (mut client, mut server) = pair();
        let short_deadline = Instant::now() + Duration::from_millis(80);
        let client_fut = negotiate(&mut client, "p:1", None, "h", 1, short_deadline);
        let server_fut = async {
            let mut greeting = [0u8; 3];
            server.read_exact(&mut greeting).await.unwrap();
            server.write_all(&[VERSION, METHOD_NO_AUTH]).await.unwrap();
            let mut buf = [0u8; 64];
            let mut total = 0;
            while total < 8 {
                total += server.read(&mut buf[total..]).await.unwrap();
            }
            // Then go silent instead of sending the CONNECT reply.
            std::future::pending::<()>().await;
        };
        let result = tokio::select! {
            r = client_fut => r,
            _ = server_fut => unreachable!(),
        };
        match result {
            Err(ProxySocks5Error::Timeout { phase, .. }) => assert_eq!(phase, "CONNECT reply"),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancellation_during_negotiation_drops_cleanly() {
        let (mut client, server) = pair();
        let _server = server; // held open, never responds
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let negotiation = negotiate(&mut client, "p:1", None, "h", 1, deadline());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = cancel_tx.send(());
        });
        let outcome = tokio::select! {
            _ = negotiation => panic!("negotiation should not complete before cancellation"),
            _ = cancel_rx => "cancelled",
        };
        assert_eq!(outcome, "cancelled");
        // Dropping `negotiation` above must not panic, hang, or leak — the
        // fact this test function returns at all proves that.
    }

    #[tokio::test]
    async fn env_config_absent_when_host_unset() {
        // Use a prefix guaranteed not to collide with real environment
        // variables or other tests running in the same process.
        let prefix = "MINERIDER_TEST_ABSENT_SOCKS5_UNIQUE";
        std::env::remove_var(format!("{prefix}_HOST"));
        assert!(Socks5ProxyConfig::from_env(prefix).unwrap().is_none());
    }

    #[tokio::test]
    async fn env_config_full_roundtrip() {
        let prefix = "MINERIDER_TEST_FULL_SOCKS5_UNIQUE";
        std::env::set_var(format!("{prefix}_HOST"), "proxy.invalid");
        std::env::set_var(format!("{prefix}_PORT"), "1080");
        std::env::set_var(format!("{prefix}_USERNAME"), "alice");
        std::env::set_var(format!("{prefix}_PASSWORD"), "hunter2");
        let config = Socks5ProxyConfig::from_env(prefix).unwrap().unwrap();
        assert_eq!(config.host, "proxy.invalid");
        assert_eq!(config.port, 1080);
        assert_eq!(
            config.credentials.unwrap().password.expose_secret(),
            "hunter2"
        );
        for var in ["_HOST", "_PORT", "_USERNAME", "_PASSWORD"] {
            std::env::remove_var(format!("{prefix}{var}"));
        }
    }

    #[tokio::test]
    async fn env_config_rejects_partial_credentials() {
        let prefix = "MINERIDER_TEST_PARTIAL_SOCKS5_UNIQUE";
        std::env::set_var(format!("{prefix}_HOST"), "proxy.invalid");
        std::env::set_var(format!("{prefix}_PORT"), "1080");
        std::env::set_var(format!("{prefix}_USERNAME"), "alice");
        std::env::remove_var(format!("{prefix}_PASSWORD"));
        let result = Socks5ProxyConfig::from_env(prefix);
        assert!(matches!(result, Err(EnvConfigError::PartialCredentials)));
        for var in ["_HOST", "_PORT", "_USERNAME"] {
            std::env::remove_var(format!("{prefix}{var}"));
        }
    }
}
