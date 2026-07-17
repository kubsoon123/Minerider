//! Shared test helpers: a mock Minecraft server that speaks the full
//! phase-1 flow using the same library primitives as the client.
//!
//! Compiled once per integration test binary; not every binary uses every
//! helper, so dead-code warnings are silenced here.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ::rsa::pkcs8::EncodePublicKey;
use ::rsa::Pkcs1v15Encrypt;
use minerider::core::error::{MineRiderError, Result};
use minerider::network::connection::Connection;
use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::crypto::rsa;
use minerider_protocol::generated::v1_21_4::{configuration, handshaking, login, play};
use minerider_protocol::generated::versions::V1_21_4;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// Protocol version the mock server expects in the handshake.
pub const EXPECTED_PROTOCOL: i32 = V1_21_4.protocol;

/// A mock Minecraft server running the full connect flow on a background
/// task against `127.0.0.1:<port>`.
pub struct MockServer {
    /// Port the server is listening on.
    pub port: u16,
    /// Number of keep-alive echoes the server observed from the client.
    pub echo_count: Arc<AtomicUsize>,
    /// The keep-alive ids the client echoed back, in order.
    pub echo_ids: Arc<Mutex<Vec<i64>>>,
    handle: JoinHandle<Result<()>>,
}

impl MockServer {
    /// Starts a mock server that performs the full RSA + AES encryption
    /// handshake, then enables compression (threshold 64), and exchanges
    /// three play-state keep-alives before closing.
    pub async fn start_encrypted() -> MockServer {
        Self::start(Mode::Encrypted).await
    }

    /// Starts a mock server that skips the encryption exchange entirely and
    /// goes straight to compression (threshold 16, so the ≥ 16-byte
    /// LoginSuccess is compressed while 9-byte keep-alives stay raw), then
    /// exchanges one play-state keep-alive before closing.
    pub async fn start_plain() -> MockServer {
        Self::start(Mode::Plain).await
    }

    /// Starts a mock server that logs the client in without encryption,
    /// then kicks it during configuration with a Disconnect packet whose
    /// reason is a network NBT text component.
    pub async fn start_config_disconnect() -> MockServer {
        Self::start(Mode::ConfigDisconnect).await
    }

    /// Scenario 2: plain login, a play-state Login packet, then three
    /// keep-alives (join and stand still).
    pub async fn start_join_idle() -> MockServer {
        Self::start(Mode::JoinIdle).await
    }

    /// Scenario 3: plain login, play Login, then a chunk batch
    /// (start, one minimal chunk, finished). The vanilla obligation is a
    /// `chunk_batch_received` response; the mock tolerates its absence.
    pub async fn start_chunk_streaming() -> MockServer {
        Self::start(Mode::ChunkStreaming).await
    }

    /// Scenario 4: plain login, play Login, then a Synchronize Player
    /// Position with teleport id 1. The vanilla obligation is an
    /// `accept_teleportation` response; the mock tolerates its absence.
    pub async fn start_teleport_correction() -> MockServer {
        Self::start(Mode::TeleportCorrection).await
    }

    /// Plain login, then during configuration the server offers a resource
    /// pack. The mock asserts the client declines it (echoing the offered
    /// uuid) rather than staying silent or claiming to have loaded it.
    pub async fn start_resource_pack() -> MockServer {
        Self::start(Mode::ResourcePack).await
    }

    async fn start(mode: Mode) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let port = listener.local_addr().expect("local addr").port();
        let echo_count = Arc::new(AtomicUsize::new(0));
        let echo_ids = Arc::new(Mutex::new(Vec::new()));
        let (count_clone, ids_clone) = (echo_count.clone(), echo_ids.clone());
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            run_server(stream, mode, count_clone, ids_clone).await
        });
        MockServer {
            port,
            echo_count,
            echo_ids,
            handle,
        }
    }

    /// Waits for the server task to finish, propagating its error.
    pub async fn finish(self) -> Result<()> {
        self.handle.await.expect("mock server task panicked")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Encrypted,
    Plain,
    ConfigDisconnect,
    JoinIdle,
    ChunkStreaming,
    TeleportCorrection,
    ResourcePack,
}

