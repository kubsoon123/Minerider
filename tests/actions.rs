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

    let brand = conn.read_packet().await.expect("read brand");
    assert_eq!(brand.id, configuration::SERVERBOUND_CUSTOM_PAYLOAD_ID);
    let settings = conn.read_packet().await.expect("read settings");
    assert_eq!(settings.id, configuration::SERVERBOUND_SETTINGS_ID);

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

/// Sends a Synchronize Player Position (teleport id 1) at a fixed spawn,
/// then a one-chunk batch containing a stone floor at the player's chunk,
/// so the client's readiness gate fires and it sends `player_loaded`. After
/// this the player is a fully in-world, ticking entity — the state in which
/// vanilla sends `tick_end` and `player_input`.
async fn send_position_and_load_chunk(conn: &mut Connection) {
    let position = play::PacketPosition {
        teleport_id: 1,
        x: 0.5,
        y: 64.0,
        z: 0.5,
        dx: 0.0,
        dy: 0.0,
        dz: 0.0,
        yaw: 0.0,
        pitch: 0.0,
        flags: play::PositionUpdateRelatives(0),
    };
    let mut w = PacketWriter::new();
    position.encode(&mut w).expect("encode position");
    conn.send_packet(play::CLIENTBOUND_POSITION_ID, &w.into_inner())
        .await
        .expect("send position");

    conn.send_packet(play::CLIENTBOUND_CHUNK_BATCH_START_ID, &[])
        .await
        .expect("send chunk batch start");
    let mut chunk_data = PacketWriter::new();
    for section in 0..24 {
        if section == 7 {
            // Stone floor at world Y=63 (local Y=15 of section 7): a 4-bit
            // indirect palette [air=0, stone=1], stone in the top rows.
            chunk_data.put_i16(256);
            chunk_data.put_u8(4);
            chunk_data.put_varint(2);
            chunk_data.put_varint(0);
            chunk_data.put_varint(1);
            chunk_data.put_varint(256);
            for packed in 0..256 {
                chunk_data.put_u64(if packed >= 240 {
                    0x1111_1111_1111_1111
                } else {
                    0
                });
            }
        } else {
            chunk_data.put_i16(0);
            chunk_data.put_u8(0);
            chunk_data.put_varint(0);
            chunk_data.put_varint(0);
        }
        chunk_data.put_u8(0);
        chunk_data.put_varint(0);
        chunk_data.put_varint(0);
    }
    let chunk_data = chunk_data.into_inner();
    let mut w = PacketWriter::new();
    w.put_i32(0);
    w.put_i32(0);
    Nbt::Compound(vec![])
        .encode(&mut w)
        .expect("encode heightmap");
    w.put_byte_array(&chunk_data);
    for _ in 0..7 {
        w.put_varint(0);
    }
    conn.send_packet(play::CLIENTBOUND_MAP_CHUNK_ID, &w.into_inner())
        .await
        .expect("send map chunk");
    let mut w = PacketWriter::new();
    w.put_varint(1);
    conn.send_packet(play::CLIENTBOUND_CHUNK_BATCH_FINISHED_ID, &w.into_inner())
        .await
        .expect("send chunk batch finished");
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

/// The play loop must answer a clientbound `ping` with a `pong` echoing the
/// same id, exactly like the vanilla client — the server uses it as a
/// liveness probe distinct from keep-alive, and a missing pong gets the
/// client disconnected. Tolerates the tick-driven movement/tick_end/
/// player_input packets that legitimately interleave.
#[tokio::test]
async fn play_ping_is_answered_with_a_matching_pong() {
    let port = start_and_run(|mut conn| async move {
        let ping = play::PacketPing { id: 0x5150_1234 };
        let mut w = PacketWriter::new();
        ping.encode(&mut w).expect("encode ping");
        conn.send_packet(play::CLIENTBOUND_PING_ID, &w.into_inner())
            .await
            .expect("send ping");

        let pong = loop {
            let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
                .await
                .expect("pong did not arrive in time")
                .expect("read packet");
            if packet.id == play::SERVERBOUND_PONG_ID {
                break packet;
            }
        };
        let decoded =
            play::PacketPong::decode(&mut PacketReader::new(&pong.payload)).expect("decode pong");
        assert_eq!(
            decoded.id, 0x5150_1234,
            "pong must echo the exact ping id back"
        );
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "PingBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());

    wait_until_connected(&handle).await;
    // The pong is driven entirely by the inbound ping; nothing to send here.
    tokio::time::sleep(Duration::from_millis(300)).await;

    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

/// Once the player is a loaded, ticking in-world entity, vanilla sends a
/// fieldless `tick_end` at the end of every client tick. Load the player,
/// then assert several `tick_end`s arrive over the following ticks.
#[tokio::test]
async fn tick_end_is_sent_every_active_play_tick() {
    let port = start_and_run(|mut conn| async move {
        send_position_and_load_chunk(&mut conn).await;

        // player_loaded first (the readiness gate), then a run of tick_ends.
        let mut tick_ends = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tick_ends < 3 && tokio::time::Instant::now() < deadline {
            let packet = tokio::time::timeout(Duration::from_secs(3), conn.read_packet())
                .await
                .expect("packet did not arrive in time")
                .expect("read packet");
            if packet.id == play::SERVERBOUND_TICK_END_ID {
                tick_ends += 1;
            }
        }
        assert!(
            tick_ends >= 3,
            "expected a tick_end every active tick, saw {tick_ends}"
        );
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "TickEndBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());
    wait_until_connected(&handle).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

/// Vanilla sends `player_input` (the seven movement-key bitset) whenever the
/// pressed set changes. Load the player, then hold "forward" and assert a
/// `player_input` carrying exactly the FORWARD bit arrives.
#[tokio::test]
async fn player_input_is_sent_when_the_bot_starts_moving() {
    let port = start_and_run(|mut conn| async move {
        send_position_and_load_chunk(&mut conn).await;

        let input = loop {
            let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
                .await
                .expect("player_input did not arrive in time")
                .expect("read packet");
            if packet.id == play::SERVERBOUND_PLAYER_INPUT_ID {
                break packet;
            }
        };
        let decoded = play::PacketPlayerInput::decode(&mut PacketReader::new(&input.payload))
            .expect("decode player_input");
        assert_eq!(
            decoded.inputs.0,
            play::PacketPlayerInputInputs::FORWARD,
            "holding forward must send exactly the FORWARD bit"
        );
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "InputBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());
    wait_until_connected(&handle).await;
    // Hold forward. The controller keeps applying it each tick; once the
    // player is loaded, the change from "no keys" to FORWARD sends one
    // player_input. Re-sent a few times in case it lands before load; stop
    // once the mock has seen it and closed (set_input then errors, which is
    // the success path, not a failure).
    for _ in 0..10 {
        if handle
            .set_input(minerider::minecraft::player::MovementInput {
                forward: 1.0,
                ..Default::default()
            })
            .await
            .is_err()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

/// `select_hotbar_slot` sends vanilla's `held_item_slot` with the chosen
/// slot and updates the locally tracked selection so a following action
/// acts on the newly held item.
#[tokio::test]
async fn select_hotbar_slot_sends_held_item_slot_and_tracks_the_selection() {
    let port = start_and_run(|mut conn| async move {
        let held = loop {
            let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
                .await
                .expect("held_item_slot did not arrive in time")
                .expect("read packet");
            if packet.id == play::SERVERBOUND_HELD_ITEM_SLOT_ID {
                break packet;
            }
        };
        let decoded =
            play::PacketHeldItemSlotServerbound::decode(&mut PacketReader::new(&held.payload))
                .expect("decode held_item_slot");
        assert_eq!(decoded.slot_id, 4, "must send the requested slot");
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "HotbarBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());
    wait_until_connected(&handle).await;
    handle
        .select_hotbar_slot(4)
        .await
        .expect("select_hotbar_slot accepted while connected");

    // The local selection is updated immediately, before any server echo.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if handle.state().borrow().inventory.selected_hotbar_slot == 4 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "selected_hotbar_slot never updated to 4"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

/// A slot outside 0..=8 is rejected before anything is sent.
#[tokio::test]
async fn select_hotbar_slot_rejects_an_out_of_range_slot() {
    let port = start_and_run(|mut conn| async move {
        // Nothing valid should be sent; just drain until close.
        while conn.read_packet().await.is_ok() {}
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "BadHotbarBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());
    wait_until_connected(&handle).await;
    let result = handle.select_hotbar_slot(9).await;
    assert!(
        result.is_err(),
        "slot 9 is out of the 0..=8 hotbar range and must be rejected"
    );

    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

/// Reconfiguration: a server can send the client back to the configuration
/// state mid-play (a server transfer, a datapack reload). Vanilla
/// acknowledges, re-runs configuration, and re-enters play. This drives the
/// full round trip and proves the play loop resumes afterward by
/// round-tripping a keep-alive on the other side.
#[tokio::test]
async fn reconfiguration_acknowledges_reruns_config_and_resumes_play() {
    let port = start_and_run(|mut conn| async move {
        // Mid-play, send the client back to configuration.
        conn.send_packet(play::CLIENTBOUND_START_CONFIGURATION_ID, &[])
            .await
            .expect("send start_configuration");

        // The client acknowledges (play-state serverbound id 14).
        let ack = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
            .await
            .expect("configuration_acknowledged did not arrive in time")
            .expect("read ack");
        assert_eq!(ack.id, play::SERVERBOUND_CONFIGURATION_ACKNOWLEDGED_ID);

        // Now back in configuration: the client re-sends brand then settings,
        // exactly as on the first configuration.
        let brand = conn.read_packet().await.expect("read brand");
        assert_eq!(brand.id, configuration::SERVERBOUND_CUSTOM_PAYLOAD_ID);
        let settings = conn.read_packet().await.expect("read settings");
        assert_eq!(settings.id, configuration::SERVERBOUND_SETTINGS_ID);

        // Finish configuration again; the client acks and re-enters play.
        send_dimension_registry(&mut conn).await;
        conn.send_packet(configuration::CLIENTBOUND_FINISH_CONFIGURATION_ID, &[])
            .await
            .expect("send finish configuration");
        let fin = conn.read_packet().await.expect("read finish ack");
        assert_eq!(fin.id, configuration::SERVERBOUND_FINISH_CONFIGURATION_ID);

        // Back in play: a keep-alive must be echoed, proving the play loop
        // resumed on the fresh configuration rather than staying stuck.
        let mut w = PacketWriter::new();
        w.put_i64(0x1234_5678);
        conn.send_packet(play::CLIENTBOUND_KEEP_ALIVE_ID, &w.into_inner())
            .await
            .expect("send keep_alive");
        let echo = loop {
            let p = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
                .await
                .expect("keep_alive echo did not arrive in time")
                .expect("read keep_alive echo");
            if p.id == play::SERVERBOUND_KEEP_ALIVE_ID {
                break p;
            }
        };
        let mut r = PacketReader::new(&echo.payload);
        assert_eq!(
            r.get_i64().unwrap(),
            0x1234_5678,
            "the resumed play loop must echo the keep-alive id"
        );
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "ReconfigBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());
    wait_until_connected(&handle).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

/// `use_item_on_block` sends vanilla's `block_place` with the target block,
/// face, hand and cursor position, plus a per-session sequence.
#[tokio::test]
async fn use_item_on_block_sends_block_place_with_the_target_and_cursor() {
    let port = start_and_run(|mut conn| async move {
        let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
            .await
            .expect("block_place did not arrive in time")
            .expect("read block_place");
        assert_eq!(packet.id, play::SERVERBOUND_BLOCK_PLACE_ID);
        let decoded = play::PacketBlockPlace::decode(&mut PacketReader::new(&packet.payload))
            .expect("decode block_place");
        assert_eq!(decoded.hand, 0, "main hand");
        assert_eq!(decoded.location.x, 10);
        assert_eq!(decoded.location.y, 64);
        assert_eq!(decoded.location.z, -3);
        assert_eq!(decoded.direction, 1, "up face");
        assert_eq!(decoded.cursor_x, 0.5);
        assert_eq!(decoded.sequence, 1, "first action is sequence 1");
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "PlaceBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());
    wait_until_connected(&handle).await;
    handle
        .use_item_on_block(minerider::minecraft::control::BlockPlacement {
            x: 10,
            y: 64,
            z: -3,
            face: 1,
            hand: Hand::Main,
            cursor_x: 0.5,
            cursor_y: 0.5,
            cursor_z: 0.5,
            inside_block: false,
        })
        .await
        .expect("use_item_on_block accepted while connected");
    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

/// `use_item_on_block` rejects a face outside 0..=5 before anything is sent.
#[tokio::test]
async fn use_item_on_block_rejects_an_out_of_range_face() {
    let port =
        start_and_run(|mut conn| async move { while conn.read_packet().await.is_ok() {} }).await;
    let cfg = ClientConfig::new("127.0.0.1", port, "BadPlaceBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());
    wait_until_connected(&handle).await;
    let result = handle
        .use_item_on_block(minerider::minecraft::control::BlockPlacement {
            x: 0,
            y: 0,
            z: 0,
            face: 6,
            hand: Hand::Main,
            cursor_x: 0.5,
            cursor_y: 0.5,
            cursor_z: 0.5,
            inside_block: false,
        })
        .await;
    assert!(result.is_err(), "face 6 is out of the 0..=5 range");
    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

/// `interact_entity` sends `use_entity` in its INTERACT form (mouse type 0)
/// with the target id, hand and sneaking flag.
#[tokio::test]
async fn interact_entity_sends_use_entity_interact() {
    let port = start_and_run(|mut conn| async move {
        let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
            .await
            .expect("use_entity did not arrive in time")
            .expect("read use_entity");
        assert_eq!(packet.id, play::SERVERBOUND_USE_ENTITY_ID);
        let decoded = play::PacketUseEntity::decode(&mut PacketReader::new(&packet.payload))
            .expect("decode use_entity");
        assert_eq!(decoded.target, 4242);
        assert_eq!(decoded.mouse, 0, "INTERACT type");
        assert!(decoded.sneaking);
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "InteractBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());
    wait_until_connected(&handle).await;
    handle
        .interact_entity(4242, Hand::Main, true)
        .await
        .expect("interact_entity accepted while connected");
    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

/// `close_gui` closes the currently open window: it sends `close_window`
/// carrying that window's id (not window 0) and drops the local open window.
#[tokio::test]
async fn close_gui_sends_close_window_for_the_open_window() {
    let port = start_and_run(|mut conn| async move {
        send_open_window(&mut conn, 5, 27).await;
        let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
            .await
            .expect("close_window did not arrive in time")
            .expect("read close_window");
        assert_eq!(packet.id, play::SERVERBOUND_CLOSE_WINDOW_ID);
        let decoded = play::PacketCloseWindow::decode(&mut PacketReader::new(&packet.payload))
            .expect("decode close_window");
        assert_eq!(decoded.window_id, 5, "must close the open window, not 0");
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "CloseGuiBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());
    wait_until_connected(&handle).await;
    wait_until_gui_open(&handle, 5).await;
    handle.close_gui().await.expect("close_gui accepted");

    // The local open window is dropped after the close.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if handle.state().borrow().inventory.open_window.is_none() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "open window was not cleared after close_gui"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

/// `attack_entity` sends `use_entity` in its ATTACK form (mouse type 1),
/// with the target id and sneaking flag, and no hand.
#[tokio::test]
async fn attack_entity_sends_use_entity_attack() {
    let port = start_and_run(|mut conn| async move {
        let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
            .await
            .expect("use_entity did not arrive in time")
            .expect("read use_entity");
        assert_eq!(packet.id, play::SERVERBOUND_USE_ENTITY_ID);
        let decoded = play::PacketUseEntity::decode(&mut PacketReader::new(&packet.payload))
            .expect("decode use_entity");
        assert_eq!(decoded.target, 77);
        assert_eq!(decoded.mouse, 1, "ATTACK type");
        assert!(!decoded.sneaking);
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "AttackBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());
    wait_until_connected(&handle).await;
    handle
        .attack_entity(77, false)
        .await
        .expect("attack_entity accepted while connected");
    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

/// `interact_at_entity` sends `use_entity` in its INTERACT_AT form (mouse
/// type 2) carrying the hit point and hand.
#[tokio::test]
async fn interact_at_entity_sends_use_entity_interact_at() {
    let port = start_and_run(|mut conn| async move {
        let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
            .await
            .expect("use_entity did not arrive in time")
            .expect("read use_entity");
        assert_eq!(packet.id, play::SERVERBOUND_USE_ENTITY_ID);
        let decoded = play::PacketUseEntity::decode(&mut PacketReader::new(&packet.payload))
            .expect("decode use_entity");
        assert_eq!(decoded.target, 88);
        assert_eq!(decoded.mouse, 2, "INTERACT_AT type");
        assert!(decoded.sneaking);
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "InteractAtBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());
    wait_until_connected(&handle).await;
    handle
        .interact_at_entity(88, Hand::Main, true, 0.1, 1.5, -0.2)
        .await
        .expect("interact_at_entity accepted while connected");
    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
}

/// `release_item` sends `block_dig` with the RELEASE_USE_ITEM status (5).
#[tokio::test]
async fn release_item_sends_block_dig_release_status() {
    let port = start_and_run(|mut conn| async move {
        let packet = tokio::time::timeout(Duration::from_secs(5), conn.read_packet())
            .await
            .expect("block_dig did not arrive in time")
            .expect("read block_dig");
        assert_eq!(packet.id, play::SERVERBOUND_BLOCK_DIG_ID);
        let decoded = play::PacketBlockDig::decode(&mut PacketReader::new(&packet.payload))
            .expect("decode block_dig");
        assert_eq!(decoded.status, 5, "RELEASE_USE_ITEM");
    })
    .await;

    let cfg = ClientConfig::new("127.0.0.1", port, "ReleaseBot");
    let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::default());
    let run_handle = tokio::spawn(supervisor.run());
    wait_until_connected(&handle).await;
    handle
        .release_item()
        .await
        .expect("release_item accepted while connected");
    handle.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), run_handle).await;
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
