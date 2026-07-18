//! CLI for the Lua runtime architecture benchmark (see
//! `docs/lua_runtime_benchmark.md`). Benchmark-only: not the wrapper.
//!
//! ```text
//! cargo run --release --features lua-benchmark --bin lua_runtime_benchmark -- \
//!   --mode shared-lua --scenario synthetic --bots 400 --lua-workers 1 \
//!   --event-rate typical --duration-secs 30 --script realistic --seed 42 \
//!   --output target/lua-benchmark/result.json
//! ```
//!
//! `--scenario full-runtime` additionally accepts `--full-scenario`
//! (idle|chunks|chunks-personalized|realistic-state|reconnect-storm|
//! proxy-groups), `--share-chunks`, `--chunk-count`, `--reconnect-percent`,
//! `--proxy-group-size`. Never contacts a public server or a live proxy.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use minerider::lua_benchmark::dispatch::{Architecture, QueueKind};
use minerider::lua_benchmark::fake_socks5::FakeSocks5Server;
use minerider::lua_benchmark::full_runtime::{
    run_full_runtime_scenario, ScenarioKind, SwarmConfig,
};
use minerider::lua_benchmark::sandbox::SandboxConfig;
use minerider::lua_benchmark::scripts::ScriptKind;
use minerider::lua_benchmark::synthetic::{
    run_synthetic_scenario, EventRateProfile, SyntheticConfig,
};
use minerider::network::socks5::Socks5ProxyConfig;

struct Args {
    mode: String,
    scenario: String,
    full_scenario: String,
    bots: u32,
    lua_workers: usize,
    event_rate: String,
    duration_secs: u64,
    script: String,
    seed: u64,
    output: Option<String>,
    queue: String,
    share_chunks: bool,
    chunk_count: usize,
    reconnect_percent: u32,
    proxy_group_size: u32,
    memory_limit_mb: usize,
    instruction_budget: u64,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            mode: "shared-lua".to_string(),
            scenario: "synthetic".to_string(),
            full_scenario: "idle".to_string(),
            bots: 100,
            lua_workers: 4,
            event_rate: "typical".to_string(),
            duration_secs: 10,
            script: "realistic".to_string(),
            seed: 42,
            output: None,
            queue: "fifo".to_string(),
            share_chunks: true,
            chunk_count: 49,
            reconnect_percent: 25,
            proxy_group_size: 3,
            memory_limit_mb: 4,
            instruction_budget: 2_000,
        }
    }
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        let flag = raw[i].as_str();
        let mut next = || {
            i += 1;
            raw.get(i)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "--mode" => args.mode = next()?,
            "--scenario" => args.scenario = next()?,
            "--full-scenario" => args.full_scenario = next()?,
            "--bots" => args.bots = next()?.parse().map_err(|_| "invalid --bots".to_string())?,
            "--lua-workers" => {
                args.lua_workers = next()?
                    .parse()
                    .map_err(|_| "invalid --lua-workers".to_string())?
            }
            "--event-rate" => args.event_rate = next()?,
            "--duration-secs" => {
                args.duration_secs = next()?
                    .parse()
                    .map_err(|_| "invalid --duration-secs".to_string())?
            }
            "--script" => args.script = next()?,
            "--seed" => args.seed = next()?.parse().map_err(|_| "invalid --seed".to_string())?,
            "--output" => args.output = Some(next()?),
            "--queue" => args.queue = next()?,
            "--share-chunks" => {
                args.share_chunks = next()?
                    .parse()
                    .map_err(|_| "invalid --share-chunks".to_string())?
            }
            "--chunk-count" => {
                args.chunk_count = next()?
                    .parse()
                    .map_err(|_| "invalid --chunk-count".to_string())?
            }
            "--reconnect-percent" => {
                args.reconnect_percent = next()?
                    .parse()
                    .map_err(|_| "invalid --reconnect-percent".to_string())?
            }
            "--proxy-group-size" => {
                args.proxy_group_size = next()?
                    .parse()
                    .map_err(|_| "invalid --proxy-group-size".to_string())?
            }
            "--memory-limit-mb" => {
                args.memory_limit_mb = next()?
                    .parse()
                    .map_err(|_| "invalid --memory-limit-mb".to_string())?
            }
            "--instruction-budget" => {
                args.instruction_budget = next()?
                    .parse()
                    .map_err(|_| "invalid --instruction-budget".to_string())?
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag: {other}")),
        }
        i += 1;
    }
    Ok(args)
}

