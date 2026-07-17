//! MineRider CLI entry point.
//!
//! Usage: `minerider <host> <port> <username>`
//!
//! **Only connect to servers you own or are explicitly authorized to use.**
//! `MINERIDER_RECONNECT` below runs an unattended, potentially long-lived
//! client; it is meant for authorized monitoring, QA/compatibility testing,
//! and load testing — not for evading AFK-kicks, bans, or any other
//! server-side protection.
//!
//! Premium (online-mode) login: set `MINERIDER_PREMIUM=1` to sign in with a
//! real Microsoft account instead of connecting offline-mode. No app
//! registration needed — see [`minerider::auth::MicrosoftAuthenticator`] for
//! why. Optionally set `MINERIDER_MS_TITLE_ID` to authenticate as a
//! different first-party title than the default (see
//! [`minerider::auth::live::titles`]). The device-code sign-in only happens
//! once: the refresh token is cached in `.minerider_msa_cache.json` (never
//! printed to the terminal — a live credential has no business passing
//! through a shell) and reused automatically on every later run, falling
//! back to a fresh device-code prompt only if the cache is missing or the
//! cached token has stopped working.
//!
//! Reliability/reconnect (see [`minerider::core::supervisor`]):
//! - `MINERIDER_WRITE_TIMEOUT_SECS=<n>` — bound on a complete packet send.
//! - `MINERIDER_CONNECT_DEADLINE_SECS=<n>` — overall TCP-connect-to-play budget.
//! - `MINERIDER_RECONNECT=1` — enable reconnect-with-backoff after the
//!   connection ends (disabled by default: a bare `minerider` run is a
//!   one-shot connection exactly as before). Never retries after an
//!   explicit server rejection or a permanent auth/protocol error.
//! - `MINERIDER_MAX_RETRIES=<n|unlimited>` — retry limit (default: 5).
//! - `MINERIDER_INITIAL_BACKOFF_MS=<n>` / `MINERIDER_MAX_BACKOFF_MS=<n>` —
//!   backoff shape (defaults: 1000 / 60000).
//!
//! `MINERIDER_TRACE` and the `MINERIDER_WALK_TO`/`MINERIDER_CHAT` manual test
//! hooks are one-shot-`Client`-only and are not available in reconnect mode
//! in this milestone (the supervisor does not yet forward a control handle
//! for the currently active session).

use std::process::ExitCode;
use std::time::Duration;

use minerider::auth::MicrosoftAuthenticator;
use minerider::core::client::{Client, ClientConfig};
use minerider::core::error::MineRiderError;
use minerider::core::supervisor::{
    ClientSupervisor, ReconnectPolicy, RetryLimit, SupervisorOutcome,
};
use tracing_subscriber::EnvFilter;

/// Where the Microsoft refresh token is cached between runs. Relative to the
/// current directory, gitignored — this file holds a live credential.
const TOKEN_CACHE_PATH: &str = ".minerider_msa_cache.json";

fn load_cached_refresh_token() -> Option<String> {
    let contents = std::fs::read_to_string(TOKEN_CACHE_PATH).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    value
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn save_refresh_token(token: &str) {
    let contents = serde_json::json!({ "refresh_token": token }).to_string();
    if let Err(e) = std::fs::write(TOKEN_CACHE_PATH, contents) {
        eprintln!("warning: could not cache sign-in to {TOKEN_CACHE_PATH}: {e}");
    }
}

/// Parses `MINERIDER_WALK_TO`'s `"x,z"` format.
fn parse_walk_target(value: &str) -> Option<(f64, f64)> {
    let (x, z) = value.split_once(',')?;
    Some((x.trim().parse().ok()?, z.trim().parse().ok()?))
}

/// `true` iff the named environment variable is set to anything other than
/// `"0"` (matching this CLI's existing `MINERIDER_PREMIUM` convention).
fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v != "0")
}

fn env_duration_secs(name: &str) -> Result<Option<Duration>, String> {
    match std::env::var(name) {
        Ok(raw) => raw
            .parse::<u64>()
            .map(|s| Some(Duration::from_secs(s)))
            .map_err(|_| format!("invalid {name} {raw:?}, expected an integer number of seconds")),
        Err(_) => Ok(None),
    }
}