fn ensure(cond: bool, msg: impl Into<String>) -> Result<()> {
    if cond {
        Ok(())
    } else {
        Err(MineRiderError::Protocol(format!(
            "mock server: {}",
            msg.into()
        )))
    }
}

fn ensure_vanilla_movement(packet: &minerider_protocol::packet::RawPacket) -> Result<()> {
    ensure(
        matches!(
            packet.id,
            play::SERVERBOUND_POSITION_ID
                | play::SERVERBOUND_POSITION_LOOK_ID
                | play::SERVERBOUND_LOOK_ID
                | play::SERVERBOUND_FLYING_ID
        ),
        format!("expected vanilla movement packet, got 0x{:02x}", packet.id),
    )
}

async fn run_server(
    stream: TcpStream,
    mode: Mode,
    echo_count: Arc<AtomicUsize>,
    echo_ids: Arc<Mutex<Vec<i64>>>,
) -> Result<()> {
    let mut conn = Connection::from_tcp_stream(stream)?;

    // Handshake (C2S 0x00).
    let hs = conn.read_packet().await?;
    ensure(
        hs.id == handshaking::SERVERBOUND_SET_PROTOCOL_ID,
        "expected handshake packet 0x00",
    )?;
    {
        let mut r = PacketReader::new(&hs.payload);
        let protocol = r.get_varint()?;
        let _address = r.read_string()?;
        let _port = r.get_u16()?;
        let next_state = r.get_varint()?;
        ensure(
            protocol == EXPECTED_PROTOCOL,
            format!("protocol {protocol}, expected {EXPECTED_PROTOCOL}"),
        )?;
        ensure(
            next_state == 2,
            format!("next state {next_state}, expected 2 (login)"),
        )?;
    }

    // Login Start (C2S 0x00).
    let ls = conn.read_packet().await?;
    ensure(
        ls.id == login::SERVERBOUND_LOGIN_START_ID,
        "expected login start 0x00",
    )?;
    {
        let mut r = PacketReader::new(&ls.payload);
        let _username = r.read_string()?;
        let _uuid = r.read_uuid()?;
    }

    if mode == Mode::Encrypted {
        encryption_exchange(&mut conn).await?;
    }

    // Set Compression (S2C 0x03). Sent uncompressed; both sides enable
    // compression for the frames that follow.
    let threshold = match mode {
        Mode::Encrypted => 64,
        _ => 16,
    };
    let mut w = PacketWriter::new();
    w.put_varint(threshold);
    conn.send_packet(login::CLIENTBOUND_COMPRESS_ID, &w.into_inner())
        .await?;
    conn.set_compression(threshold);

    // Login Success (S2C 0x02). Payload must be ≥ threshold so the
    // compressed codec path is exercised.
    let username = match mode {
        Mode::Encrypted => format!("MockPlayer{}", "x".repeat(60)),
        _ => "MockPlayer".to_string(),
    };
    let mut w = PacketWriter::new();
    w.put_uuid(0x00112233445566778899AABBCCDDEEFF);
    w.put_string(&username)?;
    w.put_varint(0); // zero properties
    conn.send_packet(login::CLIENTBOUND_SUCCESS_ID, &w.into_inner())
        .await?;

    // Login Acknowledged (C2S 0x03).
    let ack = conn.read_packet().await?;
    ensure(
        ack.id == login::SERVERBOUND_LOGIN_ACKNOWLEDGED_ID,
        format!("expected login acknowledged 0x03, got 0x{:02x}", ack.id),
    )?;
    ensure(ack.payload.is_empty(), "login acknowledged must be empty")?;

    // On entering configuration a vanilla client sends client_information
    // (settings, C2S 0x00) then the minecraft:brand plugin message
    // (custom_payload, C2S 0x02).
    let settings = conn.read_packet().await?;
    ensure(
        settings.id == configuration::SERVERBOUND_SETTINGS_ID,
        format!("expected settings 0x00, got 0x{:02x}", settings.id),
    )?;
    let brand = conn.read_packet().await?;
    ensure(
        brand.id == configuration::SERVERBOUND_CUSTOM_PAYLOAD_ID,
        format!("expected brand custom_payload 0x02, got 0x{:02x}", brand.id),
    )?;
    {
        let mut r = PacketReader::new(&brand.payload);
        let channel = r.read_string()?;
        ensure(
            channel == "minecraft:brand",
            format!("brand channel {channel:?}, expected minecraft:brand"),
        )?;
        let reported = r.read_string()?;
        ensure(
            reported == "vanilla",
            format!("brand {reported:?}, expected vanilla"),
        )?;
    }

    if mode == Mode::ResourcePack {
        use minerider_protocol::generated::v1_21_4::types::PacketCommonAddResourcePack;
        use minerider_protocol::traits::Encode;

        const OFFERED_PACK_UUID: u128 = 0x1234_5678_9abc_def0_1122_3344_5566_7788;
        let pack = PacketCommonAddResourcePack {
            uuid: OFFERED_PACK_UUID,
            url: "https://example.invalid/pack.zip".to_string(),
            hash: String::new(),
            forced: false,
            prompt_message: None,
        };
        let mut w = PacketWriter::new();
        pack.encode(&mut w)
            .map_err(|e| MineRiderError::Protocol(format!("mock server: encode pack: {e}")))?;
        conn.send_packet(
            configuration::CLIENTBOUND_ADD_RESOURCE_PACK_ID,
            &w.into_inner(),
        )
        .await?;

        let response = conn.read_packet().await?;
        ensure(
            response.id == configuration::SERVERBOUND_RESOURCE_PACK_RECEIVE_ID,
            format!(
                "expected resource_pack_receive 0x{:02x}, got 0x{:02x}",
                configuration::SERVERBOUND_RESOURCE_PACK_RECEIVE_ID,
                response.id
            ),
        )?;
        let mut r = PacketReader::new(&response.payload);
        let uuid = r.read_uuid()?;
        let result = r.get_varint()?;
        ensure(
            uuid == OFFERED_PACK_UUID,
            "resource_pack_receive echoed the wrong pack uuid",
        )?;
        ensure(
            result == 1,
            format!("expected declined (result=1), got {result}"),
        )?;
    }

    // Configuration: Finish Configuration (S2C 0x03), expect C2S 0x03.
    if mode == Mode::ConfigDisconnect {
        // Configuration Disconnect (S2C 0x02) instead: the reason is a
        // network NBT text component (anonymousNbt). Minimal compound
        // `{text:"kicked"}`: TAG_Compound, TAG_String "text" = "kicked",
        // TAG_End.
        let reason_nbt: &[u8] = &[
            0x0A, 0x08, 0x00, 0x04, b't', b'e', b'x', b't', 0x00, 0x06, b'k', b'i', b'c', b'k',
            b'e', b'd', 0x00,
        ];
        conn.send_packet(configuration::CLIENTBOUND_DISCONNECT_ID, reason_nbt)
            .await?;
        conn.close().await?;
        return Ok(());
    }
    send_dimension_registry(&mut conn).await?;
    conn.send_packet(configuration::CLIENTBOUND_FINISH_CONFIGURATION_ID, &[])
        .await?;
    let fin = conn.read_packet().await?;
    ensure(
        fin.id == configuration::SERVERBOUND_FINISH_CONFIGURATION_ID,
        format!("expected finish configuration 0x03, got 0x{:02x}", fin.id),
    )?;

    // Scenario modes start the play state with Login. The join-idle scenario
    // then supplies the same readiness ingredients vanilla waits for: an
    // authoritative position and an initial chunk batch.
    if matches!(
        mode,
        Mode::JoinIdle | Mode::ChunkStreaming | Mode::TeleportCorrection
    ) {
        send_play_login(&mut conn).await?;
    }
    if mode == Mode::JoinIdle {
        send_position(&mut conn).await?;
        send_chunk_batch(&mut conn).await?;
        let mut saw_teleport_confirm = false;
        let mut saw_batch_ack = false;
        let mut saw_player_loaded = false;
        while !(saw_teleport_confirm && saw_batch_ack && saw_player_loaded) {
            let packet =
                tokio::time::timeout(std::time::Duration::from_secs(3), conn.read_packet())
                    .await
                    .map_err(|_| {
                        MineRiderError::Protocol(
                            "mock server: play readiness packets did not arrive within 3s"
                                .to_string(),
                        )
                    })??;
            match packet.id {
                play::SERVERBOUND_TELEPORT_CONFIRM_ID => saw_teleport_confirm = true,
                play::SERVERBOUND_CHUNK_BATCH_RECEIVED_ID => saw_batch_ack = true,
                play::SERVERBOUND_PLAYER_LOADED_ID => {
                    ensure(packet.payload.is_empty(), "player_loaded must be empty")?;
                    saw_player_loaded = true;
                }
                other => {
                    return Err(MineRiderError::Protocol(format!(
                        "mock server: unexpected readiness packet 0x{other:02x}"
                    )));
                }
            }
        }
    }

    match mode {
        Mode::ChunkStreaming => {
            send_chunk_batch(&mut conn).await?;
            // Vanilla clients answer chunk_batch_finished with
            // chunk_batch_received carrying the desired chunks-per-tick.
            let ack = tokio::time::timeout(std::time::Duration::from_secs(3), conn.read_packet())
                .await
                .map_err(|_| {
                    MineRiderError::Protocol(
                        "mock server: no chunk_batch_received within 3s".to_string(),
                    )
                })??;
            ensure(
                ack.id == play::SERVERBOUND_CHUNK_BATCH_RECEIVED_ID,
                format!("expected chunk_batch_received 0x09, got 0x{:02x}", ack.id),
            )?;
            let mut r = PacketReader::new(&ack.payload);
            let chunks_per_tick = r.get_f32()?;
            ensure(
                chunks_per_tick == 1.0,
                format!("chunks_per_tick {chunks_per_tick}, expected 1.0 (batch size)"),
            )?;
            conn.close().await?;
            return Ok(());
        }
        Mode::TeleportCorrection => {
            send_position(&mut conn).await?;
            // Vanilla clients answer with teleport_confirm echoing the id.
            let confirm =
                tokio::time::timeout(std::time::Duration::from_secs(3), conn.read_packet())
                    .await
                    .map_err(|_| {
                        MineRiderError::Protocol(
                            "mock server: no teleport_confirm within 3s".to_string(),
                        )
                    })??;
            ensure(
                confirm.id == play::SERVERBOUND_TELEPORT_CONFIRM_ID,
                format!("expected teleport_confirm 0x00, got 0x{:02x}", confirm.id),
            )?;
            let mut r = PacketReader::new(&confirm.payload);
            let teleport_id = r.get_varint()?;
            ensure(
                teleport_id == 1,
                format!("teleport_confirm id {teleport_id}, expected 1"),
            )?;
            conn.close().await?;
            return Ok(());
        }
        _ => {}
    }

    if mode == Mode::ResourcePack {
        // The identical exchange also happens in the play state (a distinct
        // packet id from the configuration-state one, same wire shape);
        // vanilla answers it there too, not just during configuration.
        use minerider_protocol::generated::v1_21_4::types::PacketCommonAddResourcePack;
        use minerider_protocol::traits::Encode;

        const PLAY_PACK_UUID: u128 = 0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00;
        let pack = PacketCommonAddResourcePack {
            uuid: PLAY_PACK_UUID,
            url: "https://example.invalid/play-pack.zip".to_string(),
            hash: String::new(),
            forced: false,
            prompt_message: None,
        };
        let mut w = PacketWriter::new();
        pack.encode(&mut w)
            .map_err(|e| MineRiderError::Protocol(format!("mock server: encode pack: {e}")))?;
        conn.send_packet(play::CLIENTBOUND_ADD_RESOURCE_PACK_ID, &w.into_inner())
            .await?;

        let response = conn.read_packet().await?;
        ensure(
            response.id == play::SERVERBOUND_RESOURCE_PACK_RECEIVE_ID,
            format!(
                "expected play resource_pack_receive 0x{:02x}, got 0x{:02x}",
                play::SERVERBOUND_RESOURCE_PACK_RECEIVE_ID,
                response.id
            ),
        )?;
        let mut r = PacketReader::new(&response.payload);
        let uuid = r.read_uuid()?;
        let result = r.get_varint()?;
        ensure(
            uuid == PLAY_PACK_UUID,
            "play resource_pack_receive echoed the wrong pack uuid",
        )?;
        ensure(
            result == 1,
            format!("expected declined (result=1), got {result}"),
        )?;
    }

    // Play state: keep-alives (S2C 0x27, i64 payload), expect C2S 0x1a echoes.
    let keepalive_ids: &[i64] = match mode {
        Mode::Encrypted | Mode::JoinIdle => &[42, 43, 44],
        Mode::Plain | Mode::ResourcePack => &[42],
        Mode::ConfigDisconnect | Mode::ChunkStreaming | Mode::TeleportCorrection => {
            unreachable!("non-keepalive mock modes returned earlier")
        }
    };
    if mode == Mode::JoinIdle {
        let movement = tokio::time::timeout(std::time::Duration::from_millis(1200), async {
            loop {
                let packet = conn.read_packet().await?;
                if packet.id == play::SERVERBOUND_POSITION_ID {
                    return Ok::<_, MineRiderError>(packet);
                }
                ensure_vanilla_movement(&packet)?;
            }
        })
        .await
        .map_err(|_| {
            MineRiderError::Protocol(
                "mock server: no idle position reminder within 1200ms".to_string(),
            )
        })??;
        let mut r = PacketReader::new(&movement.payload);
        ensure(r.get_f64()? == 0.5, "idle movement x must match teleport")?;
        ensure(r.get_f64()? == 64.0, "idle movement y must match teleport")?;
        ensure(r.get_f64()? == 0.5, "idle movement z must match teleport")?;
        ensure(
            r.get_u8()? == 1,
            "idle movement flags must be on-ground/no collision",
        )?;
    }

    for &id in keepalive_ids {
        let mut w = PacketWriter::new();
        w.put_i64(id);
        conn.send_packet(play::CLIENTBOUND_KEEP_ALIVE_ID, &w.into_inner())
            .await?;

        let echo = loop {
            let packet = conn.read_packet().await?;
            if packet.id == play::SERVERBOUND_KEEP_ALIVE_ID {
                break packet;
            }
            ensure_vanilla_movement(&packet)?;
        };
        let mut r = PacketReader::new(&echo.payload);
        let echoed = r.get_i64()?;
        ensure(echoed == id, format!("echoed id {echoed}, expected {id}"))?;
        echo_count.fetch_add(1, Ordering::SeqCst);
        echo_ids.lock().await.push(echoed);
    }

    conn.close().await?;
    Ok(())
}