fn print_help() {
    println!(
        "lua_runtime_benchmark --mode <rust-baseline|shared-lua|worker-pool|per-bot-lua> \
         --scenario <synthetic|full-runtime> [--full-scenario <idle|chunks|chunks-personalized|\
         realistic-state|reconnect-storm|proxy-groups>] --bots N --lua-workers N \
         --event-rate <low|typical|busy|burst|pathological[:N]> --duration-secs N \
         --script <no-op|light-state|realistic|table-work|slow-handler|infinite-loop> \
         --seed N [--output path.json] [--queue <fifo|priority>] [--share-chunks true|false] \
         [--chunk-count N] [--reconnect-percent N] [--proxy-group-size N] \
         [--memory-limit-mb N] [--instruction-budget N]"
    );
}

fn sandbox_config(args: &Args) -> SandboxConfig {
    SandboxConfig {
        memory_limit_bytes: args.memory_limit_mb * 1024 * 1024,
        instruction_budget: args.instruction_budget,
        ..SandboxConfig::default()
    }
}

fn queue_kind(args: &Args) -> QueueKind {
    match args.queue.as_str() {
        "priority" => QueueKind::Priority {
            high_capacity: 512,
            low_capacity: 256,
        },
        _ => QueueKind::Fifo { capacity: 2048 },
    }
}

fn architecture(args: &Args) -> Result<Architecture, String> {
    Architecture::parse(&args.mode, args.lua_workers, args.bots as usize)
        .ok_or_else(|| format!("unknown --mode {:?}", args.mode))
}

fn script(args: &Args) -> Result<ScriptKind, String> {
    ScriptKind::parse(&args.script).ok_or_else(|| format!("unknown --script {:?}", args.script))
}

