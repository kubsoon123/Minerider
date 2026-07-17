//! Full-runtime memory benchmark: real `Client`s, a real Tokio runtime, real
//! loopback TCP sockets, against an in-process mock server — as opposed to
//! `shared_world_benchmark`, which only measures bare chunk data structures
//! with no connection, protocol loop, or task/channel state at all.
//!
//! Run with:
//!
//! cargo run --release --bin full_runtime_benchmark
//!
//! Every (scenario, client count, sharing) case runs in a fresh child
//! process (same pattern as `shared_world_benchmark`) so Linux/Windows RSS
//! readings never inherit allocator high-water marks from a previous case.
//! The child walks one connected swarm through a fixed lifecycle — idle,
//! chunks loaded, entity/HUD/inventory populated, update churn, unload,
//! disconnect, cleanup — printing one RESULT line per stage.
//!
//! This never talks to a real Minecraft server: everything is a
//! `127.0.0.1` mock speaking synthetic protocol data.

use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use minerider::core::client::{Client, ClientConfig};
use minerider::network::connection::Connection;
use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::generated::v1_21_4::types::{Slot, SlotValue, SlotValueDefault, Vec3i16};
use minerider_protocol::generated::v1_21_4::{configuration, handshaking, login, play};
use minerider_protocol::nbt::Nbt;
use minerider_protocol::traits::Encode;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::timeout;

const STAGE_SETTLE: Duration = Duration::from_millis(150);
const STAGE_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
enum Stage {
    Connecting = 0,
    Idle = 1,
    Chunks = 2,
    EntityHudInventory = 3,
    Churn = 4,
    Unload = 5,
    Disconnect = 6,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Idle => "idle_connected",
            Self::Chunks => "chunks_loaded",
            Self::EntityHudInventory => "entity_hud_inventory",
            Self::Churn => "update_churn",
            Self::Unload => "unloaded",
            Self::Disconnect => "disconnected",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkScenario {
    Identical49,
    Identical441,
    MostlyIdentical,
    Personalized,
}

impl ChunkScenario {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "identical-49" => Some(Self::Identical49),
            "identical-441" => Some(Self::Identical441),
            "mostly-identical" => Some(Self::MostlyIdentical),
            "personalized" => Some(Self::Personalized),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Identical49 => "identical-49",
            Self::Identical441 => "identical-441",
            Self::MostlyIdentical => "mostly-identical",
            Self::Personalized => "personalized",
        }
    }

    fn chunk_count(self) -> usize {
        match self {
            Self::Identical441 => 441,
            _ => 49,
        }
    }

    /// Whether client `index`'s chunk at grid position `chunk_index` should
    /// carry a per-client palette tweak (personalized content defeats
    /// content-based chunk-payload sharing; identical content does not).
    fn personalize(self, client_index: usize, chunk_index: usize) -> bool {
        match self {
            Self::Personalized => true,
            // ~2%: one changed chunk per client out of 49-ish, matching
            // shared_world_benchmark's mostly-identical case.
            Self::MostlyIdentical => chunk_index == client_index % 49,
            Self::Identical49 | Self::Identical441 => false,
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == "--case") {
        return run_case_process(&args);
    }
    run_suite()
}

