//! Integration tests for SOCKS5 transport: real loopback TCP throughout,
//! against the independent fake proxy in `tests/socks5_support`. Unit-level
//! protocol edge cases (every reply code, malformed/truncated frames,
//! method-negotiation/auth/CONNECT-reply timeouts, length limits, secret
//! redaction) live in `src/network/socks5.rs`'s own `#[cfg(test)]` module,
//! exercised against `tokio::io::duplex` fakes; this file proves the real
//! socket path end to end: full `Client`/`ClientSupervisor` through a real
//! proxy, arbitrary byte forwarding, and concurrency.
//!
//! No real proxy or Minecraft server is ever contacted here.

mod common;
mod socks5_support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::MockServer;
use minerider::core::client::{Client, ClientConfig};
use minerider::core::error::MineRiderError;
use minerider::core::supervisor::{
    ClientSupervisor, ReconnectPolicy, RetryLimit, SupervisorOutcome, SupervisorStatus,
};
use minerider::network::connection::Connection;
use minerider::network::socks5::{ProxySocks5Error, Socks5Credentials, Socks5ProxyConfig};
use minerider_protocol::buffer::PacketWriter;
use minerider_protocol::generated::v1_21_4::{configuration, handshaking, login};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use socks5_support::{AuthRequirement, FakeSocks5Server, Outcome};

/// Mission item 16: a full real `Client::connect` (handshake -> login ->
/// configuration -> play) through a real SOCKS5 tunnel, and mission's own
/// "the proxy confirms the requested destination is the real Minecraft
/// target" requirement — checked by reading back what the fake proxy
/// actually decoded off the wire.
#[tokio::test]
async fn full_mock_minecraft_handshake_through_socks5_tunnel() {
    let backend = MockServer::start_plain().await;
    let proxy = FakeSocks5Server::start(AuthRequirement::None, Outcome::Relay, 1).await;

    let proxy_cfg = Arc::new(Socks5ProxyConfig::new("127.0.0.1", proxy.port));
    let cfg = ClientConfig::new("127.0.0.1", backend.port, "SocksBot").with_socks5_proxy(proxy_cfg);

    let backend_port = backend.port;
    let mut client = Client::connect(&cfg)
        .await
        .expect("client connect through SOCKS5 should succeed");
    // `start_plain` closes the connection after one keep-alive; that's the
    // expected, successful end of this scenario (same contract
    // `tests/login_flow.rs` documents for its own mock server).
    let err = client
        .run()
        .await
        .expect_err("run() must end when the mock server closes");
    assert!(matches!(err, MineRiderError::ConnectionClosed));
    backend.finish().await.expect("mock server task");

    let targets = proxy.requested_targets().await;
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].host, "127.0.0.1");
    assert_eq!(
        targets[0].port, backend_port,
        "the proxy must be asked to CONNECT to the real Minecraft target, not its own endpoint"
    );
}

/// Authenticated (username/password) relay success.
#[tokio::test]
async fn authenticated_relay_success() {
    let backend = MockServer::start_plain().await;
    let proxy = FakeSocks5Server::start(
        AuthRequirement::UserPass {
            username: "alice".to_string(),
            password: "hunter2".to_string(),
        },
        Outcome::Relay,
        1,
    )
    .await;

    let proxy_cfg = Arc::new(
        Socks5ProxyConfig::new("127.0.0.1", proxy.port)
            .with_credentials(Socks5Credentials::new("alice", "hunter2")),
    );
    let cfg =
        ClientConfig::new("127.0.0.1", backend.port, "SocksAuthBot").with_socks5_proxy(proxy_cfg);

    let mut client = Client::connect(&cfg)
        .await
        .expect("authenticated SOCKS5 connect should succeed");
    let err = client
        .run()
        .await
        .expect_err("run() must end when the mock server closes");
    assert!(matches!(err, MineRiderError::ConnectionClosed));
    backend.finish().await.expect("mock server task");
}

