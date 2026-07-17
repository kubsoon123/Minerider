//! Integration tests for `core::supervisor::ClientSupervisor`: reconnect
//! policy, retry classification, cancellation and lifecycle observability,
//! all against local mock servers (no real network, no real credentials).
//!
//! These deliberately use small hand-rolled raw-socket helpers rather than
//! `tests/common`'s `MockServer` (which binds and accepts exactly once):
//! reconnect scenarios need a server that accepts more than one connection
//! in sequence, and rejection/stall scenarios need one that never reaches
//! play state at all.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use minerider::core::client::ClientConfig;
use minerider::core::error::{MineRiderError, RetryClass};
use minerider::core::supervisor::{
    ClientSupervisor, ControlError, ReconnectPolicy, RetryLimit, SupervisorOutcome,
    SupervisorStatus,
};
use minerider::minecraft::event::BotEvent;
use minerider::network::connection::Connection;
use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::generated::v1_21_4::{configuration, handshaking, login, play};
use minerider_protocol::traits::{Decode, Encode};
use tokio::net::{TcpListener, TcpStream};

/// Waits until `status_rx` reports `Connected`, ignoring earlier statuses.
async fn wait_until_connected(status_rx: &mut tokio::sync::watch::Receiver<SupervisorStatus>) {
    loop {
        if *status_rx.borrow() == SupervisorStatus::Connected {
            return;
        }
        status_rx.changed().await.expect("status channel open");
    }
}

