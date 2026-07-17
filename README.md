# MineRider

A Minecraft Java Edition client engine written in Rust, built to run many
bots per machine with low RAM and low CPU while staying close to vanilla
behavior. Aimed at server QA, load/soak testing, training-data collection and
automation on servers you own or are explicitly authorized to connect to.

> **v0.1.0-alpha.** Early, actively developed software. APIs, wire coverage
> and behavior can change without notice. Read the "Status" section below
> before relying on this for anything beyond experimentation.

MineRider is an independent project and is not an official Minecraft product.
It is not approved by or associated with Mojang or Microsoft.

Target version: **Minecraft Java Edition 1.21.4** (protocol 769). The
protocol layer is generated from vendored `minecraft-data`, so future
versions are added by regenerating, not hand-editing packet code.

## Acceptable use

MineRider is built for **authorized use only**: your own servers, or servers
whose operator has explicitly agreed to let you connect bots for testing,
monitoring or automation. It is not designed for, and must not be used for,
anti-cheat bypassing, AFK-kick evasion, ban evasion, proxy rotation, chat
spam, griefing, or account farming. If a feature would only make sense as a
way to hide bots from server administrators, it does not belong in this
project (see `CONTRIBUTING.md`).

## Status

Actively developed. Phases 1–2 (connection core, generated protocol) are
complete and validated against a local Paper 1.21.4 server and the official
Mojang vanilla 1.21.4 server (login, encryption, compression, keep-alives, a
55-minute idle soak). Phase 3 (world/tick engine, player and entity state,
physics, inventory, tab list, bot events, control API, premium auth) is
underway — see [docs/progress.md](docs/progress.md) for the detailed,
chronological log of what has landed and what its known gaps are.

**Implemented and exercised** (unit tests, mock-server integration tests,
and/or a live local-server run — see `docs/progress.md` for which, per
feature):

- Handshake → login → configuration → play, with RSA/AES-128-CFB8 encryption
  and zlib compression.
- A fully generated 1.21.4 packet layer (237 packets) — no hand-maintained
  packet ids or layouts.
- World state: chunk/section decoding, block storage, collision shapes, and a
  bounded process-wide interner for equal immutable chunk payloads. Every bot
  keeps independent visibility and copy-on-write updates; strict sharing is
  enabled by default and can be disabled with
  `ClientConfig::with_chunk_sharing(false)`.
- Vanilla-shaped player physics: gravity, walking/sprinting/jumping,
  friction-aware ground movement, step-up, knockback, auto-respawn.
- Entity tracking, bounded inventory/container state and transaction
  tracking, and tab-list (player info) tracking. Inventory clicks use typed
  protocol-769 modes and server-authoritative confirmation/correction; the
  supervisor rejects stale reconnect generations.
- Headless presentation state: structured chat/system/disguised messages,
  action bar, titles and timings, tab-list header/footer, bounded boss bars,
  and structured disconnect reasons. Signed-chat wire data is retained for
  future verification; signatures are not currently verified.
- Headless scoreboard state: bounded, deterministically ordered objectives,
  display slots, scores and teams, including protocol-769 number formats,
  team text/options and one-team-per-member lifecycle semantics.
- Headless HUD/player-facing state: vitals and experience, game mode and
  abilities, selected hotbar item, cooldowns, effects, attributes, death and
  respawn context, world border, time/weather, difficulty, spawn position,
  and a bounded deterministic modern player list.
- A push-based bot event stream (`Client::events`) and a pull-based state
  snapshot (`Client::bot_state`), plus a command-based control API
  (`Client::control`) for movement, look, ordinary chat and explicit commands.
  Protocol-769 chat/command actions are distinct, validated to vanilla's
  UTF-16 length bound and never inferred from a leading slash.
- Reliability: a configurable write timeout and an overall connect-to-play
  deadline (`ClientConfig::write_timeout`/`connect_deadline`), and a
  `core::supervisor::ClientSupervisor` for long-running authorized clients
  — reconnect-with-backoff, centralized retry classification, clean
  cancellation, and a generation-tracked command handle
  (`SupervisorHandle::send_command`/`walk_to`/`chat`/...) that targets
  whichever session is currently active and fails with a typed
  `ControlError` rather than queuing across a reconnect. Reconnect is
  disabled by default and never retries after an explicit server rejection
  or a permanent auth/protocol error unless explicitly configured to; see
  the example below and
  [`core::supervisor`](src/core/supervisor.rs) for the full policy surface.