/// Wrong credentials must fail the connect with a typed, permanent error —
/// never silently fall back to no-auth or hang.
#[tokio::test]
async fn wrong_credentials_are_rejected() {
    let proxy = FakeSocks5Server::start(
        AuthRequirement::UserPass {
            username: "alice".to_string(),
            password: "hunter2".to_string(),
        },
        Outcome::Relay,
        1,
    )
    .await;
    let proxy_cfg = Arc::new(
        Socks5ProxyConfig::new("127.0.0.1", proxy.port)
            .with_credentials(Socks5Credentials::new("alice", "wrong-password")),
    );
    let cfg = ClientConfig::new("127.0.0.1", 25565, "SocksBadAuthBot").with_socks5_proxy(proxy_cfg);

    match Client::connect(&cfg).await {
        Err(MineRiderError::Proxy(ProxySocks5Error::AuthenticationRejected { .. })) => {}
        Ok(_) => panic!("wrong credentials must not succeed"),
        Err(other) => panic!("expected AuthenticationRejected, got {other:?}"),
    }
}

/// Mission item 17: the existing direct-connection path (no proxy
/// configured at all) must keep working unchanged.
#[tokio::test]
async fn direct_connection_regression() {
    let backend = MockServer::start_plain().await;
    let cfg = ClientConfig::new("127.0.0.1", backend.port, "DirectBot");
    assert!(cfg.proxy.is_none());

    let mut client = Client::connect(&cfg)
        .await
        .expect("direct connect must keep working with no proxy configured");
    let err = client
        .run()
        .await
        .expect_err("run() must end when the mock server closes");
    assert!(matches!(err, MineRiderError::ConnectionClosed));
    backend.finish().await.expect("mock server task");
}

/// Mission item 18: `ClientSupervisor` reconnecting after a transient
/// disconnect must go through the *same configured* SOCKS5 route both
/// times, not silently fall back to a direct connection.
#[tokio::test]
async fn supervisor_reconnects_through_the_same_socks5_route() {
    let backend_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind backend");
    let backend_port = backend_listener.local_addr().unwrap().port();
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_clone = attempts.clone();
    let (conn_tx, _kept_alive) = tokio::sync::mpsc::unbounded_channel::<Connection>();

    tokio::spawn(async move {
        let (stream, _) = backend_listener.accept().await.expect("accept #1");
        attempts_clone.fetch_add(1, Ordering::SeqCst);
        drop(minimal_login_and_configuration(stream).await);

        let (stream, _) = backend_listener.accept().await.expect("accept #2");
        attempts_clone.fetch_add(1, Ordering::SeqCst);
        let conn = minimal_login_and_configuration(stream).await;
        let _ = conn_tx.send(conn);
    });

    // max_connections=2: the proxy itself must be reached twice too.
    let proxy = FakeSocks5Server::start(AuthRequirement::None, Outcome::Relay, 2).await;
    let proxy_cfg = Arc::new(Socks5ProxyConfig::new("127.0.0.1", proxy.port));
    let cfg =
        ClientConfig::new("127.0.0.1", backend_port, "SocksReconnect").with_socks5_proxy(proxy_cfg);
    let policy = ReconnectPolicy::enabled()
        .with_initial_delay(Duration::from_millis(5))
        .with_max_delay(Duration::from_millis(5))
        .with_max_retries(RetryLimit::Count(3));
    let (supervisor, handle) = ClientSupervisor::new(cfg, policy);
    let run_handle = tokio::spawn(supervisor.run());

    let mut status_rx = handle.status();
    let mut connected_count = 0;
    let wait = async {
        loop {
            status_rx.changed().await.expect("status channel open");
            if *status_rx.borrow() == SupervisorStatus::Connected {
                connected_count += 1;
                if connected_count == 2 {
                    break;
                }
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("must reconnect through the proxy and reach Connected a second time");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        proxy.accepted_connections.load(Ordering::SeqCst),
        2,
        "reconnect must go through the proxy again, not fall back to direct"
    );

    handle.stop();
    let outcome = tokio::time::timeout(Duration::from_secs(5), run_handle)
        .await
        .expect("must stop promptly")
        .expect("no panic");
    assert!(matches!(outcome, SupervisorOutcome::Cancelled));
}

/// Mission item 15: arbitrary bytes, not just a Minecraft handshake, forward
/// unmodified through the tunnel — proven independently of the Minecraft
/// protocol with a plain echo backend.
#[tokio::test]
async fn arbitrary_bytes_forward_unmodified_through_the_tunnel() {
    let echo_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut stream, _) = echo_listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        loop {
            let n = match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            if stream.write_all(&buf[..n]).await.is_err() {
                return;
            }
        }
    });

    let proxy = FakeSocks5Server::start(AuthRequirement::None, Outcome::Relay, 1).await;
    let proxy_cfg = Socks5ProxyConfig::new("127.0.0.1", proxy.port);
    let mut tunnel = minerider::network::socks5::connect(
        &proxy_cfg,
        "127.0.0.1",
        echo_port,
        Duration::from_secs(5),
    )
    .await
    .expect("tunnel establishment should succeed");

    let payload: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
    tunnel.write_all(&payload).await.expect("write payload");
    let mut echoed = vec![0u8; payload.len()];
    tunnel
        .read_exact(&mut echoed)
        .await
        .expect("read echoed payload");
    assert_eq!(echoed, payload, "bytes must forward unmodified");
}

