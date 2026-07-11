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
  `src/protocol/generated/` and never edited by hand.

Bots share static data globally (registries, packet definitions, block/item
data); each bot stores only its own connection, player state and the world
cache it actually needs.

## Layout

```text
src/
  core/         client facade, state machine, tick engine, error type
  network/      async TCP transport, connection manager
  protocol/     VarInt/VarLong, buffers, framing, compression codec, generated/
  minecraft/    handshake, login, configuration, play state handling
  crypto/       RSA key exchange, AES-128-CFB8 stream cipher
  compression/  zlib helpers
  lua/          scripting API (phase 5)
  generator/    minecraft-data pipeline (phase 2)
tests/          unit + integration tests (mock server)
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
