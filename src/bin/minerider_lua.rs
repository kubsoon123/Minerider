//! Production Lua-scripted swarm runner.
//!
//! ```text
//! cargo run --release --features lua --bin minerider-lua -- scripts/swarm.lua
//! cargo run --release --features lua --bin minerider-lua -- \
//!     --script examples/lua/swarm.lua --lua-workers 4 --log-level info --shutdown-timeout-secs 10 \
//!     --proxy-profile proxy1=PROXY1 --proxy-profile proxy2=PROXY2
//! ```
//!
//! See `docs/lua_wrapper.md` for the architecture and
//! `docs/lua_api_reference.md` for the scripting API. The sandbox never
//! grants Lua filesystem access — the script file is selected here, by the
//! host CLI, not by the script itself.
//!
//! `--proxy-profile <id>=<ENV_PREFIX>` (repeatable) is the *only* way a
//! script's `proxy = "<id>"` references resolve to anything: argv carries
//! only the profile id and an environment-variable-name *prefix*, never a
//! secret. The actual host/port/username/password are read from
//! `{PREFIX}_HOST`/`_PORT`/`_USERNAME`/`_PASSWORD` (see
//! `crate::lua::runtime::proxy_profiles_from_env`) — set those in your
//! shell/CI secret store, never on this command line.

use std::process::ExitCode;
use std::time::Duration;

use minerider::lua::runtime::{
    proxy_profiles_from_env, run_swarm, SwarmRuntimeConfig, DEFAULT_WORKER_COUNT,
};
use minerider::lua::sandbox::SandboxConfig;

#[derive(Debug)]
struct Args {
    script: String,
    lua_workers: usize,
    log_level: String,
    shutdown_timeout_secs: u64,
    /// `(profile_id, env_prefix)` pairs from repeated `--proxy-profile`
    /// flags — never a credential, only an id and a variable-name prefix.
    proxy_profiles: Vec<(String, String)>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            script: String::new(),
            lua_workers: DEFAULT_WORKER_COUNT,
            log_level: "info".to_string(),
            shutdown_timeout_secs: 10,
            proxy_profiles: Vec::new(),
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
    --proxy-profile <id>=<PREFIX> Register a proxy profile the script may reference as `proxy = "<id>"`.
                                  Credentials are read from {{PREFIX}}_HOST/_PORT/_USERNAME/_PASSWORD
                                  environment variables — never from this flag's value itself, and
                                  never from the script. Repeatable.
    -h, --help                    Print this help and exit
"#
    );
}

fn parse_args() -> Result<Args, String> {
    parse_args_from(std::env::args().skip(1))
}

/// Parses from an arbitrary argument iterator (not directly `std::env::args`)
/// so the parsing logic itself is unit-testable without touching real
/// process argv.
fn parse_args_from(argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = Args::default();
    let mut argv = argv.peekable();
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
            "--proxy-profile" => {
                let v = argv.next().ok_or("--proxy-profile requires a value")?;
                let (id, prefix) = v.split_once('=').ok_or_else(|| {
                    format!("invalid --proxy-profile value `{v}`: expected `<id>=<ENV_PREFIX>`")
                })?;
                if id.is_empty() || prefix.is_empty() {
                    return Err(format!(
                        "invalid --proxy-profile value `{v}`: both <id> and <ENV_PREFIX> must be non-empty"
                    ));
                }
                args.proxy_profiles
                    .push((id.to_string(), prefix.to_string()));
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

    // Resolved here, synchronously, from environment variables only —
    // never from argv (argv only ever carried `id=ENV_PREFIX` pairs, see
    // --proxy-profile above) and never touched by the script.
    let proxy_profiles = match proxy_profiles_from_env(args.proxy_profiles.iter().cloned()) {
        Ok(profiles) => profiles,
        Err(e) => {
            eprintln!("error: proxy profile configuration failed: {e}");
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

    runtime.block_on(async_main(args, script_body, proxy_profiles))
}

async fn async_main(
    args: Args,
    script_body: String,
    proxy_profiles: minerider::lua::registry::ProxyProfiles,
) -> ExitCode {
    let config = SwarmRuntimeConfig {
        worker_count: args.lua_workers,
        sandbox: SandboxConfig::default(),
        script_body,
        proxy_profiles: std::sync::Arc::new(proxy_profiles),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(argv: &[&str]) -> Result<Args, String> {
        parse_args_from(argv.iter().map(|s| s.to_string()))
    }

    #[test]
    fn positional_script_is_accepted() {
        let a = args(&["swarm.lua"]).unwrap();
        assert_eq!(a.script, "swarm.lua");
        assert_eq!(a.lua_workers, DEFAULT_WORKER_COUNT);
        assert_eq!(a.log_level, "info");
        assert_eq!(a.shutdown_timeout_secs, 10);
    }

    #[test]
    fn explicit_script_flag_is_accepted() {
        let a = args(&["--script", "swarm.lua"]).unwrap();
        assert_eq!(a.script, "swarm.lua");
    }

    #[test]
    fn all_flags_override_defaults() {
        let a = args(&[
            "--script",
            "swarm.lua",
            "--lua-workers",
            "8",
            "--log-level",
            "debug",
            "--shutdown-timeout-secs",
            "30",
        ])
        .unwrap();
        assert_eq!(a.lua_workers, 8);
        assert_eq!(a.log_level, "debug");
        assert_eq!(a.shutdown_timeout_secs, 30);
    }

    #[test]
    fn missing_script_is_an_error() {
        assert!(args(&[]).is_err());
    }

    #[test]
    fn zero_workers_is_rejected() {
        let err = args(&["swarm.lua", "--lua-workers", "0"]).unwrap_err();
        assert!(err.contains("--lua-workers"));
    }

    #[test]
    fn unrecognized_flag_is_an_error() {
        assert!(args(&["swarm.lua", "--not-a-real-flag"]).is_err());
    }

    #[test]
    fn missing_value_for_a_flag_is_an_error() {
        assert!(args(&["--script"]).is_err());
        assert!(args(&["swarm.lua", "--lua-workers"]).is_err());
    }

    #[test]
    fn invalid_numeric_value_is_an_error() {
        assert!(args(&["swarm.lua", "--lua-workers", "not-a-number"]).is_err());
        assert!(args(&["swarm.lua", "--shutdown-timeout-secs", "not-a-number"]).is_err());
    }

    #[test]
    fn proxy_profile_flag_parses_id_and_env_prefix() {
        let a = args(&[
            "swarm.lua",
            "--proxy-profile",
            "proxy1=PROXY1",
            "--proxy-profile",
            "proxy2=PROXY2",
        ])
        .unwrap();
        assert_eq!(
            a.proxy_profiles,
            vec![
                ("proxy1".to_string(), "PROXY1".to_string()),
                ("proxy2".to_string(), "PROXY2".to_string()),
            ]
        );
    }

    #[test]
    fn proxy_profile_flag_never_accepts_a_bare_password() {
        // No `=` at all: rejected, rather than silently treating the whole
        // value as an id with an empty prefix (which could otherwise be a
        // sneaky way to slip a secret onto argv under a permissive parser).
        assert!(args(&[
            "swarm.lua",
            "--proxy-profile",
            "just-a-secret-looking-string"
        ])
        .is_err());
        assert!(args(&["swarm.lua", "--proxy-profile", "=PROXY1"]).is_err());
        assert!(args(&["swarm.lua", "--proxy-profile", "proxy1="]).is_err());
    }
}
