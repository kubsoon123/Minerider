# SOCKS5 transport: architecture and local performance report

## Status

Branch `feat/socks5-transport`, stacked on draft PR #1
(`feat/client-presentation-state` → `main`, head
`9df9f50f68a5c6263aa249c78956320cc733da78`) — not on PR #2
(`perf/shared-chunk-store`), because this feature needs PR #1's
`ClientSupervisor`/`connect_deadline`/premium-login infrastructure but
nothing PR #2 adds (chunk-payload sharing is unrelated to the transport
layer). Draft only; no merge, force-push, publish or release action is
requested. No real proxy or Minecraft server is contacted by any automated
test or by this benchmark.

## Architecture

```text
Client::connect(cfg)
  -> cfg.proxy: Option<Arc<Socks5ProxyConfig>>
       None   -> Connection::connect_with_timeouts(cfg.host, cfg.port, ..)   (unchanged path)
       Some(p)-> Connection::connect_via_proxy(cfg.host, cfg.port, p, ..)
                   -> socks5::connect(p, cfg.host, cfg.port, timeouts.connect)
                        1. TCP connect to p.host:p.port
                        2. method negotiation (offer exactly one method)
                        3. optional RFC 1929 username/password sub-negotiation
                        4. CONNECT cfg.host:cfg.port  (the *Minecraft* target)
                        5. parse reply, return the raw TcpStream
                   -> Connection::from_tcp_stream(stream)   (identical to the direct path from here on)
  -> handshake::send(&mut conn, .., &cfg.host, cfg.port)   -- unchanged: always the real Minecraft host/port,
                                                               never the proxy's, regardless of route
```

`src/network/socks5.rs` is a small, hand-rolled RFC 1928/1929 client —
deliberately not built on an existing SOCKS5 crate (e.g. `tokio-socks`).
This codebase already hand-rolls every other wire protocol it depends on
(the Minecraft frame codec, the AES-128-CFB8 stream cipher, the RSA encrypt
wrapper, NBT), specifically to keep byte-level control and direct test
coverage of every edge case; the mission's own required test list (every
reply code, truncated/malformed frames, invalid version/address type,
credential/domain length limits, ...) is exactly the protocol-conformance
surface this project already tests its other hand-rolled codecs against,
and a third-party crate's internal parsing would sit between those tests and
the code they're meant to verify.

### Supported

- Direct connection (default, `ClientConfig::proxy = None`) — unchanged.
- SOCKS5 without authentication.
- SOCKS5 username/password authentication (RFC 1929). Exactly one method is
  ever offered in the greeting — `NO AUTHENTICATION` when no credentials are
  configured, `USERNAME/PASSWORD` when they are — never both, so a proxy can
  never silently downgrade past configured credentials.
- Domain, IPv4 and IPv6 targets. A literal IP is sent as that address type;
  anything else is sent as `ATYP_DOMAIN` with the hostname verbatim, so the
  **proxy** resolves it (proxy-side DNS) — this process never does its own
  DNS lookup for a proxied target.
- Per-client proxy configuration (`ClientConfig::proxy: Option<Arc<..>>`,
  one config per bot) and shared immutable configuration (many bots holding
  clones of the same `Arc`) via the same field.
