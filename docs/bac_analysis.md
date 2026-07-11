# BAC (Bot Automation Core) — Post-Mortem Analysis

> BAC was an experimental Minecraft 1.21.4 Java Edition bot client written in
> LuaJIT, driven by a Python code-generation pipeline consuming PrismarineJS
> `minecraft-data`. It has been deleted; this document is the only surviving
> artifact. It exists to transfer *knowledge*, not code, into MineRider.

## What BAC was

- **Runtime:** LuaJIT + LuaSocket + luaossl + lua-zlib.
- **Codegen:** Python (requests / jinja2 / pytest) generating Lua tables from
  vendored minecraft-data (`data/pc/1.21.4/`).
- **Chunk decoding:** delegated to a Node.js sidecar (`prismarine-chunk`)
  over loopback JSON-RPC.
- **Tests:** busted + pytest (pytest spawned `luajit` subprocesses with
  `package.loaded` mock injection), a real-socket Python mock server that
  derived packet ids from parsed protocol JSON, env-gated live tests against
  a Minestom server and real offline-mode servers.
- **In practice** it only ever logged into offline-mode ("cracked") servers;
  Mojang session authentication was never implemented.

## What BAC did well

1. **The minecraft-data codegen pipeline was the crown jewel.**
   `protocol.json` → intermediate flat packet model → Jinja2 templates →
   generated Lua tables, plus a version-switch module with minor-version
   fallback, plus *structure-hash drift tests* asserting generated output
   matches upstream data. A complete "new MC version drops → regenerate →
   test" machine, cron-automated. This pipeline shape must be rebuilt in Rust
   (build-time generator / xtask).
2. **Correct crypto and compression.** AES-128-CFB8 with IV = shared secret,
   no padding, proper finalization checks; compression threshold semantics
   exactly per protocol (see facts below). Verified against live servers.
3. **It actually worked end-to-end.** Handshake → login → encryption →
   compression → configuration → play → keep-alive/reconnect/kick-detection
   was battle-tested on real servers.
4. **Disciplined error handling.** Consistent `nil, err` returns, validation
   of every external input, hard bounds everywhere (2 MiB packets, NBT depth
   64, chunk byte budgets, bounded login/configuration loops).
5. **Correct vanilla movement physics.** Gravity 0.08/tick, terminal velocity
   3.92, vertical drag 0.98, ground drag 0.91, water gravity/buoyancy/drag,
   min horizontal velocity 0.003, entity-velocity packets ÷8000 clamped to
   4.0/axis, teleport confirm, auto-respawn via `client_command` actionId 0.
6. **Right inventory abstraction.** Monotonically tracked `stateId`, per-window
   click action numbers, changed-slots arrays, forced hotbar sync on login.
7. **Testing ingenuity worth copying:** mock server speaking the real protocol
   with ids from generated data; drift-detection hash tests; env-gated live
   tests.

## Architecture mistakes (do not repeat)

1. **Blocking, byte-at-a-time I/O.** `read_varint` did one `receive_exact(1)`
   syscall per VarInt byte and was copy-pasted into three modules. "Async" was
   a hand-rolled busy loop with 5 ms socket timeouts.
2. **String-based dispatch.** Incoming packets went id → name → long
   `if name == "keep_alive"` chains; handlers keyed by strings; packet ids
   re-resolved by name at runtime with multi-name candidate lists
   (`held_item_slot`/`set_held_item`/`set_carried_item`) and hardcoded legacy
   id fallbacks.
3. **Hand-written decoders duplicating generated schemas.** The generator
   emitted full field-level reader metadata, but decoding was re-implemented
   by hand per packet — the two drifted apart by construction.
4. **Node.js sidecar for chunk decoding.** Spawning Node via `os.execute`,
   loopback JSON-RPC with a hand-rolled JSON codec, base64 payloads, FFI
   `__gc` handles, fail-open teardown — hundreds of lines of fragile IPC to
   avoid writing a chunk-section/palette decoder. Pulled npm/node into a
   "Lua" client's runtime dependencies.
5. **Singletons everywhere.** Default manager/bot/bridge singletons; state
   split across three overlapping trackers (play-client position,
   world-state manager, position-sync) kept in sync manually.
6. **Alias-spraying.** Because minecraft-data field names vary across
   versions, one value was written under eight names at once; every API
   existed twice (snake_case + camelCase).
7. **Environment-hostile shortcuts.** SRV lookup by shelling out to
   `nslookup`/PowerShell/`dig` via `io.popen`.
8. **Repo hygiene.** Live-server scripts with hardcoded hosts at repo root,
   `.tmp_*` probes, a vendored Minestom clone, scratch JSON dumps at root.
9. **Semantic cheats that happened to work:** `select_known_packs` echoed back
   raw (client lies about knowing all packs), chat `acknowledged` bitset
   hardcoded to zero, unknown packet `0x1c` hardcoded as chunk unload.

## What to reuse conceptually

- The **pipeline shape**: minecraft-data → intermediate model → generated
  code, with version-switch and drift tests. In Rust: generate typed structs,
  `#[repr(i32)]` enums, const id tables, and *one generic decoder from the
  same schema as the encoder* (killing encode/decode drift).
- **Framing/codec semantics** as the spec for a tokio codec.
- **Physics constants and movement-sync design.**
- **Inventory state tracking** (stateId / action numbers / changed slots).
- **Test architecture:** scriptable mock server + drift tests + gated live
  tests.

## What to redesign

