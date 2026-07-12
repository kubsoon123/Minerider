# MineRider

A high performance Minecraft Java Edition client engine: **Rust core, Lua
scripting, generated protocol**. Built to run many bots per machine with
low RAM and low CPU, while staying close to vanilla behavior.

Target version: **Minecraft 1.21.4** (protocol 769). The protocol layer is
designed so future versions are generated, not hand-maintained.

## Status

Early development. Phase 1 (connection core) in progress — see
[docs/progress.md](docs/progress.md).

## Architecture

```text
                  User Bots
                     |
               Lua API Layer
                     |
             MineRider Engine
                     |
---------------------------------------------
 Network | Protocol | World | Physics | Behavior
---------------------------------------------
                     |
             Minecraft Server
```

- **Rust** owns everything performance critical: TCP, async runtime (tokio),
  protocol, encryption (RSA + AES-128-CFB8), compression (zlib), world
  state, entities, physics, tick engine.
- **Lua** (mlua, phase 5) owns bot logic, automation and plugins.
- **Protocol definitions are generated from minecraft-data** (phase 2) into
  `crates/minerider-protocol/src/generated/` and never edited by hand.

Bots share static data globally (registries, packet definitions, block/item
data); each bot stores only its own connection, player state and the world
cache it actually needs. Crate-level details, wire byte order and benchmark
numbers live in [docs/architecture.md](docs/architecture.md).

## Layout

```text
crates/
  minerider-protocol/  standalone wire-protocol library (no game logic, no
                       tokio): VarInt/VarLong, buffers, framing, zlib codec,
                       RSA + AES-128-CFB8, benchmarks
src/
  core/         client facade, state machine, tick engine, error type
  network/      async TCP transport, connection manager
  minecraft/    handshake, login, configuration, play state handling
  lua/          scripting API (phase 5)
  generator/    minecraft-data pipeline (phase 2)
tests/          integration tests (mock server, raw-socket stream edge cases)
docs/           architecture notes, progress log
```

## Development

```sh
cargo build
cargo test
```

Live-server tests are gated behind environment variables and never run by
default.

## History

MineRider supersedes BAC, a LuaJIT prototype that proved the concept and was
retired. Everything worth keeping from it is documented in
[docs/bac_analysis.md](docs/bac_analysis.md); no BAC code survives.

## License

MIT OR Apache-2.0