/// Mission item 21: 100 concurrent local tunnels must all complete
/// (relay + echo round-trip) without deadlock, and every spawned task must
/// actually finish (proving no leaked task).
#[tokio::test]
async fn hundred_concurrent_tunnels_no_deadlock_or_leak() {
    const N: usize = 100;
    let echo_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = echo_listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 256];
                loop {
                    let n = match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    if stream.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });

    let proxy = FakeSocks5Server::start(AuthRequirement::None, Outcome::Relay, N).await;
    let proxy_cfg = Arc::new(Socks5ProxyConfig::new("127.0.0.1", proxy.port));

    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let proxy_cfg = proxy_cfg.clone();
        handles.push(tokio::spawn(async move {
            let mut tunnel = minerider::network::socks5::connect(
                &proxy_cfg,
                "127.0.0.1",
                echo_port,
                Duration::from_secs(10),
            )
            .await
            .unwrap_or_else(|e| panic!("tunnel {i} failed: {e}"));
            let payload = format!("tunnel-{i}").into_bytes();
            tunnel.write_all(&payload).await.unwrap();
            let mut echoed = vec![0u8; payload.len()];
            tunnel.read_exact(&mut echoed).await.unwrap();
            assert_eq!(echoed, payload);
        }));
    }

    for (i, handle) in handles.into_iter().enumerate() {
        tokio::time::timeout(Duration::from_secs(15), handle)
            .await
            .unwrap_or_else(|_| panic!("tunnel task {i} did not finish (possible deadlock)"))
            .unwrap_or_else(|e| panic!("tunnel task {i} panicked: {e}"));
    }
}

/// Mission item 10 ("timeout during proxy TCP connection"), made
/// deterministic rather than dependent on real network hang timing — the
/// same trade-off `network::connection`'s own tests document for its write
/// timeout: an already-expired deadline exercises the exact same
/// `tokio::time::timeout_at` composition around the real `TcpStream::connect`
/// future without needing an actually-slow or black-holed peer.
#[tokio::test]
async fn proxy_tcp_connect_timeout_is_reported_with_the_right_phase() {
    let proxy_cfg = Socks5ProxyConfig::new("127.0.0.1", 1); // never dialed in time
    let result = minerider::network::socks5::connect(
        &proxy_cfg,
        "127.0.0.1",
        25565,
        Duration::from_nanos(1),
    )
    .await;
    match result {
        Err(ProxySocks5Error::Timeout { phase, .. }) => assert_eq!(phase, "proxy TCP connect"),
        other => panic!("expected Timeout(proxy TCP connect), got {other:?}"),
    }
}