- **I/O:** tokio async, buffered frame reader (parse VarInts from memory,
  never per-byte reads), one task per connection, keep-alive watchdog as a
  timer, not a poll side effect.
- **Dispatch:** generated numeric-id → typed packet enum; `match`, not string
  tables; version selected once at connect time.
- **Native chunk decoding** in Rust; no sidecar processes.
- **Ownership:** single bot state object; N bots per process sharing static
  registries; no singletons.
- **DNS SRV** via a resolver crate, never shelling out.
- **Typed errors** (`Result`, `thiserror`) instead of `nil, err` strings.
- **Chunk cache** as bounded LRU over decoded sections with real memory-cost
  eviction.

## Minecraft 1.21.4 protocol facts (verified against live servers)

Protocol version: **769**.

### Handshake (C2S 0x00, state=handshaking)

`{protocol_version: VarInt, server_address: String, server_port: u16 BE,
next_state: VarInt}`. `next_state`: 1 = status, 2 = login.

### Login

1. C2S **Login Start** (0x00): `{username: String, uuid: u128 (16 bytes)}`.
   Offline mode accepts an all-zero / offline-derived UUID.
2. S2C **Encryption Request** (0x01): `{server_id: String, public_key:
   DER ByteArray, verify_token: ByteArray, should_authenticate: bool}`.
3. Client generates a random 16-byte shared secret, RSA-PKCS1v1.5-encrypts
   the shared secret and the verify token *separately* against the server
   public key, replies C2S **Encryption Response** (0x01):
   `{shared_secret: ByteArray, verify_token: ByteArray}`.
4. Both sides enable **AES-128-CFB8 on the entire byte stream** with
   key = IV = shared secret. No padding. Applies to length prefixes too.
5. S2C **Set Compression** (login state): `{threshold: VarInt}`.
6. S2C **Login Plugin Request** → respond with "not understood"
   (message id echoed, no data).
7. S2C **Login Success** (0x02): `{uuid, username, properties:
   [{name, value, has_signature, signature?}], strict_error_handling: bool}`.
8. C2S **Login Acknowledged** (0x03) → connection enters **configuration**.

### Compression framing

Once a threshold `t ≥ 0` is set, every packet body is prefixed with a VarInt
`data_length`:

- `data_length == 0` → the rest is the raw, uncompressed packet.
- `data_length > 0` → the rest is zlib-compressed and inflates to exactly
  `data_length` bytes; **verify the inflated length**.

A packet is compressed iff its uncompressed size is `>= t`. Threshold nil or
negative means compression disabled. Max packet size 2 MiB is a sane bound.

### Configuration

Echo S2C keep-alive / ping / `select_known_packs` payloads back verbatim
(echoing known-packs means claiming to know every pack — works, but it is a
lie). S2C **Finish Configuration** → reply C2S Finish Configuration →
**play** state.

### Play state essentials

- **Keep-alive:** i64 id, echo back. Servers disconnect silent clients;
  treat 30 s without keep-alive as dead.
- **Synchronize Player Position (1.21.4 layout):** teleport id VarInt
  **first**, then x/y/z f64, **dx/dy/dz f64 (velocity)**, yaw/pitch f32,
  flags u32. Flag bits 0x01/0x02/0x04/0x08/0x10 = relative X/Y/Z/yaw/pitch.
  Must send **Confirm Teleport** with the teleport id.
- **Packed block position:** `x<<38 | z<<12 | y` — 26/26/12 signed bits.
- **multi_block_change:** section coords packed u64 (22/22/20 signed bits);
  each record VarInt `state_id<<12 | local_pos`, where
  `local_pos = y + (z<<4) + (x<<8)`.
- **Chunk unload (1.21.4 quirk):** fields arrive **chunkZ first, then chunkX**.
- **Entity velocity:** three i16, divide by 8000 → blocks/tick.
- **map_chunk:** chunkX/Z i32, heightmaps anonymous NBT, VarInt length + raw
  section data, trailing block entities / light data.
- **Chat C2S:** timestamp ms, salt, signature, offset, fixed 3-byte
  `acknowledged` bitset (last 20 messages). Unsigned chat only works where
  secure chat is not enforced. `/`-prefixed messages go as `chat_command`.
- **Container click:** needs monotonic `stateId`, per-window action number,
  changed-slots array, cursor item. Slot in 1.21.4 = `item_count VarInt
  (0 ⇒ empty)`, `item_id VarInt`, then added/removed data-component arrays.
- **Respawn:** `client_command` actionId 0.
- **Kicks:** disconnect payloads carry translation keys such as
  `multiplayer.disconnect.duplicate_login` — useful for reconnect policy.

### VarInt / VarLong

VarInt: max 5 bytes, 7 bits per byte, continuation bit 0x80, two's complement
i32. VarLong: max 10 bytes, i64. Enforce the byte caps while decoding;
a 6th/11th byte means malformed input.

## Ideas worth keeping

- "New MC version released → regenerate everything → run tests" automation
  (reimplement as Rust xtask / CI job).
- Packet-structure hash tests as drift detector between generated code and
  upstream minecraft-data.
- Minor-version fallback resolution (`1.21.x` → latest known patch).
- Forced hotbar sync (Set Held Item) right after login — servers expect it.
- Kick-reason sniffing via translation keys for reconnect policy.
- Mock-server-that-reads-packet-ids-from-generated-data test trick.
- Bounded read bursts when draining sockets; bounded everything by default.
- Fail-open degradation as a general resilience principle (lose a
  capability, not the connection).
