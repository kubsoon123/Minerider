//! A minimal fake SOCKS5 server for the full-runtime proxy-group scenario
//! and its correctness tests. Adapted from `tests/socks5_support/mod.rs`
//! (deliberately not shared with it — `src/bin/*` and `tests/*` are
//! separate compilation units, and this repo's established convention,
//! e.g. `tests/actions.rs` vs. `tests/supervisor.rs`, is small mock-infra
//! helpers are duplicated per consumer rather than forced into one shared
//! crate). Lives inside the library (not `tests/`) specifically so both
//! `src/bin/lua_runtime_benchmark.rs` and `tests/lua_runtime.rs` can use
//! the exact same implementation instead of a third copy.
//!
//! Independently implemented from `minerider::network::socks5`'s own
//! encoder — this stands in for a real, separate SOCKS5 server. On a
//! successful `CONNECT` it really dials the requested target and relays
//! bytes bidirectionally, so proxy-group tests prove real routing, not
//! just a completed handshake.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const VERSION: u8 = 0x05;
const METHOD_NO_AUTH: u8 = 0x00;
const METHOD_NO_ACCEPTABLE: u8 = 0xFF;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

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
    /// Starts a no-auth accept loop bound to `127.0.0.1:0`, relaying up to
    /// `max_connections` real `CONNECT`s to their requested target.
    pub async fn start(max_connections: usize) -> Self {
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
                let targets = targets_clone.clone();
                tokio::spawn(async move {
                    let _ = handle_one(stream, targets).await;
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

    pub async fn finish(self) {
        let _ = self.handle.await;
    }
}

async fn handle_one(
    mut stream: TcpStream,
    targets: Arc<Mutex<Vec<RequestedTarget>>>,
) -> std::io::Result<()> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    let mut methods = vec![0u8; head[1] as usize];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&METHOD_NO_AUTH) {
        stream.write_all(&[VERSION, METHOD_NO_ACCEPTABLE]).await?;
        return Ok(());
    }
    stream.write_all(&[VERSION, METHOD_NO_AUTH]).await?;

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

    let backend = match TcpStream::connect((target_host.as_str(), target_port)).await {
        Ok(backend) => backend,
        Err(_) => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relays_a_real_connection_and_records_the_requested_target() {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = echo.accept().await.unwrap();
            let mut buf = [0u8; 5];
            stream.read_exact(&mut buf).await.unwrap();
            stream.write_all(&buf).await.unwrap();
        });

        let proxy = FakeSocks5Server::start(4).await;
        let mut conn = TcpStream::connect(("127.0.0.1", proxy.port)).await.unwrap();
        conn.write_all(&[VERSION, 1, METHOD_NO_AUTH]).await.unwrap();
        let mut reply = [0u8; 2];
        conn.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [VERSION, METHOD_NO_AUTH]);

        let mut request = vec![VERSION, 0x01, 0x00, ATYP_DOMAIN, 9];
        request.extend_from_slice(b"127.0.0.1");
        request.extend_from_slice(&echo_port.to_be_bytes());
        conn.write_all(&request).await.unwrap();
        let mut connect_reply = [0u8; 10];
        conn.read_exact(&mut connect_reply).await.unwrap();
        assert_eq!(connect_reply[1], 0x00);

        conn.write_all(b"hello").await.unwrap();
        let mut echoed = [0u8; 5];
        conn.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"hello");

        let targets = proxy.requested_targets().await;
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].port, echo_port);
    }
}