/// A misconfigured/unreachable proxy is a normal, real (not injected)
/// connection failure, and must classify as transient — retrying later may
/// succeed once the proxy is reachable.
#[tokio::test]
async fn unreachable_proxy_is_transient() {
    // Bind then drop: a port nobody listens on.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let proxy_cfg = Socks5ProxyConfig::new("127.0.0.1", port);
    let result =
        minerider::network::socks5::connect(&proxy_cfg, "127.0.0.1", 25565, Duration::from_secs(5))
            .await;
    let error = result.expect_err("nothing listens on this port");
    assert_eq!(
        error.retry_class(),
        minerider::core::error::RetryClass::Transient
    );
}

/// Manual, opt-in smoke test against a **real** SOCKS5 proxy and a real
/// Minecraft server, both configured entirely through environment
/// variables. Never runs as part of `cargo test`/`cargo test --workspace`
/// (that's what `#[ignore]` guarantees — CI never passes `--ignored`) and
/// never prints a secret: only connection *outcomes* (success/failure/error
/// variant) are printed, never `MINERIDER_LIVE_SOCKS5_PASSWORD`'s value or
/// even whether it was set.
///
/// Prerequisite: rotate any credential that was ever pasted into a chat or
/// logged in plaintext before pointing this at a real proxy — this test
/// cannot verify that for you.
///
/// Run explicitly with:
///
/// ```text
/// MINERIDER_LIVE_SOCKS5_HOST=... \
/// MINERIDER_LIVE_SOCKS5_PORT=... \
/// MINERIDER_LIVE_SOCKS5_USERNAME=... \
/// MINERIDER_LIVE_SOCKS5_PASSWORD=... \
/// MINERIDER_LIVE_MC_HOST=... \
/// MINERIDER_LIVE_MC_PORT=... \
/// MINERIDER_LIVE_MC_USERNAME=SomeOfflineName \
/// cargo test --test socks5 live_socks5_smoke -- --ignored --nocapture
/// ```
///
/// `MINERIDER_LIVE_SOCKS5_USERNAME`/`_PASSWORD` are optional (omit both for
/// a no-auth proxy). Skips (does not fail) if `MINERIDER_LIVE_SOCKS5_HOST`
/// or the `MINERIDER_LIVE_MC_*` variables are unset, so an accidental
/// `--ignored` run in an environment without them is a no-op, not a
/// spurious failure.
#[tokio::test]
#[ignore = "manual: requires a real SOCKS5 proxy and real Minecraft server, configured via env vars"]
async fn live_socks5_smoke() {
    let proxy = match Socks5ProxyConfig::from_env("MINERIDER_LIVE_SOCKS5") {
        Ok(Some(proxy)) => proxy,
        Ok(None) => {
            println!("skipping: MINERIDER_LIVE_SOCKS5_HOST not set");
            return;
        }
        Err(error) => panic!("invalid live SOCKS5 env configuration: {error}"),
    };
    let Ok(mc_host) = std::env::var("MINERIDER_LIVE_MC_HOST") else {
        println!("skipping: MINERIDER_LIVE_MC_HOST not set");
        return;
    };
    let mc_port: u16 = std::env::var("MINERIDER_LIVE_MC_PORT")
        .expect("MINERIDER_LIVE_MC_PORT must be set alongside MINERIDER_LIVE_MC_HOST")
        .parse()
        .expect("MINERIDER_LIVE_MC_PORT must be a valid port number");
    let username =
        std::env::var("MINERIDER_LIVE_MC_USERNAME").unwrap_or_else(|_| "MineriderSmoke".into());

    println!(
        "connecting to {mc_host}:{mc_port} through SOCKS5 proxy {}:{} (auth: {})",
        proxy.host,
        proxy.port,
        if proxy.credentials.is_some() {
            "yes"
        } else {
            "no"
        }
    );
    let cfg = ClientConfig::new(mc_host, mc_port, username).with_socks5_proxy(Arc::new(proxy));
    match Client::connect(&cfg).await {
        Ok(client) => println!(
            "SUCCESS: reached {:?} as {:?} (uuid {:032x})",
            client.state(),
            client.username,
            client.uuid
        ),
        Err(error) => {
            println!("connect failed (see error variant/message, never a secret): {error}")
        }
    }
}

