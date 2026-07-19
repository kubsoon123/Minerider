//! The full-runtime integration benchmark: real `ClientSupervisor`s, real
//! loopback TCP against an in-process mock Minecraft server, real fake
//! SOCKS5 proxies where proxy routing is tested — as opposed to
//! `synthetic.rs`, which measures the Lua dispatch layer in isolation with
//! no network at all. Never talks to a public server or a live proxy.
//!
//! Mock server handshake/login/configuration/chunk-sending is adapted from
//! `src/bin/full_runtime_benchmark.rs`'s already-proven implementation
//! (same repo convention as `tests/actions.rs` vs. `tests/supervisor.rs`:
//! small mock-infra is duplicated per consumer rather than shared).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::generated::v1_21_4::types::Vec3i16;
use minerider_protocol::generated::v1_21_4::{configuration, handshaking, login, play};
use minerider_protocol::nbt::Nbt;
use minerider_protocol::traits::Encode;

use crate::core::client::ClientConfig;
use crate::core::supervisor::{ClientSupervisor, ReconnectPolicy, RetryLimit, SupervisorHandle};
use crate::minecraft::event::BotEvent;
use crate::minecraft::inventory::GuiClick;
use crate::network::connection::Connection;
use crate::network::socks5::Socks5ProxyConfig;

use super::command::{BenchCommand, CommandSink};
use super::dispatch::{Architecture, Dispatcher, QueueKind};
use super::event::{BenchEvent, BenchGuiSlot, BotId, Envelope};
use super::metrics::{process_rss_kib, DispatcherMetrics};
use super::sandbox::SandboxConfig;

/// What the mock server does with each connected bot, beyond the shared
/// login/configuration/play-login/settle sequence every scenario needs.
#[derive(Debug, Clone, Copy)]
pub enum ScenarioKind {
    /// Connect, settle, stay idle.
    Idle,
    /// Connect, settle, then send `chunk_count` chunks — identical content
    /// across every bot when `personalize` is `false`.
    Chunks {
        chunk_count: usize,
        personalize: bool,
    },
    /// Connect, settle, populate entities/HUD/inventory, open one GUI
    /// window with a realistic slot array, send a few chat messages.
    RealisticState,
    /// Connect, settle, then kick a deterministic fraction of bots
    /// (`bot_id % 100 < percent`) once, letting their (enabled)
    /// `ReconnectPolicy` bring them back.
    ReconnectStorm { percent: u32 },
}

pub struct SwarmConfig {
    pub bot_count: u32,
    pub scenario: ScenarioKind,
    pub share_chunk_payloads: bool,
    pub architecture: Architecture,
    pub script_body: &'static str,
    pub sandbox_config: SandboxConfig,
    /// `proxy_for(bot_id)` returns the proxy config (if any) that bot should
    /// use — lets proxy-group scenarios assign deterministic ratios.
    pub proxy_for: Arc<dyn Fn(u32) -> Option<Arc<Socks5ProxyConfig>> + Send + Sync>,
    pub settle_timeout: Duration,
    pub run_duration: Duration,
    pub cleanup_wait: Duration,
}

pub struct FullRuntimeResult {
    pub bot_count: u32,
    pub bots_connected: usize,
    pub rss_baseline: Option<u64>,
    pub rss_after_connect: Option<u64>,
    pub rss_after_scenario: Option<u64>,
    pub rss_after_shutdown: Option<u64>,
    pub rss_after_cleanup_wait: Option<u64>,
    pub events_dispatched: u64,
    pub handler_executions: u64,
    pub handler_errors: u64,
    pub commands_emitted: u64,
    pub commands_dropped: u64,
    pub commands_routed_to_supervisor: u64,
    pub reconnects_observed: u64,
    pub enqueue_to_start: super::metrics::LatencySummary,
    pub enqueue_to_complete: super::metrics::LatencySummary,
    pub command_latency: super::metrics::LatencySummary,
    pub queue_peak_depth_total: usize,
    pub queue_dropped_total: u64,
    pub wall_time: Duration,
}