async fn send_dimension_registry(conn: &mut Connection) -> Result<()> {
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
    packet
        .encode(&mut w)
        .map_err(|e| MineRiderError::Protocol(format!("mock server: encode registry: {e}")))?;
    conn.send_packet(configuration::CLIENTBOUND_REGISTRY_DATA_ID, &w.into_inner())
        .await
}

/// Sends a play-state Login packet with fixed overworld values.
async fn send_play_login(conn: &mut Connection) -> Result<()> {
    use minerider_protocol::traits::Encode;
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
            previous_gamemode: 255, // -1: no previous gamemode
            is_debug: false,
            is_flat: false,
            death: None,
            portal_cooldown: 0,
            sea_level: 63,
        },
        enforces_secure_chat: false,
    };
    let mut w = PacketWriter::new();
    packet
        .encode(&mut w)
        .map_err(|e| MineRiderError::Protocol(format!("mock server: encode login: {e}")))?;
    conn.send_packet(play::CLIENTBOUND_LOGIN_ID, &w.into_inner())
        .await?;
    Ok(())
}

/// Sends one valid 24-section Overworld chunk batch.
async fn send_chunk_batch(conn: &mut Connection) -> Result<()> {
    use minerider_protocol::nbt::Nbt;
    use minerider_protocol::traits::Encode;

    conn.send_packet(play::CLIENTBOUND_CHUNK_BATCH_START_ID, &[])
        .await?;
    let mut chunk_data = PacketWriter::new();
    for section in 0..24 {
        if section == 7 {
            // Stone floor at world Y=63 (local Y=15 in section 7), with air
            // above it. Four-bit indirect palette: air=0, stone=1.
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
        chunk_data.put_u8(0); // single biome value
        chunk_data.put_varint(0);
        chunk_data.put_varint(0); // no packed biome longs
    }
    let chunk_data = chunk_data.into_inner();

    let mut w = PacketWriter::new();
    w.put_i32(0);
    w.put_i32(0);
    Nbt::Compound(vec![]).encode(&mut w)?;
    w.put_byte_array(&chunk_data);
    w.put_varint(0); // block entities
    w.put_varint(0); // sky light mask (empty bitset)
    w.put_varint(0); // block light mask
    w.put_varint(0); // empty sky light mask
    w.put_varint(0); // empty block light mask
    w.put_varint(0); // sky light arrays
    w.put_varint(0); // block light arrays
    conn.send_packet(play::CLIENTBOUND_MAP_CHUNK_ID, &w.into_inner())
        .await?;
    let mut w = PacketWriter::new();
    w.put_varint(1); // batch size
    conn.send_packet(play::CLIENTBOUND_CHUNK_BATCH_FINISHED_ID, &w.into_inner())
        .await?;
    Ok(())
}

/// Sends Synchronize Player Position with teleport id 1.
async fn send_position(conn: &mut Connection) -> Result<()> {
    use minerider_protocol::traits::Encode;
    let packet = play::PacketPosition {
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
    packet
        .encode(&mut w)
        .map_err(|e| MineRiderError::Protocol(format!("mock server: encode position: {e}")))?;
    conn.send_packet(play::CLIENTBOUND_POSITION_ID, &w.into_inner())
        .await?;
    Ok(())
}

async fn encryption_exchange(conn: &mut Connection) -> Result<()> {
    let (public, private) = rsa::generate_keypair(1024)?;
    let der = public
        .to_public_key_der()
        .map_err(|e| MineRiderError::Crypto(format!("mock server: DER encode failed: {e}")))?;
    let verify_token = [0x5A, 0x11, 0x22, 0x33];

    // Encryption Request (S2C 0x01), sent unencrypted.
    let mut w = PacketWriter::new();
    w.put_string("")?;
    w.put_byte_array(der.as_bytes());
    w.put_byte_array(&verify_token);
    w.put_bool(false); // should_authenticate
    conn.send_packet(login::CLIENTBOUND_ENCRYPTION_BEGIN_ID, &w.into_inner())
        .await?;

    // Encryption Response (C2S 0x01), still unencrypted.
    let resp = conn.read_packet().await?;
    ensure(
        resp.id == login::SERVERBOUND_ENCRYPTION_BEGIN_ID,
        "expected encryption response 0x01",
    )?;
    let (encrypted_secret, encrypted_token) = {
        let mut r = PacketReader::new(&resp.payload);
        let secret = r.read_byte_array()?.to_vec();
        let token = r.read_byte_array()?.to_vec();
        (secret, token)
    };

    let shared_secret = private
        .decrypt(Pkcs1v15Encrypt, &encrypted_secret)
        .map_err(|e| MineRiderError::Crypto(format!("mock server: secret decrypt failed: {e}")))?;
    let token = private
        .decrypt(Pkcs1v15Encrypt, &encrypted_token)
        .map_err(|e| MineRiderError::Crypto(format!("mock server: token decrypt failed: {e}")))?;
    ensure(token == verify_token, "verify token mismatch")?;
    let secret: [u8; 16] = shared_secret.try_into().map_err(|_| {
        MineRiderError::Crypto("mock server: shared secret not 16 bytes".to_string())
    })?;

    conn.enable_encryption(&secret);
    Ok(())
}
