//! Production Lua-scripted swarm runner.
//!
//! ```text
//! cargo run --release --features lua --bin minerider-lua -- scripts/swarm.lua
//! cargo run --release --features lua --bin minerider-lua -- \
//!     --script examples/lua/swarm.lua --lua-workers 4 --log-level info --shutdown-timeout-secs 10
//! ```
//!
//! See `docs/lua_wrapper.md` for the architecture and
//! `docs/lua_api_reference.md` for the scripting API. The sandbox never
//! grants Lua filesystem access — the script file is selected here, by the
//! host CLI, not by the script itself.

use std::process::ExitCode;
use std::time::Duration;

use minerider::lua::runtime::{run_swarm, SwarmRuntimeConfig, DEFAULT_WORKER_COUNT};
use minerider::lua::sandbox::SandboxConfig;

struct Args {
    script: String,
    lua_workers: usize,
    log_level: String,
    shutdown_timeout_secs: u64,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            script: String::new(),
            lua_workers: DEFAULT_WORKER_COUNT,
            log_level: "info".to_string(),
            shutdown_timeout_secs: 10,
        }
    }
}

/// Exit statuses: distinct codes for configuration/script-init failures vs.
/// a clean run, matching the mission's "distinct exit statuses for config
/// errors vs. script-init errors vs. runtime handler errors" requirement.
/// Handler errors inside a running script never reach this process's exit
/// code at all — see `crate::lua::worker`'s per-bot consecutive-error
/// disabling, which keeps the process (and every other bot) alive.
mod exit {
    pub const OK: u8 = 0;
    pub const USAGE: u8 = 1;
    pub const CONFIG_ERROR: u8 = 2;
    pub const SCRIPT_INIT_ERROR: u8 = 3;
}

fn print_usage() {
    eprintln!(
        r#"minerider-lua: run a Lua-scripted Minecraft bot swarm

USAGE:
    minerider-lua <script.lua> [OPTIONS]
    minerider-lua --script <script.lua> [OPTIONS]

OPTIONS:
    --script <path>              Lua script to run (also accepted as the first positional arg)
    --lua-workers <N>             Persistent Lua worker count (default: {DEFAULT_WORKER_COUNT}; does not scale with bot count)
    --log-level <level>           trace|debug|info|warn|error (default: info)
    --shutdown-timeout-secs <N>   Bound on graceful shutdown after Ctrl+C (default: 10)
    -h, --help                    Print this help and exit
"#
    );
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut argv = std::env::args().skip(1).peekable();
    let mut positional_script: Option<String> = None;

    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(exit::OK as i32);
            }
            "--script" => {
                args.script = argv.next().ok_or("--script requires a value")?;
            }
            "--lua-workers" => {
                let v = argv.next().ok_or("--lua-workers requires a value")?;
                args.lua_workers = v
                    .parse()
                    .map_err(|_| format!("invalid --lua-workers value: {v}"))?;
            }
            "--log-level" => {
                args.log_level = argv.next().ok_or("--log-level requires a value")?;
            }
            "--shutdown-timeout-secs" => {
                let v = argv
                    .next()
                    .ok_or("--shutdown-timeout-secs requires a value")?;
                args.shutdown_timeout_secs = v
                    .parse()
                    .map_err(|_| format!("invalid --shutdown-timeout-secs value: {v}"))?;
            }
            other if !other.starts_with('-') && positional_script.is_none() => {
                positional_script = Some(other.to_string());
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    if args.script.is_empty() {
        args.script =
            positional_script.ok_or("a script path is required (positional or --script)")?;
    }
    if args.lua_workers == 0 {
        return Err("--lua-workers must be at least 1".to_string());
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            print_usage();
            return ExitCode::from(exit::USAGE);
        }
    };

    let filter = tracing_subscriber::EnvFilter::try_new(&args.log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let script_body = match std::fs::read_to_string(&args.script) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: could not read script `{}`: {e}", args.script);
            return ExitCode::from(exit::CONFIG_ERROR);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: could not start the tokio runtime: {e}");
            return ExitCode::from(exit::CONFIG_ERROR);
        }
    };

    runtime.block_on(async_main(args, script_body))
}

async fn async_main(args: Args, script_body: String) -> ExitCode {
    let config = SwarmRuntimeConfig {
        worker_count: args.lua_workers,
        sandbox: SandboxConfig::default(),
        script_body,
        ..SwarmRuntimeConfig::default()
    };

    tracing::info!(
        workers = config.worker_count,
        script = %args.script,
        "starting swarm"
    );

    let swarm = match run_swarm(config).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: swarm failed to start: {e}");
            return ExitCode::from(exit::SCRIPT_INIT_ERROR);
        }
    };

    tracing::info!(
        bots = swarm.registry.bots.len(),
        "swarm running; press Ctrl+C to stop"
    );

    // First Ctrl+C: graceful shutdown. Second: force exit immediately —
    // a hung shutdown must never require killing the process externally.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Ctrl+C received; shutting down gracefully");
        }
    }

    let shutdown_timeout = Duration::from_secs(args.shutdown_timeout_secs);
    tokio::select! {
        _ = swarm.shutdown(shutdown_timeout) => {
            tracing::info!("shutdown complete");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::warn!("second Ctrl+C received; forcing exit without waiting for shutdown");
            return ExitCode::from(exit::OK);
        }
    }

    ExitCode::from(exit::OK)
}
