//! Local performance report for the SOCKS5 transport: direct vs.
//! SOCKS5-loopback connection setup latency, raw negotiation latency,
//! concurrency scaling (1/10/100 tunnels), retained memory per tunnel, and
//! task cleanup after disconnect.
//!
//! Run with:
//!
//! cargo run --release --bin socks5_benchmark
//!
//! Every measurement is against `127.0.0.1`: a real Tokio runtime, real
//! sockets, an in-process mock Minecraft backend and an in-process minimal
//! SOCKS5 relay (independent of `tests/socks5_support`, same rationale
//! `src/bin/full_runtime_benchmark.rs` documents for not sharing code with
//! `tests/common`). This never contacts a real proxy or Minecraft server —
//! see docs/socks5_benchmark.md for why public-proxy numbers cannot be
//! claimed from loopback results.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use minerider::core::client::{Client, ClientConfig};
use minerider::network::connection::Connection;
use minerider::network::socks5::{self, Socks5ProxyConfig};
use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::generated::v1_21_4::{configuration, handshaking, login};
use tokio::net::{TcpListener, TcpStream};

const LATENCY_SAMPLES: usize = 25;
const CONCURRENCY_TIERS: [usize; 3] = [1, 10, 100];

fn main() -> ExitCode {
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
    runtime.block_on(run())
}

async fn run() -> ExitCode {
    println!(
        "ENV,os={},arch={}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("NOTE,all measurements are 127.0.0.1 loopback; see docs/socks5_benchmark.md for what this does and does not prove");

    // --- 1. Connection setup latency: direct vs SOCKS5 loopback ---
    // One shared backend serves both phases below (direct connections, then
    // SOCKS5-routed connections), so its accept budget must cover both.
    let backend = MinecraftBackend::start(LATENCY_SAMPLES * 2 + 2).await;
    let mut direct_samples = Vec::with_capacity(LATENCY_SAMPLES);
    for i in 0..LATENCY_SAMPLES {
        let cfg = ClientConfig::new("127.0.0.1", backend.port, format!("Direct{i}"));
        let start = Instant::now();
        let _client = Client::connect(&cfg)
            .await
            .expect("direct connect should succeed");
        direct_samples.push(start.elapsed());
    }
    report_latency("connect_setup,route=direct", &mut direct_samples);

    let relay = SocksRelay::start(LATENCY_SAMPLES + 1).await;
    let mut socks5_samples = Vec::with_capacity(LATENCY_SAMPLES);
    for i in 0..LATENCY_SAMPLES {
        let proxy_cfg = Arc::new(Socks5ProxyConfig::new("127.0.0.1", relay.port));
        let cfg = ClientConfig::new("127.0.0.1", backend.port, format!("Socks{i}"))
            .with_socks5_proxy(proxy_cfg);
        let start = Instant::now();
        let _client = Client::connect(&cfg)
            .await
            .expect("SOCKS5 connect should succeed");
        socks5_samples.push(start.elapsed());
    }
    report_latency("connect_setup,route=socks5_loopback", &mut socks5_samples);

    // --- 2. Raw SOCKS5 negotiation latency (tunnel only, no Minecraft) ---
    let bare = BareAcceptor::start(LATENCY_SAMPLES).await;
    let bare_relay = SocksRelay::start(LATENCY_SAMPLES).await;
    let mut negotiation_samples = Vec::with_capacity(LATENCY_SAMPLES);
    for _ in 0..LATENCY_SAMPLES {
        let proxy_cfg = Socks5ProxyConfig::new("127.0.0.1", bare_relay.port);
        let start = Instant::now();
        let _tunnel = socks5::connect(&proxy_cfg, "127.0.0.1", bare.port, Duration::from_secs(5))
            .await
            .expect("raw SOCKS5 negotiation should succeed");
        negotiation_samples.push(start.elapsed());
    }
    report_latency("socks5_negotiation_only", &mut negotiation_samples);
    bare.finish().await;

    // --- 3. Concurrency + retained memory + cleanup ---
    for &n in &CONCURRENCY_TIERS {
        run_concurrency_tier(n).await;
    }

    ExitCode::SUCCESS
}

async fn run_concurrency_tier(n: usize) {
    let echo = EchoBackend::start(n).await;
    let relay = SocksRelay::start(n).await;
    let proxy_cfg = Arc::new(Socks5ProxyConfig::new("127.0.0.1", relay.port));

    let rss_baseline = process_rss_kib();
    let start = Instant::now();
    let mut tunnels = Vec::with_capacity(n);
    for _ in 0..n {
        let tunnel = socks5::connect(&proxy_cfg, "127.0.0.1", echo.port, Duration::from_secs(10))
            .await
            .expect("tunnel establishment should succeed");
        tunnels.push(tunnel);
    }
    let setup_elapsed = start.elapsed();
    let rss_peak = process_rss_kib();

    let cleanup_start = Instant::now();
    drop(tunnels);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let cleanup_elapsed = cleanup_start.elapsed();
    let rss_after_cleanup = process_rss_kib();

    let retained_per_tunnel = match (rss_baseline, rss_peak) {
        (Some(base), Some(peak)) if n > 0 => Some((peak.saturating_sub(base)) as f64 / n as f64),
        _ => None,
    };

    println!(
        "RESULT,concurrency={n},setup_total_ms={},rss_baseline_kib={},rss_peak_kib={},retained_kib_per_tunnel={},cleanup_ms={},rss_after_cleanup_kib={}",
        setup_elapsed.as_millis(),
        optional(rss_baseline),
        optional(rss_peak),
        retained_per_tunnel.map_or_else(|| "unavailable".to_string(), |v| format!("{v:.2}")),
        cleanup_elapsed.as_millis(),
        optional(rss_after_cleanup),
    );
}

fn report_latency(label: &str, samples: &mut [Duration]) {
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    let min = samples[0];
    let max = samples[samples.len() - 1];
    println!(
        "RESULT,metric={label},median_us={},min_us={},max_us={},samples={}",
        median.as_micros(),
        min.as_micros(),
        max.as_micros(),
        samples.len(),
    );
}

fn optional(value: Option<u64>) -> String {
    value.map_or_else(|| "unavailable".to_string(), |v| v.to_string())
}

// ---------------------------------------------------------------------
// In-process fixtures. Deliberately not shared with `tests/common` or
// `tests/socks5_support` (a bin target can't depend on the `tests/`
// directory); each is a compact, independent reimplementation, same
// rationale `full_runtime_benchmark.rs` documents.
// ---------------------------------------------------------------------

/// Accepts `count` connections and completes the minimal handshake -> login
/// -> configuration flow on each (no encryption, no play-state traffic —
/// this benchmark only measures time-to-Play, not play-loop behavior).
struct MinecraftBackend {
    port: u16,
}

impl MinecraftBackend {
    async fn start(count: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind backend");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            for _ in 0..count {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let _ = serve_one(stream).await;
                });
            }
        });
        MinecraftBackend { port }
    }
}