/// Runs the minimal exchange needed for `Client::connect` to succeed —
/// handshake, login (no encryption/compression), then configuration
/// finishing immediately — and returns the still-open `Connection`. No play
/// state traffic at all, since `Client::connect` returns as soon as
/// configuration finishes.
async fn minimal_login_and_configuration(stream: TcpStream) -> Connection {
    let mut conn = Connection::from_tcp_stream(stream).expect("wrap stream");

    let hs = conn.read_packet().await.expect("read handshake");
    assert_eq!(hs.id, handshaking::SERVERBOUND_SET_PROTOCOL_ID);
    let ls = conn.read_packet().await.expect("read login start");
    assert_eq!(ls.id, login::SERVERBOUND_LOGIN_START_ID);

    let mut w = PacketWriter::new();
    w.put_uuid(0x1111_2222_3333_4444_5555_6666_7777_8888);
    w.put_string("ReconnectBot").unwrap();
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

/// Sends the minimal `minecraft:dimension_type` registry the play-state
/// Login packet needs to resolve its dimension index against.
async fn send_dimension_registry(conn: &mut Connection) {
    use minerider_protocol::nbt::Nbt;

    let packet = configuration::PacketRegistryData {
        id: "minecraft:dimension_type".to_string(),
        entries: vec![configuration::PacketRegistryDataEntriesItem {
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
    packet.encode(&mut w).expect("encode registry data");
    conn.send_packet(configuration::CLIENTBOUND_REGISTRY_DATA_ID, &w.into_inner())
        .await
        .expect("send registry data");
}

/// Sends a play-state Login packet with fixed values (entity id 1), so a
/// test can observe the play loop actually publish a live, non-default
/// [`StateSnapshot`] before a connection ends.
async fn send_play_login(conn: &mut Connection) {
    let packet = play::PacketLogin {
        entity_id: 1,
        is_hardcore: false,
        world_names: vec!["minecraft:overworld".to_string()],
        max_players: 100,
        view_distance: 10,
        simulation_distance: 10,
        reduced_debug_info: false,
        enable_respawn_screen: true,
        do_limited_crafting: false,
        world_state: play::SpawnInfo {
            dimension: 0,
            name: "minecraft:overworld".to_string(),
            hashed_seed: 0,
            gamemode: play::SpawnInfoGamemode::Survival,
            previous_gamemode: 255,
            is_debug: false,
            is_flat: false,
            death: None,
            portal_cooldown: 0,
            sea_level: 63,
        },
        enforces_secure_chat: false,
    };
    let mut w = PacketWriter::new();
    packet.encode(&mut w).expect("encode play login");
    conn.send_packet(play::CLIENTBOUND_LOGIN_ID, &w.into_inner())
        .await
        .expect("send play login");
}

/// Sends an Encryption Request with `should_authenticate = true` right
/// after the handshake/login-start, without ever completing the exchange —
/// enough to make the client's `login()` fail locally (no premium session
/// attached) without the mock needing to speak RSA/AES at all.
async fn reject_as_online_mode_without_premium(stream: TcpStream) {
    use ::rsa::pkcs8::EncodePublicKey;
    use minerider_protocol::crypto::rsa as mc_rsa;

    let mut conn = Connection::from_tcp_stream(stream).expect("wrap stream");
    let _hs = conn.read_packet().await.expect("read handshake");
    let _ls = conn.read_packet().await.expect("read login start");

    let (public, _private) = mc_rsa::generate_keypair(1024).expect("keypair");
    let der = public.to_public_key_der().expect("der encode");
    let mut w = PacketWriter::new();
    w.put_string("").unwrap();
    w.put_byte_array(der.as_bytes());
    w.put_byte_array(&[0x11, 0x22, 0x33, 0x44]);
    w.put_bool(true); // should_authenticate: online-mode, but we attach no premium session
    let _ = conn
        .send_packet(login::CLIENTBOUND_ENCRYPTION_BEGIN_ID, &w.into_inner())
        .await;
    // The client errors out locally without ever responding; nothing left
    // to do here.
}

/// Sends a login Disconnect with a stated reason — an explicit server
/// rejection (a kick/ban message), as opposed to a bare connection drop.
async fn reject_with_login_disconnect(stream: TcpStream) {
    let mut conn = Connection::from_tcp_stream(stream).expect("wrap stream");
    let _hs = conn.read_packet().await.expect("read handshake");
    let _ls = conn.read_packet().await.expect("read login start");
    let mut w = PacketWriter::new();
    w.put_string(r#"{"text":"you are banned from this server"}"#)
        .unwrap();
    conn.send_packet(login::CLIENTBOUND_DISCONNECT_ID, &w.into_inner())
        .await
        .expect("send login disconnect");
    let _ = conn.close().await;
}

#[tokio::test]
async fn connects_reports_lifecycle_in_order_and_resets_state_on_disconnect() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut conn = minimal_login_and_configuration(stream).await;
        send_play_login(&mut conn).await;
        // Give the play loop a moment to receive and publish this before
        // the connection ends.
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(conn);
    });

    let cfg = ClientConfig::new("127.0.0.1", port, "OrderBot");
    // Reconnect disabled (the default): exactly one attempt, no retry.
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let mut events = handle.events();
    let mut state_rx = handle.state();
    let run_handle = tokio::spawn(supervisor.run());

    assert_eq!(events.recv().await.expect("event"), BotEvent::Connecting);
    assert_eq!(events.recv().await.expect("event"), BotEvent::Connected);
    match events.recv().await.expect("event") {
        BotEvent::Login { entity_id } => assert_eq!(entity_id, 1),
        other => panic!("expected the play-session Login event, got {other:?}"),
    }

    // A live snapshot reflecting the play-session login must appear before
    // the connection ends — otherwise the later "reset to default" check
    // wouldn't prove anything (it could just have always been default).
    tokio::time::timeout(Duration::from_secs(5), state_rx.changed())
        .await
        .expect("must publish a state update")
        .expect("state channel open");
    assert_eq!(
        state_rx.borrow_and_update().player.entity_id,
        Some(1),
        "must reflect the live play session before disconnect"
    );

    match events.recv().await.expect("event") {
        BotEvent::Disconnected { .. } => {}
        other => panic!("expected Disconnected, got {other:?}"),
    }

    let outcome = tokio::time::timeout(Duration::from_secs(5), run_handle)
        .await
        .expect("must not hang")
        .expect("no panic");
    assert!(
        matches!(outcome, SupervisorOutcome::NotRetried { .. }),
        "reconnect disabled: must not retry, got {outcome:?}"
    );

    // State must not silently keep reporting the last-known (now stale)
    // snapshot after the session ended.
    assert_eq!(
        state_rx.borrow_and_update().player.entity_id,
        None,
        "state must reset once the session ends, not report a stale live session"
    );
    assert_eq!(*handle.status().borrow(), SupervisorStatus::Stopped);
}

#[tokio::test]
async fn transient_disconnect_then_successful_reconnect() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("addr").port();
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_clone = attempts.clone();
    // Keeps the second (reconnected) connection alive for the test's
    // duration without leaking a real OS handle via `mem::forget`: it just
    // sits in this channel's queue until `_kept_alive` is dropped at the
    // end of the test function.
    let (conn_tx, _kept_alive) = tokio::sync::mpsc::unbounded_channel::<Connection>();

    tokio::spawn(async move {
        // First connection: accept, complete the flow, then drop it
        // immediately (a transient disconnect). Second connection (the
        // supervisor's reconnect): accept, complete the flow, then hand it
        // off to be kept alive — proving a real second TCP connection was
        // established after the first was torn down, sequentially, not
        // concurrently (this loop only calls `accept()` again after the
        // previous iteration's connection has already been fully handled).
        let (stream, _) = listener.accept().await.expect("accept #1");
        attempts_clone.fetch_add(1, Ordering::SeqCst);
        drop(minimal_login_and_configuration(stream).await);

        let (stream, _) = listener.accept().await.expect("accept #2");
        attempts_clone.fetch_add(1, Ordering::SeqCst);
        let conn = minimal_login_and_configuration(stream).await;
        let _ = conn_tx.send(conn);
    });

    let cfg = ClientConfig::new("127.0.0.1", port, "ReconnectBot");
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
        .expect("must reconnect and reach Connected a second time");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    handle.stop();
    let outcome = tokio::time::timeout(Duration::from_secs(5), run_handle)
        .await
        .expect("must stop promptly")
        .expect("no panic");
    assert!(matches!(outcome, SupervisorOutcome::Cancelled));
}

#[tokio::test]
async fn max_retries_exhausted_stops_supervisor() {
    // Bind then drop: a port nobody listens on, so every connect attempt
    // fails fast and deterministically with a real connection-refused
    // error — no server task needed at all.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to reserve a port");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);

    let cfg = ClientConfig::new("127.0.0.1", port, "NoOneHomeBot")
        .with_connect_deadline(Duration::from_secs(2));
    let policy = ReconnectPolicy::enabled()
        .with_initial_delay(Duration::from_millis(5))
        .with_max_delay(Duration::from_millis(5))
        .with_max_retries(RetryLimit::Count(2));
    let (supervisor, _handle) = ClientSupervisor::new(cfg, policy);

    let outcome = tokio::time::timeout(Duration::from_secs(10), supervisor.run())
        .await
        .expect("must not hang");
    match outcome {
        SupervisorOutcome::RetriesExhausted { last_error } => {
            assert_eq!(last_error.retry_class(), RetryClass::Transient);
        }
        other => panic!("expected RetriesExhausted, got {other:?}"),
    }
}

#[tokio::test]
async fn cancellation_interrupts_backoff_immediately() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to reserve a port");
    let port = listener.local_addr().expect("addr").port();
    drop(listener); // nobody home; every connect attempt fails

    let cfg = ClientConfig::new("127.0.0.1", port, "CancelBot");
    let policy = ReconnectPolicy::enabled()
        // Deliberately much longer than this test's own timeout below —
        // if cancellation didn't interrupt the sleep, the test would fail
        // by timing out instead of by a wrong assertion.
        .with_initial_delay(Duration::from_secs(30))
        .with_max_retries(RetryLimit::Unlimited);
    let (supervisor, handle) = ClientSupervisor::new(cfg, policy);
    let run_handle = tokio::spawn(supervisor.run());

    let mut status_rx = handle.status();
    let wait_for_backoff = async {
        loop {
            status_rx.changed().await.expect("status channel open");
            if matches!(
                *status_rx.borrow(),
                SupervisorStatus::ReconnectScheduled { .. }
            ) {
                break;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait_for_backoff)
        .await
        .expect("must enter backoff");

    handle.stop();
    let outcome = tokio::time::timeout(Duration::from_secs(2), run_handle)
        .await
        .expect("cancellation must interrupt a 30s backoff almost immediately")
        .expect("no panic");
    assert!(matches!(outcome, SupervisorOutcome::Cancelled));
}

#[tokio::test]
async fn server_rejection_does_not_retry_by_default() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        reject_with_login_disconnect(stream).await;
    });

    let cfg = ClientConfig::new("127.0.0.1", port, "RejectedBot");
    // Default policy: on_server_rejected = Stop.
    let (supervisor, _handle) = ClientSupervisor::new(cfg, ReconnectPolicy::enabled());
    let outcome = tokio::time::timeout(Duration::from_secs(5), supervisor.run())
        .await
        .expect("must not hang");
    match outcome {
        SupervisorOutcome::NotRetried { reason } => {
            assert_eq!(reason.retry_class(), RetryClass::ServerRejected);
            assert!(matches!(reason, MineRiderError::Disconnected(_)));
        }
        other => panic!("expected NotRetried, got {other:?}"),
    }
}

