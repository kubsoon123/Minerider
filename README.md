# MineRider

A high-performance **Minecraft Java Edition client engine written in Rust** —
built to run many bots per machine with low RAM and CPU while staying close to
vanilla wire behavior, with an optional embedded **Lua** scripting layer.

[![CI](https://github.com/kubsoon123/Minerider/actions/workflows/ci.yml/badge.svg)](https://github.com/kubsoon123/Minerider/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
![Minecraft](https://img.shields.io/badge/Minecraft-Java%201.21.4%20(769)-brightgreen.svg)
![Rust](https://img.shields.io/badge/Rust-1.85%2B-orange.svg)
![Status](https://img.shields.io/badge/status-alpha-yellow.svg)

Aimed at **server QA, load/soak testing, training-data collection, and
automation on servers you own or are explicitly authorized to connect to.**
Drive bots directly from Rust, or script whole swarms in Lua — no per-bot VM,
no per-bot OS thread.

> ⚠️ **v0.1.0-alpha.** Early, actively developed software. APIs, wire coverage
> and behavior can change without notice. Read [Status](#status) before
> relying on this for anything beyond experimentation.

> MineRider is an independent project and is **not** an official Minecraft
> product. It is not approved by or associated with Mojang or Microsoft.

## ⚖️ Acceptable use

MineRider is built for **authorized use only**: your own servers, or servers
whose operator has explicitly agreed to let you connect bots for testing,
monitoring or automation. It is **not** designed for, and must not be used
for, anti-cheat bypassing, AFK-kick evasion, ban evasion, proxy rotation to
hide bots, chat spam, griefing, or account farming. If a feature would only
make sense as a way to hide bots from server administrators, it does not
belong in this project — see [CONTRIBUTING.md](CONTRIBUTING.md).

## Features

- **Full connection lifecycle** — handshake → login → configuration → play,
  with RSA / AES-128-CFB8 encryption and zlib compression.
- **Generated protocol** — the entire 1.21.4 packet layer (protocol 769) is
  generated from vendored `minecraft-data`; no hand-maintained packet ids or
  layouts. New versions are added by regenerating, not hand-editing.
- **World knowledge** — chunk/section decoding, block storage, collision
  shapes, and a bounded process-wide interner that shares equal immutable
  chunk payloads across bots on the same server/world/dimension.
- **Vanilla-shaped physics** — gravity, walking/sprinting/jumping,
  friction-aware ground movement, step-up, knockback, auto-respawn.
- **Entity, inventory & tab-list tracking** — server-authoritative inventory
  transactions with typed protocol-769 click modes; reconnect-generation
  safety.
- **Close-to-vanilla play loop** — `tick_end`, `player_input` on key change,
  post-teleport `position_look`, adaptive chunk-batch pacing, play-state
  ping/pong, mid-play **reconfiguration**, server-brand plugin messages.
- **Action API** — movement, look (incl. bounded random look), chat/commands,
  held-item use & swing, hotbar selection, block place / dig (start/finish/
  cancel), drop item, swap hands, entity interact / attack / interact-at,
  GUI open/click/close.
- **Reconnect supervisor** — reconnect-with-backoff, retry classification and
  cancellation, layered on top of a plain session; never fakes activity,
  never retries an explicit rejection unless you opt in.
- **Premium (online-mode) login** — Microsoft / Xbox Live device-code sign-in,
  with the refresh token cached locally (never printed, never committed).
- **Lua scripting** — a 4-worker swarm runtime with a sandboxed API to script
  bots, groups, timers, shared state and pub/sub, with host-trusted proxy
  profiles.
- **SOCKS5 proxy support** — per-bot / per-group routing (for your own,
  authorized proxies only).

## Supported version

**Minecraft Java Edition 1.21.4** (protocol **769**) only. The protocol layer
is generated, so future versions are added by regenerating the packet code,
not by hand-editing it.

## Install / build

Requires **Rust 1.85+** ([rustup.rs](https://rustup.rs)).

```sh
git clone https://github.com/kubsoon123/Minerider
cd Minerider
cargo build --release
```

The Lua scripting layer is behind a feature flag (off by default, so a normal
build never pays the vendored-Lua compile cost):

```sh
cargo build --release --features lua
```

## Quick start

### One bot from the command line

Connect a single bot to a server **you own or are authorized to use**:

```sh
# Offline-mode server (any username works):
cargo run --release -- <host> <port> <username>

# Online-mode (premium) server — one-time Microsoft device-code sign-in,
# the refresh token is then cached locally and reused:
MINERIDER_PREMIUM=1 cargo run --release -- <host> <port> <ignored-username>

# Many bots at once (a small demo swarm):
cargo run --release --bin swarm -- <host> <port> <count> [username_prefix]

# Long-running, authorized client with reconnect-with-backoff:
MINERIDER_RECONNECT=1 MINERIDER_MAX_RETRIES=10 \
  cargo run --release -- <host> <port> <username>
```

See `src/main.rs` for the full set of `MINERIDER_*` environment variables
(view distance, packet tracing, scripted walk/chat, reconnect/backoff/timeouts).

### Scripting a swarm in Lua

```lua
-- examples/lua/swarm.lua (excerpt)
swarm:configure(function()
    swarm:add_server({ name = "local", host = "127.0.0.1", port = 25565, view_distance = 10 })
    swarm:add_group({ name = "group1", count = 3, username_prefix = "Bot_", server = "local" })
end)

swarm:on("connected", function(bot, event)
    minerider.log("info", bot:username() .. " connected")
end)

swarm:on("chat", function(bot, event)
    if event.message == "!hello" then
        bot:chat("hello from " .. bot:username())
    end
end)
```

```sh
cargo run --release --features lua --bin minerider-lua -- examples/lua/swarm.lua
```

The full script (including host-trusted SOCKS5 proxy groups) is in
[examples/lua/swarm.lua](examples/lua/swarm.lua); the complete scripting API
is documented in [docs/lua_api_reference.md](docs/lua_api_reference.md).

### Driving a bot from Rust

```rust
use minerider::core::client::ClientConfig;
use minerider::core::supervisor::{ClientSupervisor, ReconnectPolicy};

let cfg = ClientConfig::new("localhost", 25565, "MonitorBot");
let (supervisor, handle) = ClientSupervisor::new(cfg, ReconnectPolicy::enabled());

// From another task, at any time:
//   handle.events() / handle.state() / handle.status()  — observe
//   handle.walk_to(x, z).await / handle.chat("hi").await — control the live session
//   handle.attack_entity(id, false).await / handle.start_digging(...).await
//   handle.stop()                                        — clean shutdown
let outcome = supervisor.run().await;
```

Every action fails with a typed `ControlError` (never silently queued) if
there is no active session, and never crosses a reconnect generation.

## Documentation

| Doc | What's in it |
|-----|--------------|
| [architecture.md](docs/architecture.md) | Crate/module design, wire pipeline, benchmarks |
| [codegen.md](docs/codegen.md) | The `minecraft-data` → Rust protocol generator |
| [lua_wrapper.md](docs/lua_wrapper.md) | The production Lua runtime (4-worker pool, config, sandbox, proxy/chunk semantics) |
| [lua_api_reference.md](docs/lua_api_reference.md) | The complete Lua scripting API, method by method |
| [capability_matrix.md](docs/capability_matrix.md) | Every core capability audited against public access |
| [vanilla_conformance_1_21_4.md](docs/vanilla_conformance_1_21_4.md) | Generated packet coverage / obligation matrix |
| [progress.md](docs/progress.md) | Chronological development log and known gaps |
| [real_server_validation.md](docs/real_server_validation.md) · [paper_1_21_4_validation_report.md](docs/paper_1_21_4_validation_report.md) | Real-server validation evidence |
| [engineering_review.md](docs/engineering_review.md) | Architecture, security & performance audit, roadmap |
| [SECURITY.md](SECURITY.md) · [CONTRIBUTING.md](CONTRIBUTING.md) | Reporting vulnerabilities; contribution & codegen rules |

## Architecture

```text
        Your bot code  —  Rust (Client / SupervisorHandle)  or  Lua script
                                     │
                              MineRider engine
        ┌──────────┬──────────┬──────────┬──────────┬──────────┐
        │ Network  │ Protocol │  World   │ Physics  │  State   │
        └──────────┴──────────┴──────────┴──────────┴──────────┘
                                     │
                             Minecraft server
```

Rust owns everything — TCP, async runtime (tokio), protocol, encryption,
compression, world state, entities, physics, tick engine, inventory/tab-list
tracking, and Microsoft/Xbox Live auth. Bots share static data globally
(registries, packet definitions, block/item data, collision tables); each bot
owns its own connection, player state and chunk-position map.

| Crate | Description |
|-------|-------------|
| `minerider` | The client engine: core, network, game logic, auth, trace, Lua runtime, binaries |
| [`minerider-protocol`](crates/minerider-protocol) | Standalone wire-protocol library (no game logic, no tokio): VarInt/VarLong, framing, zlib, RSA + AES-128-CFB8, NBT, generated packets |
| [`minerider-codegen`](crates/minerider-codegen) | Build-time generator: vendored `minecraft-data` → Rust, with a drift gate |

### Modules

MineRider is a **single engine**, not a constellation of packages — but
internally it is organized into focused components, roughly mirroring the
Node/prismarine module family that [mineflayer](https://github.com/PrismarineJS/mineflayer)
is built from. If you know that ecosystem, this is the map:

| Component | Description | ≈ mineflayer / prismarine |
|-----------|-------------|---------------------------|
| `minerider-protocol` | Parse & serialize packets, framing, zlib compression, RSA/AES encryption | `minecraft-protocol` |
| `minerider-protocol` (`nbt`) | NBT parser/serializer | `prismarine-nbt` |
| `minerider-codegen` | `minecraft-data` → Rust protocol generator (build-time) | `minecraft-data` |
| `minecraft::physics` + `player` | Vanilla-shaped player physics (gravity, friction, step-up, jump, knockback) | `prismarine-physics` |
| `minecraft::world` | Chunk/section storage, block state queries, collision boxes | `prismarine-chunk` + `prismarine-world` + `prismarine-block` |
| `minecraft::inventory` | Windows, slots, cursor, hotbar, server-authoritative transactions | `prismarine-windows` + `prismarine-item` |
| `minecraft::entity` | Entity spawn/move/despawn tracking | `prismarine-entity` |
| `minecraft::presentation` | Structured chat / title / tab-list parsing | `prismarine-chat` |
| `auth` | Microsoft / Xbox Live / Minecraft Services login | `node-yggdrasil` (Microsoft-era) |
| `core::supervisor` | Reconnect-with-backoff lifecycle, retry classification, cancellation | *(mineflayer bot lifecycle)* |
| `lua` | Embedded Lua scripting runtime (4-worker swarm, sandbox) | *(mineflayer plugins, in-process)* |
| `trace` | Packet capture, normalization and semantic diffing | *(no direct equivalent)* |

### Repository layout

```text
crates/
  minerider-codegen/   minecraft-data → Rust generator (parser, IR, emitter, drift gate)
  minerider-protocol/  standalone wire-protocol library + benchmarks
src/
  core/         client facade, state machine, tick engine, error type
  network/      async TCP transport, connection manager
  minecraft/    handshake, login, configuration, play, world, physics,
                player/entity/inventory/tab-list state, control, events
  auth/         Microsoft/Xbox Live/Minecraft Services premium login
  lua/          the production Lua scripting runtime (feature = "lua")
  trace/        packet capture, normalization and semantic diffing
  bin/          minerider-lua (Lua CLI), swarm (many-bots demo), benchmarks
examples/lua/   runnable example scripts
tests/          integration tests (mock server, conformance scenarios)
docs/           architecture, codegen, progress log, validation reports
```

## Development

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo run -p minerider-codegen -- --check        # generated-files drift gate
cargo run --bin conformance_matrix               # regenerate the coverage doc
```

`crates/minerider-protocol/src/generated/**` and
`src/minecraft/collision_data.rs` are machine-generated — regenerate them,
never hand-edit them (see [CONTRIBUTING.md](CONTRIBUTING.md)). Live-server and
premium-login paths require real network access and are not part of the
offline test suite.

## Status

**Alpha, actively developed** — the connection core is validated, the API
surface is still moving; expect breaking changes between versions.

- **Validated end to end** against a local **Paper 1.21.4** server and the
  official **Mojang vanilla 1.21.4** server: login, encryption, compression,
  keep-alives, and a 55-minute idle soak.
- **Implemented and tested** — generated protocol, world/tick/physics,
  player/entity/inventory/tab-list state, the action API, the reconnect
  supervisor, premium auth, and the Lua runtime.
- **Per-feature history** — what landed, when, and how it was verified — is in
  [docs/progress.md](docs/progress.md).

### Honest limitations

- Alpha software from an actively changing codebase — expect breaking API
  changes between versions.
- Only Minecraft **1.21.4** (protocol 769) is supported.
- Wire behavior is verified against the **vanilla source semantics** (packet
  order, formulas, flags), not yet against a byte-for-byte capture of the
  real client — the conformance fixtures compare MineRider to its own earlier
  traces (see [docs/vanilla_capture.md](docs/vanilla_capture.md)).
- **GUI clicks are server-authoritative by design** — MineRider does not
  predict a click's outcome (no local `changed_slots`); this avoids inventory
  desync but is not the vanilla client's optimistic-prediction behavior.
- **Chat is unsigned** — signed secure-chat requires an online-mode Mojang
  signing key, so it is out of scope; unsigned chat is correct for
  `enforce-secure-profile=false` servers only.
- No item/block registry **names** yet (numeric ids only), no bounded
  on-demand block/chunk query API yet, no pathfinding, no crafting/trading
  helpers — these are mineflayer-style conveniences, not implemented here. See
  [docs/wrapper_api_readiness.md](docs/wrapper_api_readiness.md).
- No Python/JavaScript/HTTP/WebSocket wrapper, and none planned — bots are
  driven from Rust or Lua.

## Contributing

Contributions are welcome — please read [CONTRIBUTING.md](CONTRIBUTING.md)
(especially the codegen and acceptable-use rules) first. Security issues:
see [SECURITY.md](SECURITY.md).

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE), or
- [MIT license](LICENSE-MIT)

at your option. Unless you explicitly state otherwise, any contribution you
intentionally submit for inclusion in the work, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

Vendored `minecraft-data` (used only at build time by the protocol generator)
is upstream-licensed separately; see
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
