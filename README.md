# MineRider

A high performance Minecraft Java Edition client engine: **Rust core, Lua
scripting, generated protocol**. Built to run many bots per machine with
low RAM and low CPU, while staying close to vanilla behavior.

Target version: **Minecraft 1.21.4** (protocol 769). The protocol layer is
designed so future versions are generated, not hand-maintained.

## Status

Early development. Phase 1 (connection core) and phase 2 (minecraft-data
packet pipeline) are complete — see [docs/progress.md](docs/progress.md)
and [docs/codegen.md](docs/codegen.md). Local Paper and official vanilla
1.21.4 server validation passes login, play entry, keep-alives, teleport
confirmation, chunk-batch acknowledgement, and a 55-minute idle soak.
Full vanilla-client reference parity remains pending; see the
[validation report](docs/paper_1_21_4_validation_report.md).

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
- **Protocol definitions are generated from minecraft-data** into
  `crates/minerider-protocol/src/generated/` and never edited by hand
  (237 packets for 1.21.4).

Bots share static data globally (registries, packet definitions, block/item
data); each bot stores only its own connection, player state and the world
cache it actually needs. Crate-level details, wire byte order and benchmark
numbers live in [docs/architecture.md](docs/architecture.md).

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
  minecraft/    handshake, login, configuration, play state handling
  lua/          scripting API (phase 5)
tests/          integration tests (mock server, raw-socket stream edge cases)
docs/           architecture notes, codegen pipeline, progress log
```

## Development

```sh
cargo build
cargo test
cargo run -p minerider-codegen -- --check   # generated-files drift gate
```

Live-server tests are gated behind environment variables and never run by
default.

## History

MineRider supersedes BAC, a LuaJIT prototype that proved the concept and was
retired. Everything worth keeping from it is documented in
[docs/bac_analysis.md](docs/bac_analysis.md); no BAC code survives.

## License

MIT OR Apache-2.0