#[tokio::test]
async fn protocol_incompatibility_does_not_retry_by_default() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        reject_as_online_mode_without_premium(stream).await;
    });

    // No premium session attached, so the server demanding online-mode
    // auth is a permanent, this-server-will-never-work configuration
    // mismatch, not a transient hiccup.
    let cfg = ClientConfig::new("127.0.0.1", port, "NoPremiumBot");
    let (supervisor, _handle) = ClientSupervisor::new(cfg, ReconnectPolicy::enabled());
    let outcome = tokio::time::timeout(Duration::from_secs(5), supervisor.run())
        .await
        .expect("must not hang");
    match outcome {
        SupervisorOutcome::NotRetried { reason } => {
            assert_eq!(reason.retry_class(), RetryClass::ProtocolIncompatible);
        }
        other => panic!("expected NotRetried, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// Phase 4b: supervised active-session control (SupervisorHandle commands).
// ---------------------------------------------------------------------

#[tokio::test]
async fn commands_reach_the_active_session() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut conn = minimal_login_and_configuration(stream).await;
        // Chat is handled directly in the play loop regardless of world
        // readiness, so no play-login/chunk setup is needed here.
        let chat = conn.read_packet().await.expect("read chat command");
        assert_eq!(chat.id, play::SERVERBOUND_CHAT_MESSAGE_ID);
        let mut r = PacketReader::new(&chat.payload);
        let message = play::PacketChatMessage::decode(&mut r).expect("decode chat");
        assert_eq!(message.message, "hello from the supervisor");
    });

    let cfg = ClientConfig::new("127.0.0.1", port, "ControlBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());

    wait_until_connected(&mut handle.status()).await;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        handle.chat("hello from the supervisor"),
    )
    .await
    .expect("must not hang");
    assert_eq!(result, Ok(()));

    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

