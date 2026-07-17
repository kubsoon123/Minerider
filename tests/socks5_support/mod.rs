//! A deterministic, independently-implemented fake SOCKS5 server for
//! integration tests. Deliberately does not share any decode/encode logic
//! with `minerider::network::socks5` — it is meant to stand in for a real,
//! independent SOCKS5 implementation on the other end of the wire, not to
//! prove our own encoder round-trips through our own decoder.
//!
//! On a successful `CONNECT`, [`Outcome::Relay`] makes this a genuine,
//! minimal working SOCKS5 proxy: it dials the *requested* target for real
//! and relays raw bytes bidirectionally, so tests can prove real
//! byte-for-byte forwarding (including a full mock Minecraft handshake)
//! through the tunnel, not just that the negotiation handshake completes.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const VERSION: u8 = 0x05;
const METHOD_NO_AUTH: u8 = 0x00;
const METHOD_USER_PASS: u8 = 0x02;
const METHOD_NO_ACCEPTABLE: u8 = 0xFF;
const USER_PASS_VERSION: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

#[derive(Clone)]
pub enum AuthRequirement {
    None,
    UserPass { username: String, password: String },
}

#[derive(Clone, Copy)]
pub enum Outcome {
    /// Accept the CONNECT and really relay bytes to the requested target.
    Relay,
    /// Reply with this SOCKS5 REP code and close (no relay).
    Reply(u8),
}

/// One requested `CONNECT` target, as decoded off the wire by this fake
/// server's own independent parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedTarget {
    pub host: String,
    pub port: u16,
}

pub struct FakeSocks5Server {
    pub port: u16,
    pub requested_targets: Arc<Mutex<Vec<RequestedTarget>>>,
    pub accepted_connections: Arc<AtomicUsize>,
    handle: JoinHandle<()>,
}

impl FakeSocks5Server {
    /// Starts an accept loop bound to `127.0.0.1:0`, handling up to
    /// `max_connections` connections (each with `auth`/`outcome`), then
    /// stopping. Use a large `max_connections` for a server meant to stay up
    /// for the test's whole duration.
    pub async fn start(auth: AuthRequirement, outcome: Outcome, max_connections: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake SOCKS5 server");
        let port = listener.local_addr().expect("local addr").port();
        let requested_targets = Arc::new(Mutex::new(Vec::new()));
        let accepted_connections = Arc::new(AtomicUsize::new(0));
        let targets_clone = requested_targets.clone();
        let count_clone = accepted_connections.clone();
        let handle = tokio::spawn(async move {
            for _ in 0..max_connections {
                let (stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                count_clone.fetch_add(1, Ordering::SeqCst);
                let auth = auth.clone();
                let targets = targets_clone.clone();
                tokio::spawn(async move {
                    let _ = handle_one(stream, auth, outcome, targets).await;
                });
            }
        });
        FakeSocks5Server {
            port,
            requested_targets,
            accepted_connections,
            handle,
        }
    }

    pub async fn requested_targets(&self) -> Vec<RequestedTarget> {
        self.requested_targets.lock().await.clone()
    }

    /// Waits for the accept loop to finish (all `max_connections` handled).
    pub async fn finish(self) {
        let _ = self.handle.await;
    }
}

async fn handle_one(
    mut stream: TcpStream,
    auth: AuthRequirement,
    outcome: Outcome,
    targets: Arc<Mutex<Vec<RequestedTarget>>>,
) -> std::io::Result<()> {
    // Method negotiation.
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    let nmethods = head[1] as usize;
    let mut methods = vec![0u8; nmethods];
    stream.read_exact(&mut methods).await?;

    let required = match &auth {
        AuthRequirement::None => METHOD_NO_AUTH,
        AuthRequirement::UserPass { .. } => METHOD_USER_PASS,
    };
    if !methods.contains(&required) {
        stream.write_all(&[VERSION, METHOD_NO_ACCEPTABLE]).await?;
        return Ok(());
    }
    stream.write_all(&[VERSION, required]).await?;

    if let AuthRequirement::UserPass { username, password } = &auth {
        let mut sub_head = [0u8; 2];
        stream.read_exact(&mut sub_head).await?;
        let mut uname = vec![0u8; sub_head[1] as usize];
        stream.read_exact(&mut uname).await?;
        let mut plen = [0u8; 1];
        stream.read_exact(&mut plen).await?;
        let mut pass = vec![0u8; plen[0] as usize];
        stream.read_exact(&mut pass).await?;
        let ok = uname == username.as_bytes() && pass == password.as_bytes();
        stream
            .write_all(&[USER_PASS_VERSION, if ok { 0x00 } else { 0x01 }])
            .await?;
        if !ok {
            return Ok(());
        }
    }

    // CONNECT request.
    let mut req_head = [0u8; 4];
    stream.read_exact(&mut req_head).await?;
    let target_host = match req_head[3] {
        ATYP_IPV4 => {
            let mut addr = [0u8; 4];
            stream.read_exact(&mut addr).await?;
            std::net::Ipv4Addr::from(addr).to_string()
        }
        ATYP_IPV6 => {
            let mut addr = [0u8; 16];
            stream.read_exact(&mut addr).await?;
            std::net::Ipv6Addr::from(addr).to_string()
        }
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut domain = vec![0u8; len[0] as usize];
            stream.read_exact(&mut domain).await?;
            String::from_utf8_lossy(&domain).into_owned()
        }
        _ => return Ok(()),
    };
    let mut port_buf = [0u8; 2];
    stream.read_exact(&mut port_buf).await?;
    let target_port = u16::from_be_bytes(port_buf);
    targets.lock().await.push(RequestedTarget {
        host: target_host.clone(),
        port: target_port,
    });

    match outcome {
        Outcome::Reply(code) => {
            stream
                .write_all(&[VERSION, code, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
                .await?;
            Ok(())
        }
        Outcome::Relay => {
            let backend = match TcpStream::connect((target_host.as_str(), target_port)).await {
                Ok(backend) => backend,
                Err(_) => {
                    // 0x04 = host unreachable.
                    stream
                        .write_all(&[VERSION, 0x04, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
                        .await?;
                    return Ok(());
                }
            };
            stream
                .write_all(&[VERSION, 0x00, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
                .await?;
            let mut backend = backend;
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut backend).await;
            Ok(())
        }
    }
}