// ---------------------------------------------------------------------
// Shared backend handshake helper (mirrors tests/supervisor.rs's own
// hand-rolled helper — a real second accepted connection is needed for the
// reconnect test, which `tests/common::MockServer` doesn't support since it
// only ever accepts once).
// ---------------------------------------------------------------------

async fn minimal_login_and_configuration(stream: TcpStream) -> Connection {
    let mut conn = Connection::from_tcp_stream(stream).expect("wrap stream");

    let hs = conn.read_packet().await.expect("read handshake");
    assert_eq!(hs.id, handshaking::SERVERBOUND_SET_PROTOCOL_ID);
    let ls = conn.read_packet().await.expect("read login start");
    assert_eq!(ls.id, login::SERVERBOUND_LOGIN_START_ID);

    let mut w = PacketWriter::new();
    w.put_uuid(0x1111_2222_3333_4444_5555_6666_7777_8888);
    w.put_string("SocksReconnect").unwrap();
    w.put_varint(0);
    conn.send_packet(login::CLIENTBOUND_SUCCESS_ID, &w.into_inner())
        .await
        .expect("send login success");

    let ack = conn.read_packet().await.expect("read login acknowledged");
    assert_eq!(ack.id, login::SERVERBOUND_LOGIN_ACKNOWLEDGED_ID);

    let settings = conn.read_packet().await.expect("read settings");
    assert_eq!(settings.id, configuration::SERVERBOUND_SETTINGS_ID);
    let brand = conn.read_packet().await.expect("read brand");
    assert_eq!(brand.id, configuration::SERVERBOUND_CUSTOM_PAYLOAD_ID);

    send_dimension_registry(&mut conn).await;

    conn.send_packet(configuration::CLIENTBOUND_FINISH_CONFIGURATION_ID, &[])
        .await
        .expect("send finish configuration");
    let fin = conn
        .read_packet()
        .await
        .expect("read finish configuration ack");
    assert_eq!(fin.id, configuration::SERVERBOUND_FINISH_CONFIGURATION_ID);

    conn
}

async fn send_dimension_registry(conn: &mut Connection) {
    use minerider_protocol::generated::v1_21_4::configuration::{
        PacketRegistryData, PacketRegistryDataEntriesItem,
    };
    use minerider_protocol::nbt::Nbt;
    use minerider_protocol::traits::Encode;

    let packet = PacketRegistryData {
        id: "minecraft:dimension_type".to_string(),
        entries: vec![PacketRegistryDataEntriesItem {
            key: "minecraft:overworld".to_string(),
            value: Some(Nbt::Compound(vec![
                ("min_y".to_string(), Nbt::Int(-64)),
                ("height".to_string(), Nbt::Int(384)),
                ("logical_height".to_string(), Nbt::Int(384)),
                ("coordinate_scale".to_string(), Nbt::Double(1.0)),
                ("ultrawarm".to_string(), Nbt::Byte(0)),
                ("has_ceiling".to_string(), Nbt::Byte(0)),
            ])),
        }],
    };
    let mut w = PacketWriter::new();
    packet.encode(&mut w).expect("encode registry");
    conn.send_packet(configuration::CLIENTBOUND_REGISTRY_DATA_ID, &w.into_inner())
        .await
        .expect("send registry");
}
