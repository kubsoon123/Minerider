//! Integration tests for held-item use, arm swing, and end-to-end GUI
//! clicks (Missions C/D): real loopback TCP, a local mock server, no real
//! Minecraft server.
//!
//! Uses the same small hand-rolled raw-socket helpers `tests/supervisor.rs`
//! documents preferring over `tests/common::MockServer` here too — these
//! scenarios need to keep reading server-side after login (to observe
//! outgoing action packets), which `MockServer`'s fixed scenarios don't
//! support.

use std::time::Duration;

use minerider::core::client::ClientConfig;
use minerider::core::supervisor::{ClientSupervisor, ReconnectPolicy};
use minerider::minecraft::control::Hand;
use minerider::minecraft::inventory::GuiClick;
use minerider::network::connection::Connection;
use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::generated::v1_21_4::{configuration, handshaking, login, play};
use minerider_protocol::nbt::Nbt;
use minerider_protocol::traits::{Decode, Encode};
use tokio::net::{TcpListener, TcpStream};

async fn minimal_login_and_configuration(stream: TcpStream) -> Connection {
    let mut conn = Connection::from_tcp_stream(stream).expect("wrap stream");

    let hs = conn.read_packet().await.expect("read handshake");
    assert_eq!(hs.id, handshaking::SERVERBOUND_SET_PROTOCOL_ID);
    let ls = conn.read_packet().await.expect("read login start");
    assert_eq!(ls.id, login::SERVERBOUND_LOGIN_START_ID);

    let mut w = PacketWriter::new();
    w.put_uuid(0x1111_2222_3333_4444_5555_6666_7777_8888);
    w.put_string("ActionsBot").unwrap();
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

/// Starts a mock server that completes the handshake through play-state
/// login, then hands the still-open server-side `Connection` to `body` for
/// the test to drive further.
async fn start_and_run<F, Fut>(body: F) -> u16
where
    F: FnOnce(Connection) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut conn = minimal_login_and_configuration(stream).await;
        send_play_login(&mut conn).await;
        body(conn).await;
    });
    port
}