fn run_suite() -> ExitCode {
    println!(
        "ENV,os={},arch={}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("NOTE,rss_kib is this process's own working-set/RSS at the time of the reading; each case runs in a fresh child process");

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("failed to locate benchmark executable: {error}");
            return ExitCode::FAILURE;
        }
    };

    // (scenario, clients, sharing_enabled, samples)
    let cases: Vec<(ChunkScenario, usize, bool, usize)> = build_case_matrix();

    for (scenario, clients, sharing, samples) in cases {
        for sample in 0..samples {
            let output = match Command::new(&exe)
                .args([
                    "--case",
                    scenario.name(),
                    &clients.to_string(),
                    if sharing { "on" } else { "off" },
                    &sample.to_string(),
                ])
                .output()
            {
                Ok(output) => output,
                Err(error) => {
                    eprintln!("failed to spawn case: {error}");
                    return ExitCode::FAILURE;
                }
            };
            print!("{}", String::from_utf8_lossy(&output.stdout));
            if !output.status.success() {
                eprintln!(
                    "case scenario={} clients={clients} sharing={sharing} sample={sample} FAILED",
                    scenario.name()
                );
                eprint!("{}", String::from_utf8_lossy(&output.stderr));
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

/// The real matrix executed locally. Trimmed relative to the full mission
/// spec at high client counts / large view distances, where a single
/// developer machine cannot repeat a 100-client x 441-chunk real-socket run
/// several times in reasonable wall time; see docs/shared_world_benchmark.md
/// for exactly which cells are measured vs extrapolated.
fn build_case_matrix() -> Vec<(ChunkScenario, usize, bool, usize)> {
    let mut cases = Vec::new();
    let client_tiers = [1usize, 10, 25, 50, 100];
    // Primary matrix: identical-49, both sharing settings, all tiers.
    for &clients in &client_tiers {
        let samples = if clients >= 50 { 3 } else { 5 };
        cases.push((ChunkScenario::Identical49, clients, true, samples));
        cases.push((ChunkScenario::Identical49, clients, false, samples));
    }
    // Large view distance: only up to 25 clients (441 chunks x 100 clients
    // is multiple GiB of synthetic chunk data pre-sharing; not attempted).
    for &clients in &[1usize, 10, 25] {
        cases.push((ChunkScenario::Identical441, clients, true, 3));
    }
    // Worst-case content scenarios at max scale only.
    cases.push((ChunkScenario::MostlyIdentical, 100, true, 3));
    cases.push((ChunkScenario::MostlyIdentical, 100, false, 3));
    cases.push((ChunkScenario::Personalized, 100, true, 3));
    cases.push((ChunkScenario::Personalized, 100, false, 3));
    cases
}

fn run_case_process(args: &[String]) -> ExitCode {
    let Some(scenario) = args.get(2).and_then(|v| ChunkScenario::parse(v)) else {
        eprintln!("unknown or missing scenario");
        return ExitCode::FAILURE;
    };
    let Some(clients) = args.get(3).and_then(|v| v.parse::<usize>().ok()) else {
        eprintln!("clients must be a positive integer");
        return ExitCode::FAILURE;
    };
    let Some(sharing) = args.get(4).map(|v| v == "on") else {
        eprintln!("sharing must be on|off");
        return ExitCode::FAILURE;
    };
    let sample = args.get(5).cloned().unwrap_or_else(|| "0".to_string());

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(error) => {
            eprintln!("failed to start async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run_case(scenario, clients, sharing, &sample)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("case failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run_case(
    scenario: ChunkScenario,
    clients: usize,
    sharing: bool,
    sample: &str,
) -> Result<(), String> {
    let baseline_rss = process_rss_kib();
    report(
        scenario,
        clients,
        sharing,
        sample,
        "baseline",
        baseline_rss,
        Duration::ZERO,
    );

    let case_start = Instant::now();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("bind mock server: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();

    let (stage_tx, stage_rx) = watch::channel(Stage::Connecting);
    let reached: Arc<[AtomicUsize; 7]> = Arc::new(Default::default());

    // Accept loop: spawns one handler per connection, up to `clients`.
    let accept_reached = reached.clone();
    let accept_stage_rx = stage_rx.clone();
    let accept_task = tokio::spawn(async move {
        let mut handles = Vec::with_capacity(clients);
        for _ in 0..clients {
            let (stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("accept failed: {e}");
                    break;
                }
            };
            let reached = accept_reached.clone();
            let stage_rx = accept_stage_rx.clone();
            handles.push(tokio::spawn(async move {
                if let Err(e) = serve_one(stream, scenario, reached, stage_rx).await {
                    eprintln!("mock server connection ended: {e}");
                }
            }));
        }
        for handle in handles {
            let _ = handle.await;
        }
    });

    // Client swarm.
    let mut client_handles = Vec::with_capacity(clients);
    for i in 0..clients {
        let cfg = ClientConfig::new("127.0.0.1", port, format!("Bench{i}"))
            .with_view_distance(if scenario == ChunkScenario::Identical441 {
                10
            } else {
                5
            })
            .with_chunk_sharing(sharing)
            .with_connect_deadline(Duration::from_secs(30));
        client_handles.push(tokio::spawn(async move {
            let mut client = Client::connect(&cfg).await?;
            client.run().await
        }));
    }

    wait_for_stage(&reached, Stage::Idle, clients).await?;
    tokio::time::sleep(STAGE_SETTLE).await;
    report(
        scenario,
        clients,
        sharing,
        sample,
        Stage::Idle.name(),
        process_rss_kib(),
        case_start.elapsed(),
    );

    for stage in [
        Stage::Chunks,
        Stage::EntityHudInventory,
        Stage::Churn,
        Stage::Unload,
    ] {
        stage_tx
            .send(stage)
            .map_err(|_| "stage channel closed".to_string())?;
        wait_for_stage(&reached, stage, clients).await?;
        tokio::time::sleep(STAGE_SETTLE).await;
        report(
            scenario,
            clients,
            sharing,
            sample,
            stage.name(),
            process_rss_kib(),
            case_start.elapsed(),
        );
    }

    // Disconnect: server kicks every connection; client.run() tasks return.
    stage_tx
        .send(Stage::Disconnect)
        .map_err(|_| "stage channel closed".to_string())?;
    for handle in client_handles {
        // A clean benchmark disconnect surfaces as Err(Disconnected(_)) from
        // client.run(); anything else (panic, hang) is a real bug.
        let _ = timeout(STAGE_TIMEOUT, handle)
            .await
            .map_err(|_| "client task did not finish within stage timeout".to_string())?;
    }
    let _ = timeout(STAGE_TIMEOUT, accept_task).await;
    tokio::time::sleep(STAGE_SETTLE).await;
    report(
        scenario,
        clients,
        sharing,
        sample,
        Stage::Disconnect.name(),
        process_rss_kib(),
        case_start.elapsed(),
    );

    drop(stage_rx);
    tokio::time::sleep(STAGE_SETTLE).await;
    report(
        scenario,
        clients,
        sharing,
        sample,
        "cleanup",
        process_rss_kib(),
        case_start.elapsed(),
    );

    Ok(())
}

async fn wait_for_stage(
    reached: &Arc<[AtomicUsize; 7]>,
    stage: Stage,
    clients: usize,
) -> Result<(), String> {
    let index = stage as usize;
    timeout(STAGE_TIMEOUT, async {
        loop {
            if reached[index].load(Ordering::Acquire) >= clients {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| {
        format!(
            "timed out waiting for all clients to reach {}",
            stage.name()
        )
    })
}

fn report(
    scenario: ChunkScenario,
    clients: usize,
    sharing: bool,
    sample: &str,
    stage: &str,
    rss_kib: Option<u64>,
    elapsed: Duration,
) {
    println!(
        "RESULT,scenario={},clients={clients},sharing={},sample={sample},stage={stage},rss_kib={},elapsed_ms={}",
        scenario.name(),
        if sharing { "on" } else { "off" },
        rss_kib.map_or_else(|| "unavailable".to_string(), |v| v.to_string()),
        elapsed.as_millis(),
    );
}

// ---------------------------------------------------------------------
// Mock server: one handler per connection, gated on the shared stage
// watch channel so all clients in a case receive each stage's packets in
// lockstep (the benchmark measures RSS once every client has *processed*
// a stage, not merely once the server has sent it).
// ---------------------------------------------------------------------

async fn serve_one(
    stream: TcpStream,
    scenario: ChunkScenario,
    reached: Arc<[AtomicUsize; 7]>,
    mut stage_rx: watch::Receiver<Stage>,
) -> Result<(), String> {
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    let mut conn = Connection::from_tcp_stream(stream).map_err(|e| e.to_string())?;

    // Handshake.
    let hs = conn.read_packet().await.map_err(|e| e.to_string())?;
    if hs.id != handshaking::SERVERBOUND_SET_PROTOCOL_ID {
        return Err(format!("expected handshake, got 0x{:02x}", hs.id));
    }

    // Login start, straight to success (no encryption, matches offline mode).
    let ls = conn.read_packet().await.map_err(|e| e.to_string())?;
    if ls.id != login::SERVERBOUND_LOGIN_START_ID {
        return Err(format!("expected login start, got 0x{:02x}", ls.id));
    }
    let username = {
        let mut r = PacketReader::new(&ls.payload);
        r.read_string().map_err(|e| e.to_string())?
    };

    let mut w = PacketWriter::new();
    w.put_varint(256);
    conn.send_packet(login::CLIENTBOUND_COMPRESS_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())?;
    conn.set_compression(256);

    let mut w = PacketWriter::new();
    w.put_uuid(0);
    w.put_string(username).map_err(|e| e.to_string())?;
    w.put_varint(0);
    conn.send_packet(login::CLIENTBOUND_SUCCESS_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())?;

    let ack = conn.read_packet().await.map_err(|e| e.to_string())?;
    if ack.id != login::SERVERBOUND_LOGIN_ACKNOWLEDGED_ID {
        return Err(format!("expected login acknowledged, got 0x{:02x}", ack.id));
    }

    // Configuration.
    let settings = conn.read_packet().await.map_err(|e| e.to_string())?;
    if settings.id != configuration::SERVERBOUND_SETTINGS_ID {
        return Err(format!("expected settings, got 0x{:02x}", settings.id));
    }
    let brand = conn.read_packet().await.map_err(|e| e.to_string())?;
    if brand.id != configuration::SERVERBOUND_CUSTOM_PAYLOAD_ID {
        return Err(format!("expected brand, got 0x{:02x}", brand.id));
    }

    send_dimension_registry(&mut conn).await?;
    conn.send_packet(configuration::CLIENTBOUND_FINISH_CONFIGURATION_ID, &[])
        .await
        .map_err(|e| e.to_string())?;
    let fin = conn.read_packet().await.map_err(|e| e.to_string())?;
    if fin.id != configuration::SERVERBOUND_FINISH_CONFIGURATION_ID {
        return Err(format!(
            "expected finish configuration, got 0x{:02x}",
            fin.id
        ));
    }

    // Play: login + position, then wait for the client to settle (idle).
    send_play_login(&mut conn).await?;
    send_position(&mut conn, 1).await?;
    wait_for_ready(&mut conn).await?;
    reached[Stage::Idle as usize].fetch_add(1, Ordering::AcqRel);

    // Chunks.
    wait_for(&mut stage_rx, Stage::Chunks).await;
    let client_index: usize = username.trim_start_matches("Bench").parse().unwrap_or(0);
    send_chunks(&mut conn, scenario, client_index).await?;
    reached[Stage::Chunks as usize].fetch_add(1, Ordering::AcqRel);

    // Entity/HUD/inventory.
    wait_for(&mut stage_rx, Stage::EntityHudInventory).await;
    send_entity_hud_inventory(&mut conn, client_index).await?;
    reached[Stage::EntityHudInventory as usize].fetch_add(1, Ordering::AcqRel);

    // Update churn: repeated health/chunk-section updates.
    wait_for(&mut stage_rx, Stage::Churn).await;
    send_churn(&mut conn, scenario, client_index).await?;
    reached[Stage::Churn as usize].fetch_add(1, Ordering::AcqRel);

    // Unload every chunk this client was sent.
    wait_for(&mut stage_rx, Stage::Unload).await;
    send_unload(&mut conn, scenario).await?;
    reached[Stage::Unload as usize].fetch_add(1, Ordering::AcqRel);

    // Disconnect.
    wait_for(&mut stage_rx, Stage::Disconnect).await;
    let reason = Nbt::Compound(vec![(
        "text".to_string(),
        Nbt::String("benchmark done".to_string()),
    )]);
    let mut w = PacketWriter::new();
    reason.encode(&mut w).map_err(|e| e.to_string())?;
    conn.send_packet(play::CLIENTBOUND_KICK_DISCONNECT_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())?;
    let _ = conn.close().await;
    reached[Stage::Disconnect as usize].fetch_add(1, Ordering::AcqRel);

    Ok(())
}

async fn wait_for(stage_rx: &mut watch::Receiver<Stage>, target: Stage) {
    loop {
        if *stage_rx.borrow() >= target {
            return;
        }
        if stage_rx.changed().await.is_err() {
            return;
        }
    }
}

/// Reads packets until teleport_confirm is seen, tolerating vanilla
/// movement re-sends in between. `player_loaded` vanilla-requires a chunk
/// loaded under the player, so it is *not* waited for here — the client
/// only sends it once `send_chunks` has actually delivered that chunk (see
/// [`wait_for_player_loaded`]).
async fn wait_for_ready(conn: &mut Connection) -> Result<(), String> {
    loop {
        let packet = timeout(Duration::from_secs(10), conn.read_packet())
            .await
            .map_err(|_| "no readiness packets within 10s".to_string())?
            .map_err(|e| e.to_string())?;
        match packet.id {
            play::SERVERBOUND_TELEPORT_CONFIRM_ID => return Ok(()),
            play::SERVERBOUND_POSITION_ID
            | play::SERVERBOUND_POSITION_LOOK_ID
            | play::SERVERBOUND_LOOK_ID
            | play::SERVERBOUND_FLYING_ID => {}
            other => return Err(format!("unexpected readiness packet 0x{other:02x}")),
        }
    }
}

async fn send_dimension_registry(conn: &mut Connection) -> Result<(), String> {
    use configuration::{PacketRegistryData, PacketRegistryDataEntriesItem};
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
    packet.encode(&mut w).map_err(|e| e.to_string())?;
    conn.send_packet(configuration::CLIENTBOUND_REGISTRY_DATA_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())
}

async fn send_play_login(conn: &mut Connection) -> Result<(), String> {
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
    packet.encode(&mut w).map_err(|e| e.to_string())?;
    conn.send_packet(play::CLIENTBOUND_LOGIN_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())
}

async fn send_position(conn: &mut Connection, teleport_id: i32) -> Result<(), String> {
    let packet = play::PacketPosition {
        teleport_id,
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
    packet.encode(&mut w).map_err(|e| e.to_string())?;
    conn.send_packet(play::CLIENTBOUND_POSITION_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())
}

/// Sends `scenario.chunk_count()` chunks as one batch. Content differs by
/// client index exactly as `ChunkScenario::personalize` dictates, so
/// content-addressed chunk-payload sharing across the swarm sees exactly
/// the intended fraction of duplicate vs unique payloads.
async fn send_chunks(
    conn: &mut Connection,
    scenario: ChunkScenario,
    client_index: usize,
) -> Result<(), String> {
    let count = scenario.chunk_count();
    let side = (count as f64).sqrt().ceil() as i32;
    conn.send_packet(play::CLIENTBOUND_CHUNK_BATCH_START_ID, &[])
        .await
        .map_err(|e| e.to_string())?;
    for index in 0..count {
        let x = index as i32 % side - side / 2;
        let z = index as i32 / side - side / 2;
        let personalized = scenario.personalize(client_index, index);
        send_one_chunk(conn, x, z, personalized, client_index).await?;
    }
    let mut w = PacketWriter::new();
    w.put_varint(count as i32);
    conn.send_packet(play::CLIENTBOUND_CHUNK_BATCH_FINISHED_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())?;

    // `player_loaded` can arrive interleaved with, or even before, the
    // chunk_batch_received ack: the play loop processes each `map_chunk`
    // packet as it streams in (not only at chunk_batch_finished), and its
    // own tick interval — independent of packet arrival order — may notice
    // the player's chunk is already loaded and send `player_loaded` right
    // away. Wait for both, in whichever order they show up, tolerating
    // vanilla movement re-sends in between (same contract as
    // `tests/common`'s `JoinIdle` mode).
    let mut saw_batch_ack = false;
    let mut saw_player_loaded = false;
    while !(saw_batch_ack && saw_player_loaded) {
        let packet = timeout(Duration::from_secs(15), conn.read_packet())
            .await
            .map_err(|_| "no chunk_batch_received/player_loaded within 15s".to_string())?
            .map_err(|e| e.to_string())?;
        match packet.id {
            play::SERVERBOUND_CHUNK_BATCH_RECEIVED_ID => saw_batch_ack = true,
            play::SERVERBOUND_PLAYER_LOADED_ID => saw_player_loaded = true,
            play::SERVERBOUND_POSITION_ID
            | play::SERVERBOUND_POSITION_LOOK_ID
            | play::SERVERBOUND_LOOK_ID
            | play::SERVERBOUND_FLYING_ID => {}
            other => return Err(format!("unexpected post-chunk-batch packet 0x{other:02x}")),
        }
    }
    Ok(())
}

async fn send_one_chunk(
    conn: &mut Connection,
    x: i32,
    z: i32,
    personalized: bool,
    client_index: usize,
) -> Result<(), String> {
    let mut chunk_data = PacketWriter::new();
    for section in 0..24 {
        if section == 7 {
            let filler: u64 = if personalized {
                0x1111_1111_1111_1111u64 ^ (client_index as u64)
            } else {
                0x1111_1111_1111_1111
            };
            chunk_data.put_i16(256);
            chunk_data.put_u8(4);
            chunk_data.put_varint(2);
            chunk_data.put_varint(0);
            chunk_data.put_varint(1);
            chunk_data.put_varint(256);
            for packed in 0..256 {
                chunk_data.put_u64(if packed >= 240 { filler } else { 0 });
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
    w.put_i32(x);
    w.put_i32(z);
    Nbt::Compound(vec![])
        .encode(&mut w)
        .map_err(|e| e.to_string())?;
    w.put_byte_array(&chunk_data);
    w.put_varint(0);
    w.put_varint(0);
    w.put_varint(0);
    w.put_varint(0);
    w.put_varint(0);
    w.put_varint(0);
    w.put_varint(0);
    conn.send_packet(play::CLIENTBOUND_MAP_CHUNK_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())
}

/// Populates entities, HUD/vitals and a non-trivial inventory, the state
/// this benchmark's predecessor (`shared_world_benchmark`) never touches.
async fn send_entity_hud_inventory(
    conn: &mut Connection,
    client_index: usize,
) -> Result<(), String> {
    for i in 0..8u32 {
        let packet = play::PacketSpawnEntity {
            entity_id: 1000 + client_index as i32 * 8 + i as i32,
            object_uuid: u128::from(client_index as u64) << 64 | u128::from(i),
            r#type: 50, // zombie
            x: f64::from(i),
            y: 64.0,
            z: f64::from(i),
            pitch: 0,
            yaw: 0,
            head_pitch: 0,
            object_data: 0,
            velocity: Vec3i16 { x: 0, y: 0, z: 0 },
        };
        let mut w = PacketWriter::new();
        packet.encode(&mut w).map_err(|e| e.to_string())?;
        conn.send_packet(play::CLIENTBOUND_SPAWN_ENTITY_ID, &w.into_inner())
            .await
            .map_err(|e| e.to_string())?;
    }

    let abilities = play::PacketAbilities {
        flags: 0,
        flying_speed: 0.05,
        walking_speed: 0.1,
    };
    let mut w = PacketWriter::new();
    abilities.encode(&mut w).map_err(|e| e.to_string())?;
    conn.send_packet(play::CLIENTBOUND_ABILITIES_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())?;

    let experience = play::PacketExperience {
        experience_bar: 0.5,
        level: 10,
        total_experience: 250,
    };
    let mut w = PacketWriter::new();
    experience.encode(&mut w).map_err(|e| e.to_string())?;
    conn.send_packet(play::CLIENTBOUND_EXPERIENCE_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())?;

    let health = play::PacketUpdateHealth {
        health: 20.0,
        food: 20,
        food_saturation: 5.0,
    };
    let mut w = PacketWriter::new();
    health.encode(&mut w).map_err(|e| e.to_string())?;
    conn.send_packet(play::CLIENTBOUND_UPDATE_HEALTH_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())?;

    let held = play::PacketHeldItemSlot { slot: 0 };
    let mut w = PacketWriter::new();
    held.encode(&mut w).map_err(|e| e.to_string())?;
    conn.send_packet(play::CLIENTBOUND_HELD_ITEM_SLOT_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())?;

    let items: Vec<Slot> = (0..46)
        .map(|slot| {
            if slot % 5 == 0 {
                Slot {
                    item_count: 1,
                    value: SlotValue::Default(SlotValueDefault {
                        item_id: 1,
                        added_component_count: 0,
                        removed_component_count: 0,
                        components: vec![],
                        remove_components: vec![],
                    }),
                }
            } else {
                Slot {
                    item_count: 0,
                    value: SlotValue::V0,
                }
            }
        })
        .collect();
    let window = play::PacketWindowItems {
        window_id: 0,
        state_id: 1,
        items,
        carried_item: Slot {
            item_count: 0,
            value: SlotValue::V0,
        },
    };
    let mut w = PacketWriter::new();
    window.encode(&mut w).map_err(|e| e.to_string())?;
    conn.send_packet(play::CLIENTBOUND_WINDOW_ITEMS_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())
}

const CHURN_ROUNDS: usize = 20;

/// Repeatedly resends health and re-sends one chunk (a section update in
/// all but name — `map_chunk` is a full resend, but it exercises the
/// realistic path: the client decodes and re-interns/replaces its stored
/// chunk snapshot on every round, which is what update churn is meant to
/// stress) to model steady play-state traffic.
async fn send_churn(
    conn: &mut Connection,
    scenario: ChunkScenario,
    client_index: usize,
) -> Result<(), String> {
    for round in 0..CHURN_ROUNDS {
        let health = play::PacketUpdateHealth {
            health: 20.0 - (round % 5) as f32,
            food: 20,
            food_saturation: 5.0,
        };
        let mut w = PacketWriter::new();
        health.encode(&mut w).map_err(|e| e.to_string())?;
        conn.send_packet(play::CLIENTBOUND_UPDATE_HEALTH_ID, &w.into_inner())
            .await
            .map_err(|e| e.to_string())?;

        send_one_chunk(
            conn,
            0,
            0,
            scenario.personalize(client_index, 0),
            client_index + round,
        )
        .await?;
    }
    Ok(())
}

async fn send_unload(conn: &mut Connection, scenario: ChunkScenario) -> Result<(), String> {
    let count = scenario.chunk_count();
    let side = (count as f64).sqrt().ceil() as i32;
    for index in 0..count {
        let x = index as i32 % side - side / 2;
        let z = index as i32 / side - side / 2;
        let packet = play::PacketUnloadChunk {
            chunk_z: z,
            chunk_x: x,
        };
        let mut w = PacketWriter::new();
        packet.encode(&mut w).map_err(|e| e.to_string())?;
        conn.send_packet(play::CLIENTBOUND_UNLOAD_CHUNK_ID, &w.into_inner())
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Process RSS reading.
// ---------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn process_rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let rest = line.strip_prefix("VmRSS")?.trim_start().strip_prefix(':')?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

#[cfg(windows)]
fn process_rss_kib() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb);
        if ok != 0 {
            Some((counters.WorkingSetSize as u64) / 1024)
        } else {
            None
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn process_rss_kib() -> Option<u64> {
    None
}