/// Starts the mock server, spawns `config.bot_count` real `ClientSupervisor`
/// instances, bridges their events into a real [`Dispatcher`], routes
/// selected Lua-emitted commands back through the real `SupervisorHandle`,
/// runs for `config.run_duration`, then tears everything down.
pub async fn run_full_runtime_scenario(config: SwarmConfig) -> FullRuntimeResult {
    let wall_start = Instant::now();
    let rss_baseline = process_rss_kib();

    let (server_port, server_task, kick_tx) =
        spawn_mock_server(config.scenario, config.bot_count).await;

    let (sink, command_rx) = CommandSink::bounded(4096);
    let sink_stats_handle = sink.clone();
    let metrics = Arc::new(DispatcherMetrics::new(1 << 16));
    let dispatcher = Arc::new(Dispatcher::start(
        config.architecture,
        QueueKind::Fifo { capacity: 4096 },
        config.sandbox_config,
        config.script_body,
        sink,
        metrics.clone(),
    ));

    let mut handles: HashMap<u32, SupervisorHandle> =
        HashMap::with_capacity(config.bot_count as usize);
    let mut supervisor_tasks = Vec::with_capacity(config.bot_count as usize);
    for bot in 0..config.bot_count {
        let mut cfg = ClientConfig::new("127.0.0.1", server_port, format!("Bot{bot}"))
            .with_chunk_sharing(config.share_chunk_payloads)
            .with_connect_deadline(Duration::from_secs(20));
        if let Some(proxy) = (config.proxy_for)(bot) {
            cfg = cfg.with_socks5_proxy(proxy);
        }
        let policy = ReconnectPolicy {
            enabled: true,
            max_retries: RetryLimit::Count(3),
            initial_delay: Duration::from_millis(50),
            max_delay: Duration::from_millis(200),
            ..ReconnectPolicy::default()
        };
        let (supervisor, handle) = ClientSupervisor::new(cfg, policy);
        supervisor_tasks.push(tokio::spawn(supervisor.run()));
        handles.insert(bot, handle);
    }

    // One bridge task per bot: tracks connect/reconnect and forwards every
    // relevant BotEvent into the shared Dispatcher.
    let connected_count = Arc::new(AtomicUsize::new(0));
    let reconnects_observed = Arc::new(AtomicUsize::new(0));
    let mut bridge_tasks: Vec<JoinHandle<()>> = Vec::with_capacity(handles.len());
    for (&bot, handle) in &handles {
        bridge_tasks.push(spawn_event_bridge(
            BotId(bot),
            handle.clone(),
            dispatcher.clone(),
            connected_count.clone(),
            reconnects_observed.clone(),
        ));
    }

    // Command router: drains Lua-emitted commands and routes them through
    // the real SupervisorHandle for the bot they targeted.
    let commands_routed = Arc::new(AtomicUsize::new(0));
    let router_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let router_handles = handles.clone();
    let router_commands_routed = commands_routed.clone();
    let router_stop_flag = router_stop.clone();
    let command_router = tokio::spawn(async move {
        route_commands(
            command_rx,
            router_handles,
            router_commands_routed,
            router_stop_flag,
        )
        .await;
    });

    // Settle: wait until every bot reports Connected (or the timeout).
    let _ = timeout(config.settle_timeout, async {
        while connected_count.load(Ordering::Relaxed) < config.bot_count as usize {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let rss_after_connect = process_rss_kib();

    if let ScenarioKind::ReconnectStorm { .. } = config.scenario {
        // Every bot has connected once and reported Idle; release the kick
        // so the server disconnects the deterministic subset.
        let _ = kick_tx.send(true);
    }

    tokio::time::sleep(config.run_duration).await;
    let rss_after_scenario = process_rss_kib();

    // Stop the bridges first so no new events reach Lua, then shut the
    // dispatcher down: every worker drains whatever it was already holding
    // and finishes any in-flight handler call (and thus any `bot.command`
    // it emits) before its thread returns, and shutdown only returns once
    // every worker (and its `CommandSink` clone) is gone.
    for task in bridge_tasks {
        task.abort();
        let _ = task.await;
    }
    let dispatcher = Arc::try_unwrap(dispatcher)
        .unwrap_or_else(|_| panic!("dispatcher still shared after every bridge task joined"));
    let queue_peak_depth_total = dispatcher.queue_peak_depth_total();
    let queue_dropped_total = dispatcher.queue_dropped_total();
    let _worker_reports = dispatcher.shutdown();

    // Every worker (and any `bot.command` call it was mid-executing) has
    // now finished, so every command it will ever emit is already sitting
    // in `command_rx`. Tell the router to drain and stop instead of
    // relying on the channel disconnecting on its own — a Lua closure's
    // captured `CommandSink` clone drops on Lua's GC schedule, not
    // synchronously with `Lua`'s own drop, so waiting on disconnection
    // alone is not bounded.
    router_stop.store(true, Ordering::Relaxed);
    let _ = timeout(Duration::from_secs(5), command_router).await;

    for handle in handles.values() {
        handle.stop();
    }
    for task in supervisor_tasks {
        let _ = timeout(Duration::from_secs(10), task).await;
    }
    server_task.abort();
    let rss_after_shutdown = process_rss_kib();

    tokio::time::sleep(config.cleanup_wait).await;
    let rss_after_cleanup_wait = process_rss_kib();

    let throughput = metrics.throughput.snapshot();
    let sink_stats = sink_stats_handle.stats();
    let enqueue_to_start = metrics.enqueue_to_start.lock().unwrap().summary();
    let enqueue_to_complete = metrics.enqueue_to_complete.lock().unwrap().summary();
    let command_latency = metrics.command_latency.lock().unwrap().summary();

    FullRuntimeResult {
        bot_count: config.bot_count,
        bots_connected: connected_count.load(Ordering::Relaxed),
        rss_baseline,
        rss_after_connect,
        rss_after_scenario,
        rss_after_shutdown,
        rss_after_cleanup_wait,
        events_dispatched: throughput.events_dispatched,
        handler_executions: throughput.handler_executions,
        handler_errors: throughput.handler_errors,
        commands_emitted: sink_stats.emitted,
        commands_dropped: sink_stats.dropped,
        commands_routed_to_supervisor: commands_routed.load(Ordering::Relaxed) as u64,
        reconnects_observed: reconnects_observed.load(Ordering::Relaxed) as u64,
        enqueue_to_start,
        enqueue_to_complete,
        command_latency,
        queue_peak_depth_total,
        queue_dropped_total,
        wall_time: wall_start.elapsed(),
    }
}

fn spawn_event_bridge(
    bot_id: BotId,
    handle: SupervisorHandle,
    dispatcher: Arc<Dispatcher>,
    connected_count: Arc<AtomicUsize>,
    reconnects_observed: Arc<AtomicUsize>,
) -> JoinHandle<()> {
    let mut events = handle.events();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if matches!(event, BotEvent::Connected) {
                        connected_count.fetch_add(1, Ordering::Relaxed);
                    }
                    if matches!(event, BotEvent::ReconnectScheduled { .. }) {
                        reconnects_observed.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Some(bench_event) = bot_event_to_bench_event(&event, &handle).await {
                        dispatcher.dispatch(Envelope {
                            bot_id,
                            event: bench_event,
                            enqueued_at: Instant::now(),
                            bot_seq: 0,
                        });
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

async fn bot_event_to_bench_event(
    event: &BotEvent,
    handle: &SupervisorHandle,
) -> Option<BenchEvent> {
    match event {
        BotEvent::Connected => Some(BenchEvent::Connected),
        BotEvent::Disconnected { reason } => Some(BenchEvent::Disconnected {
            reason_len: reason.len().min(u16::MAX as usize) as u16,
        }),
        BotEvent::ReconnectScheduled { attempt, delay } => Some(BenchEvent::ReconnectScheduled {
            attempt: *attempt,
            delay_ms: delay.as_millis().min(u32::MAX as u128) as u32,
        }),
        BotEvent::Health { health, food, .. } => Some(BenchEvent::Health {
            health: *health,
            food: *food,
        }),
        BotEvent::Chat { sender, message } => Some(BenchEvent::Chat {
            sender_len: sender.len().min(u8::MAX as usize) as u8,
            message: message.clone(),
        }),
        BotEvent::SystemChat { message } => Some(BenchEvent::Chat {
            sender_len: 0,
            message: message.clone(),
        }),
        BotEvent::PlayerJoined { name, .. } => Some(BenchEvent::PlayerJoined {
            name_len: name.len().min(u8::MAX as usize) as u8,
        }),
        BotEvent::Inventory(inv_event) => match inv_event.as_ref() {
            // Fires once `window_items` has been applied to local state —
            // unlike `WindowOpened` (fires on `open_window`, before any
            // slot contents have arrived), so `handle.open_gui()` here
            // always sees the real slot array, not an empty placeholder.
            crate::minecraft::inventory::InventoryEvent::WindowSynchronized {
                window_id, ..
            } => {
                let slots: Vec<BenchGuiSlot> = handle
                    .open_gui()
                    .map(|view| {
                        view.slots
                            .iter()
                            .take(super::event::GUI_OPENED_MAX_SLOTS)
                            .map(|slot| BenchGuiSlot {
                                raw_slot: slot.index.min(u16::MAX as usize) as u16,
                                item_id: slot.item_id.unwrap_or(0),
                                count: slot.count,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(BenchEvent::GuiOpened {
                    window_id: *window_id,
                    slots,
                })
            }
            crate::minecraft::inventory::InventoryEvent::SlotUpdated {
                window_id: _, slot, ..
            } => Some(BenchEvent::InventorySlotUpdate {
                slot: (*slot).max(0) as u16,
                item_id: 0,
                count: 0,
            }),
            _ => None,
        },
        _ => None,
    }
}

/// Polls `command_rx` with `try_recv` instead of blocking on `recv`: this
/// crate does not enable `mlua`'s `send` feature, so nothing here can
/// assume a registered Lua closure's captured `CommandSink` clone drops
/// (and thus disconnects the channel) as soon as `Lua` itself does — that
/// depends on Lua's own GC/finalizer timing, not a guarantee. `stop` is set
/// once every worker has been joined; polling continues until both `stop`
/// is set *and* the channel is empty, so nothing already in flight is lost.
async fn route_commands(
    command_rx: std::sync::mpsc::Receiver<super::command::CommandRecord>,
    handles: HashMap<u32, SupervisorHandle>,
    routed: Arc<AtomicUsize>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    loop {
        match command_rx.try_recv() {
            Ok(record) => {
                if let Some(handle) = handles.get(&record.bot_id.0) {
                    let ok = match record.command {
                        BenchCommand::Forward(on) => handle.forward(on).await.is_ok(),
                        BenchCommand::Chat(message) => handle.chat(message).await.is_ok(),
                        // The `realistic` script's `event.slots[1].raw_slot`
                        // came from the currently open GUI window (see
                        // `WindowSynchronized` above), so the click must
                        // target that same open window, not the
                        // always-window-0 `click_inventory_slot`.
                        BenchCommand::ClickSlot {
                            raw_slot,
                            right_click,
                        } => handle
                            .click_open_gui_slot(
                                raw_slot as usize,
                                if right_click {
                                    GuiClick::Right
                                } else {
                                    GuiClick::Left
                                },
                            )
                            .await
                            .is_ok(),
                    };
                    if ok {
                        routed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
        }
    }
}

// ---------------------------------------------------------------------
// Mock server
// ---------------------------------------------------------------------

async fn spawn_mock_server(
    scenario: ScenarioKind,
    bot_count: u32,
) -> (u16, JoinHandle<()>, watch::Sender<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("local addr").port();
    let (kick_tx, kick_rx) = watch::channel(false);
    // One flag per bot so each targeted bot is kicked exactly once, even
    // though `ReconnectStorm`'s condition (`bot_index % 100 < percent`) is
    // otherwise still true on the bot's own reconnect — without this, a
    // targeted bot would be kicked again on every connection attempt up to
    // `max_retries`, turning one deliberate blip into a cascade.
    let already_kicked: Arc<Vec<std::sync::atomic::AtomicBool>> = Arc::new(
        (0..bot_count)
            .map(|_| std::sync::atomic::AtomicBool::new(false))
            .collect(),
    );

    let task = tokio::spawn(async move {
        // Accept until this task is aborted (`run_full_runtime_scenario`
        // aborts it at teardown; in `production_smoke` tests the tokio
        // test runtime drops it) — never a fixed count. A supervisor's
        // retry after ANY transient failure (a serve_one read error, a
        // connect-deadline timeout on a contended 2-core CI runner)
        // arrives as a NEW connection on this listener; an accept loop
        // that stops at the theoretical minimum leaves that retry's SYN
        // sitting in the backlog forever, permanently stranding the bot
        // no matter how long the caller's settle timeout is. That exact
        // failure — `bots_connected` one short with a 90s settle, a
        // different test each time, only on slow shared runners — is
        // what every CI-only flake in this suite actually was (Actions
        // runs 29661321026 on Windows and 29684306799 on Ubuntu are the
        // runs that finally isolated it to this loop, by failing in the
        // process that *excludes* production_smoke). `already_kicked`
        // still guarantees each ReconnectStorm-targeted bot is kicked
        // exactly once regardless of how many times it connects.
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let kick_rx = kick_rx.clone();
            let already_kicked = already_kicked.clone();
            tokio::spawn(async move {
                let _ = serve_one(stream, scenario, kick_rx, already_kicked).await;
            });
        }
    });

    (port, task, kick_tx)
}

async fn serve_one(
    stream: TcpStream,
    scenario: ScenarioKind,
    mut kick_rx: watch::Receiver<bool>,
    already_kicked: Arc<Vec<std::sync::atomic::AtomicBool>>,
) -> Result<(), String> {
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    let mut conn = Connection::from_tcp_stream(stream).map_err(|e| e.to_string())?;

    let hs = conn.read_packet().await.map_err(|e| e.to_string())?;
    if hs.id != handshaking::SERVERBOUND_SET_PROTOCOL_ID {
        return Err(format!("expected handshake, got 0x{:02x}", hs.id));
    }

    let ls = conn.read_packet().await.map_err(|e| e.to_string())?;
    if ls.id != login::SERVERBOUND_LOGIN_START_ID {
        return Err(format!("expected login start, got 0x{:02x}", ls.id));
    }
    let username = {
        let mut r = PacketReader::new(&ls.payload);
        r.read_string().map_err(|e| e.to_string())?
    };
    let bot_index: u32 = username.trim_start_matches("Bot").parse().unwrap_or(0);

    let mut w = PacketWriter::new();
    w.put_uuid(u128::from(bot_index) + 1);
    w.put_string(username).map_err(|e| e.to_string())?;
    w.put_varint(0);
    conn.send_packet(login::CLIENTBOUND_SUCCESS_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())?;

    let ack = conn.read_packet().await.map_err(|e| e.to_string())?;
    if ack.id != login::SERVERBOUND_LOGIN_ACKNOWLEDGED_ID {
        return Err(format!("expected login acknowledged, got 0x{:02x}", ack.id));
    }

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

    send_play_login(&mut conn, bot_index).await?;
    send_position(&mut conn).await?;
    wait_for_ready(&mut conn).await?;

    match scenario {
        ScenarioKind::Idle => {}
        ScenarioKind::Chunks {
            chunk_count,
            personalize,
        } => {
            send_chunks(
                &mut conn,
                chunk_count,
                if personalize { bot_index } else { 0 },
            )
            .await?;
        }
        ScenarioKind::RealisticState => {
            send_chunks(&mut conn, 9, 0).await?;
            send_entity_hud_inventory(&mut conn, bot_index).await?;
            send_gui_open(&mut conn).await?;
            send_chat(&mut conn, "welcome").await?;
        }
        ScenarioKind::ReconnectStorm { percent } => {
            let targeted = (bot_index % 100) < percent;
            let is_first_connection = targeted
                && already_kicked
                    .get(bot_index as usize)
                    .map(|flag| {
                        flag.compare_exchange(
                            false,
                            true,
                            std::sync::atomic::Ordering::SeqCst,
                            std::sync::atomic::Ordering::SeqCst,
                        )
                        .is_ok()
                    })
                    .unwrap_or(false);
            if is_first_connection {
                // First connection for this bot: wait for the kick signal,
                // then drop the raw connection (no `kick_disconnect`
                // packet) — a network blip, not a server-issued ban. The
                // client classifies this as a transient error, which (by
                // design, unlike an explicit kick) `ReconnectPolicy`
                // retries by default; the bot's *second* connection (its
                // own reconnect) finds `is_first_connection` false (the
                // flag is already set) and falls through to idle below —
                // exactly one kick per targeted bot, not one per attempt.
                if !*kick_rx.borrow() {
                    let _ = kick_rx.changed().await;
                }
                return conn.close().await.map_err(|e| e.to_string());
            }
        }
    }

    // Idle until the caller tears the connection down (aborting this task).
    loop {
        match conn.read_packet().await {
            Ok(_) => continue,
            Err(_) => return Ok(()),
        }
    }
}

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

async fn send_play_login(conn: &mut Connection, bot_index: u32) -> Result<(), String> {
    let packet = play::PacketLogin {
        entity_id: 1 + bot_index as i32,
        is_hardcore: false,
        world_names: vec!["minecraft:overworld".to_string()],
        max_players: 100,
        view_distance: 6,
        simulation_distance: 6,
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

async fn send_position(conn: &mut Connection) -> Result<(), String> {
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
    packet.encode(&mut w).map_err(|e| e.to_string())?;
    conn.send_packet(play::CLIENTBOUND_POSITION_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())
}

async fn send_chunks(conn: &mut Connection, count: usize, salt: u32) -> Result<(), String> {
    let side = (count as f64).sqrt().ceil() as i32;
    conn.send_packet(play::CLIENTBOUND_CHUNK_BATCH_START_ID, &[])
        .await
        .map_err(|e| e.to_string())?;
    for index in 0..count {
        let x = index as i32 % side - side / 2;
        let z = index as i32 / side - side / 2;
        send_one_chunk(conn, x, z, salt).await?;
    }
    let mut w = PacketWriter::new();
    w.put_varint(count as i32);
    conn.send_packet(play::CLIENTBOUND_CHUNK_BATCH_FINISHED_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())?;

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

async fn send_one_chunk(conn: &mut Connection, x: i32, z: i32, salt: u32) -> Result<(), String> {
    let mut chunk_data = PacketWriter::new();
    for section in 0..24 {
        if section == 7 {
            let filler: u64 = 0x1111_1111_1111_1111u64 ^ (salt as u64);
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

async fn send_entity_hud_inventory(conn: &mut Connection, bot_index: u32) -> Result<(), String> {
    let packet = play::PacketSpawnEntity {
        entity_id: 1000 + bot_index as i32,
        object_uuid: u128::from(bot_index),
        r#type: 50,
        x: 1.0,
        y: 64.0,
        z: 1.0,
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

    let health = play::PacketUpdateHealth {
        health: 20.0,
        food: 20,
        food_saturation: 5.0,
    };
    let mut w = PacketWriter::new();
    health.encode(&mut w).map_err(|e| e.to_string())?;
    conn.send_packet(play::CLIENTBOUND_UPDATE_HEALTH_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())
}

async fn send_gui_open(conn: &mut Connection) -> Result<(), String> {
    use minerider_protocol::generated::v1_21_4::types::{Slot, SlotValue, SlotValueDefault};
    let open = play::PacketOpenWindow {
        window_id: 1,
        inventory_type: 0,
        window_title: Nbt::Compound(vec![("text".to_string(), Nbt::String("Chest".to_string()))]),
    };
    let mut w = PacketWriter::new();
    open.encode(&mut w).map_err(|e| e.to_string())?;
    conn.send_packet(play::CLIENTBOUND_OPEN_WINDOW_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())?;

    let items: Vec<Slot> = (0..27)
        .map(|slot| {
            if slot == 0 {
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
        window_id: 1,
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

async fn send_chat(conn: &mut Connection, message: &str) -> Result<(), String> {
    let packet = play::PacketSystemChat {
        content: Nbt::Compound(vec![("text".to_string(), Nbt::String(message.to_string()))]),
        is_action_bar: false,
    };
    let mut w = PacketWriter::new();
    packet.encode(&mut w).map_err(|e| e.to_string())?;
    conn.send_packet(play::CLIENTBOUND_SYSTEM_CHAT_ID, &w.into_inner())
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua_benchmark::scripts::ScriptKind;

    // `settle_timeout: Duration::from_secs(90)` below: widened from 45s
    // after a third CI-only flake. The attribution that widening rested on
    // ("real environment slowness, not a logic bug") turned out to be
    // wrong: every one of this suite's CI-only flakes — including the
    // 2/4-bots-connected one the widening reacted to — was the mock
    // server's accept loop stopping at exactly `bot_count` connections,
    // so a bot whose first attempt hit any transient failure could never
    // be accepted again no matter the timeout (see the comment inside
    // `spawn_mock_server`, and Actions runs 29661321026/29684306799 for
    // the failures that isolated it). The wide timeout stays anyway: it
    // now only has to cover genuinely slow runners, which is what it's
    // for — it is no longer masking a stranded bot.
    fn no_proxy() -> Arc<dyn Fn(u32) -> Option<Arc<Socks5ProxyConfig>> + Send + Sync> {
        Arc::new(|_bot| None)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn idle_scenario_connects_every_bot_and_reports_rss() {
        let config = SwarmConfig {
            bot_count: 3,
            scenario: ScenarioKind::Idle,
            share_chunk_payloads: true,
            architecture: Architecture::RustBaseline,
            script_body: ScriptKind::NoOp.source(),
            sandbox_config: SandboxConfig::default(),
            proxy_for: no_proxy(),
            settle_timeout: Duration::from_secs(90),
            run_duration: Duration::from_millis(100),
            cleanup_wait: Duration::from_millis(20),
        };
        let result = run_full_runtime_scenario(config).await;
        assert_eq!(result.bots_connected, 3, "every bot must reach Connected");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chunks_scenario_delivers_events_through_a_shared_lua_vm() {
        let config = SwarmConfig {
            bot_count: 2,
            scenario: ScenarioKind::Chunks {
                chunk_count: 9,
                personalize: false,
            },
            share_chunk_payloads: true,
            architecture: Architecture::Lua { worker_count: 1 },
            script_body: ScriptKind::LightState.source(),
            sandbox_config: SandboxConfig::default(),
            proxy_for: no_proxy(),
            settle_timeout: Duration::from_secs(90),
            run_duration: Duration::from_millis(500),
            cleanup_wait: Duration::from_millis(20),
        };
        let result = run_full_runtime_scenario(config).await;
        assert_eq!(result.bots_connected, 2);
        assert!(
            result.events_dispatched > 0,
            "at least the Connected event must reach the dispatcher"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn realistic_state_scenario_routes_a_lua_emitted_command_to_the_real_supervisor() {
        let config = SwarmConfig {
            bot_count: 1,
            scenario: ScenarioKind::RealisticState,
            share_chunk_payloads: true,
            architecture: Architecture::Lua { worker_count: 1 },
            script_body: ScriptKind::Realistic.source(),
            sandbox_config: SandboxConfig::default(),
            proxy_for: no_proxy(),
            settle_timeout: Duration::from_secs(90),
            run_duration: Duration::from_millis(500),
            cleanup_wait: Duration::from_millis(20),
        };
        let result = run_full_runtime_scenario(config).await;
        assert_eq!(result.bots_connected, 1);
        // The mock server's `welcome` chat message doesn't say "move", so
        // the realistic script emits nothing from it; the GUI-opened event
        // does trigger a `click_slot` command since slot 0 is non-empty.
        assert!(
            result.commands_routed_to_supervisor > 0,
            "the GUI-opened handler must have routed a click_slot command"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reconnect_storm_scenario_reconnects_the_targeted_bots() {
        let config = SwarmConfig {
            bot_count: 4,
            scenario: ScenarioKind::ReconnectStorm { percent: 50 },
            share_chunk_payloads: true,
            architecture: Architecture::RustBaseline,
            script_body: ScriptKind::NoOp.source(),
            sandbox_config: SandboxConfig::default(),
            proxy_for: no_proxy(),
            settle_timeout: Duration::from_secs(90),
            run_duration: Duration::from_millis(500),
            cleanup_wait: Duration::from_millis(20),
        };
        let result = run_full_runtime_scenario(config).await;
        assert!(
            result.reconnects_observed >= 2,
            "half of 4 bots must have a reconnect scheduled, got {}",
            result.reconnects_observed
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_groups_scenario_routes_every_bot_through_its_assigned_proxy() {
        let proxy_a = crate::lua_benchmark::fake_socks5::FakeSocks5Server::start(8).await;
        let proxy_a_port = proxy_a.port;
        let proxy_a_cfg = Arc::new(Socks5ProxyConfig::new("127.0.0.1", proxy_a_port));

        let assignment = proxy_a_cfg.clone();
        let proxy_for: Arc<dyn Fn(u32) -> Option<Arc<Socks5ProxyConfig>> + Send + Sync> =
            Arc::new(move |bot| {
                if bot % 2 == 0 {
                    Some(assignment.clone())
                } else {
                    None
                }
            });

        let config = SwarmConfig {
            bot_count: 4,
            scenario: ScenarioKind::Idle,
            share_chunk_payloads: true,
            architecture: Architecture::RustBaseline,
            script_body: ScriptKind::NoOp.source(),
            sandbox_config: SandboxConfig::default(),
            proxy_for,
            settle_timeout: Duration::from_secs(90),
            run_duration: Duration::from_millis(200),
            cleanup_wait: Duration::from_millis(20),
        };
        let result = run_full_runtime_scenario(config).await;
        assert_eq!(result.bots_connected, 4);

        // `>=` rather than `==`: every bot ends Connected (asserted above)
        // and an even bot's *only* path to the server is proxy_a (its
        // supervisor reuses its config's proxy on every attempt), so ≥2
        // CONNECTs proves both assigned bots routed through it. A rare
        // transient failure legitimately adds a retry CONNECT, so an exact
        // count here would flake precisely when the client behaves
        // correctly; odd bots have no proxy configured at all, so no
        // unassigned bot can ever contribute to this count.
        let targets = proxy_a.requested_targets().await;
        assert!(
            targets.len() >= 2,
            "both bots assigned to proxy_a must have connected through it, saw {} CONNECTs",
            targets.len()
        );
    }

    /// `ClientSupervisor::new` documents that `cfg` (including its `proxy`)
    /// "is reused unchanged for every (re)connect attempt" — this proves it
    /// end to end: a bot assigned to a proxy, disconnected by a network
    /// blip, and reconnected must route its *second* connection through
    /// that exact same proxy too, not fall back to direct or a different
    /// one. Every accepted connection on the fake proxy's port is counted,
    /// so a bot that reconnects through it shows up twice.
    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_assignment_persists_across_a_reconnect() {
        let proxy = crate::lua_benchmark::fake_socks5::FakeSocks5Server::start(16).await;
        let proxy_cfg = Arc::new(Socks5ProxyConfig::new("127.0.0.1", proxy.port));
        let proxy_for: Arc<dyn Fn(u32) -> Option<Arc<Socks5ProxyConfig>> + Send + Sync> =
            Arc::new(move |_bot| Some(proxy_cfg.clone()));

        let config = SwarmConfig {
            bot_count: 2,
            // Every bot (100%) gets disconnected once and must reconnect.
            scenario: ScenarioKind::ReconnectStorm { percent: 100 },
            share_chunk_payloads: true,
            architecture: Architecture::RustBaseline,
            script_body: ScriptKind::NoOp.source(),
            sandbox_config: SandboxConfig::default(),
            proxy_for,
            settle_timeout: Duration::from_secs(90),
            run_duration: Duration::from_millis(600),
            cleanup_wait: Duration::from_millis(20),
        };
        let result = run_full_runtime_scenario(config).await;
        assert!(
            result.reconnects_observed >= 2,
            "both bots must have a reconnect scheduled, got {}",
            result.reconnects_observed
        );

        let accepted = proxy
            .accepted_connections
            .load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            accepted >= 4,
            "2 bots x (>= 1 initial connection + >= 1 reconnect) must all route through the \
             same proxy, got {accepted}"
        );
    }
}

/// Production-wrapper smoke tests: these exercise the *real* production
/// runtime (`crate::lua::runtime::run_swarm`, `crate::lua::worker`,
/// `crate::lua::api::*`) end to end against the same local mock Minecraft
/// server and fake SOCKS5 relay the architecture benchmark above uses —
/// never a parallel/duplicated test server. This is the mission's
/// requested "small production-wrapper smoke (4 workers, several bots,
/// local proxy groups, reconnect, shared chunks, GUI click, graceful
/// shutdown)"; the exhaustive per-feature correctness matrix lives in
/// `docs/lua_wrapper.md#testing` and in `src/lua/*`'s own unit tests.
#[cfg(test)]
mod production_smoke {
    use std::sync::Arc;
    use std::time::Duration;

    use super::{spawn_mock_server, ScenarioKind};
    use crate::lua::registry::SwarmRegistryBuilder;
    use crate::lua::runtime::{run_swarm, SwarmRuntimeConfig};
    use crate::lua::sandbox::SandboxConfig;
    use crate::lua::shared_value::SharedValue;
    use crate::network::socks5::Socks5ProxyConfig;

    async fn wait_for<F: Fn() -> bool>(condition: F, timeout: Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        condition()
    }

    fn base_config(worker_count: usize, script_body: String) -> SwarmRuntimeConfig {
        base_config_with_proxies(
            worker_count,
            script_body,
            crate::lua::registry::ProxyProfiles::new(),
        )
    }

    fn base_config_with_proxies(
        worker_count: usize,
        script_body: String,
        proxy_profiles: crate::lua::registry::ProxyProfiles,
    ) -> SwarmRuntimeConfig {
        SwarmRuntimeConfig {
            worker_count,
            sandbox: SandboxConfig::default(),
            high_queue_capacity: 256,
            low_queue_capacity: 64,
            callback_timeout: Duration::from_secs(10),
            script_body,
            proxy_profiles: Arc::new(proxy_profiles),
            startup_timeout: Duration::from_secs(30),
        }
    }

    // ---- Shared infrastructure for the manual, ignored production-wrapper
    // benchmarks below (`production_wrapper_scale_100_400_800`,
    // `production_wrapper_combined_proxy_groups_reconnect_shared_chunks`) --

    /// Every number these benchmarks produce comes from a **local loopback
    /// mock Minecraft server and local fake SOCKS5 relays**
    /// (`spawn_mock_server`/`crate::lua_benchmark::fake_socks5::FakeSocks5Server`
    /// — never an external server or a real proxy). They reflect this
    /// machine's local scheduling/memory/allocator behavior only — never
    /// real-world public-proxy latency/throughput, or real Minecraft
    /// server behavior. Do not extrapolate wall-clock or RSS numbers here
    /// to a deployment using a real, network-distant proxy.
    const LOOPBACK_DISCLAIMER: &str =
        "Local loopback mock server + local fake SOCKS5 relays only — \
        does not represent real-world public proxy or Minecraft server performance.";

    #[derive(Debug, Clone, Default, serde::Serialize)]
    struct StageRss {
        baseline_kib: Option<u64>,
        connected_kib: Option<u64>,
        scenario_kib: Option<u64>,
        shutdown_kib: Option<u64>,
        cleanup_kib: Option<u64>,
    }

    #[derive(Debug, serde::Serialize)]
    struct TimingStatsMs {
        median: f64,
        min: f64,
        max: f64,
        samples: Vec<f64>,
    }

    /// Median and min/max of `samples_ms`, not a single arbitrary
    /// timing — local loopback wall-clock still varies run to run (OS
    /// scheduling jitter, allocator/GC-adjacent behavior in the Lua/tokio
    /// runtimes), and one sample can't distinguish a fluke from a real
    /// regression. `samples_ms` must be non-empty.
    fn compute_timing_stats(mut samples_ms: Vec<f64>) -> TimingStatsMs {
        samples_ms.sort_by(|a, b| a.partial_cmp(b).expect("wall-clock ms is always finite"));
        let min = *samples_ms.first().expect("at least one sample");
        let max = *samples_ms.last().expect("at least one sample");
        let mid = samples_ms.len() / 2;
        let median = if samples_ms.len() % 2 == 0 {
            (samples_ms[mid - 1] + samples_ms[mid]) / 2.0
        } else {
            samples_ms[mid]
        };
        TimingStatsMs {
            median,
            min,
            max,
            samples: samples_ms,
        }
    }

    /// Writes `value` as pretty-printed JSON to
    /// `docs/lua_wrapper_benchmark_results/<name>.json` — raw,
    /// machine-readable output a PR reviewing a benchmark run can diff
    /// against a previous one, rather than only a human-readable log
    /// line. Best-effort: a write failure (e.g. a read-only checkout) is
    /// logged, never a test failure — the benchmark's own measurement is
    /// the point, not the act of persisting it.
    fn write_benchmark_json(name: &str, value: &impl serde::Serialize) {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs")
            .join("lua_wrapper_benchmark_results");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("[benchmark] could not create {}: {e}", dir.display());
            return;
        }
        let path = dir.join(format!("{name}.json"));
        match serde_json::to_string_pretty(value) {
            Ok(json) => match std::fs::write(&path, json) {
                Ok(()) => println!("[benchmark] wrote {}", path.display()),
                Err(e) => eprintln!("[benchmark] could not write {}: {e}", path.display()),
            },
            Err(e) => eprintln!("[benchmark] could not serialize result: {e}"),
        }
    }

    /// Runs one full start → connect → settle → shutdown → cleanup cycle
    /// for `bot_count` bots under `scenario` (`Idle` or a real
    /// `Chunks{..}` scenario — the mock server sends genuine synthetic
    /// chunk packets for the latter, not just a `shared_chunks = true`
    /// server flag with nothing behind it), returning the wall-clock time
    /// to all-connected and process RSS at all five stages.
    async fn run_one_scale_cycle(bot_count: u32, scenario: ScenarioKind) -> (Duration, StageRss) {
        let (port, _server_task, _kick_tx) = spawn_mock_server(scenario, bot_count).await;
        let mut add_bots = String::new();
        for i in 0..bot_count {
            add_bots.push_str(&format!(
                "swarm:add_bot({{id = {i}, username = \"Bot{i}\", server = \"main\"}})\n"
            ));
        }
        let script = format!(
            r#"
            swarm:configure(function()
                swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}, shared_chunks = true}})
                {add_bots}
            end)
            swarm:connect_all()
            swarm:run()
            "#
        );

        let baseline_kib = crate::lua_benchmark::metrics::process_rss_kib();
        let start = std::time::Instant::now();
        let mut config = base_config(4, script);
        config.high_queue_capacity = 4096;
        config.low_queue_capacity = 1024;
        let swarm = run_swarm(config).await.expect("swarm must start");

        let all_connected = wait_for(
            || {
                swarm.bot_handles.values().all(|h| {
                    matches!(
                        *h.status().borrow(),
                        crate::core::supervisor::SupervisorStatus::Connected
                    )
                })
            },
            Duration::from_secs(120),
        )
        .await;
        assert!(all_connected, "all {bot_count} bots must reach Connected");
        let elapsed = start.elapsed();
        let connected_kib = crate::lua_benchmark::metrics::process_rss_kib();

        // Real synthetic packets for `scenario` (chunks in particular) are
        // sent by the mock server *after* the connection settles (see
        // `serve_one`), not as part of reaching `Connected` — a bounded
        // settle delay, not a signal this harness can poll for, is what
        // actually lets this process observe that traffic's memory
        // effect before measuring.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let scenario_kib = crate::lua_benchmark::metrics::process_rss_kib();

        swarm.shutdown(Duration::from_secs(15)).await;
        let shutdown_kib = crate::lua_benchmark::metrics::process_rss_kib();

        tokio::time::sleep(Duration::from_millis(200)).await;
        let cleanup_kib = crate::lua_benchmark::metrics::process_rss_kib();

        (
            elapsed,
            StageRss {
                baseline_kib,
                connected_kib,
                scenario_kib,
                shutdown_kib,
                cleanup_kib,
            },
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connects_dispatches_events_and_completes_a_gui_click_action() {
        let (port, _server_task, _kick_tx) =
            spawn_mock_server(ScenarioKind::RealisticState, 1).await;
        let script = format!(
            r#"
            swarm:configure(function()
                swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}}})
                swarm:add_bot({{username = "Bot0", server = "main"}})
            end)
            swarm:on("gui_opened", function(bot, event)
                bot:click_gui(0, "left", function(result)
                    swarm.shared:set("click_result_seen", true)
                end)
            end)
            swarm:on("system_chat", function(bot, event)
                swarm.shared:set("chat_message", event.message)
            end)
            swarm:connect_all()
            swarm:run()
            "#
        );
        let swarm = run_swarm(base_config(1, script))
            .await
            .expect("swarm must start");
        assert_eq!(swarm.registry.bots.len(), 1);

        let got_chat = wait_for(
            || matches!(swarm.shared_state.get("chat_message"), Some(SharedValue::Str(s)) if s == "welcome"),
            Duration::from_secs(5),
        )
        .await;
        assert!(
            got_chat,
            "chat handler must observe the mock server's welcome message"
        );

        let got_click_result = wait_for(
            || {
                matches!(
                    swarm.shared_state.get("click_result_seen"),
                    Some(SharedValue::Bool(true))
                )
            },
            Duration::from_secs(8),
        )
        .await;
        assert!(
            got_click_result,
            "click_gui's one-shot callback must fire (confirmed or timed out) within the bound"
        );

        swarm.shutdown(Duration::from_secs(5)).await;
    }

    /// The core Fix 8 regression: an action's one-shot callback must never
    /// be silently dropped by shutdown, even when it's still in flight the
    /// moment `shutdown()` is called — either it completes normally first,
    /// or `RunningSwarm::shutdown`'s wait for every in-flight action task
    /// (via each worker's `action_task_semaphore`) blocks shutdown from
    /// returning until it does, and the callback function itself always
    /// runs (with a normal or a typed `shutdown`-error result) — never
    /// neither. Deliberately shuts down as soon as the action is known to
    /// have been *issued* (not waiting for it to naturally resolve first),
    /// to actually exercise the in-flight race rather than only the
    /// already-settled case `connects_dispatches_events_and_completes_a_gui_click_action`
    /// above covers.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_never_silently_drops_an_in_flight_action_callback() {
        let (port, _server_task, _kick_tx) =
            spawn_mock_server(ScenarioKind::RealisticState, 1).await;
        let script = format!(
            r#"
            swarm:configure(function()
                swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}}})
                swarm:add_bot({{username = "Bot0", server = "main"}})
            end)
            swarm:on("gui_opened", function(bot, event)
                swarm.shared:set("click_issued", true)
                bot:click_gui(0, "left", function(result)
                    swarm.shared:set("callback_ran", true)
                    swarm.shared:set("callback_ok", result.ok)
                end)
            end)
            swarm:connect_all()
            swarm:run()
            "#
        );
        let swarm = run_swarm(base_config(1, script))
            .await
            .expect("swarm must start");
        let shared_state = swarm.shared_state.clone();

        let issued = wait_for(
            || {
                matches!(
                    shared_state.get("click_issued"),
                    Some(SharedValue::Bool(true))
                )
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(
            issued,
            "the click action must have been issued before shutdown is tested against it"
        );

        swarm.shutdown(Duration::from_secs(5)).await;

        assert!(
            matches!(
                shared_state.get("callback_ran"),
                Some(SharedValue::Bool(true))
            ),
            "the action's one-shot callback must have run by the time shutdown() returns \
             — it must never be silently dropped, regardless of whether it completed \
             normally or was resolved with a shutdown error"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn default_four_workers_assign_bots_deterministically_by_id() {
        let bot_count = 6u32;
        let (port, _server_task, _kick_tx) = spawn_mock_server(ScenarioKind::Idle, bot_count).await;
        let mut add_bots = String::new();
        for i in 0..bot_count {
            add_bots.push_str(&format!(
                "swarm:add_bot({{id = {i}, username = \"Bot{i}\", server = \"main\"}})\n"
            ));
        }
        let script = format!(
            r#"
            swarm:configure(function()
                swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}}})
                {add_bots}
            end)
            swarm:on("connected", function(bot, event)
                swarm.shared:set("worker_for_" .. tostring(bot:id()), bot:worker_id())
            end)
            swarm:connect_all()
            swarm:run()
            "#
        );
        let swarm = run_swarm(base_config(4, script))
            .await
            .expect("swarm must start");
        assert_eq!(swarm.dispatcher.worker_count(), 4);

        for i in 0..bot_count {
            let key = format!("worker_for_{i}");
            let seen = wait_for(
                || swarm.shared_state.get(&key).is_some(),
                Duration::from_secs(5),
            )
            .await;
            assert!(seen, "bot {i} never reported its worker id");
            let expected = (i % 4) as f64;
            match swarm.shared_state.get(&key) {
                Some(SharedValue::Number(n)) => {
                    assert_eq!(n, expected, "bot {i} landed on the wrong worker");
                }
                other => panic!("unexpected shared value for {key}: {other:?}"),
            }
        }

        swarm.shutdown(Duration::from_secs(5)).await;
    }

    /// The full Fix 3 regression, through the real Lua/dispatcher/network
    /// stack with all four production workers: every bot's `gui_opened`
    /// handler (which fires on that bot's *own* worker) deliberately
    /// reaches across to `swarm:bot((id+1) % 4)` — a bot owned by a
    /// *different* worker — and clicks its open GUI with a one-shot
    /// callback. This proves, simultaneously, that: (1) `swarm:bot(id)`'s
    /// remote lookup and `bot:worker_id()` report the bot's true owner
    /// regardless of who's asking; (2) the action actually executes
    /// against the target bot's own `SupervisorHandle`, not the caller's;
    /// and (3) the one-shot callback fires back on the *originating*
    /// worker (the bot's own worker), never lost to the target's owner.
    #[tokio::test(flavor = "multi_thread")]
    async fn cross_worker_bot_lookup_action_and_callback_all_land_on_the_right_worker() {
        let bot_count = 4u32;
        let (port, _server_task, _kick_tx) =
            spawn_mock_server(ScenarioKind::RealisticState, bot_count).await;
        let script = format!(
            r#"
            swarm:configure(function()
                swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}}})
                for i = 0, 3 do
                    swarm:add_bot({{id = i, username = "Bot" .. i, server = "main"}})
                end
            end)
            swarm:on("gui_opened", function(bot, event)
                local my_id = bot:id()
                local target_id = (my_id + 1) % 4
                local target = swarm:bot(target_id)
                swarm.shared:set("own_worker_" .. tostring(my_id), bot:worker_id())
                swarm.shared:set("target_worker_" .. tostring(my_id), target:worker_id())
                target:click_gui(0, "left", function(result)
                    swarm.shared:set("callback_worker_" .. tostring(my_id), swarm:status().worker_index)
                    swarm.shared:set("callback_fired_" .. tostring(my_id), true)
                end)
            end)
            swarm:connect_all()
            swarm:run()
            "#
        );
        let swarm = run_swarm(base_config(4, script))
            .await
            .expect("swarm must start");
        assert_eq!(swarm.dispatcher.worker_count(), 4);

        for i in 0..bot_count {
            let own_key = format!("own_worker_{i}");
            let target_key = format!("target_worker_{i}");
            let callback_key = format!("callback_worker_{i}");
            let fired_key = format!("callback_fired_{i}");

            let seen = wait_for(
                || swarm.shared_state.get(&fired_key).is_some(),
                Duration::from_secs(8),
            )
            .await;
            assert!(
                seen,
                "bot {i}'s cross-worker callback never fired (own={:?} target={:?})",
                swarm.shared_state.get(&own_key),
                swarm.shared_state.get(&target_key)
            );

            let expected_own = (i % 4) as f64;
            match swarm.shared_state.get(&own_key) {
                Some(SharedValue::Number(n)) => {
                    assert_eq!(
                        n, expected_own,
                        "bot {i}:worker_id() reported the wrong owner"
                    )
                }
                other => panic!("unexpected own_worker_{i}: {other:?}"),
            }

            let expected_target = ((i + 1) % 4) as f64;
            match swarm.shared_state.get(&target_key) {
                Some(SharedValue::Number(n)) => assert_eq!(
                    n,
                    expected_target,
                    "remote lookup swarm:bot({}):worker_id() reported the wrong owner",
                    (i + 1) % 4
                ),
                other => panic!("unexpected target_worker_{i}: {other:?}"),
            }

            // The callback was registered by bot i's own `gui_opened`
            // handler (running on worker `i`), for an action against a
            // bot owned by a *different* worker — it must fire back on
            // worker `i`, never on the target's owning worker.
            match swarm.shared_state.get(&callback_key) {
                Some(SharedValue::Number(n)) => assert_eq!(
                    n, expected_own,
                    "bot {i}'s callback fired on the wrong worker — it must return to the originating worker, not the target bot's owner"
                ),
                other => panic!("unexpected callback_worker_{i}: {other:?}"),
            }
        }

        swarm.shutdown(Duration::from_secs(5)).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reconnect_preserves_proxy_assignment_in_the_production_runtime() {
        let proxy = crate::lua_benchmark::fake_socks5::FakeSocks5Server::start(16).await;
        let (port, _server_task, kick_tx) =
            spawn_mock_server(ScenarioKind::ReconnectStorm { percent: 100 }, 1).await;
        let script = format!(
            r#"
            swarm:configure(function()
                swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}}})
                swarm:add_bot({{
                    username = "Bot0",
                    server = "main",
                    proxy = "p1",
                    reconnect = {{enabled = true, initial_delay_ms = 20, max_delay_ms = 100}},
                }})
            end)
            swarm:connect_all()
            swarm:run()
            "#
        );
        let mut proxy_profiles = crate::lua::registry::ProxyProfiles::new();
        proxy_profiles.insert(
            "p1".to_string(),
            Arc::new(Socks5ProxyConfig::new("127.0.0.1", proxy.port)),
        );
        let swarm = run_swarm(base_config_with_proxies(1, script, proxy_profiles))
            .await
            .expect("swarm must start");

        let handle = swarm.bot_handles.get(&0).expect("bot 0 handle").clone();
        let saw_reconnect_schedule = wait_for(
            || {
                matches!(
                    *handle.status().borrow(),
                    crate::core::supervisor::SupervisorStatus::ReconnectScheduled { .. }
                )
            },
            Duration::from_secs(5),
        )
        .await;
        // The kick only happens once the mock server's connection handler
        // observes it; give it a nudge in case it's still waiting.
        let _ = kick_tx.send(true);
        assert!(
            saw_reconnect_schedule || handle.generation() >= 1,
            "bot must have reconnected at least once after the mock server's kick"
        );

        let reconnected = wait_for(|| handle.generation() >= 2, Duration::from_secs(5)).await;
        assert!(
            reconnected,
            "bot must complete a second (reconnect) session, generation={}",
            handle.generation()
        );

        let accepted = proxy
            .accepted_connections
            .load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            accepted >= 2,
            "both the initial connection and the reconnect must route through the same proxy, got {accepted}"
        );

        swarm.shutdown(Duration::from_secs(5)).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn graceful_shutdown_completes_within_the_bound_and_stops_every_bot() {
        let (port, _server_task, _kick_tx) = spawn_mock_server(ScenarioKind::Idle, 2).await;
        let script = format!(
            r#"
            swarm:configure(function()
                swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}}})
                swarm:add_bot({{username = "Bot0", server = "main"}})
                swarm:add_bot({{username = "Bot1", server = "main"}})
            end)
            swarm:connect_all()
            swarm:run()
            "#
        );
        let swarm = run_swarm(base_config(2, script))
            .await
            .expect("swarm must start");
        let handles: Vec<_> = swarm.bot_handles.values().cloned().collect();

        let connected = wait_for(
            || {
                handles.iter().all(|h| {
                    matches!(
                        *h.status().borrow(),
                        crate::core::supervisor::SupervisorStatus::Connected
                    )
                })
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(connected, "both bots must connect before testing shutdown");

        let shutdown_finished = tokio::time::timeout(
            Duration::from_secs(5),
            swarm.shutdown(Duration::from_secs(4)),
        )
        .await
        .is_ok();
        assert!(
            shutdown_finished,
            "shutdown must complete within its bound, not hang"
        );

        for handle in &handles {
            assert!(matches!(
                *handle.status().borrow(),
                crate::core::supervisor::SupervisorStatus::Stopped
            ));
        }
    }

    /// Fix 8's "no surviving tasks or worker threads" guarantee, proven two
    /// ways: the same timing-based inference
    /// `crate::lua::runtime::tests::repeated_failed_startups_each_return_promptly_and_do_not_accumulate_state`
    /// uses for *failed* startups (repeated cycles must not progressively
    /// slow down), plus a direct cleanup invariant this Fix 10 adds —
    /// `crate::lua_benchmark::metrics::process_thread_count` (OS thread
    /// count on Linux, total handle count on Windows — either way, a
    /// leaked worker thread, socket, or task shows up here) must not grow
    /// materially across cycles. Timing alone can't distinguish "genuinely
    /// slower" from "resources leaking" as the cause of a flake; this
    /// gives a direct, platform-level signal instead of only inferring one
    /// from wall-clock variance.
    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_start_connect_shutdown_cycles_do_not_accumulate_state() {
        let mut durations = Vec::new();
        let mut thread_counts_after_shutdown = Vec::new();
        for _ in 0..5 {
            let (port, _server_task, _kick_tx) =
                spawn_mock_server(ScenarioKind::RealisticState, 1).await;
            let script = format!(
                r#"
                swarm:configure(function()
                    swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}}})
                    swarm:add_bot({{username = "Bot0", server = "main"}})
                end)
                swarm:on("gui_opened", function(bot, event)
                    bot:click_gui(0, "left", function(result) end)
                end)
                swarm:connect_all()
                swarm:run()
                "#
            );
            let start = std::time::Instant::now();
            let swarm = run_swarm(base_config(1, script))
                .await
                .expect("swarm must start");
            let handle = swarm.bot_handles.get(&0).expect("bot 0 handle").clone();
            wait_for(
                || {
                    matches!(
                        *handle.status().borrow(),
                        crate::core::supervisor::SupervisorStatus::Connected
                    )
                },
                Duration::from_secs(5),
            )
            .await;
            let shutdown_finished = tokio::time::timeout(
                Duration::from_secs(5),
                swarm.shutdown(Duration::from_secs(4)),
            )
            .await
            .is_ok();
            assert!(
                shutdown_finished,
                "shutdown must complete within its bound on every cycle"
            );
            durations.push(start.elapsed());
            thread_counts_after_shutdown
                .push(crate::lua_benchmark::metrics::process_thread_count());
        }

        let first = durations.first().copied().unwrap();
        let last = durations.last().copied().unwrap();
        assert!(
            last < first * 3 + Duration::from_secs(2),
            "iterations should not progressively slow down (leak smoke check): \
             first={first:?} last={last:?} all={durations:?}"
        );

        // Cleanup invariant: compares steady state *after* the first
        // cycle's shutdown against *after* the last — not "must return to
        // exactly its pre-loop value every single cycle", since tokio's
        // blocking-pool threads are pooled/reused rather than necessarily
        // reaped immediately after one `spawn_blocking` task returns (that
        // reuse is expected, correct behavior, not a leak). Growth
        // *without bound* across cycles is what a real leak looks like.
        if let (Some(first_threads), Some(last_threads)) = (
            thread_counts_after_shutdown.first().copied().flatten(),
            thread_counts_after_shutdown.last().copied().flatten(),
        ) {
            assert!(
                last_threads <= first_threads + 8,
                "OS thread/handle count must not grow across repeated cycles \
                 (cleanup-invariant leak check): after_first_cycle={first_threads} \
                 after_last_cycle={last_threads} all={thread_counts_after_shutdown:?}"
            );
        }
    }

    /// The pure-Rust registry layer used by `swarm:configure` refuses
    /// obviously-wrong config before any network I/O — covered thoroughly
    /// in `crate::lua::registry`'s own unit tests; this just confirms the
    /// Lua-facing `add_server`/`add_bot` methods surface those same
    /// validation errors as `(nil, error_table)` rather than a raised
    /// Lua error or a silent no-op.
    #[tokio::test(flavor = "multi_thread")]
    async fn add_bot_with_unknown_server_returns_a_typed_error_not_a_panic() {
        let mut builder = SwarmRegistryBuilder::default();
        let err = builder
            .add_bot(crate::lua::registry::BotSpec {
                id: None,
                username: "ghost".to_string(),
                server: "does-not-exist".to_string(),
                proxy: None,
                reconnect: crate::core::supervisor::ReconnectPolicy::default(),
                label: None,
            })
            .unwrap_err();
        assert_eq!(err.code(), "unknown_server");
    }

    /// Regression test for the proxy-secret-exfiltration fix: before this
    /// fix, a script could call `swarm:add_proxy({host=..., port=...,
    /// username_env=..., password_env=...})` with *any* host/port and
    /// *any* environment variable names — Rust would then read those
    /// variables (regardless of what secret actually lived there) and send
    /// them as SOCKS5 credentials to that script-chosen endpoint. This
    /// proves the fix: `add_proxy` is now always a typed error (never
    /// silently accepted), a bot's `proxy` field is validated as an opaque
    /// profile id against a host-supplied set (never resolved as an
    /// environment variable name), and the *only* endpoint any bot in this
    /// test ever successfully connects through is the one the host
    /// (`proxy_profiles`, constructed here exactly as a CLI would) actually
    /// registered.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_script_cannot_choose_an_arbitrary_proxy_endpoint_or_env_var() {
        // A secret that must never be read as a side effect of anything
        // this test's script does. If the old vulnerability were still
        // present, a script could get Rust to read this by naming it as
        // `username_env`/`password_env`, or by using its name as a proxy
        // reference — this test asserts neither ever happens.
        unsafe {
            std::env::set_var("MINERIDER_MUST_NEVER_BE_READ", "top-secret-value");
        }

        let (port, _server_task, _kick_tx) = spawn_mock_server(ScenarioKind::Idle, 1).await;
        // The one and only proxy the host actually registers.
        let real_proxy = crate::lua_benchmark::fake_socks5::FakeSocks5Server::start(8).await;
        let mut proxy_profiles = crate::lua::registry::ProxyProfiles::new();
        proxy_profiles.insert(
            "trusted".to_string(),
            Arc::new(Socks5ProxyConfig::new("127.0.0.1", real_proxy.port)),
        );

        let script = format!(
            r#"
            swarm:configure(function()
                swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}}})

                -- Attempt 1: try to define a proxy directly (the old,
                -- removed API shape). Must fail with a typed error, never
                -- silently succeed or crash the configure phase.
                local ok, err = swarm:add_proxy({{
                    name = "evil",
                    host = "127.0.0.1",
                    port = 9,
                    username_env = "MINERIDER_MUST_NEVER_BE_READ",
                    password_env = "MINERIDER_MUST_NEVER_BE_READ",
                }})
                swarm.shared:set("add_proxy_ok", ok == true)
                swarm.shared:set("add_proxy_err_code", err and err.code or nil)

                -- Attempt 2: use the secret's own name as a proxy *id* —
                -- still just an opaque string key looked up in the host's
                -- map, never resolved as an environment variable name, so
                -- this must fail exactly like any other unregistered id.
                local bot_id, add_err = swarm:add_bot({{
                    username = "Evil0",
                    server = "main",
                    proxy = "MINERIDER_MUST_NEVER_BE_READ",
                }})
                swarm.shared:set("evil_bot_id", bot_id)
                swarm.shared:set("evil_bot_err_code", add_err and add_err.code or nil)

                -- The one legitimate bot, via the host-registered profile id.
                swarm:add_bot({{id = 0, username = "Bot0", server = "main", proxy = "trusted"}})
            end)
            swarm:connect_all()
            swarm:run()
            "#
        );

        let swarm = run_swarm(base_config_with_proxies(1, script, proxy_profiles))
            .await
            .expect("swarm must start (the one legitimate bot is valid config)");

        let configured = wait_for(
            || swarm.shared_state.get("add_proxy_ok").is_some(),
            Duration::from_secs(5),
        )
        .await;
        assert!(configured, "configure() must have run to completion");

        assert_eq!(
            swarm.shared_state.get("add_proxy_ok"),
            Some(SharedValue::Bool(false)),
            "add_proxy must never succeed"
        );
        assert_eq!(
            swarm.shared_state.get("add_proxy_err_code"),
            Some(SharedValue::Str("invalid_configuration".to_string()))
        );

        assert_eq!(
            swarm.shared_state.get("evil_bot_id"),
            Some(SharedValue::Nil),
            "a bot referencing an unregistered proxy id must never be created"
        );
        assert_eq!(
            swarm.shared_state.get("evil_bot_err_code"),
            Some(SharedValue::Str("unknown_proxy".to_string()))
        );
        assert_eq!(
            swarm.registry.bots.len(),
            1,
            "only the one legitimate bot may exist"
        );

        let connected = wait_for(
            || {
                matches!(
                    *swarm.bot_handles.get(&0).unwrap().status().borrow(),
                    crate::core::supervisor::SupervisorStatus::Connected
                )
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(
            connected,
            "the one legitimate bot must connect through the trusted profile"
        );

        let accepted = real_proxy
            .accepted_connections
            .load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            accepted, 1,
            "exactly the one legitimate bot must have routed through the real proxy — \
             no other endpoint was ever reachable"
        );

        swarm.shutdown(Duration::from_secs(5)).await;

        unsafe {
            std::env::remove_var("MINERIDER_MUST_NEVER_BE_READ");
        }
    }

    /// Loads the *actual shipped* `examples/lua/swarm.lua` (via
    /// `include_str!`, so this test breaks if the file and this test drift)
    /// and runs it for real: 2 proxy groups of 3 bots each, through two
    /// independent local fake SOCKS5 relays, against the local mock
    /// server. The script itself never defines a proxy (there is no
    /// `add_proxy` anymore — see `crate::lua::registry`'s module doc
    /// comment): the two profiles it references (`"proxy1"`/`"proxy2"`)
    /// are supplied here exactly as a host CLI would, via
    /// `SwarmRuntimeConfig::proxy_profiles`, entirely outside the script.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_shipped_example_script_connects_two_full_proxy_groups() {
        const EXAMPLE: &str = include_str!("../../examples/lua/swarm.lua");

        let (port, _server_task, _kick_tx) = spawn_mock_server(ScenarioKind::Idle, 6).await;
        let proxy1 = crate::lua_benchmark::fake_socks5::FakeSocks5Server::start(8).await;
        let proxy2 = crate::lua_benchmark::fake_socks5::FakeSocks5Server::start(8).await;

        let mut proxy_profiles = crate::lua::registry::ProxyProfiles::new();
        proxy_profiles.insert(
            "proxy1".to_string(),
            Arc::new(Socks5ProxyConfig::new("127.0.0.1", proxy1.port)),
        );
        proxy_profiles.insert(
            "proxy2".to_string(),
            Arc::new(Socks5ProxyConfig::new("127.0.0.1", proxy2.port)),
        );

        let script = EXAMPLE
            .replace("port = 25565", &format!("port = {port}"))
            // The mock server only accepts usernames it can parse as
            // `Bot<index>`, and needs every bot's index unique across both
            // groups; the example's own naming is what real servers would
            // see, so this substitution is test-fixture-only.
            .replace(
                "username_prefix = \"Swarm1_\"",
                "username_prefix = \"Bot1\"",
            )
            .replace(
                "username_prefix = \"Swarm2_\"",
                "username_prefix = \"Bot2\"",
            );

        let config = base_config_with_proxies(4, script, proxy_profiles);
        let swarm = run_swarm(config)
            .await
            .expect("the shipped example script must start cleanly");
        assert_eq!(swarm.registry.bots.len(), 6, "2 groups of 3 bots each");
        assert_eq!(swarm.registry.groups.len(), 2);

        let all_connected = wait_for(
            || {
                swarm.bot_handles.values().all(|h| {
                    matches!(
                        *h.status().borrow(),
                        crate::core::supervisor::SupervisorStatus::Connected
                    )
                })
            },
            Duration::from_secs(10),
        )
        .await;
        assert!(all_connected, "every bot in both proxy groups must connect");

        let proxy1_accepted = proxy1
            .accepted_connections
            .load(std::sync::atomic::Ordering::SeqCst);
        let proxy2_accepted = proxy2
            .accepted_connections
            .load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            proxy1_accepted, 3,
            "group1's 3 bots must route through proxy1"
        );
        assert_eq!(
            proxy2_accepted, 3,
            "group2's 3 bots must route through proxy2"
        );

        swarm.shutdown(Duration::from_secs(5)).await;
    }

    /// Every targeted state view (`state`/`player`/`entities`/`players`/
    /// `inventory`/`hud`/`presentation`/`scoreboard`) must be reachable and
    /// well-formed once the mock server has sent real chunk/entity/HUD/
    /// inventory data — not just compile, but actually produce sane values
    /// through a real Lua VM.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_targeted_state_view_is_reachable_and_well_formed() {
        let (port, _server_task, _kick_tx) =
            spawn_mock_server(ScenarioKind::RealisticState, 1).await;
        let script = format!(
            r#"
            swarm:configure(function()
                swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}}})
                swarm:add_bot({{username = "Bot0", server = "main"}})
            end)
            swarm:on("gui_opened", function(bot, event)
                local state = bot:state()
                local player = bot:player()
                local entities = bot:entities()
                local players = bot:players()
                local inventory = bot:inventory()
                local hud = bot:hud()
                local presentation = bot:presentation()
                local scoreboard = bot:scoreboard()
                swarm.shared:set("state_ok", state ~= nil and type(state.tick) == "number")
                swarm.shared:set("player_ok", player ~= nil and type(player.position.x) == "number")
                swarm.shared:set("entities_ok", entities ~= nil)
                swarm.shared:set("players_ok", players ~= nil)
                swarm.shared:set("inventory_ok", inventory ~= nil and inventory.player_inventory ~= nil)
                swarm.shared:set("hud_ok", hud ~= nil and hud.vitals ~= nil and type(hud.vitals.health) == "number")
                swarm.shared:set("presentation_ok", presentation ~= nil and presentation.chat ~= nil)
                swarm.shared:set("scoreboard_ok", scoreboard ~= nil and scoreboard.objectives ~= nil)
                local view = bot:open_gui()
                swarm.shared:set(
                    "gui_slot_shape_ok",
                    view ~= nil
                        and view.slots[1] ~= nil
                        and type(view.slots[1].raw_slot) == "number"
                        and type(view.slots[1].lua_index) == "number"
                        and view.slots[1].lua_index == view.slots[1].raw_slot + 1
                )
            end)
            swarm:connect_all()
            swarm:run()
            "#
        );
        let swarm = run_swarm(base_config(1, script))
            .await
            .expect("swarm must start");

        for key in [
            "state_ok",
            "player_ok",
            "entities_ok",
            "players_ok",
            "inventory_ok",
            "hud_ok",
            "presentation_ok",
            "scoreboard_ok",
            "gui_slot_shape_ok",
        ] {
            let seen = wait_for(
                || matches!(swarm.shared_state.get(key), Some(SharedValue::Bool(true))),
                Duration::from_secs(5),
            )
            .await;
            assert!(seen, "state view check `{key}` did not report true");
        }

        swarm.shutdown(Duration::from_secs(5)).await;
    }

    /// `bot:set_timeout`/`set_interval`/`clear_timer` must actually fire on
    /// a real per-worker timer scheduler (not just be callable), and
    /// `clear_timer` must stop further firings of an interval.
    #[tokio::test(flavor = "multi_thread")]
    async fn bot_timers_fire_and_clear_timer_stops_further_firings() {
        let (port, _server_task, _kick_tx) = spawn_mock_server(ScenarioKind::Idle, 1).await;
        let script = format!(
            r#"
            swarm:configure(function()
                swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}}})
                swarm:add_bot({{username = "Bot0", server = "main"}})
            end)
            swarm:on("connected", function(bot, event)
                bot:set_timeout(50, function()
                    swarm.shared:set("timeout_fired", true)
                end)
                local interval_id
                interval_id = bot:set_interval(30, function()
                    swarm.shared:update("interval_count", function(v)
                        return (v or 0) + 1
                    end)
                end)
                bot:set_timeout(200, function()
                    bot:clear_timer(interval_id)
                    swarm.shared:set("interval_cleared", true)
                end)
            end)
            swarm:connect_all()
            swarm:run()
            "#
        );
        let swarm = run_swarm(base_config(1, script))
            .await
            .expect("swarm must start");

        let timeout_fired = wait_for(
            || {
                matches!(
                    swarm.shared_state.get("timeout_fired"),
                    Some(SharedValue::Bool(true))
                )
            },
            Duration::from_secs(3),
        )
        .await;
        assert!(timeout_fired, "one-shot set_timeout must fire");

        let cleared = wait_for(
            || {
                matches!(
                    swarm.shared_state.get("interval_cleared"),
                    Some(SharedValue::Bool(true))
                )
            },
            Duration::from_secs(3),
        )
        .await;
        assert!(cleared, "the clearing timeout must itself fire");

        let count_at_clear = match swarm.shared_state.get("interval_count") {
            Some(SharedValue::Number(n)) => n,
            other => panic!("interval never fired: {other:?}"),
        };
        assert!(
            count_at_clear >= 3.0,
            "interval must have fired multiple times before being cleared, got {count_at_clear}"
        );

        // Give the (now-cleared) interval a generous window in which it
        // must NOT fire again.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let count_after_wait = match swarm.shared_state.get("interval_count") {
            Some(SharedValue::Number(n)) => n,
            other => panic!("interval count disappeared: {other:?}"),
        };
        assert_eq!(
            count_after_wait, count_at_clear,
            "clear_timer must stop further interval firings"
        );

        swarm.shutdown(Duration::from_secs(5)).await;
    }

    /// Bot-scoped timer ownership, through the real Lua/network stack with
    /// all four production workers: every bot's own worker must still be
    /// able to schedule a timer for *its own* bot, but reaching across to
    /// `swarm:bot((id+1) % 4):set_timeout(...)` — a bot owned by a
    /// *different* worker — must be rejected with a typed
    /// `cross_worker_timer` error rather than silently registering a timer
    /// that looks bot-scoped but isn't actually tied to that bot's worker.
    #[tokio::test(flavor = "multi_thread")]
    async fn bot_scoped_timers_reject_cross_worker_registration_across_all_four_workers() {
        let bot_count = 4u32;
        let (port, _server_task, _kick_tx) = spawn_mock_server(ScenarioKind::Idle, bot_count).await;
        let script = format!(
            r#"
            swarm:configure(function()
                swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}}})
                for i = 0, 3 do
                    swarm:add_bot({{id = i, username = "Bot" .. i, server = "main"}})
                end
            end)
            swarm:on("connected", function(bot, event)
                local my_id = bot:id()
                local ok_own = pcall(function()
                    bot:set_timeout(10000, function() end)
                end)
                swarm.shared:set("own_timer_ok_" .. tostring(my_id), ok_own)

                local target = swarm:bot((my_id + 1) % 4)
                local ok_cross, err_cross = pcall(function()
                    target:set_timeout(10000, function() end)
                end)
                swarm.shared:set("cross_timer_ok_" .. tostring(my_id), ok_cross)
                swarm.shared:set("cross_timer_err_" .. tostring(my_id), tostring(err_cross))
            end)
            swarm:connect_all()
            swarm:run()
            "#
        );
        let swarm = run_swarm(base_config(4, script))
            .await
            .expect("swarm must start");

        for i in 0..bot_count {
            let own_key = format!("own_timer_ok_{i}");
            let cross_ok_key = format!("cross_timer_ok_{i}");
            let cross_err_key = format!("cross_timer_err_{i}");

            let seen = wait_for(
                || swarm.shared_state.get(&cross_ok_key).is_some(),
                Duration::from_secs(5),
            )
            .await;
            assert!(
                seen,
                "bot {i} never reported its cross-worker timer attempt"
            );

            match swarm.shared_state.get(&own_key) {
                Some(SharedValue::Bool(true)) => {}
                other => panic!("bot {i}'s own-worker timer must succeed, got {other:?}"),
            }
            match swarm.shared_state.get(&cross_ok_key) {
                Some(SharedValue::Bool(false)) => {}
                other => panic!("bot {i}'s cross-worker timer must be rejected, got {other:?}"),
            }
            match swarm.shared_state.get(&cross_err_key) {
                Some(SharedValue::Str(s)) => assert!(
                    s.contains("cross_worker_timer"),
                    "bot {i}'s rejection error must be the typed cross_worker_timer error, got: {s}"
                ),
                other => panic!("unexpected cross_timer_err_{i}: {other:?}"),
            }
        }

        swarm.shutdown(Duration::from_secs(5)).await;
    }

    /// A "global" `swarm:set_timeout` registered identically by every
    /// worker (since every worker runs the same script) must still fire
    /// exactly once overall, not once per worker — see
    /// `crate::lua::api::timers`'s coordinator-only gating.
    #[tokio::test(flavor = "multi_thread")]
    async fn global_swarm_timer_fires_exactly_once_across_all_workers() {
        let (port, _server_task, _kick_tx) = spawn_mock_server(ScenarioKind::Idle, 4).await;
        let script = format!(
            r#"
            swarm:configure(function()
                swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}}})
                for i = 0, 3 do
                    swarm:add_bot({{id = i, username = "Bot" .. i, server = "main"}})
                end
            end)
            swarm:set_timeout(100, function()
                swarm.shared:update("global_timer_fires", function(v)
                    return (v or 0) + 1
                end)
            end)
            swarm:connect_all()
            swarm:run()
            "#
        );
        let swarm = run_swarm(base_config(4, script))
            .await
            .expect("swarm must start");

        let fired = wait_for(
            || swarm.shared_state.get("global_timer_fires").is_some(),
            Duration::from_secs(3),
        )
        .await;
        assert!(fired, "the global timer must fire at least once");

        // Give plenty of margin past the fire time, then confirm the count
        // never exceeded 1 despite 4 workers all having run this script.
        tokio::time::sleep(Duration::from_millis(400)).await;
        match swarm.shared_state.get("global_timer_fires") {
            Some(SharedValue::Number(n)) => {
                assert_eq!(n, 1.0, "must fire exactly once, not once per worker")
            }
            other => panic!("unexpected value: {other:?}"),
        }

        swarm.shutdown(Duration::from_secs(5)).await;
    }

    /// A single handler error for one bot must never crash the worker,
    /// never stop that same event from still firing that bot's other
    /// handlers or delivering it to unrelated bots, and must never affect
    /// any bot's underlying Minecraft connection. The consecutive-error
    /// *threshold* → disable transition itself is exercised at the unit
    /// level in `crate::lua::worker::tests::consecutive_handler_errors_disable_the_offending_bot_only`
    /// (fast, no network) rather than by driving 10 real network events
    /// through this integration harness.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_with_a_broken_handler_does_not_affect_other_bots_or_the_connection() {
        let (port, _server_task, _kick_tx) = spawn_mock_server(ScenarioKind::Idle, 2).await;
        let script = format!(
            r#"
            swarm:configure(function()
                swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}}})
                swarm:add_bot({{id = 0, username = "Bot0", server = "main"}})
                swarm:add_bot({{id = 1, username = "Bot1", server = "main"}})
            end)
            swarm:on("connected", function(bot, event)
                swarm.shared:update("connected_count", function(v) return (v or 0) + 1 end)
                if bot:id() == 0 then
                    error("boom")
                end
            end)
            swarm:connect_all()
            swarm:run()
            "#
        );
        let swarm = run_swarm(base_config(1, script))
            .await
            .expect("swarm must start");

        let both_connected = wait_for(
            || matches!(swarm.shared_state.get("connected_count"), Some(SharedValue::Number(n)) if n >= 2.0),
            Duration::from_secs(5),
        )
        .await;
        assert!(
            both_connected,
            "both bots must still connect and fire their connected handler once"
        );

        for handle in swarm.bot_handles.values() {
            let ok = wait_for(
                || {
                    matches!(
                        *handle.status().borrow(),
                        crate::core::supervisor::SupervisorStatus::Connected
                    )
                },
                Duration::from_secs(5),
            )
            .await;
            assert!(ok, "every bot's underlying connection must remain healthy despite bot 0's broken handler");
        }

        swarm.shutdown(Duration::from_secs(5)).await;
    }

    #[derive(serde::Serialize)]
    struct ScaleBenchmarkResult {
        disclaimer: &'static str,
        bots: u32,
        lua_workers: usize,
        scenario: &'static str,
        warmup_runs: usize,
        measured_runs: usize,
        time_to_all_connected_ms: TimingStatsMs,
        rss_kib: StageRss,
    }

    const SCALE_WARMUP_RUNS: usize = 1;
    const SCALE_MEASURED_RUNS: usize = 5;

    /// Production-wrapper scale measurement: 100/400/800 bots through the
    /// *real* `crate::lua::runtime::run_swarm` (4 workers), measured
    /// separately for an idle connection and a real loaded-chunk scenario
    /// (genuine synthetic chunk packets from the mock server — the
    /// previous version of this benchmark set `shared_chunks = true` on
    /// the server but only ever ran `ScenarioKind::Idle`, so no chunk ever
    /// actually flowed through the chunk-sharing code path it claimed to
    /// measure). One discarded warm-up run, then
    /// `SCALE_MEASURED_RUNS` (≥5) measured runs per (bot count, scenario)
    /// pair, reporting median/min/max wall-clock time-to-all-connected —
    /// never a single arbitrary sample — plus baseline/connected/scenario/
    /// shutdown/cleanup process RSS from the last measured run. Raw
    /// results are also written as JSON under
    /// `docs/lua_wrapper_benchmark_results/`. See `LOOPBACK_DISCLAIMER`:
    /// every number here comes from a local loopback mock server, never a
    /// real Minecraft server or real-world proxy.
    ///
    /// Not run in normal CI (too slow, and the mission is explicit that
    /// the full-scale benchmark should be manual, not per-commit) — run
    /// explicitly with:
    /// `cargo test --release --features lua-benchmark --lib \
    ///   lua_benchmark::full_runtime::production_smoke::production_wrapper_scale_100_400_800 \
    ///   -- --ignored --nocapture --test-threads=1`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "manual production-wrapper scale benchmark; see doc comment for the exact invocation"]
    async fn production_wrapper_scale_100_400_800() {
        for &bot_count in &[100u32, 400, 800] {
            for (scenario_name, scenario) in [
                ("idle", ScenarioKind::Idle),
                (
                    "chunks",
                    ScenarioKind::Chunks {
                        chunk_count: 25,
                        personalize: true,
                    },
                ),
            ] {
                for _ in 0..SCALE_WARMUP_RUNS {
                    let _ = run_one_scale_cycle(bot_count, scenario).await;
                }

                let mut samples_ms = Vec::with_capacity(SCALE_MEASURED_RUNS);
                let mut last_rss = StageRss::default();
                for _ in 0..SCALE_MEASURED_RUNS {
                    let (elapsed, rss) = run_one_scale_cycle(bot_count, scenario).await;
                    samples_ms.push(elapsed.as_secs_f64() * 1000.0);
                    last_rss = rss;
                }
                let timing = compute_timing_stats(samples_ms);

                println!(
                    "[production_wrapper_scale] bots={bot_count} scenario={scenario_name} \
                     median_ms={:.1} min_ms={:.1} max_ms={:.1} rss={last_rss:?}",
                    timing.median, timing.min, timing.max
                );

                let result = ScaleBenchmarkResult {
                    disclaimer: LOOPBACK_DISCLAIMER,
                    bots: bot_count,
                    lua_workers: 4,
                    scenario: scenario_name,
                    warmup_runs: SCALE_WARMUP_RUNS,
                    measured_runs: SCALE_MEASURED_RUNS,
                    time_to_all_connected_ms: timing,
                    rss_kib: last_rss,
                };
                write_benchmark_json(&format!("scale_{bot_count}_{scenario_name}"), &result);
            }
        }
    }

    struct CombinedCycleResult {
        elapsed_to_initial_connect_ms: f64,
        elapsed_total_ms: f64,
        rss: StageRss,
        proxy1_accepted: usize,
        proxy2_accepted: usize,
        reconnected_count: usize,
    }

    /// One full cycle of the combined benchmark: 100 bots, 4 workers, 2
    /// local proxy groups (50 bots each, alternating even/odd id), a
    /// `ReconnectStorm{percent: 20}` — which targets *exactly* bots
    /// `0..19` for `bot_count = 100` (`bot_index % 100 < 20`), split
    /// evenly 10 on proxy1 (even ids) / 10 on proxy2 (odd ids) — shared
    /// chunks enabled. Asserts the *exact* expected reconnect and
    /// per-proxy connection counts inline (not just a lower bound): every
    /// one of the 20 targeted bots must reconnect exactly once (generation
    /// reaches exactly 2, never more — the mock server only ever kicks a
    /// bot's *first* connection), giving exactly 60 accepted connections
    /// on each proxy (50 initial + 10 reconnects).
    async fn run_one_combined_cycle(bot_count: u32) -> CombinedCycleResult {
        let (port, _server_task, kick_tx) =
            spawn_mock_server(ScenarioKind::ReconnectStorm { percent: 20 }, bot_count).await;
        let proxy1 = crate::lua_benchmark::fake_socks5::FakeSocks5Server::start(64).await;
        let proxy2 = crate::lua_benchmark::fake_socks5::FakeSocks5Server::start(64).await;

        let mut add_bots = String::new();
        for i in 0..bot_count {
            let proxy = if i % 2 == 0 { "proxy1" } else { "proxy2" };
            add_bots.push_str(&format!(
                "swarm:add_bot({{id = {i}, username = \"Bot{i}\", server = \"main\", proxy = \"{proxy}\", \
                 reconnect = {{enabled = true, initial_delay_ms = 20, max_delay_ms = 200}}}})\n"
            ));
        }
        let script = format!(
            r#"
            swarm:configure(function()
                swarm:add_server({{name = "main", host = "127.0.0.1", port = {port}, shared_chunks = true}})
                {add_bots}
            end)
            swarm:connect_all()
            swarm:run()
            "#
        );

        let mut proxy_profiles = crate::lua::registry::ProxyProfiles::new();
        proxy_profiles.insert(
            "proxy1".to_string(),
            Arc::new(Socks5ProxyConfig::new("127.0.0.1", proxy1.port)),
        );
        proxy_profiles.insert(
            "proxy2".to_string(),
            Arc::new(Socks5ProxyConfig::new("127.0.0.1", proxy2.port)),
        );

        let baseline_kib = crate::lua_benchmark::metrics::process_rss_kib();
        let start = std::time::Instant::now();
        let mut config = base_config_with_proxies(4, script, proxy_profiles);
        config.high_queue_capacity = 4096;
        config.low_queue_capacity = 1024;
        let swarm = run_swarm(config).await.expect("swarm must start");

        let all_connected = wait_for(
            || {
                swarm.bot_handles.values().all(|h| {
                    matches!(
                        *h.status().borrow(),
                        crate::core::supervisor::SupervisorStatus::Connected
                    )
                })
            },
            Duration::from_secs(60),
        )
        .await;
        assert!(
            all_connected,
            "all {bot_count} bots must reach Connected initially"
        );
        let elapsed_to_initial_connect = start.elapsed();
        let connected_kib = crate::lua_benchmark::metrics::process_rss_kib();

        // The mock server's targeted-bot kick logic waits for this signal
        // before dropping each targeted bot's first connection (see
        // `serve_one`'s `ScenarioKind::ReconnectStorm` branch) — without
        // sending it, no reconnect is ever triggered.
        let _ = kick_tx.send(true);

        let expected_reconnects = (bot_count as usize * 20) / 100;
        let reconnected_exactly = wait_for(
            || {
                swarm
                    .bot_handles
                    .values()
                    .filter(|h| h.generation() >= 2)
                    .count()
                    >= expected_reconnects
            },
            Duration::from_secs(30),
        )
        .await;
        let elapsed_total = start.elapsed();
        let scenario_kib = crate::lua_benchmark::metrics::process_rss_kib();

        let reconnected_count = swarm
            .bot_handles
            .values()
            .filter(|h| h.generation() >= 2)
            .count();
        assert!(
            reconnected_exactly,
            "all {expected_reconnects} targeted bots must reconnect, only {reconnected_count} did \
             within the bound"
        );
        assert_eq!(
            reconnected_count, expected_reconnects,
            "exactly {expected_reconnects} bots must reconnect — no more, no fewer"
        );
        for handle in swarm.bot_handles.values() {
            assert!(
                handle.generation() <= 2,
                "the mock server only ever kicks a bot's first connection — no bot should \
                 reconnect more than once, but bot reached generation {}",
                handle.generation()
            );
        }

        let proxy1_accepted = proxy1
            .accepted_connections
            .load(std::sync::atomic::Ordering::SeqCst);
        let proxy2_accepted = proxy2
            .accepted_connections
            .load(std::sync::atomic::Ordering::SeqCst);
        // Half of `bot_count` bots (even ids) use proxy1, half (odd ids)
        // use proxy2; among the 20 targeted (lowest-id) bots the same
        // even/odd split applies, so each proxy sees exactly half the
        // reconnects too.
        let expected_per_proxy_initial = (bot_count as usize) / 2;
        let expected_per_proxy_reconnects = expected_reconnects / 2;
        let expected_per_proxy = expected_per_proxy_initial + expected_per_proxy_reconnects;
        assert_eq!(
            proxy1_accepted, expected_per_proxy,
            "proxy1 must see exactly {expected_per_proxy} accepted connections \
             ({expected_per_proxy_initial} initial + {expected_per_proxy_reconnects} reconnects)"
        );
        assert_eq!(
            proxy2_accepted, expected_per_proxy,
            "proxy2 must see exactly {expected_per_proxy} accepted connections \
             ({expected_per_proxy_initial} initial + {expected_per_proxy_reconnects} reconnects)"
        );

        swarm.shutdown(Duration::from_secs(15)).await;
        let shutdown_kib = crate::lua_benchmark::metrics::process_rss_kib();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let cleanup_kib = crate::lua_benchmark::metrics::process_rss_kib();

        CombinedCycleResult {
            elapsed_to_initial_connect_ms: elapsed_to_initial_connect.as_secs_f64() * 1000.0,
            elapsed_total_ms: elapsed_total.as_secs_f64() * 1000.0,
            rss: StageRss {
                baseline_kib,
                connected_kib,
                scenario_kib,
                shutdown_kib,
                cleanup_kib,
            },
            proxy1_accepted,
            proxy2_accepted,
            reconnected_count,
        }
    }

    #[derive(serde::Serialize)]
    struct CombinedBenchmarkResult {
        disclaimer: &'static str,
        bots: u32,
        lua_workers: usize,
        warmup_runs: usize,
        measured_runs: usize,
        expected_reconnects: usize,
        expected_per_proxy_accepted: usize,
        time_to_initial_connect_ms: TimingStatsMs,
        time_to_total_ms: TimingStatsMs,
        rss_kib: StageRss,
    }

    const COMBINED_WARMUP_RUNS: usize = 1;
    const COMBINED_MEASURED_RUNS: usize = 5;

    /// The "representative" combined production-wrapper benchmark the
    /// mission asks for in one run: 100 bots, 4 workers, 2 local proxy
    /// groups, a reconnect subset, shared chunks enabled — see
    /// `run_one_combined_cycle`'s doc comment for the exact scenario and
    /// the *exact* (not lower-bound) reconnect/per-proxy-connection counts
    /// it asserts on every one of `COMBINED_MEASURED_RUNS` (≥5) runs,
    /// after one discarded warm-up run. Reports median/min/max wall-clock
    /// timing and RSS from the last measured run, and writes raw JSON
    /// under `docs/lua_wrapper_benchmark_results/`. See
    /// `LOOPBACK_DISCLAIMER`. Manual — see the invocation in
    /// `production_wrapper_scale_100_400_800`'s doc comment (same
    /// pattern, different test name).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "manual production-wrapper combined benchmark; see doc comment for the exact invocation"]
    async fn production_wrapper_combined_proxy_groups_reconnect_shared_chunks() {
        let bot_count = 100u32;

        for _ in 0..COMBINED_WARMUP_RUNS {
            let _ = run_one_combined_cycle(bot_count).await;
        }

        let mut initial_connect_samples_ms = Vec::with_capacity(COMBINED_MEASURED_RUNS);
        let mut total_samples_ms = Vec::with_capacity(COMBINED_MEASURED_RUNS);
        let mut last_rss = StageRss::default();
        let mut last = None;
        for _ in 0..COMBINED_MEASURED_RUNS {
            let result = run_one_combined_cycle(bot_count).await;
            initial_connect_samples_ms.push(result.elapsed_to_initial_connect_ms);
            total_samples_ms.push(result.elapsed_total_ms);
            last_rss = result.rss.clone();
            last = Some((
                result.proxy1_accepted,
                result.proxy2_accepted,
                result.reconnected_count,
            ));
        }
        let (proxy1_accepted, proxy2_accepted, reconnected_count) = last.expect(
            "COMBINED_MEASURED_RUNS must be at least 1 (currently 5), so `last` is always set",
        );

        let initial_connect_timing = compute_timing_stats(initial_connect_samples_ms);
        let total_timing = compute_timing_stats(total_samples_ms);

        println!(
            "[production_wrapper_combined] bots={bot_count} \
             reconnected={reconnected_count} proxy1_accepted={proxy1_accepted} \
             proxy2_accepted={proxy2_accepted} \
             median_initial_connect_ms={:.1} median_total_ms={:.1} rss={last_rss:?}",
            initial_connect_timing.median, total_timing.median
        );

        let expected_reconnects = (bot_count as usize * 20) / 100;
        let expected_per_proxy_accepted = (bot_count as usize) / 2 + expected_reconnects / 2;
        let result = CombinedBenchmarkResult {
            disclaimer: LOOPBACK_DISCLAIMER,
            bots: bot_count,
            lua_workers: 4,
            warmup_runs: COMBINED_WARMUP_RUNS,
            measured_runs: COMBINED_MEASURED_RUNS,
            expected_reconnects,
            expected_per_proxy_accepted,
            time_to_initial_connect_ms: initial_connect_timing,
            time_to_total_ms: total_timing,
            rss_kib: last_rss,
        };
        write_benchmark_json("combined_proxy_groups_reconnect_shared_chunks", &result);
    }
}