- Offline-mode (cracked-server) login.
- Microsoft/Xbox Live/Minecraft Services (premium) login — implemented and
  unit-tested against realistic fixtures and a local mock session server;
  **the full live chain against the real Microsoft/Xbox/Mojang services has
  not been run in this environment** (it requires a real Microsoft account
  and cannot be exercised by an automated session). Treat it as
  implemented-and-tested-against-spec, not field-proven, until you've run it
  yourself once.
- A conformance/tracing harness (`src/trace`, `tests/conformance*`) that
  captures and diffs real packet traces against documented vanilla behavior.

**Partial or unverified:**

- Byte/timing parity with a real official vanilla client has not been
  captured (see [docs/vanilla_capture.md](docs/vanilla_capture.md)); current
  evidence is unit tests, mock-server assertions, and local Paper/vanilla
  server runs, which is a real but narrower guarantee than a client-to-client
  diff.
- Physics branches not yet implemented: fluids (water/lava), ladders/
  climbables, elytra, potion-effect movement modifiers, honey/soul-sand
  slowdown multipliers.
- Inventory interaction currently provides generic typed click transactions
  (pickup, shift-click, hotbar/offhand swap, creative clone, drop, drag and
  double-click). It deliberately does not simulate menu-specific client-side
  results or crafting/anvil/merchant semantics; changed-slot prediction is
  empty and the server's authoritative update confirms or corrects state.
- No pathfinding/obstacle avoidance (`walk_to` is straight-line steering).
- No chat message signing (messages are sent unsigned; servers that enforce
  secure chat will reject or kick for this).

**Not implemented:**

- **Lua scripting.** The architecture anticipates a Lua automation layer
  (`minerider-lua`, see roadmap in `docs/engineering_review.md`), but it does
  not exist yet in this repository: there is no `src/lua`, no `.lua` files,
  and no `mlua` dependency. Bots are currently driven directly through the
  Rust `Client` API (`control()`, `bot_state()`, `events()`). Treat any
  mention of Lua elsewhere in the docs as a roadmap item, not a shipped
  feature.

## Architecture

```text
                  Your bot code (Rust, via Client)
                     |
             MineRider Engine
                     |
---------------------------------------------
 Network | Protocol | World | Physics | State
---------------------------------------------
                     |
             Minecraft Server
```

- **Rust** owns everything: TCP, async runtime (tokio), protocol, encryption
  (RSA + AES-128-CFB8), compression (zlib), world state, entities, physics,
  tick engine, inventory/player-list tracking, Microsoft/Xbox Live auth.
- **Protocol definitions are generated from minecraft-data** into
  `crates/minerider-protocol/src/generated/` and are never edited by hand.

Bots share static data globally (registries, packet definitions, block/item
data, collision tables). Each bot owns its connection, player state and chunk
position map; fully equal decoded chunk payloads can share bounded immutable
ownership only within the same server/world/dimension scope. See
[the ownership audit and benchmark](docs/shared_world_benchmark.md) and the
byte-level pipeline in [docs/architecture.md](docs/architecture.md).

## Layout

```text
crates/
  minerider-codegen/   build-time generator: vendored minecraft-data → Rust
                       (parser, IR, emitter, drift gate)
  minerider-protocol/  standalone wire-protocol library (no game logic, no
                       tokio): VarInt/VarLong, buffers, framing, zlib codec,
                       RSA + AES-128-CFB8, NBT, generated packets, benchmarks
src/
  core/         client facade, state machine, tick engine, error type
  network/      async TCP transport, connection manager
  minecraft/    handshake, login, configuration, play state, world, physics,
                player/entity/inventory/player-list state, control, events
  auth/         Microsoft/Xbox Live/Minecraft Services premium login
  trace/        packet capture, normalization and semantic diffing
  bin/          conformance_matrix (docs generator), swarm (many-bots demo)
tests/          integration tests (mock server, conformance scenarios,
                raw-socket stream edge cases)
docs/           architecture, codegen pipeline, progress log, validation
                reports, conformance matrix
```

## Development

```sh
cargo build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo run -p minerider-codegen -- --check   # generated-files drift gate
cargo run --release --bin shared_world_benchmark  # paired ownership benchmark
```

`crates/minerider-protocol/src/generated/**` and
`src/minecraft/collision_data.rs` are machine-generated — regenerate them
with `cargo run -p minerider-codegen` and `python
scripts/generate_collision_data.py` respectively; never hand-edit them (see
[CONTRIBUTING.md](CONTRIBUTING.md)).

