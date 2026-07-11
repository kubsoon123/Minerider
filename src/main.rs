//! MineRider CLI entry point.
//!
//! Usage: `minerider <host> <port> <username>`

use std::process::ExitCode;

use minerider::core::client::{Client, ClientConfig};
use minerider::core::error::MineRiderError;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: {} <host> <port> <username>", args.first().map(String::as_str).unwrap_or("minerider"));
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

    let cfg = ClientConfig::new(host.clone(), port, username.clone());
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(async move {
        let mut client = match Client::connect(&cfg).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("connect failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        println!("reached PLAY state as {}", client.username);
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