#[tokio::test]
async fn use_item_main_hand_carries_hand_sequence_and_current_orientation() {
    let port = start_and_run(|mut conn| async move {
        let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
            .await
            .expect("use_item did not arrive in time")
            .expect("read use_item");
        assert_eq!(packet.id, play::SERVERBOUND_USE_ITEM_ID);
        let decoded = play::PacketUseItem::decode(&mut PacketReader::new(&packet.payload))
            .expect("decode use_item");
        assert_eq!(decoded.hand, 0, "main hand must be wire value 0");
        assert_eq!(
            decoded.sequence, 1,
            "first action in a fresh session is sequence 1"
        );
        // The mock never sends a position sync, so the player's default
        // orientation (0.0/0.0) is what should be echoed back.
        assert_eq!(decoded.rotation.x, 0.0);
        assert_eq!(decoded.rotation.y, 0.0);
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "UseItemBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());

    wait_until_connected(&handle).await;
    handle
        .use_item(Hand::Main)
        .await
        .expect("use_item should be accepted while connected");

    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

#[tokio::test]
async fn use_item_offhand_sends_wire_value_one() {
    let port = start_and_run(|mut conn| async move {
        let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
            .await
            .expect("use_item did not arrive in time")
            .expect("read use_item");
        let decoded = play::PacketUseItem::decode(&mut PacketReader::new(&packet.payload))
            .expect("decode use_item");
        assert_eq!(decoded.hand, 1, "off hand must be wire value 1");
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "OffhandBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());

    wait_until_connected(&handle).await;
    handle
        .use_item(Hand::Off)
        .await
        .expect("use_item should be accepted while connected");

    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

#[tokio::test]
async fn use_item_sequence_never_repeats_within_a_session() {
    let port = start_and_run(|mut conn| async move {
        let mut sequences = Vec::new();
        for _ in 0..3 {
            let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
                .await
                .expect("use_item did not arrive in time")
                .expect("read use_item");
            let decoded = play::PacketUseItem::decode(&mut PacketReader::new(&packet.payload))
                .expect("decode use_item");
            sequences.push(decoded.sequence);
        }
        assert_eq!(
            sequences,
            vec![1, 2, 3],
            "sequence must strictly increase, never repeat"
        );
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "SequenceBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());

    wait_until_connected(&handle).await;
    for _ in 0..3 {
        handle
            .use_item(Hand::Main)
            .await
            .expect("use_item should be accepted while connected");
    }

    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

#[tokio::test]
async fn swing_sends_arm_animation_independent_of_use_item() {
    let port = start_and_run(|mut conn| async move {
        let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
            .await
            .expect("arm_animation did not arrive in time")
            .expect("read arm_animation");
        assert_eq!(packet.id, play::SERVERBOUND_ARM_ANIMATION_ID);
        let decoded = play::PacketArmAnimation::decode(&mut PacketReader::new(&packet.payload))
            .expect("decode arm_animation");
        assert_eq!(decoded.hand, 0);
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "SwingBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());

    wait_until_connected(&handle).await;
    handle
        .swing(Hand::Main)
        .await
        .expect("swing should be accepted while connected");

    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

#[tokio::test]
async fn use_item_and_swing_are_rejected_while_disconnected() {
    // Nobody is listening on this port; the supervisor never reaches
    // Connected, so these must fail with NotConnected rather than hang or
    // silently queue.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let cfg = ClientConfig::new("127.0.0.1", port, "NeverBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());

    use minerider::core::supervisor::ControlError;
    assert_eq!(
        handle.use_item(Hand::Main).await,
        Err(ControlError::NotConnected)
    );
    assert_eq!(
        handle.swing(Hand::Main).await,
        Err(ControlError::NotConnected)
    );

    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

#[tokio::test]
async fn click_open_gui_slot_end_to_end_sends_the_expected_window_click() {
    let port = start_and_run(|mut conn| async move {
        send_open_window(&mut conn, 5, 27).await;

        let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
            .await
            .expect("window_click did not arrive in time")
            .expect("read window_click");
        assert_eq!(packet.id, play::SERVERBOUND_WINDOW_CLICK_ID);
        let decoded = play::PacketWindowClick::decode(&mut PacketReader::new(&packet.payload))
            .expect("decode window_click");
        assert_eq!(
            decoded.window_id, 5,
            "must target the open window, not window 0"
        );
        assert_eq!(
            decoded.slot, 13,
            "raw slot index must reach the wire unmodified"
        );
        assert_eq!(
            decoded.mouse_button, 1,
            "GuiClick::Right must send the right-click button"
        );
        assert_eq!(decoded.mode, 0, "a plain click is protocol mode 0");
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "GuiClickBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());

    wait_until_connected(&handle).await;
    wait_until_gui_open(&handle, 5).await;

    let task = tokio::spawn({
        let handle = handle.clone();
        async move { handle.click_open_gui_slot(13, GuiClick::Right).await }
    });
    // The mock server never sends a follow-up state_id confirmation, so the
    // transaction stays pending; this test only proves the outgoing packet
    // shape, matching the other action tests above.
    tokio::time::sleep(Duration::from_millis(200)).await;
    task.abort();

    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

async fn send_open_window(conn: &mut Connection, window_id: i32, slot_count: usize) {
    let open = play::PacketOpenWindow {
        window_id,
        inventory_type: 2,
        window_title: Nbt::Compound(vec![]),
    };
    let mut w = PacketWriter::new();
    open.encode(&mut w).expect("encode open_window");
    conn.send_packet(play::CLIENTBOUND_OPEN_WINDOW_ID, &w.into_inner())
        .await
        .expect("send open_window");

    let empty = minerider_protocol::generated::v1_21_4::types::Slot {
        item_count: 0,
        value: minerider_protocol::generated::v1_21_4::types::SlotValue::V0,
    };
    let items = play::PacketWindowItems {
        window_id,
        state_id: 3,
        items: vec![empty.clone(); slot_count],
        carried_item: empty,
    };
    let mut w = PacketWriter::new();
    items.encode(&mut w).expect("encode window_items");
    conn.send_packet(play::CLIENTBOUND_WINDOW_ITEMS_ID, &w.into_inner())
        .await
        .expect("send window_items");
}

async fn wait_until_connected(handle: &minerider::core::supervisor::SupervisorHandle) {
    let mut status = handle.status();
    loop {
        if *status.borrow() == minerider::core::supervisor::SupervisorStatus::Connected {
            return;
        }
        tokio::time::timeout(Duration::from_secs(5), status.changed())
            .await
            .expect("must reach Connected")
            .expect("status channel open");
    }
}

async fn wait_until_gui_open(
    handle: &minerider::core::supervisor::SupervisorHandle,
    window_id: i32,
) {
    let mut state = handle.state();
    loop {
        if state
            .borrow()
            .inventory
            .open_window
            .as_ref()
            .is_some_and(|w| w.id == window_id)
        {
            return;
        }
        tokio::time::timeout(Duration::from_secs(5), state.changed())
            .await
            .expect("must observe the window open")
            .expect("state channel open");
    }
}