Live-server tests and the premium-login CLI path require real network access
and, for premium login, a Microsoft account; neither runs as part of the
default test suite.

### Basic usage

```sh
cargo run -- <host> <port> <username>            # offline-mode connect
MINERIDER_PREMIUM=1 cargo run -- <host> <port> <ignored-username>  # premium
cargo run --bin swarm -- <host> <port> <count> [username_prefix]   # many bots

# Long-running, authorized client with reconnect-with-backoff. Only use this
# against servers you own or are explicitly permitted to run bots on.
MINERIDER_RECONNECT=1 MINERIDER_MAX_RETRIES=10 cargo run -- <host> <port> <username>
```

See `src/main.rs` for the full set of `MINERIDER_*` environment variables
(view distance, packet tracing, scripted walk/chat for manual testing,
reconnect/backoff/timeouts).

### Reconnect supervisor (library usage)

For authorized monitoring, compatibility testing, or a long-running bot,
`core::supervisor::ClientSupervisor` layers reconnect-with-backoff on top of
an ordinary `Client` — it never fakes activity and never retries after an
explicit server rejection or a permanent auth/protocol error unless you
configure it to:

```rust
use minerider::core::client::ClientConfig;
use minerider::core::supervisor::{ClientSupervisor, ReconnectPolicy};

let cfg = ClientConfig::new("localhost", 25565, "MonitorBot");
let policy = ReconnectPolicy::enabled(); // disabled by default; opt in explicitly
let (supervisor, handle) = ClientSupervisor::new(cfg, policy);

// Observe from another task: handle.events() / handle.state() / handle.status().
// handle.stop() requests a clean shutdown from anywhere, at any point.
// handle.walk_to(x, z).await / handle.chat("hi").await /
// handle.command("say hi").await control whichever session is currently
// active; each fails with a typed `ControlError`
// (never silently queued) if there isn't one right now.
// handle.inventory_click(window_id, click).await additionally waits for a
// typed server-authoritative outcome and never crosses a reconnect generation.
let outcome = supervisor.run().await;
```

See [docs/progress.md](docs/progress.md) (Phase 3l/4a and 4b-4h) for the full
design — retry classification, backoff shape, cancellation, and how
commands are kept from ever executing against a session a reconnect has
already replaced.

## Honest limitations

- This is alpha software from an actively changing codebase; expect breaking
  API changes between versions.
- Only Minecraft 1.21.4 (protocol 769) is supported today.
- No performance claims (bots/core, RAM/bot, "N bots on a machine") are made
  in this README; aspirational targets and any measured numbers that do
  exist live in `docs/engineering_review.md` and are explicitly labeled as
  goals, not guarantees, unless backed by a benchmark in this repository.
- See the "Partial or unverified" and "Not implemented" sections above for
  the concrete gaps, and `docs/progress.md` for the full history.

## History

MineRider supersedes BAC, a LuaJIT prototype that proved the concept and was
retired. Everything worth keeping from it is documented in
[docs/bac_analysis.md](docs/bac_analysis.md); no BAC code survives.

## Documentation index

- [docs/architecture.md](docs/architecture.md) — crate/module design, wire
  pipeline, benchmarks.
- [docs/codegen.md](docs/codegen.md) — the minecraft-data → Rust generator.
- [docs/progress.md](docs/progress.md) — chronological development log.
- [docs/vanilla_conformance_1_21_4.md](docs/vanilla_conformance_1_21_4.md) —
  generated packet coverage/obligation matrix.
- [docs/real_server_validation.md](docs/real_server_validation.md) and
  [docs/paper_1_21_4_validation_report.md](docs/paper_1_21_4_validation_report.md)
  — real-server validation evidence.
- [docs/vanilla_capture.md](docs/vanilla_capture.md) — the manual procedure
  for a real vanilla-client reference capture (not yet performed).
- [docs/engineering_review.md](docs/engineering_review.md) — architecture,
  security and performance audit, and the longer-term roadmap.
- [docs/lua_design.md](docs/lua_design.md) — design for the planned Lua
  scripting layer (not implemented yet; see "Not implemented" above).
- [SECURITY.md](SECURITY.md), [CONTRIBUTING.md](CONTRIBUTING.md) — reporting
  vulnerabilities, contribution and codegen rules.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Vendored `minecraft-data` (used only at build time by the protocol
generator) is upstream-licensed separately; see
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