#[derive(Serialize)]
struct Report {
    mode: String,
    scenario: String,
    script: String,
    bots: u32,
    lua_workers: usize,
    seed: u64,
    wall_time_ms: u128,
    rss_baseline_kib: Option<u64>,
    rss_after_start_kib: Option<u64>,
    rss_after_events_kib: Option<u64>,
    rss_after_shutdown_kib: Option<u64>,
    rss_after_cleanup_kib: Option<u64>,
    events_dispatched: u64,
    handler_executions: u64,
    handler_errors: u64,
    commands_emitted: u64,
    commands_dropped: u64,
    events_per_sec: f64,
    commands_per_sec: f64,
    enqueue_to_start_p50_us: u128,
    enqueue_to_start_p95_us: u128,
    enqueue_to_start_p99_us: u128,
    enqueue_to_complete_p50_us: u128,
    enqueue_to_complete_p95_us: u128,
    enqueue_to_complete_p99_us: u128,
    command_latency_p50_us: u128,
    command_latency_p95_us: u128,
    command_latency_p99_us: u128,
    queue_peak_depth_total: usize,
    queue_dropped_total: u64,
    bots_connected: Option<usize>,
    reconnects_observed: Option<u64>,
    commands_routed_to_supervisor: Option<u64>,
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            print_help();
            return ExitCode::FAILURE;
        }
    };

    let report = match args.scenario.as_str() {
        "synthetic" => match run_synthetic(&args) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        },
        "full-runtime" => {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("failed to start async runtime: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match runtime.block_on(run_full_runtime(&args)) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        other => {
            eprintln!("unknown --scenario {other:?}");
            return ExitCode::FAILURE;
        }
    };

    print_console_summary(&report);

    if let Some(path) = &args.output {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&report) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    eprintln!("failed to write {path}: {e}");
                    return ExitCode::FAILURE;
                }
                println!("wrote {path}");
            }
            Err(e) => {
                eprintln!("failed to serialize report: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}

fn run_synthetic(args: &Args) -> Result<Report, String> {
    let arch = architecture(args)?;
    let script_kind = script(args)?;
    let rate = EventRateProfile::parse(&args.event_rate)
        .ok_or_else(|| format!("unknown --event-rate {:?}", args.event_rate))?;

    let config = SyntheticConfig {
        bot_count: args.bots,
        rate,
        duration: Duration::from_secs(args.duration_secs),
        seed: args.seed,
    };
    let result = run_synthetic_scenario(
        config,
        arch,
        queue_kind(args),
        sandbox_config(args),
        script_kind.source(),
        script_kind.name(),
        4096,
        Duration::from_millis(200),
    );

    Ok(Report {
        mode: result.architecture_label.clone(),
        scenario: "synthetic".to_string(),
        script: result.script_label.to_string(),
        bots: result.bot_count,
        lua_workers: args.lua_workers,
        seed: args.seed,
        wall_time_ms: result.wall_time.as_millis(),
        rss_baseline_kib: result.rss.baseline,
        rss_after_start_kib: result.rss.after_dispatcher_start,
        rss_after_events_kib: result.rss.after_events,
        rss_after_shutdown_kib: result.rss.after_shutdown,
        rss_after_cleanup_kib: result.rss.after_cleanup_wait,
        events_dispatched: result.throughput.events_dispatched,
        handler_executions: result.throughput.handler_executions,
        handler_errors: result.throughput.handler_errors,
        commands_emitted: result.command_sink_emitted,
        commands_dropped: result.command_sink_dropped,
        events_per_sec: result.throughput_rates.events_per_sec,
        commands_per_sec: result.throughput_rates.commands_per_sec,
        enqueue_to_start_p50_us: result.enqueue_to_start.p50.as_micros(),
        enqueue_to_start_p95_us: result.enqueue_to_start.p95.as_micros(),
        enqueue_to_start_p99_us: result.enqueue_to_start.p99.as_micros(),
        enqueue_to_complete_p50_us: result.enqueue_to_complete.p50.as_micros(),
        enqueue_to_complete_p95_us: result.enqueue_to_complete.p95.as_micros(),
        enqueue_to_complete_p99_us: result.enqueue_to_complete.p99.as_micros(),
        command_latency_p50_us: result.command_latency.p50.as_micros(),
        command_latency_p95_us: result.command_latency.p95.as_micros(),
        command_latency_p99_us: result.command_latency.p99.as_micros(),
        queue_peak_depth_total: result.queue_peak_depth_total,
        queue_dropped_total: result.queue_dropped_total,
        bots_connected: None,
        reconnects_observed: None,
        commands_routed_to_supervisor: None,
    })
}

async fn run_full_runtime(args: &Args) -> Result<Report, String> {
    let arch = architecture(args)?;
    let script_kind = script(args)?;

    let (scenario, _proxy_guard): (ScenarioKind, Option<FakeSocks5Server>) =
        match args.full_scenario.as_str() {
            "idle" => (ScenarioKind::Idle, None),
            "chunks" => (
                ScenarioKind::Chunks {
                    chunk_count: args.chunk_count,
                    personalize: false,
                },
                None,
            ),
            "chunks-personalized" => (
                ScenarioKind::Chunks {
                    chunk_count: args.chunk_count,
                    personalize: true,
                },
                None,
            ),
            "realistic-state" => (ScenarioKind::RealisticState, None),
            "reconnect-storm" => (
                ScenarioKind::ReconnectStorm {
                    percent: args.reconnect_percent,
                },
                None,
            ),
            "proxy-groups" => (ScenarioKind::Idle, None),
            other => return Err(format!("unknown --full-scenario {other:?}")),
        };

    let proxy_for: Arc<dyn Fn(u32) -> Option<Arc<Socks5ProxyConfig>> + Send + Sync> =
        if args.full_scenario == "proxy-groups" {
            let group_size = args.proxy_group_size.max(1);
            let server_count = (args.bots / group_size).clamp(1, 8);
            let mut servers = Vec::with_capacity(server_count as usize);
            for _ in 0..server_count {
                servers.push(FakeSocks5Server::start((args.bots + 1) as usize).await);
            }
            let configs: Vec<Arc<Socks5ProxyConfig>> = servers
                .iter()
                .map(|s| Arc::new(Socks5ProxyConfig::new("127.0.0.1", s.port)))
                .collect();
            std::mem::forget(servers); // kept alive for the process lifetime of this run
            Arc::new(move |bot| {
                let index = (bot / group_size) as usize % configs.len().max(1);
                configs.get(index).cloned()
            })
        } else {
            Arc::new(|_bot| None)
        };

    let config = SwarmConfig {
        bot_count: args.bots,
        scenario,
        share_chunk_payloads: args.share_chunks,
        architecture: arch,
        script_body: script_kind.source(),
        sandbox_config: sandbox_config(args),
        proxy_for,
        settle_timeout: Duration::from_secs(30),
        run_duration: Duration::from_secs(args.duration_secs),
        cleanup_wait: Duration::from_millis(300),
    };

    let result = run_full_runtime_scenario(config).await;

    Ok(Report {
        mode: arch.label(),
        scenario: format!("full-runtime:{}", args.full_scenario),
        script: script_kind.name().to_string(),
        bots: result.bot_count,
        lua_workers: args.lua_workers,
        seed: args.seed,
        wall_time_ms: result.wall_time.as_millis(),
        rss_baseline_kib: result.rss_baseline,
        rss_after_start_kib: result.rss_after_connect,
        rss_after_events_kib: result.rss_after_scenario,
        rss_after_shutdown_kib: result.rss_after_shutdown,
        rss_after_cleanup_kib: result.rss_after_cleanup_wait,
        events_dispatched: result.events_dispatched,
        handler_executions: result.handler_executions,
        handler_errors: result.handler_errors,
        commands_emitted: result.commands_emitted,
        commands_dropped: result.commands_dropped,
        events_per_sec: result.events_dispatched as f64 / result.wall_time.as_secs_f64().max(1e-9),
        commands_per_sec: result.commands_emitted as f64 / result.wall_time.as_secs_f64().max(1e-9),
        enqueue_to_start_p50_us: result.enqueue_to_start.p50.as_micros(),
        enqueue_to_start_p95_us: result.enqueue_to_start.p95.as_micros(),
        enqueue_to_start_p99_us: result.enqueue_to_start.p99.as_micros(),
        enqueue_to_complete_p50_us: result.enqueue_to_complete.p50.as_micros(),
        enqueue_to_complete_p95_us: result.enqueue_to_complete.p95.as_micros(),
        enqueue_to_complete_p99_us: result.enqueue_to_complete.p99.as_micros(),
        command_latency_p50_us: result.command_latency.p50.as_micros(),
        command_latency_p95_us: result.command_latency.p95.as_micros(),
        command_latency_p99_us: result.command_latency.p99.as_micros(),
        queue_peak_depth_total: result.queue_peak_depth_total,
        queue_dropped_total: result.queue_dropped_total,
        bots_connected: Some(result.bots_connected),
        reconnects_observed: Some(result.reconnects_observed),
        commands_routed_to_supervisor: Some(result.commands_routed_to_supervisor),
    })
}

fn print_console_summary(report: &Report) {
    println!("=== lua_runtime_benchmark result ===");
    println!(
        "mode={} scenario={} script={}",
        report.mode, report.scenario, report.script
    );
    println!(
        "bots={} lua_workers={} seed={}",
        report.bots, report.lua_workers, report.seed
    );
    println!("wall_time_ms={}", report.wall_time_ms);
    println!(
        "rss_kib baseline={:?} after_start={:?} after_events={:?} after_shutdown={:?} after_cleanup={:?}",
        report.rss_baseline_kib,
        report.rss_after_start_kib,
        report.rss_after_events_kib,
        report.rss_after_shutdown_kib,
        report.rss_after_cleanup_kib,
    );
    println!(
        "events_dispatched={} handler_executions={} handler_errors={} commands_emitted={} commands_dropped={}",
        report.events_dispatched,
        report.handler_executions,
        report.handler_errors,
        report.commands_emitted,
        report.commands_dropped,
    );
    println!(
        "events_per_sec={:.1} commands_per_sec={:.1}",
        report.events_per_sec, report.commands_per_sec
    );
    println!(
        "enqueue_to_start_us p50={} p95={} p99={}",
        report.enqueue_to_start_p50_us,
        report.enqueue_to_start_p95_us,
        report.enqueue_to_start_p99_us
    );
    println!(
        "enqueue_to_complete_us p50={} p95={} p99={}",
        report.enqueue_to_complete_p50_us,
        report.enqueue_to_complete_p95_us,
        report.enqueue_to_complete_p99_us
    );
    println!(
        "queue_peak_depth_total={} queue_dropped_total={}",
        report.queue_peak_depth_total, report.queue_dropped_total
    );
    if let Some(connected) = report.bots_connected {
        println!(
            "bots_connected={connected} reconnects_observed={:?} commands_routed_to_supervisor={:?}",
            report.reconnects_observed, report.commands_routed_to_supervisor
        );
    }
}