- Cancellation at every stage (plain `async`/`.await`, drop-safe throughout;
  proven with a duplex-based test and the 100-concurrent-tunnel integration
  test's clean task shutdown).
- The existing per-write timeout (`ClientConfig::write_timeout`, unaffected —
  it applies to Minecraft packets after the tunnel is established) and the
  existing overall connect-to-play deadline (`ClientConfig::connect_deadline`
  still wraps proxy negotiation, now reported as its own
  `ConnectStage::ProxyNegotiate` if it's what timed out).
- Reconnect through the same configured route: `ClientSupervisor` stores one
  `ClientConfig` and calls `Client::connect(&cfg)` on every attempt, so a
  `proxy` field on that config is automatically reused on every reconnect —
  no supervisor changes were needed, only the config plumbing (proven by
  `tests/socks5.rs::supervisor_reconnects_through_the_same_socks5_route`,
  which asserts the fake proxy is dialed exactly twice, not once).
- Typed, redacted proxy errors (`ProxySocks5Error`, wrapped as
  `MineRiderError::Proxy`) — see "Secret handling" below.

### Not implemented (explicitly out of scope for this phase)

- Proxy rotation or pools.
- Routing Microsoft/Xbox/Mojang HTTP authentication through the proxy — that
  traffic (via `reqwest`) remains direct by default, unchanged.
- `BIND`/`UDP ASSOCIATE` SOCKS5 commands — only `CONNECT` (RFC 1928 §4,
  `CMD = 0x01`) is implemented, the only command a Minecraft client needs.

## Secret handling

- `ProxyPassword` has no `Display` impl and a fixed-text `Debug` impl
  (`ProxyPassword(<redacted>)`); the only way to read the plaintext is the
  loudly-named `expose_secret()`, called exactly once, right before writing
  the RFC 1929 sub-negotiation bytes to the socket.
- `Socks5Credentials`'s `Debug` impl redacts both the username and the
  password. `Socks5ProxyConfig` derives `Debug` and — because Rust's derive
  delegates to each field's own `Debug` — that redaction holds transitively:
  printing a whole `ClientConfig` (which derives `Debug` and contains
  `proxy: Option<Arc<Socks5ProxyConfig>>`) still never prints the password.
- No `ProxySocks5Error` variant has a field capable of holding a password —
  checked structurally (the type definitions) and with a regression test
  (`errors_never_contain_a_planted_password`) against a planted marker
  string.
- No password is ever accepted via a command-line argument (which would
  remain visible in `ps`/Task Manager process-argument listings); the
  supported non-programmatic path is `Socks5ProxyConfig::from_env(prefix)`,
  reading `{prefix}_HOST`/`_PORT`/`_USERNAME`/`_PASSWORD`.
- No `.env` file is read, written, or committed by anything in this branch.
- No live proxy credentials appear anywhere in fixtures, tests, or GitHub
  Actions — every automated test (`src/network/socks5.rs`'s unit tests,
  `tests/socks5.rs`'s integration tests) uses a local fake SOCKS5 server
  (duplex streams or loopback sockets only). The one test that can reach a
  real proxy, `tests/socks5.rs::live_socks5_smoke`, is `#[ignore]`d — it
  never runs under plain `cargo test` or in CI — reads every value from
  environment variables, and only ever prints connection *outcomes*
  (success/failure/error variant), never a credential value or even whether
  one was configured beyond a yes/no "auth: yes/no" line.
- Error `Display`/`Debug` text may include the proxy `host:port` and the
  Minecraft target `host:port` (both configuration, not secrets) but never
  authentication data.

## Local tests

`src/network/socks5.rs` (27 unit tests, `tokio::io::duplex` fakes, no real
sockets): no-auth success, authenticated success, rejected authentication,
no acceptable method, every documented SOCKS5 reply code (with its expected
`RetryClass`), domain targets (proxy-side DNS, no local resolution),
IPv4, IPv6 (including an IPv6 bound address in a reply), truncated
method-negotiation and CONNECT replies, invalid SOCKS version, invalid
bound-address type, username/password/domain length-limit rejection before
any byte is written, timeouts during method negotiation/authentication/the
CONNECT reply, cancellation, `Socks5ProxyConfig::from_env` (absent, full
round-trip, partial-credentials rejection), and three dedicated redaction
tests (`ProxyPassword` Debug, `Socks5Credentials` Debug, a planted-secret
regression check across every auth-adjacent error variant).

`tests/socks5.rs` (8 integration tests + 1 `#[ignore]`d manual smoke test,
real loopback TCP against an independently-implemented fake SOCKS5 relay in
`tests/socks5_support`): a full mock Minecraft handshake through the tunnel
(and confirms the proxy is asked to `CONNECT` to the *real* Minecraft
target, not its own endpoint), authenticated relay success, wrong
credentials rejected, the direct-connection path unchanged with no proxy
configured, `ClientSupervisor` reconnecting through the same proxy route
twice, arbitrary bytes forwarding unmodified through the tunnel (independent
of the Minecraft protocol, via a plain echo backend), 100 concurrent local
tunnels completing without deadlock or a leaked task, and an
unreachable-proxy case asserting `RetryClass::Transient`.

Mission item 10 ("timeout during proxy TCP connection") has no dedicated
real-network integration test: a first attempt raced an already-expired
deadline against a real `TcpStream::connect` to a refusing loopback target
and passed locally on Windows but **failed on Ubuntu CI** — on Linux,
connecting to a refusing loopback destination resolves synchronously inside
the `connect()` syscall itself (no reactor round trip), so the wrapped
future is already `Ready` on `tokio::time::timeout_at`'s very first poll and
the deadline is never consulted, for any budget including zero. That's
correct, real behavior (`unreachable_proxy_is_transient` exercises exactly
this path and correctly expects `ProxyConnect`/`Transient`, not `Timeout`),
but it means no loopback target can deterministically exercise the *timeout*
branch of this phase across platforms — the same category of
platform-dependent flakiness `network::connection`'s own tests document
giving up on for its write-timeout test. The `tokio::time::timeout_at`
composition wrapping this phase is code-identical to the three phases
already proven deterministic against `tokio::io::duplex` fakes (method
negotiation, authentication, CONNECT reply); see `tests/socks5.rs` for the
full explanation kept next to the code.

Workspace test count on this branch: 204 lib unit tests (177 at PR #1's base
plus 27 new SOCKS5 unit tests) plus the existing integration suites,
plus 8 new `tests/socks5.rs` integration tests (9 counting the ignored
smoke test) — see the Verification section of the final report for the
exact `cargo test --workspace --locked` run and totals.

## Performance report

### Methodology

`src/bin/socks5_benchmark.rs` (`cargo run --release --bin socks5_benchmark`)
measures entirely against `127.0.0.1`: a real Tokio multi-thread runtime,
real sockets, an in-process mock Minecraft backend (handshake → login →
configuration, no encryption — same simplification `tests/common`'s
`Mode::Plain` uses, to isolate transport overhead from RSA cost) and an
in-process minimal no-auth SOCKS5 relay that really dials the requested
target and relays bytes (independent reimplementation from
`tests/socks5_support`, since a `[[bin]]` target can't depend on `tests/`).
25 samples per latency metric, reported as median/min/max; three
concurrency tiers (1/10/100) for the memory/cleanup numbers. Release profile
(thin LTO, `codegen-units = 1`).

**This never contacts a real proxy.** Loopback has no real network latency,
no real proxy-side load, and no real-world TLS-terminating/logging proxy
overhead — these numbers characterize this implementation's own negotiation
cost, not what any specific public or commercial SOCKS5 proxy will add on
top.

### Environment

Windows 11 Home (build 26200), Intel64 Family 6 Model 167 (~2.6 GHz base),
32 GiB RAM, `rustc 1.97.0 (2d8144b78 2026-07-07)`, three independent runs.

### Results

Connection setup latency (`Client::connect` to reaching the `Configuration`
→ `Play` boundary), median across 25 samples, three separate runs:

| Route | Run 1 | Run 2 | Run 3 |
|---|---:|---:|---:|
| Direct | 520 µs | 542 µs | 575 µs |
| SOCKS5 (loopback) | 956 µs | 921 µs | 1,104 µs |
| **Delta** | **+436 µs** | **+379 µs** | **+529 µs** |

Raw SOCKS5 negotiation latency alone (tunnel establishment only, no
Minecraft protocol bytes exchanged — isolates the proxy's own overhead):

| Run | Median | Min | Max |
|---|---:|---:|---:|
| 1 | 567 µs | 480 µs | 1,134 µs |
| 2 | 682 µs | 538 µs | 933 µs |
| 3 | 577 µs | 482 µs | 979 µs |

The connect-to-play delta (≈380–530 µs) and the isolated negotiation cost
(≈570–680 µs median) are the same order of magnitude, consistent with the
delta being *mostly* the SOCKS5 round trip itself (one extra TCP RTT is
effectively free on loopback; the added cost is the two extra
request/reply exchanges — method negotiation, then CONNECT — each a
separate `write` + `read` pair) plus scheduling noise.

Concurrency, retained memory per tunnel, and cleanup (run 3 shown; all three
runs agreed within noise):

| Concurrency | Setup total | RSS baseline | RSS peak | Retained/tunnel | Cleanup | RSS after cleanup |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 0 ms | 8,084 KiB | 8,084 KiB | ~0 KiB (noise floor) | 203 ms | 8,084 KiB |
| 10 | 61 ms | 8,088 KiB | 8,256 KiB | 16.8 KiB | 202 ms | 8,216 KiB |
| 100 | 174 ms | 8,220 KiB | 10,196 KiB | 19.8 KiB | 214 ms | 8,768 KiB |

Across all three runs, retained memory per open tunnel converged to roughly
**15–20 KiB** at 10 and 100 concurrent tunnels (a single tunnel's signal is
below the process's own RSS measurement noise floor). This is the SOCKS5
layer alone (one `TcpStream` plus whatever the Tokio I/O driver retains per
registered socket) — a real bot's *total* per-client memory also includes
everything `docs/shared_world_benchmark.md` Mission B measures (connection
buffers, codec state, `PlayState`, HUD/inventory, control/event channels),
which is unrelated to and not duplicated by this number.

"Cleanup" (`cleanup_ms` above) is dominated by this benchmark's own fixed
200 ms settle sleep, not by any slow teardown — RSS after cleanup drops back
most of the way toward baseline in every run (100-tunnel tier: 10,196 →
8,768 KiB, i.e. released ~73% of the peak's growth over baseline), with the
remainder consistent with ordinary allocator arena retention rather than a
leak (compare the same pattern already documented and accepted in
`docs/shared_world_benchmark.md`'s own RSS-after-drop discussion).

### What this does not include

- Any real SOCKS5 proxy's own negotiation latency, load, TLS termination,
  logging, or rate limiting — loopback-only, see Methodology above.
- Kernel-side TCP memory (socket buffers, TIME_WAIT state) for either the
  client-to-proxy or proxy-to-target leg — outside process RSS, not
  measured here or in `docs/shared_world_benchmark.md`.
- CI-verified numbers: like `docs/shared_world_benchmark.md`'s Mission B
  section, this is a local, single-machine benchmark, not run in GitHub
  Actions (informational only, matching this project's "no unstable RSS
  thresholds in CI" convention already established for the chunk benchmark).
- Real Minecraft server behavior, real player traffic patterns, or a real
  proxy's authentication latency (Microsoft/Xbox/Mojang auth stays direct
  by design in this phase and is not measured here).
