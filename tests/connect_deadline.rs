//! Tests for `ClientConfig::connect_deadline`: the overall TCP-connect →
//! handshake → login → configuration budget, independent of (and tighter
//! than) the per-read timeout inside each stage.
//!
//! These deliberately don't use `tests/common`'s full mock server: the point
//! here is a server that *stalls* (never responds after accepting), not one
//! that completes the flow, so a minimal raw listener is clearer.

use std::time::Duration;

use minerider::core::client::{Client, ClientConfig};
use minerider::core::error::MineRiderError;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

/// A server that stalls forever right after the handshake — never sends
/// Encryption Request, Set Compression, or Login Success. Real bytes are
/// exchanged over real loopback I/O (fast, not virtualized); only the
/// *deadline* is virtualized, via a clock pause taken after a short real
/// settle delay (see the comment below on why paused-time attributes alone
/// aren't used here).
async fn stall_after_handshake(listener: TcpListener) {
    let (mut stream, _) = listener.accept().await.expect("accept");
    let mut buf = [0u8; 4096];
    // Handshake.
    let _ = stream.read(&mut buf).await.expect("read handshake");
    // Login Start.
    let _ = stream.read(&mut buf).await.expect("read login start");
    // Never respond; hold the connection open indefinitely.
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn connect_deadline_fires_during_login_stall() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(stall_after_handshake(listener));

    let cfg = ClientConfig::new("127.0.0.1", port, "DeadlineTestBot")
        .with_connect_deadline(Duration::from_secs(5));

    let connect = tokio::spawn(async move { Client::connect(&cfg).await });

    // Let the real handshake/login-start exchange actually happen over the
    // real (loopback, effectively instant) socket before virtualizing time —
    // this avoids computing the deadline against a clock we then immediately
    // rewind past before the client task has even started running.
    tokio::time::sleep(Duration::from_millis(100)).await;

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(6)).await;

    let result = connect.await.expect("connect task must not panic");
    match result {
        Ok(_client) => panic!("connect must not succeed against a server that never responds"),
        Err(MineRiderError::Timeout(msg)) => {
            assert!(
                msg.contains("login"),
                "deadline error must name the login stage, got: {msg}"
            );
            assert!(
                msg.contains("deadline"),
                "deadline error must say it's the overall deadline, got: {msg}"
            );
        }
        Err(other) => panic!("expected a connect-deadline Timeout during login, got: {other}"),
    }
}

#[tokio::test]
async fn connect_succeeds_well_within_a_generous_deadline() {
    // Realistic path: a full mock login (reusing the shared mock server, a
    // real socket, real encryption/compression) with a generous deadline
    // must succeed normally — the deadline must never interfere with an
    // ordinary, healthy connect.
    let server = common::MockServer::start_plain().await;
    let cfg = ClientConfig::new("127.0.0.1", server.port, "DeadlineOkBot")
        .with_connect_deadline(Duration::from_secs(30));
    let client = Client::connect(&cfg)
        .await
        .expect("connect within deadline");
    assert_eq!(client.username, "MockPlayer");
}

#[path = "common/mod.rs"]
mod common;