async fn serve_one(stream: TcpStream) -> std::io::Result<()> {
    let mut conn = Connection::from_tcp_stream(stream).map_err(std::io::Error::other)?;
    let hs = conn.read_packet().await.map_err(std::io::Error::other)?;
    debug_assert_eq!(hs.id, handshaking::SERVERBOUND_SET_PROTOCOL_ID);
    let ls = conn.read_packet().await.map_err(std::io::Error::other)?;
    debug_assert_eq!(ls.id, login::SERVERBOUND_LOGIN_START_ID);
    let username = {
        let mut r = PacketReader::new(&ls.payload);
        r.read_string().map_err(std::io::Error::other)?
    };

    let mut w = PacketWriter::new();
    w.put_uuid(0);
    w.put_string(username).map_err(std::io::Error::other)?;
    w.put_varint(0);
    conn.send_packet(login::CLIENTBOUND_SUCCESS_ID, &w.into_inner())
        .await
        .map_err(std::io::Error::other)?;
    let ack = conn.read_packet().await.map_err(std::io::Error::other)?;
    debug_assert_eq!(ack.id, login::SERVERBOUND_LOGIN_ACKNOWLEDGED_ID);

    let _settings = conn.read_packet().await.map_err(std::io::Error::other)?;
    let _brand = conn.read_packet().await.map_err(std::io::Error::other)?;

    conn.send_packet(configuration::CLIENTBOUND_FINISH_CONFIGURATION_ID, &[])
        .await
        .map_err(std::io::Error::other)?;
    let _fin = conn.read_packet().await.map_err(std::io::Error::other)?;
    Ok(())
}

/// A minimal, no-auth SOCKS5 relay: negotiates, then really dials the
/// requested target and relays bytes bidirectionally.
struct SocksRelay {
    port: u16,
}

impl SocksRelay {
    async fn start(count: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            for _ in 0..count {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let _ = relay_one(stream).await;
                });
            }
        });
        SocksRelay { port }
    }
}

async fn relay_one(mut stream: TcpStream) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    let mut methods = vec![0u8; head[1] as usize];
    stream.read_exact(&mut methods).await?;
    stream.write_all(&[0x05, 0x00]).await?; // no-auth selected

    let mut req_head = [0u8; 4];
    stream.read_exact(&mut req_head).await?;
    let target_host = match req_head[3] {
        0x01 => {
            let mut addr = [0u8; 4];
            stream.read_exact(&mut addr).await?;
            std::net::Ipv4Addr::from(addr).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut domain = vec![0u8; len[0] as usize];
            stream.read_exact(&mut domain).await?;
            String::from_utf8_lossy(&domain).into_owned()
        }
        _ => return Ok(()),
    };
    let mut port_buf = [0u8; 2];
    stream.read_exact(&mut port_buf).await?;
    let target_port = u16::from_be_bytes(port_buf);

    let mut backend = TcpStream::connect((target_host.as_str(), target_port)).await?;
    stream
        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;
    let _ = tokio::io::copy_bidirectional(&mut stream, &mut backend).await;
    Ok(())
}

/// Accepts connections and does nothing else (used to isolate SOCKS5
/// negotiation latency from any Minecraft protocol overhead).
struct BareAcceptor {
    port: u16,
    handle: tokio::task::JoinHandle<()>,
}

impl BareAcceptor {
    async fn start(count: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind acceptor");
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            for _ in 0..count {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                // Held open until the client closes; nothing sent.
                tokio::spawn(async move {
                    let mut buf = [0u8; 1];
                    use tokio::io::AsyncReadExt;
                    let mut stream = stream;
                    let _ = stream.read(&mut buf).await;
                });
            }
        });
        BareAcceptor { port, handle }
    }

    async fn finish(self) {
        self.handle.abort();
    }
}

/// Accepts connections and echoes whatever it reads back (used for the
/// concurrency/memory tier — a tunnel needs a live backend on the other end
/// even though this benchmark never writes through it).
struct EchoBackend {
    port: u16,
}

impl EchoBackend {
    async fn start(count: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind echo backend");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            for _ in 0..count {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut stream = stream;
                    let mut buf = [0u8; 256];
                    loop {
                        let n = match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        if stream.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        EchoBackend { port }
    }
}

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
