//! Runs many independent bots concurrently in a single OS process, to
//! measure and demonstrate the memory/CPU savings of sharing one process
//! (one runtime, one set of statically-shared tables like collision data)
//! instead of paying each bot's fixed process overhead separately.
//!
//! Usage: `minerider-swarm <host> <port> <count> [username_prefix]`
//!
//! Each bot is an entirely independent `Client` — its own connection, own
//! world/chunk cache, own physics — this only removes the *process*-level
//! duplication (OS process overhead, binary/runtime pages, the collision
//! table, which is a Rust `static` and was already shared within a process).
//! Bots sharing chunk data for the same server region is a further, separate
//! optimization this does not attempt.

use std::process::ExitCode;
use std::time::Duration;

use minerider::core::client::{Client, ClientConfig};
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "usage: {} <host> <port> <count> [username_prefix]",
            args.first().map(String::as_str).unwrap_or("swarm")
        );
        return ExitCode::FAILURE;
    }
    let host = args[1].clone();
    let port: u16 = match args[2].parse() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("invalid port: {}", args[2]);
            return ExitCode::FAILURE;
        }
    };
    let count: usize = match args[3].parse() {
        Ok(n) => n,
        Err(_) => {
            eprintln!("invalid count: {}", args[3]);
            return ExitCode::FAILURE;
        }
    };
    let prefix = args.get(4).cloned().unwrap_or_else(|| "Swarm".to_string());

    // Same memory lever as the single-bot CLI: a lower render distance
    // means less chunk data per bot.
    let view_distance: i8 = std::env::var("MINERIDER_VIEW_DISTANCE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(minerider::minecraft::DEFAULT_VIEW_DISTANCE);

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
        let mut handles = Vec::with_capacity(count);
        for i in 0..count {
            let host = host.clone();
            let username = format!("{prefix}{i}");
            handles.push(tokio::spawn(async move {
                let cfg = ClientConfig::new(host, port, username.clone())
                    .with_view_distance(view_distance);
                let mut client = match Client::connect(&cfg).await {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[{username}] connect failed: {e}");
                        return;
                    }
                };
                println!("[{username}] reached PLAY state");
                if let Err(e) = client.run().await {
                    eprintln!("[{username}] ended: {e}");
                }
            }));
            // Stagger joins so N simultaneous handshakes don't hit the
            // server (or its own rate limiting) all at once — a real
            // server also sees real players trickle in, not arrive as one
            // burst.
            tokio::time::sleep(Duration::from_millis(75)).await;
        }

        println!("all {count} bots launched; running until Ctrl+C");
        for handle in handles {
            let _ = handle.await;
        }
    });

    ExitCode::SUCCESS
}