#[tokio::test]
async fn commands_fail_immediately_while_offline() {
    // A port nobody listens on: the supervisor never reaches Connected.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to reserve a port");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);

    let cfg = ClientConfig::new("127.0.0.1", port, "OfflineBot");
    let policy = ReconnectPolicy::enabled().with_initial_delay(Duration::from_secs(30));
    let (supervisor, handle) = ClientSupervisor::new(cfg, policy);
    let run_handle = tokio::spawn(supervisor.run());

    let result = tokio::time::timeout(Duration::from_secs(5), handle.chat("hello"))
        .await
        .expect("an offline command must fail immediately, not hang");
    assert_eq!(result, Err(ControlError::NotConnected));

    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

#[tokio::test]
async fn stale_command_fails_rather_than_silently_succeeding_on_teardown() {
    // `ControlHandle::send` succeeding only ever meant "accepted into the
    // play loop's own command queue", never "confirmed on the wire" — that
    // pre-existing, fire-and-forget contract means a command can race a
    // real TCP teardown and still get queued into the *old*, about-to-die
    // session before that session's task notices the socket is gone (that
    // detection isn't instantaneous). That race is not what this test
    // proves. What this design *does* guarantee unconditionally is: once
    // the supervisor has fully processed a session ending (with reconnect
    // disabled, `run()` returns for good right after), no command can ever
    // reach it again — proven deterministically by waiting for `run()`
    // itself to finish before sending.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        drop(minimal_login_and_configuration(stream).await);
    });

    let cfg = ClientConfig::new("127.0.0.1", port, "StaleBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());

    let outcome = tokio::time::timeout(Duration::from_secs(5), run_handle)
        .await
        .expect("must not hang")
        .expect("no panic");
    assert!(
        matches!(outcome, SupervisorOutcome::NotRetried { .. }),
        "reconnect disabled: the session ending must stop the supervisor for good, got {outcome:?}"
    );

    let result = tokio::time::timeout(Duration::from_secs(5), handle.chat("hello"))
        .await
        .expect("must not hang");
    assert_eq!(
        result,
        Err(ControlError::SupervisorStopped),
        "a command after the supervisor has fully stopped must never succeed"
    );
}