fn env_duration_millis(name: &str) -> Result<Option<Duration>, String> {
    match std::env::var(name) {
        Ok(raw) => raw
            .parse::<u64>()
            .map(|ms| Some(Duration::from_millis(ms)))
            .map_err(|_| {
                format!("invalid {name} {raw:?}, expected an integer number of milliseconds")
            }),
        Err(_) => Ok(None),
    }
}

/// Parses `MINERIDER_MAX_RETRIES`: a non-negative integer, or `"unlimited"`.
fn env_retry_limit(name: &str) -> Result<Option<RetryLimit>, String> {
    match std::env::var(name) {
        Ok(raw) if raw.eq_ignore_ascii_case("unlimited") => Ok(Some(RetryLimit::Unlimited)),
        Ok(raw) => raw
            .parse::<u32>()
            .map(|n| Some(RetryLimit::Count(n)))
            .map_err(|_| format!("invalid {name} {raw:?}, expected an integer or \"unlimited\"")),
        Err(_) => Ok(None),
    }
}

/// Runs Microsoft/Xbox Live sign-in: reuses the cached refresh token if one
/// exists and still works, otherwise runs the device-code flow and prints
/// the user code/URL to sign in with. The (possibly new) refresh token is
/// cached for next time either way.
async fn premium_sign_in() -> minerider::core::error::Result<minerider::auth::PremiumSession> {
    let authenticator = match std::env::var("MINERIDER_MS_TITLE_ID") {
        Ok(title) => MicrosoftAuthenticator::with_title(title),
        Err(_) => MicrosoftAuthenticator::new(),
    };

    if let Some(cached) = load_cached_refresh_token() {
        match authenticator.resume(&cached).await {
            Ok(session) => {
                save_refresh_token(&session.refresh_token);
                return Ok(session);
            }
            Err(e) => {
                eprintln!("cached sign-in no longer works ({e}); signing in again");
            }
        }
    }

    let session = authenticator
        .sign_in(|device| eprintln!("{}", device.message()))
        .await?;
    save_refresh_token(&session.refresh_token);
    Ok(session)
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!(
            "usage: {} <host> <port> <username>",
            args.first().map(String::as_str).unwrap_or("minerider")
        );
        return ExitCode::FAILURE;
    }
    let host = &args[1];
    let port: u16 = match args[2].parse() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("invalid port: {}", args[2]);
            return ExitCode::FAILURE;
        }
    };
    let username = &args[3];

    let mut cfg = ClientConfig::new(host.clone(), port, username.clone());
    // Optional: MINERIDER_VIEW_DISTANCE=<chunks> lowers the render distance
    // sent in client_information, an ordinary client setting that directly
    // bounds how many chunks the server streams (and this client stores) —
    // the main per-bot memory lever when running many bots on one machine.
    if let Ok(raw) = std::env::var("MINERIDER_VIEW_DISTANCE") {
        match raw.parse::<i8>() {
            Ok(view_distance) => cfg = cfg.with_view_distance(view_distance),
            Err(_) => {
                eprintln!("invalid MINERIDER_VIEW_DISTANCE {raw:?}, expected an integer");
                return ExitCode::FAILURE;
            }
        }
    }
    match env_duration_secs("MINERIDER_WRITE_TIMEOUT_SECS") {
        Ok(Some(d)) => cfg = cfg.with_write_timeout(d),
        Ok(None) => {}
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    }
    match env_duration_secs("MINERIDER_CONNECT_DEADLINE_SECS") {
        Ok(Some(d)) => cfg = cfg.with_connect_deadline(d),
        Ok(None) => {}
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    }

    let reconnect_enabled = env_flag("MINERIDER_RECONNECT");
    let mut reconnect_policy = ReconnectPolicy::enabled();
    if reconnect_enabled {
        match env_retry_limit("MINERIDER_MAX_RETRIES") {
            Ok(Some(limit)) => reconnect_policy = reconnect_policy.with_max_retries(limit),
            Ok(None) => {}
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        }
        match env_duration_millis("MINERIDER_INITIAL_BACKOFF_MS") {
            Ok(Some(d)) => reconnect_policy = reconnect_policy.with_initial_delay(d),
            Ok(None) => {}
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        }
        match env_duration_millis("MINERIDER_MAX_BACKOFF_MS") {
            Ok(Some(d)) => reconnect_policy = reconnect_policy.with_max_delay(d),
            Ok(None) => {}
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // Optional trace capture: MINERIDER_TRACE=<path> records every packet
    // in both directions (used by the real-server validation scripts).
    let trace = match std::env::var("MINERIDER_TRACE") {
        Ok(path) => {
            let scenario =
                std::env::var("MINERIDER_TRACE_SCENARIO").unwrap_or_else(|_| "cli".to_string());
            match minerider::trace::TraceRecorder::create(&path, "SESSION_1", scenario) {
                Ok(recorder) => {
                    eprintln!("recording packet trace to {path}");
                    Some(recorder)
                }
                Err(e) => {
                    eprintln!("cannot create trace file {path}: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        Err(_) => None,
    };
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

    runtime.block_on(async move {
        let mut cfg = cfg;
        if std::env::var("MINERIDER_PREMIUM").is_ok_and(|v| v != "0") {
            match premium_sign_in().await {
                Ok(session) => {
                    eprintln!(
                        "signed in as {} (uuid {:032x}); cached to {TOKEN_CACHE_PATH} for next run",
                        session.username, session.uuid
                    );
                    cfg = cfg.with_premium(session);
                }
                Err(e) => {
                    eprintln!("premium sign-in failed: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }

        if reconnect_enabled {
            if trace.is_some() {
                eprintln!(
                    "MINERIDER_TRACE is not supported together with MINERIDER_RECONNECT; ignoring the trace"
                );
            }
            let (supervisor, handle) = ClientSupervisor::new(cfg, reconnect_policy);
            let mut events = handle.events();
            tokio::spawn(async move {
                while let Ok(event) = events.recv().await {
                    println!("[lifecycle] {event:?}");
                }
            });
            return match supervisor.run().await {
                SupervisorOutcome::Cancelled => ExitCode::SUCCESS,
                SupervisorOutcome::RetriesExhausted { last_error } => {
                    eprintln!("retries exhausted: {last_error}");
                    ExitCode::FAILURE
                }
                SupervisorOutcome::NotRetried { reason } => {
                    eprintln!("stopped (not retried): {reason}");
                    ExitCode::FAILURE
                }
            };
        }

        let connect = match trace {
            Some(recorder) => Client::connect_with_trace(&cfg, recorder).await,
            None => Client::connect(&cfg).await,
        };
        let mut client = match connect {
            Ok(c) => c,
            Err(e) => {
                eprintln!("connect failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        println!("reached PLAY state as {}", client.username);

        // Optional manual test hook: MINERIDER_WALK_TO="x,z" drives the bot
        // to a horizontal target through Controller::walk_to once it has
        // loaded, straight-line steering only (no obstacle avoidance).
        if let Ok(target) = std::env::var("MINERIDER_WALK_TO") {
            match parse_walk_target(&target) {
                Some((x, z)) => {
                    let control = client.control();
                    tokio::spawn(async move {
                        // Give the readiness gate (player_loaded) time to
                        // complete before issuing the goal.
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        println!("walk_to({x}, {z})");
                        let _ = control.walk_to(x, z);
                    });
                }
                None => eprintln!("invalid MINERIDER_WALK_TO {target:?}, expected \"x,z\""),
            }
        }

        // Optional manual test hook: MINERIDER_CHAT="text" sends one chat
        // message (or a command, if it starts with `/`) shortly after join.
        if let Ok(message) = std::env::var("MINERIDER_CHAT") {
            let control = client.control();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                println!("chat: {message}");
                let _ = control.chat(message);
            });
        }

        match client.run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(MineRiderError::Disconnected(reason)) => {
                println!("disconnected by server: {reason}");
                ExitCode::SUCCESS
            }
            Err(MineRiderError::ConnectionClosed) => {
                println!("connection closed by server");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        }
    })
}