#[tokio::test]
async fn reconnect_increments_generation_and_new_session_accepts_commands() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("addr").port();
    let (conn_tx, _kept_alive) = tokio::sync::mpsc::unbounded_channel::<Connection>();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept #1");
        drop(minimal_login_and_configuration(stream).await);

        let (stream, _) = listener.accept().await.expect("accept #2");
        let mut conn = minimal_login_and_configuration(stream).await;
        let chat = conn.read_packet().await.expect("read chat on new session");
        assert_eq!(chat.id, play::SERVERBOUND_CHAT_MESSAGE_ID);
        let _ = conn_tx.send(conn);
    });

    let cfg = ClientConfig::new("127.0.0.1", port, "GenerationBot");
    let policy = ReconnectPolicy::enabled()
        .with_initial_delay(Duration::from_millis(5))
        .with_max_delay(Duration::from_millis(5))
        .with_max_retries(RetryLimit::Count(3));
    let (supervisor, handle) = ClientSupervisor::new(cfg, policy);
    let run_handle = tokio::spawn(supervisor.run());

    assert_eq!(handle.generation(), 0, "no session has connected yet");

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
        .expect("must reconnect to a second session");

    assert_eq!(
        handle.generation(),
        2,
        "generation must increment once per successful connect"
    );

    // The new session's command path must work — proving commands route to
    // whichever session is *currently* active, not a stale reference to the
    // first one.
    let result = tokio::time::timeout(Duration::from_secs(5), handle.chat("hello again"))
        .await
        .expect("must not hang");
    assert_eq!(result, Ok(()));

    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

#[tokio::test]
async fn cancellation_closes_the_command_path() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("addr").port();
    let (conn_tx, _kept_alive) = tokio::sync::mpsc::unbounded_channel::<Connection>();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let conn = minimal_login_and_configuration(stream).await;
        let _ = conn_tx.send(conn);
    });

    let cfg = ClientConfig::new("127.0.0.1", port, "CancelCommandBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());

    wait_until_connected(&mut handle.status()).await;
    handle.stop();
    tokio::time::timeout(Duration::from_secs(5), run_handle)
        .await
        .expect("must not hang")
        .expect("no panic");

    // The supervisor task has fully exited; the command channel's receiver
    // is gone, so sending must fail cleanly and immediately, never hang or
    // panic on a dropped one-shot.
    let result = tokio::time::timeout(Duration::from_secs(5), handle.chat("too late"))
        .await
        .expect("must not hang after the supervisor has stopped");
    assert_eq!(result, Err(ControlError::SupervisorStopped));
}

#[tokio::test]
async fn concurrent_commands_are_serialized_onto_one_writer() {
    const MESSAGES: usize = 8;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut conn = minimal_login_and_configuration(stream).await;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..MESSAGES {
            let packet = conn.read_packet().await.expect("read chat command");
            assert_eq!(packet.id, play::SERVERBOUND_CHAT_MESSAGE_ID);
            let mut r = PacketReader::new(&packet.payload);
            let message = play::PacketChatMessage::decode(&mut r).expect("decode chat");
            // Each message must arrive whole and distinct — proof that
            // concurrent callers never interleaved partial writes onto the
            // wire (there is exactly one task, `run_session`, that ever
            // calls `Connection::send_packet`).
            assert!(
                seen.insert(message.message),
                "duplicate/corrupted message: every concurrent send must be distinct and intact"
            );
        }
    });

    let cfg = ClientConfig::new("127.0.0.1", port, "ConcurrentBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());

    wait_until_connected(&mut handle.status()).await;

    let send_tasks: Vec<_> = (0..MESSAGES)
        .map(|i| {
            let handle = handle.clone();
            tokio::spawn(async move { handle.chat(format!("concurrent message {i}")).await })
        })
        .collect();
    for task in send_tasks {
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("must not hang")
            .expect("send task must not panic");
        assert_eq!(result, Ok(()));
    }

    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}
