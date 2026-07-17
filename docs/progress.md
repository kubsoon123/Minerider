# MineRider — Progress Log

## Milestone: BAC analysis (commit `docs: analyze BAC architecture`)

**Completed:**
- Full analysis of the retired BAC prototype (LuaJIT Minecraft 1.21.4 client).
- All reusable knowledge captured in `docs/bac_analysis.md`: protocol facts
  (protocol 769, login/encryption/compression flow, play-state quirks),
  architecture mistakes to avoid, ideas worth keeping.
- BAC directory deleted; no old code remains.

**Files changed:** `docs/bac_analysis.md`, `.gitignore`

**Tests:** n/a (documentation only)

**Problems:** none

**Next step:** initialize the Rust architecture.

---

## Milestone: Rust skeleton (commit `chore: initialize MineRider Rust architecture`)

**Completed:**
- Cargo project `minerider` 0.1.0 with the dependency set mandated by the
  architecture: tokio, bytes, serde, tracing, thiserror, rand, rsa, aes,
  cfb8, flate2.
- Module skeleton under `src/`: `core`, `network`, `protocol`, `minecraft`,
  `crypto`, `compression` — matching the target repository structure.
- Shared error type (`core::error::MineRiderError`) and connection state
  enum (`core::state::ConnectionState`).
- README with architecture overview.

**Files changed:** `Cargo.toml`, `src/**`, `README.md`

**Tests:** `cargo build` clean; no tests yet.

**Problems:** none

**Next step:** Phase 1 — connection core (VarInt/VarLong, framing,
compression, RSA/AES, handshake → login → configuration → play).

---

## Milestone: Phase 1 connection core (commit `feat: implement Minecraft networking core`)

**Completed:**
- Protocol primitives: VarInt/VarLong (5/10-byte hard limits), bounds-checked
  `PacketReader`/`PacketWriter` (all primitives, strings, byte arrays, UUID,
  packed positions), `RawPacket`, compression-aware `FrameCodec`
  (2 MiB frame cap, exact `data_length` verification, non-consuming
  incomplete-frame handling).
- zlib helpers with expected-length enforcement and zip-bomb cap.
- Crypto: RSA PKCS#1 v1.5 encryption of the login exchange (DER
  SubjectPublicKeyInfo), AES-128-CFB8 stream cipher (key = IV = shared
  secret, state preserved across arbitrarily chunked reads/writes).
- `Connection`: tokio TCP, nodelay, 8 KiB buffered reads (never per-byte
  syscalls), 30 s read timeout, decrypt-before-parse ordering, clean
  shutdown. `Connection::from_tcp_stream` lets tests reuse it server-side.
- Minecraft state flow for protocol 769 (1.21.4): handshake → login
  (LoginStart, EncryptionRequest/Response, SetCompression, LoginSuccess,
  LoginAcknowledged, plugin requests answered "not understood", 64-packet
  bound) → configuration (keep-alive/ping echo, SelectKnownPacks echo,
  FinishConfiguration, 512-packet bound) → play (keep-alive echo loop).
- `Client`/`ClientConfig` high-level API and a CLI:
  `minerider <host> <port> <username>`.
- Mock-server integration tests (Rust port of BAC's best test idea): full
  encrypted + compressed login flow against a real socket, keep-alive echo
  verification, no-encryption server variant, malformed-frame and
  mid-frame-close error paths.

**Files changed:** all of `src/**`, new `tests/` (common mock + 3 integration
test files), `Cargo.lock`.

**Tests:** 42 tests, all green (37 unit + 5 integration).
`cargo build -q` and `cargo clippy --all-targets -- -D warnings` clean.

**Phase 1 checklist:**
- [x] Cargo build works
- [x] Unit tests pass
- [ ] Connects to Paper 1.21.4 — **not verified**: no Paper server available
  in this environment. The mock server implements the exact 1.21.4 packet
  layout, so semantics are covered, but a real-server run is still owed.
  Try: `cargo run -- <host> <port> <username>` against an offline-mode
  Paper 1.21.4 server.
- [x] Login works (mock, exact 1.21.4 layout)
- [x] Encryption works (RSA + AES-128-CFB8, chunked-stream verified)
- [x] Compression works (threshold semantics, both paths)
- [x] PLAY state reached (mock)

**Problems / known limitations:**
- AES-128-CFB8 is driven per byte (cipher-0.4 `AsyncStreamCipher` consumes
  `self`); correct but slower than a bulk path — revisit in the performance
  phase.
- Offline mode only (uuid = 0); Mojang session join not implemented.
- Play-state keep-alive ids (0x27/0x1a) are hardcoded until the phase 2
  minecraft-data generator makes all ids data-driven.
- SelectKnownPacks is echoed verbatim (claims all packs known) — matches
  vanilla clients but should be revisited with real registry handling.

**Next step:** Phase 2 — minecraft-data packet pipeline: generator producing
`crates/minerider-protocol/src/generated/` (packet ids, structs,
serializers/deserializers) plus encode/decode round-trip tests. Real-server
validation of Phase 1 should happen in parallel with that.

---

## Milestone: Phase 1 hardening (commits `refactor: …`, `docs: …`)

**Completed:**
- Workspace split per the design requirement: new standalone
  `minerider-protocol` crate (wire primitives, codec, zlib, RSA/AES — zero
  game logic, zero tokio); root `minerider` crate keeps network, minecraft
  state flow and client. Own `ProtocolError` type; client error wraps it.
- Allocation cleanup: decrypt-in-place reads (per-read `to_vec()` gone),
  zero-copy `read_string`/`Cow` decode path, `Handshake<'a>` borrows the
  address, echo arms de-duplicated in configuration/play.
- AES-128-CFB8 rewritten as one bulk call over `Aes128Enc` with in-struct IV
  shift registers (correctness pinned by an openssl known-answer vector).
  Measured parity with the old per-byte path — CFB8 is AES-latency-bound;
  the win is API cleanliness, not throughput.
- New protocol primitives with tests: Identifier (vanilla charset),
  Option, length-prefixed arrays; byte-exact vanilla vectors for
  VarInt/VarLong/Position/UUID.
- 13 new stream-level integration tests: 1-byte dribbles, multi-frame
  segments, split frame-length VarInt, garbage-input robustness,
  encryption+compression wire ordering, clean shutdown, reconnect, timeouts.
- Criterion benches in `crates/minerider-protocol/benches/protocol.rs`.
- `docs/architecture.md`: crate diagram, module responsibilities, state
  machine, byte-level pipeline order, benchmark numbers, testing strategy.
- rustfmt + clippy (`--workspace --all-targets -- -D warnings`) clean;
  unused `serde` dependency dropped.

**Files changed:** full workspace restructure — `crates/minerider-protocol/**`
(new), `Cargo.toml`, `src/**` rewired, `tests/**` (rewired + `stream.rs`
new), `README.md`, `docs/architecture.md`, `docs/progress.md`.

**Tests:** 68 green (was 42): 49 protocol unit + 1 client unit + 18
integration.

**Benchmarks (i5-11400F, release, medians):** VarInt decode 1.13 GiB/s;
frame decode 6.8–15.4 GiB/s raw, 210 MiB/s–2.1 GiB/s compressed; AES-CFB8
~62 MiB/s (AES-latency-bound, documented).

**Problems / remaining issues before Phase 2:**
- Real Paper 1.21.4 validation still owed (no server in this environment).
- `ConnectionState` transitions not enforced; Status (server-list ping)
  path unimplemented.
- Play-state packet ids hardcoded for 769 — Phase 2 generator replaces them.
- Online-mode auth and login plugin handling intentionally unimplemented.

**Next step:** Phase 2 — minecraft-data packet pipeline
(`crates/minerider-protocol/src/generated/`, encode/decode round-trip tests).

---

## Milestone: Phase 1 audit fixes (engineering audit, uncommitted)

**Completed:**
- Corrected two 1.21.1-vintage protocol facts against PrismarineJS
  `minecraft-data` `data/pc/1.21.4/protocol.json`:
  - Play-state keep-alive ids: S2C **0x27** / C2S **0x1a** (was 0x26/0x18;
    every real 1.21.4 server's keep-alives were silently ignored).
  - Login Success layout is `{uuid, username, properties}` — the phantom
    trailing `strict_error_handling` bool read (which broke every real login
    with BufferUnderflow) is gone.
- Disconnect reasons in configuration (0x02) and play (kick_disconnect
  0x1d) are network NBT text components, not strings: now read as the raw
  remaining payload kept as lossy UTF-8 (human-readable; proper NBT decoding
  lands with the generated protocol in Phase 2). Login disconnect (0x00)
  stays a JSON string, per the data.
- DoS hardening in `FrameCodec::try_decode`: a compressed frame's declared
  `data_length` above the 2 MiB packet cap is rejected before decompression
  (compression.rs's 64 MiB cap remains as defense in depth).
- DoS hardening in `PacketReader::read_array`: a count above the remaining
  bytes is rejected with BufferUnderflow before `Vec::with_capacity`, so a
  hostile VarInt can no longer trigger a giant preallocation.

**Files changed:** `src/minecraft/{play,login,configuration}.rs`,
`crates/minerider-protocol/src/{codec,buffer}.rs`,
`tests/common/mod.rs`, `tests/config_disconnect.rs` (new),
`docs/{bac_analysis,progress}.md`.

**Tests:** 71 green (was 68): 51 protocol unit (was 49: +oversized
`data_length` rejected, +`read_array` huge-count rejected without
allocation), 1 client unit, 19 integration (was 18: +configuration
disconnect with NBT reason surfaces as `Disconnected`).
`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo fmt --all --check` clean.

**Problems:** none

**Next step:** unchanged — Phase 2 minecraft-data packet pipeline.

---

## Milestone: Phase 2 — minecraft-data packet pipeline (complete)

**Completed:**
- `minerider-codegen` crate: protocol.json parser → resolved IR → Rust
  emitter, vendored minecraft-data 1.21.4 (protocol 769) as the single
  source of truth.
- Generated output in `crates/minerider-protocol/src/generated/`:
  237 packets (handshaking 2/0, status 2/2, login 6/5, configuration
  17/10, play 131/62 clientbound/serverbound) + 53 shared types +
  `ProtocolVersion` model. Strongly typed packet enums with integer-match
  dispatch, `Encode`/`Decode` both directions.
- Hard problems solved deliberately (nothing skipped): nested switches
  (scoreboard objective), switch-on-option discriminants flattened to
  `Option<i64>`, recursive types boxed via per-direction Tarjan SCC,
  direction-local name collisions suffixed
  (`PacketEncryptionBeginServerbound` — this fixed a wire-critical
  serverbound encryption layout), shared types referenced from `types.rs`
  instead of duplicated.
- Client refactor: handshake/login/configuration/play and the mock server
  use generated structs, id constants and `ProtocolVersion`; zero
  hardcoded 1.21.4 packet ids remain in runtime code. Disconnect reasons
  decoded as NBT text components.
- Tests: 129 green (was 71). New: 19 golden byte-exact vectors, enum
  round-trips for all 44 small-state packets + 9 play packets (nested
  switches, NBT, metadata loop, holder sets), 10 malformed-input tests,
  drift gate + determinism test. Clippy `-D warnings` and rustfmt clean.
- Benchmarks (generated): keep-alive decode 14.2 ns, encode 135.7 ns;
  settings decode 62.3 ns, encode 147.1 ns; int-match dispatch 1.1 ns vs
  string if-chain baseline 2.0 ns; play dispatch over 131 ids 1.5 ns.

**Files changed:** `crates/minerider-codegen/**` (new crate),
`crates/minerider-protocol/src/generated/**` (new),
`crates/minerider-protocol/tests/{golden,roundtrip,malformed_generated}.rs`
(new), `crates/minerider-protocol/benches/protocol.rs`,
`src/{core,minecraft}/**`, `tests/common/mod.rs`, `docs/**`.

**Tests:** 129 passed, 0 failed (`cargo test --workspace`).

**Problems:**
- Real Paper 1.21.4 validation NOT performed (no server available). Gate
  remains open; command: `cargo run -- <host> <port> <username>`.
- minecraft-data models `number_format` in scoreboard packets as
  `option varint`; wire-compatibility vs vanilla (varint enum 0/1/2 in
  older docs) is inherited from the data and needs the Paper validation
  to confirm.

**Next step:** Phase 3 — tick engine, player/world/entity state on top of
the generated packet layer.

---

## Vanilla conformance infrastructure (capture, coverage, scenarios 1-4)

**Completed:**
- Trace capture (`src/trace/`): JSONL format with mono/relative timestamps,
  tick, direction, state, id, generated name, decoded fields (serde on
  generated packets), payload hex, crypto flags, size, session, scenario,
  step. Recorder hooked into `Connection::send_packet`/`read_packet`;
  `Client::connect_with_trace` captures the full session from handshake.
- Normalization (`src/trace/normalize.rs`): session-specific values become
  stable symbols preserving cross-packet relations — `UUID_N`,
  `TELEPORT_ID_N`, `KEEPALIVE_ID_N`, `ENTITY_ID_N`, `TIMESTAMP_N`,
  `SALT_N`, `SEQUENCE_N`, `SERVER_HOST/PORT`, `SESSION_N`. Absolute
  timestamps and raw payloads redacted in fixtures.
- Semantic diff engine (`src/trace/diff.rs`): first meaningful divergence
  (missing/extra/wrong packet, wrong field, timing violation) with fallout
  grouping; timing tolerance classes (strict/tick-bound/periodic/
  best-effort) driven by the coverage obligation table.
- Packet coverage classification (`src/minecraft/coverage.rs`): every
  clientbound login/configuration/play packet classified as
  handled/ignored/stored/unsupported with vanilla obligations (response,
  timing, state update, status, scenario, evidence). Structured warnings
  in play/configuration loops — no packet disappears silently.
- Obligation matrix `docs/vanilla_conformance_1_21_4.md` generated from
  the live coverage table against generated registries
  (`cargo run --bin conformance_matrix`); drift test keeps it in sync.
- Scenarios (`tests/conformance.rs` + 3 new mock server modes):
  1. configuration completion — PASS vs mock, fixture committed
  2. join and idle (play login + 3 keep-alives) — PASS vs mock, fixture
     committed; keep-alive echo correlation verified
  3. initial chunk streaming — initially BLOCKED with
     `MissingPacket: chunk_batch_received`; resolved in the real-server
     validation milestone below
  4. teleport correction — initially BLOCKED with
     `MissingPacket: teleport_confirm`; resolved in the real-server
     validation milestone below

**Files changed:** `src/trace/**` (new: format, decode, recorder,
normalize, diff), `src/minecraft/coverage.rs` (new),
`src/bin/conformance_matrix.rs` (new), `src/core/client.rs`,
`src/minecraft/{mod,play,configuration}.rs`, `src/network/connection.rs`,
`crates/minerider-codegen/src/emit.rs` (CLIENTBOUND_IDS/SERVERBOUND_IDS),
`tests/conformance.rs`, `tests/conformance/fixtures/*.jsonl`,
`tests/common/mod.rs`, `tests/{trace,conformance_matrix}.rs`,
`docs/vanilla_conformance_1_21_4.md`.

**Tests:** 146 passed, 0 failed (`cargo test --workspace`). Clippy
`-D warnings` and rustfmt clean; codegen drift and matrix drift gates OK.

**Problems:**
- At this milestone, no vanilla/Paper 1.21.4 server was available and
  scenarios 3-4 were blocked. Both items were exercised and resolved in
  the real-server validation milestone below.

**Historical next step:** validate against a real 1.21.4 server before
starting Phase 3.

---

## Local Paper / vanilla 1.21.4 validation (complete)

**Completed:**
- Added reproducible localhost-only Paper 1.21.4 build 232 management under
  `.test-servers/`, using a repository-local Temurin 21 runtime. Downloads
  are checksum-verified and all generated server state is git-ignored.
- Reached play against Paper and the official Mojang vanilla 1.21.4 server
  (protocol 769), with compression, configuration, and keep-alive handling.
- Confirmed the two trace-derived protocol gaps and fixed them separately:
  `teleport_confirm` for synchronized positions and
  `chunk_batch_received` for completed vanilla chunk batches.
- Revalidated deterministic teleport corrections against Paper and vanilla.
  Vanilla produced 21 chunk batches and MineRider acknowledged all 21.
- Completed a 55-minute Paper soak: 219/219 keep-alives, 959,801 packet
  events, zero unknown packets, 7.8 MB to 7.89 MB RSS, and no unplanned
  disconnect. The client then handled a controlled RCON kick normally.
- Reduced ignored-packet warnings to one per packet ID; the 55-minute client
  log stayed at 8.4 KiB despite a 343.4 MiB streamed trace.

**Files changed:** local server scripts, `src/minecraft/play.rs`, coverage
fixtures/tests, conformance evidence, and the real-server validation docs.

**Tests:** 146 passed, 0 failed. Rustfmt, clippy `-D warnings`, protocol
codegen drift, and conformance-matrix drift gates pass.

**Remaining limitation:** a manually authorized official vanilla-client
reference capture has not been performed, so vanilla-client parity remains
PARTIAL. The credential-safe procedure is in
`docs/real_server_validation.md`.

**Next step:** end this validation phase. Phase 3 is outside this milestone.

---

## Phase 3a — tick engine + player/entity state (complete)

**Completed:**
- Tick engine (`src/core/tick.rs`): a fixed 20 TPS `TickClock` — a plain
  monotonic counter with no embedded time source, plus `TICK_DURATION`
  (50 ms) and a `ticks_in(Duration)` helper. The play loop owns the real
  `tokio` interval; the clock stays pure and unit-testable.
- Local player state (`src/minecraft/player.rs`): `LocalPlayer` with own
  entity id (from play `login`), server-confirmed position (relative-flag
  application moved here from `play.rs` as `PlayerPosition`), health/food/
  saturation (`update_health`), and experience (`experience`). Vitals
  default to the vanilla spawn values.
- Entity state (`src/minecraft/entity.rs`): `EntityStore`, a table of
  tracked non-local entities. Applies the full movement family —
  `spawn_entity`, `entity_destroy`, `rel_entity_move` (1/4096-block
  fixed-point deltas), `entity_move_look`, `entity_look`, `entity_teleport`,
  `sync_entity_position`, `entity_velocity` (1/8000-block/tick), and
  `entity_head_rotation` (protocol angle bytes → degrees). Relative moves for
  an un-spawned entity are ignored (no absolute base to apply against).
- Play loop rewrite (`src/minecraft/play.rs`): a `tokio::select!` between the
  packet stream (`biased`, so packets drain before ticks) and the tick
  interval, advancing the clock each tick. `read_packet` is cancellation-safe
  (its only await is `TcpStream::read`, and incomplete frames stay buffered),
  so dropping the read future on a tick loses no bytes. A new `PlayState`
  (player + entities + clock) is threaded through; state-only packets are
  decoded and folded in by `apply_state_packet` and carry no wire response.
  No new serverbound traffic — the validated idle/keep-alive/teleport/chunk
  wire behavior is unchanged, so the conformance fixtures still match.
- Coverage table (`src/minecraft/coverage.rs`): `login`, `update_health`,
  `experience` and the nine entity packets reclassified from ignored/default
  to `Handled` with an honest `PARTIAL` status and a new `EVIDENCE_UNIT`
  string ("unit-tested state projection; mock/vanilla capture pending").
  Conformance matrix regenerated.

**Files changed:** `src/core/tick.rs`, `src/minecraft/{player,entity}.rs`
(new), `src/minecraft/{mod,play,coverage}.rs`,
`docs/vanilla_conformance_1_21_4.md`, `docs/progress.md`.

**Tests:** 161 passed, 0 failed (was 146): +15 unit tests (4 tick, 5 player,
6 entity). Rustfmt, clippy `--workspace --all-targets -D warnings`, protocol
codegen drift, and conformance-matrix drift gates pass.

**Problems / limitations:**
- The new state projections are unit-tested but have not been re-run against a
  live server with the phase-3 handling active; status is `PARTIAL`, evidence
  `EVIDENCE_UNIT`. A soak + entity-heavy capture would raise them.
- The tick loop advances the clock but has no per-tick side effects yet
  (idle movement, physics). Serverbound movement is the next sub-step.
- `sync_entity_position` is treated as absolute position + velocity deltas;
  no relative-flag handling (the generated struct exposes none for 1.21.4).

**Next step:** per-tick behavior — serverbound player movement each tick and
the physics/gravity step — then world/chunk block storage.

---

## Phase 3b — vanilla configuration parity: client_information + brand

Goal for this and following phases: MineRider's serverbound packet stream
should match a real vanilla 1.21.4 client 1:1. First gap closed: a vanilla
client sends `client_information` (settings) and a `minecraft:brand` plugin
message on entering configuration; MineRider sent neither.

**Completed:**
- `minecraft/mod.rs`: `vanilla_client_information()` (the settings a fresh
  vanilla install sends — `en_us`, render distance 12, chat enabled+colored,
  all skin layers `0x7f`, right hand, no text filter, server listing on, all
  particles) and `brand_payload()` (the brand written as a length-prefixed
  Minecraft string; the custom-payload `data` field is a raw rest-buffer, so
  the bytes are exactly `07 "vanilla"`). Unit-tested for exact bytes/values.
- `configuration.rs`: `run_configuration` now sends `settings` then the
  brand `custom_payload` on entry, before processing the server's packets,
  matching the documented vanilla join order.
- Mock server (`tests/common`): after Login Acknowledged it now reads and
  asserts the client's `settings` (0x00) and `minecraft:brand`
  `custom_payload` (0x02), locking the behavior in for every
  configuration-reaching scenario.
- Conformance fixtures (scenarios 1-4) regenerated: the normalized captures
  now include the two new serverbound configuration packets (payloads
  redacted as before; settings fields decoded, brand data `[7, "vanilla"]`).

**Files changed:** `src/minecraft/{mod,configuration}.rs`,
`tests/common/mod.rs`, `tests/conformance/fixtures/*.jsonl`,
`docs/progress.md`.

**Tests:** 161 passed, 0 failed (+2 unit tests for brand bytes and settings
values; the four conformance scenarios re-pass against regenerated fixtures).
Clippy `-D warnings`, rustfmt, codegen drift and conformance-matrix drift
gates all clean.

**Problems / limitations:**
- This matches *documented* vanilla serverbound behavior and is verified by
  golden bytes + mock assertions + self-consistency fixtures, but **byte-1:1
  parity with a real vanilla client is still gated on the manual reference
  capture** (see `docs/real_server_validation.md`, "Manual vanilla-client
  reference capture" — not performed, needs an authorized account). The
  intra-configuration ordering (client settings vs. brand vs. the server's
  packets) and the render-distance/locale values are the fields most likely
  to need alignment once that capture exists; they are isolated in
  `vanilla_client_information()`.

**Remaining vanilla serverbound gaps (next steps):**
- Play: per-tick movement via the vanilla `sendPosition` logic (position
  reminder forces a `set_player_position` at least once per second even when
  idle; `move_player_status_only` on ground-state change).
- Play: `player_loaded` after the world finishes loading (1.21.x).
- Play: whether vanilla re-sends `client_information` on play entry.
- `select_known_packs`: currently echoes the server's list; vanilla sends its
  own known-packs list (`minecraft:core` + version).

**Next step:** play-state per-tick movement (`sendPosition`) + `player_loaded`.

---

## Phase 3c — vanilla play readiness + movement cadence (complete)

**Completed:**
- Reproduced the mapped vanilla 1.21.4 `LocalPlayer.sendPosition()` decision
  state in `minecraft/player.rs`: movement threshold `(2.0e-4)^2`, forced
  position reminder every 20 eligible ticks, packet choice order
  `position_look` → `position` → `look` → `flying` (status-only), and the
  packed `on_ground` / `horizontal_collision` bits.
- Added a delayed 20 TPS play ticker (first tick after 50 ms, not Tokio's
  immediate interval tick) and exact generated-packet encoding in `play.rs`.
- Added minimal honest load readiness: a server position, at least one decoded
  chunk coordinate, and completion of the initial chunk batch are required
  before the empty `player_loaded` packet is sent once. Movement starts only
  after that transition.
- Strengthened `join_idle`: the mock sends login + synchronize-position + an
  initial chunk batch, requires teleport confirmation, chunk-batch ack and
  `player_loaded`, then waits for the unchanged position reminder and verifies
  exact coordinates and flags before exchanging keep-alives. The trace fixture
  records the whole order and conformance asserts one `player_loaded` before
  movement.
- The trace comparator no longer assigns an unrelated clientbound response
  deadline to voluntary play packets (`player_loaded` and movement).
- Conformance coverage/matrix now records that chunk coordinates participate in
  readiness while full palette/block storage remains pending.

**Evidence:** official Mojang 1.21.4 client artifact/mappings (published SHA-1),
Yarn mapped names, generated protocol 769 layouts, unit tests and the local mock
wire trace. A real vanilla-client loopback packet capture remains the final
byte/timing parity authority.

**Tests:** 166 passed, 0 failed (workspace total; +5 movement decision tests).
All four conformance scenarios pass; clippy `--workspace --all-targets -D
warnings`, rustfmt, protocol-codegen drift and conformance-matrix drift pass.

**Limitations / next step:**
- The readiness model stores chunk coordinates rather than full chunk sections;
  implement palette/block world storage and compare the exact vanilla
  `LevelLoadStatusManager` player-chunk visibility condition.
- Physics still does not alter position/on-ground/collision. The movement
  transmitter is exact for its state inputs; gravity/collision must supply
  vanilla-equivalent state next.
- `select_known_packs` remains an echo rather than vanilla's own known-pack
  selection. This is the next configuration parity gap.

---

## Phase 3d — dimension-aware world + collision/gravity foundation (complete)

**Completed:**
- Configuration `registry_data` now retains `minecraft:dimension_type` values
  (`min_y`, height, logical height, coordinate scale, ultrawarm/ceiling) and
  threads them through `Client` into play. Login's dimension index selects the
  active type; section count is no longer hardcoded to the Overworld.
- New `minecraft/world.rs`: bounded 1.21.4 section decoder for all paletted
  container modes (single, indirect and direct), non-spanning packed longs,
  X-fastest indexing, biome palettes, negative chunk coordinates, unload,
  single-block and section multi-block updates.
- Vendored pinned 1.21.4 `blocks.json` and `blockCollisionShapes.json`. A
  reproducible generator emits deduplicated AABB mappings for all 27,866 global
  block-state ids and 4,989 collision shapes; unknown ids fail rather than
  silently becoming full blocks.
- New `minecraft/physics.rs`: player/world AABBs, voxel-shape collection and
  vanilla axis clipping order (Y, then the longer horizontal component), with
  vertical/horizontal collision and `on_ground` results.
- Local player now applies synchronize-position velocity relative flags and an
  explicit normal-air idle-survival branch: gravity `0.08`, collision clipping,
  velocity zeroing on clipped axes and drag (`0.98` vertical, `0.91`
  horizontal) before Phase 3c packet selection.
- The mock now sends a real dimension registry and valid 24-section chunk with
  a paletted stone floor. Join-idle verifies grounded movement flags instead of
  relying on an empty synthetic chunk.

**Verification:** 170 workspace tests pass, 0 fail. Four conformance scenarios,
strict clippy, rustfmt, protocol codegen drift and conformance-matrix drift pass.

**Honest limits / next:**
- This completes the no-input/no-effect/no-fluid survival foundation, not every
  `LivingEntity.travel` branch. Walking acceleration, jumping, sprinting,
  crouching, step-up choice, fluids, ladders, effects and attribute modifiers
  remain explicit follow-ups.
- Collision data is pinned from minecraft-data's 1.21.4 generated dataset; a
  drift check for the new generated Rust table should be added to CI.
- A local Paper 1.21.4 physics-enabled soak completed on 2026-07-15: the client
  remained connected, decoded the live dimension/chunk stream and sent
  `player_loaded` on tick 5. Official-vanilla/client-reference comparison is
  still required before raising byte/timing evidence beyond `PARTIAL`.

---

## Phase 3e — vanilla input movement: walk/sprint/jump (complete)

**Completed:**
- New `MovementInput` control surface on `LocalPlayer` (forward/strafe impulses
  in `-1.0..=1.0`, plus jump/sprint/sneak), the Mineflayer-style intent applied
  each physics tick. This is the input half of `LivingEntity.travel`/`aiStep`
  that the Phase 3d collision foundation was built to receive.
- `tick_physics` implements the normal (non-fluid, non-elytra, no-effect)
  branch in vanilla order: sub-`0.003` per-axis velocity clamp → ground jump
  (`0.42` impulse, plus the sprint-jump `±0.2` horizontal boost) → `moveRelative`
  input acceleration using the friction-influenced speed → `move` collision
  clip → gravity → drag. Ground speed uses the `0.21600002 / friction³`
  attribute scaling (walk `0.1`, sprint `0.13`); air speed uses `0.02` / `0.026`.
  Sneaking scales impulses by `0.3`; a `10`-tick ground-jump cooldown matches
  vanilla `noJumpDelay`.
- **Gravity-ordering correction:** the Phase 3d idle branch applied gravity
  *before* the move, which lands one tick early and converges to a `-4.0`
  terminal velocity. Reconstructing 1.21.4 `LivingEntity.travel` shows gravity
  is applied to the *post-move* velocity, so stored `v.y = (moved_y - 0.08) *
  0.98`. `tick_physics` now does this, reproducing vanilla's `-3.92` terminal
  velocity, `-0.0784` first-fall-tick velocity and `1.2522`-block jump apex.
  Unit tests pin these constants so behavior is anchored to physics, not to the
  (self-generated) conformance fixture.
- Vanilla `Mth.sin`/`Mth.cos` lookup tables (65536 entries, `10430.378` index
  scale) in `physics.rs` drive input rotation, so a server's own movement
  re-simulation agrees with ours for any non-cardinal facing — not just the
  cardinal directions where `Math.sin` and the table coincide.

**Verification:** 185 workspace tests pass, 0 fail (+9 movement/trig tests).
Four conformance scenarios, strict clippy (`--workspace --all-targets -D
warnings`), rustfmt, protocol codegen drift and conformance-matrix drift pass.

**Fixture note:** the corrected gravity ordering shifts the join-idle scenario's
first `on_ground=true` report from tick 2 to tick 3 (a resting player has
`v.y = 0`, so its first physics tick detects no ground and lands the next tick,
exactly as vanilla does). `join_idle.jsonl` was regenerated to record this; the
other three fixtures were unchanged (their diffs were pure run-timing noise).

**Honest limits / next:**
- Block friction is the default `0.6`. Ice, packed/blue ice, slime, honey and
  soul sand need a friction table keyed by the block under the player; their
  movement is currently wrong. This is the next parity gap for input movement.
- Rotation uses the exact `Mth` table, but positions are still driven by our own
  `f64` arithmetic; a real vanilla-client capture of walking/jumping remains the
  authority before raising this beyond `PARTIAL`.
- Still unimplemented `travel` branches: fluids (water/lava), ladders/climbables,
  elytra, step-up assist, auto-jump, movement-affecting effects (speed, jump
  boost, slow falling, levitation) and attribute modifiers beyond sprint.
- The input is not yet wired to a scripting/bot API surface; the play loop
  applies whatever `LocalPlayer.input` holds (default: none), so on-server
  behavior is unchanged until a controller sets it.

---

## Phase 3f — bot control API, block friction, step-up (complete)

**Completed:**
- **Control API (`minecraft/control.rs`).** A cloneable `ControlHandle` pushes
  `BotCommand`s (`walk_to`, `look`, `set_input`, `sprint`, `sneak`, `jump`,
  `stop`) over an unbounded channel into the play loop; `Client::control()`
  hands one out. Each tick a `Controller` folds the active goal and manual
  overlay into the player's `input` and facing before physics. `walk_to` steers
  yaw with `atan2(-dx, dz)` and holds forward until within `0.3` blocks. The
  play `select!` gained a command branch that disables itself once all handles
  drop, so a closed channel can't spin the loop. An end-to-end test walks a bot
  to a target through real physics.
- **Block friction (generated).** `generate_collision_data.py` now emits
  `FRICTION_OVERRIDES` (ice/packed/frosted ice `0.98`, blue ice `0.989`, slime
  `0.8`; everything else the default `0.6`), resolved from vendored `blocks.json`
  state ranges. `tick_physics` samples the block at `floor(feetY - 0.2)` below
  the player, so ground speed and drag are now surface-correct — ice is slippery,
  slime grippy — pinned by tests (ice out-slides stone, override values match).
- **Auto step-up (`collide_with_step`).** Vanilla's `Entity.collide` two-probe
  step algorithm: a blocked grounded move retries lifted by `maxUpStep` (`0.6`),
  keeps whichever variant travels farther horizontally, then settles down. The
  collision core was refactored into a pure `clip_movement` that both `collide`
  and the step path share; the world query is expanded upward by the step height.
  A walking `walk_to` bot now climbs slabs/paths/single steps but not full
  blocks, and does not step while airborne (tested).

**Verification:** 197 workspace tests pass, 0 fail (+12 control/friction/step
tests). Four conformance scenarios unchanged, strict clippy, rustfmt (hand-
written files; the generated `collision_data.rs` keeps its compact generated
form), protocol codegen drift and the collision generator's own reproducibility
all pass.

**Capture harness (`docs/vanilla_capture.md`).** Documented the manual
procedure for a real vanilla-client byte/timing capture and diff (mitm proxy +
`MINERIDER_TRACE` + the existing `normalize`/`diff` tooling), with the scenario
matrix and the deltas to expect. This is the step that would raise movement
evidence past `PARTIAL`; it needs a human with a Minecraft account and cannot be
automated.

**Honest limits / next:**
- Movement effects (speed, jump boost, slow falling, levitation) and fluids
  (water/lava buoyancy + drag), ladders/climbables and elytra are still
  unimplemented. Effects specifically need local-player effect tracking
  (`entity_effect`/`remove_entity_effect`), which does not exist yet — their
  physics would be dead code until then. Do not run those parity scenarios.
- Honey-block and soul-sand *slowdowns* (velocity multipliers, distinct from
  friction) are not modeled.
- Positions are still `f64` throughout; vanilla mixes `f32`/`f64`. Sub-ULP
  drift is expected and only a real-client capture can bound it.
- `walk_to` is straight-line steering with no obstacle avoidance or pathfinding;
  it climbs steps but will push into walls it cannot step over.

---

## Phase 3g — knockback, respawn, and world-query resilience (complete)

Found live against a local Paper 1.21.4 server, not in unit tests: the bot
joined in creative (server `force-gamemode`), so hits were silently absorbed;
switching it to Survival and killing it exposed three real gaps at once.

**Completed:**
- **Self-targeted knockback.** `entity_velocity` was always forwarded to the
  generic remote-`EntityStore`, so a hit that targeted the bot's own entity id
  never reached its physics — zero knockback even outside creative. `play.rs`
  now compares the packet's id against `LocalPlayer.entity_id` and calls the
  new `LocalPlayer::apply_velocity` (an absolute set, 1/8000-block units, not
  additive) when they match. Verified: the bot now flies back and lands
  correctly on a real hit.
- **World-query resilience.** `World::collision_boxes` treated any block
  outside currently-loaded chunks as a fatal `Result::Err` that tore down the
  whole connection — triggered live when a post-death state pushed the query
  outside tracked terrain. It now returns `Ok(false)` for "not loaded yet" and
  the physics tick is skipped and retried next tick, while an unrecognized
  block-state id (a genuine table bug, not a streaming gap) is still fatal.
- **Auto-respawn.** The client had no death/respawn handling at all: once
  killed, it reconnected into Paper's persisted dead player state forever
  (`Health: 0`, climbing `DeathTime`), invisible and unable to be hit again.
  `handle_tick` now detects `!player.is_alive()` and sends
  `client_command(action_id=PERFORM_RESPAWN)` once (a `respawn_pending` flag
  prevents resending every tick); the `respawn` clientbound packet resets
  dimension/world exactly like play `login` and calls the new
  `LocalPlayer::on_respawn` (clears velocity/ground state, drops `loaded` so
  the existing wait-for-position-and-chunk gate re-runs cleanly). This is also
  just correct bot-framework behavior — Mineflayer auto-respawns by default,
  since there is no death screen to click.

**Verification:** 201 workspace tests pass (+4: knockback semantics and
displacement, world-query completeness, respawn state reset), strict clippy,
rustfmt (hand-written files only — an editor auto-format hook reformatted the
generated `collision_data.rs` mid-session twice; both times it was restored via
`python scripts/generate_collision_data.py`, confirming the generator stays the
source of truth), protocol codegen drift all pass. Confirmed live: after the
fix, `data get entity MineRiderBot Health` reports `20.0f` immediately on
reconnect with no manual intervention.

**Honest limits / next:**
- Respawn is immediate with no delay, unlike a human's death-screen click
  latency; if that pattern matters for a specific anti-cheat's heuristics, a
  configurable delay is a small follow-up.
- The respawn position/dimension come from whatever the server assigns (world
  spawn or bed); we do not yet expose a way to choose or override it.
- Death is detected by `health <= 0` after `update_health`; a kill packet
  sequence that omits or delays that update (uncommon, but not verified against
  a real server) would delay the respawn request correspondingly.

---

## Phase 3h — inventory/container ("GUI") state tracking (complete)

**Completed:**
- New `minecraft/inventory.rs`: a pure projection of every clientbound
  container packet, mirroring `entity.rs`/`world.rs`. `InventoryState` holds
  the player's own inventory (window id 0, always present), at most one open
  non-player container (`open_window`, replaced wholesale when a new one opens
  — matching vanilla, which implicitly closes the previous one), the single
  shared cursor item, and the server-selected hotbar slot.
- Wired into `PlayState.inventory` and `apply_state_packet`: `open_window`,
  `close_window`, `window_items` (full slot refresh + cursor), `set_slot`
  (single-index update, including the legacy `window_id: -1` cursor address),
  `set_cursor_item`, `craft_progress_bar` (container properties — furnace
  progress, enchanting-table levels/costs, ...) and `held_item_slot`.
  Server-driven surprises degrade gracefully rather than panicking: a
  `set_slot` index past the known window length grows the slot list instead of
  panicking; updates for a window id that isn't currently open/player-owned
  (stale/already-replaced) are silently ignored, matching this project's
  established stance on treating unexpected server data as recoverable.
- `coverage.rs` gained entries for all seven packets (previously falling
  through to `log_unhandled` warnings every time an inventory changed) plus a
  fix for `CLIENTBOUND_RESPAWN_ID`, which still carried its pre-Phase-3g
  `ignored(not_implemented(...))` entry — a real drift between code and the
  coverage table that `every_clientbound_id_is_classified` doesn't catch
  because it only requires *some* entry, not the *correct* one.
- Deliberately **not** implemented: sending `window_click` to actually
  interact with a menu. Each menu type (crafting, anvil, enchanting table,
  furnace, ...) has distinct slot semantics and shift-click/quick-move rules;
  this phase is the passive "always know what's in every GUI" foundation that
  interaction logic would sit on top of.

**Verification:** 213 workspace tests pass (+12: inventory module unit tests),
strict clippy, rustfmt (hand-written files), protocol codegen drift, and both
conformance-matrix tests (`conformance_doc_is_up_to_date`,
`every_clientbound_id_is_classified`) pass — the doc was regenerated via
`cargo run --bin conformance_matrix` after the coverage-table changes.

**Process note:** an editor auto-format hook reformatted the generated
`collision_data.rs` into verbose rustfmt style twice more this session
(previously seen in Phase 3g); both times restored via
`python scripts/generate_collision_data.py`. This is now a recurring friction
point — the honest-limits item from Phase 3d ("add a drift check for the
generated collision Rust table") would also make this class of accidental
edit fail loudly in CI instead of relying on manual vigilance.

**Honest limits / next:**
- No `window_click`/`set_creative_slot` sending: the bot can see every GUI but
  cannot act in one yet (take items, craft, place in a furnace, rename in an
  anvil). This is the natural next step once interaction is wanted.
- `state_id` (the window's revision counter) is tracked but unused; sending
  clicks correctly requires echoing the latest value, which only matters once
  `window_click` exists.
- Multi-block containers (double chests) and horse/donkey inventories
  (`open_horse_window`, a distinct packet) are covered by the same generic
  `Window` model but `open_horse_window` itself is not yet decoded/handled.

---

## Phase 3i — live state channel, resource-pack handling, username validation (complete)

Autonomous session (user away ~5h): three self-contained gaps closed before
the larger Phase 3j premium-login work below.

**Completed:**
- **`Client::bot_state()`**: a `tokio::sync::watch::Receiver<StateSnapshot>`,
  the read-side counterpart to `Client::control()`. `PlayState::snapshot`
  clones player/entities/inventory (not the full `World` — cloning chunk data
  every tick would be expensive; block queries are a future on-demand
  accessor) and the play loop publishes after every clientbound packet and at
  the end of every tick. `EntityStore` gained `Clone` (its `Entity` fields
  were already `Copy`) to make this possible. Verified live: a new
  conformance test spawns a bot against the join-idle mock, grabs a receiver
  before calling `run()`, and asserts it observes `loaded` flip to `true`.
- **Resource pack response.** Vanilla always answers `add_resource_pack` with
  `resource_pack_receive` (configuration *and* play state both have this
  exchange); MineRider previously never responded, which some servers would
  eventually time out and kick for. It now responds `Declined` — the same
  honest choice a real player unchecking "Server Resource Packs" makes, not a
  fabricated "loaded" claim; a server that force-kicks for declining a
  *required* pack still kicks MineRider, exactly as it would a real player.
  New `RESOURCE_PACK_STATUS_DECLINED` constant shared by both state handlers.
  Verified with a new mock-server scenario (`Mode::ResourcePack`) exercising
  both the configuration- and play-state packet variants in one real round
  trip; `coverage.rs`'s two entries upgraded from `NotImplemented` to
  `Partial`/`EVIDENCE_MOCK` accordingly.
- **Client-side username validation.** Vanilla validates the offline/legacy
  username shape (3-16 ASCII letters/digits/underscore) before ever opening a
  connection. MineRider previously sent anything verbatim; an invalid name
  reached the server as a raw string and came back as an opaque Netty
  decode-exception disconnect (found live: a 20-character test username
  triggered exactly this). `login()` now rejects it locally with a clear
  error before sending `login_start`. Premium usernames (from a verified
  profile) skip this check — they're valid by construction.

**Verification:** 234 workspace tests pass (89 lib + 145 across integration
binaries), strict clippy, rustfmt (hand-written files), protocol codegen
drift, and both conformance-matrix tests pass. One test binary
(`config_disconnect`) intermittently fails to *execute* under
`cargo test --workspace` with a Windows access-denied error — confirmed to be
Windows Defender transiently locking a freshly-built `.exe`, not a code
regression: the same test passes cleanly every time when run in isolation
(`cargo test --test config_disconnect`).

---

## Phase 3j — premium (Microsoft/Xbox Live) login (complete, live sign-in unverified)

The other half of this autonomous session: online-mode servers require a
real Microsoft account, not just a valid-shaped username. New top-level
`auth` module implements the full chain a vanilla launcher runs.

**Completed:**
- **`auth::server_hash`**: the Mojang session-server "server ID hash" — SHA-1
  over server id + shared secret + server public key, then Minecraft's
  non-standard signed hex digest (the raw digest interpreted as a
  two's-complement big-endian integer, exactly like Java's
  `BigInteger(byte[])`, printed in base 16 with a `-` for negative values).
  Getting the sign/negation order backwards silently breaks every
  negative-hash server id — the kind of "one bad packet" mistake that would
  desync online-mode auth without ever throwing an error — so this is
  verified against the three canonical test vectors from the protocol
  encryption documentation (fetched and cross-checked live, not from memory:
  `"Notch"`, `"jeb_"` (negative), `"simon"`), not just spot-checked.
- **`auth::microsoft`**: the OAuth2 device-code flow (RFC 8628) against the
  Microsoft identity platform — no browser-redirect listener needed, suits a
  headless bot. Polls the token endpoint honoring the server's `interval` and
  `slow_down` backoff; maps `authorization_declined`/`expired_token`/
  `bad_verification_code` to clear errors. A `refresh_token` path skips the
  device-code prompt on subsequent runs.
- **`auth::xbox`**: Xbox Live user authentication then XSTS authorization for
  the `rp://api.minecraftservices.com/` relying party (the wrong relying
  party is a silent way to get a token Minecraft Services then rejects).
  Failed XSTS authorizations carry a numeric `XErr` Microsoft documents (no
  Xbox account, region-blocked, needs adult verification, child account not
  in a family group); these are mapped to specific, actionable messages
  instead of a bare HTTP status.
- **`auth::minecraft_services`**: trades the XSTS token for a Minecraft
  Services access token, verifies game ownership via the entitlements
  endpoint (fails fast with a clear message instead of a confusing profile
  404 later), and fetches the real profile (UUID parsed from undashed hex,
  username).
- **`auth::session`**: the session-server `join` call.
- **`MicrosoftAuthenticator`** ties the chain together: `sign_in` (device
  code) and `resume` (refresh token) both end at a `PremiumSession`.
- **Wired into the vanilla handshake**, not bolted on separately:
  `login()` now takes `premium: Option<&PremiumSession>`. With a session, it
  sends the real username/UUID in `login_start`, and — critically — calls the
  session join **after computing the shared secret but before answering
  `encryption_begin`**, matching the exact ordering an online-mode server
  requires (the server may call Mojang's `hasJoined` as soon as it decrypts
  the encryption response; a join that races behind that fails). Without a
  premium session, a server reporting `should_authenticate` now fails with a
  clear error instead of silently proceeding offline-mode and getting kicked
  downstream. `ClientConfig::with_premium(session)` and CLI support
  (`MINERIDER_MS_CLIENT_ID`, `MINERIDER_MS_REFRESH_TOKEN`) wire it end to end.

**Verification:** 16 unit tests cover the parsing/error-mapping logic of every
stage (device code, poll outcomes including pending/slow-down/decline,
XBL/XSTS success and every documented `XErr`, Minecraft Services login,
entitlement check, profile parsing including malformed UUIDs, the session
join's UUID hex formatting) using realistic fixture JSON — this is where
"did I get the field name right" bugs hide, and all of it runs with no
network access. The `server_id_hash` vectors are the one piece verified
against ground truth rather than internal self-consistency.

**Follow-up in the same session — the join-before-response wire ordering is
now proven, not just inspected.** `session::join_to` and a
`#[cfg(test)] PremiumSession::for_test` make the session-server URL
overridable; a new `premium_login_joins_session_server_before_encryption_response`
test in `login.rs` drives the real `login()` function over an actual TCP
socket against a hand-rolled mock Minecraft login server (reusing
`Connection`, real RSA keypair generation, real encryption) *and* a
hand-rolled mock HTTP server capturing the join POST — both matching this
project's existing no-framework-mocking convention. It asserts the captured
`accessToken`/`selectedProfile`/`serverId` match values computed
independently from the same live-negotiated shared secret and public key,
and that `login()` only completes (compress → success) after that join
lands, which by construction proves the ordering the source already
guarantees: `join_session(...).await?` sits before the encryption-response
send in a single linear `async fn`, so the mock's `read_packet` for that
response cannot even be reached otherwise.

**Honest limits / next — read before relying on this for a real server:**
- **The full live chain end to end is still unverified against the real
  Microsoft/Xbox/Mojang services.** Running it requires (a) a Microsoft
  account to click through the device-code page, and (b) an Azure AD
  application registered for device-code sign-in with Xbox Live delegated
  permissions — the one-time setup every third-party launcher documents in
  its own README, which only the account owner can do (MineRider deliberately
  does not, and should not, ship a hardcoded client id). Neither is available
  to an unattended coding session. Treat this the same as the vanilla-capture
  gap in `docs/vanilla_capture.md`: implemented and tested against documented
  behavior and a live mock of the *shape* of the exchange, not yet checked
  against the real services.
- Token persistence is manual: the CLI prints the refresh token to stderr for
  the user to save as an environment variable. No on-disk token cache.
  `resume()` does not fall back to `sign_in()` on failure (e.g. an expired
  refresh token) — it just errors, so the user knows to unset the env var and
  re-run.
- No skin/cape data is fetched or used (irrelevant to protocol conformance,
  since MineRider has no renderer).

**Final verification for this session:** 236 workspace tests pass, strict
clippy, rustfmt (hand-written files), protocol codegen drift, and both
conformance-matrix tests — after the join-ordering test above. Offline-mode
login re-confirmed live against the local Paper server (unaffected by any of
this session's changes to `login()`).

---

## Stabilization session — build fix, generated-file formatting, RSA hardening, v0.1.0-alpha release prep (autonomous, owner away ~2h)

Found the working tree mid-Phase-3k: `player.rs`'s new `players`/`event`
modules and `play.rs`'s `BotEvent` broadcast plumbing were present, but
`core/client.rs` had not been updated to match — **the workspace did not
compile.** Fixed rather than reverted, since the in-progress design was
sound and nearly complete:

**Completed:**
- **Build fix.** `Client` now owns a `broadcast::Sender<BotEvent>`, created
  in `connect_inner` alongside the existing `state_tx`/`state_rx` watch pair,
  cloned into `play::run_play` on each `run()` call; a new
  `Client::events()` mirrors `bot_state()` as the push-based counterpart.
  Separately, `players.rs` assumed `PacketPlayerInfoDataItemListed::True`
  carried a `bool`, but minecraft-data models the `listed` field's wire type
  as `varint` (same shape as `gamemode`) — the generated variant is
  `True(i32)`. Fixed at the point of use (`listed != 0`); the wire bytes for
  0/1 are identical either way, so this is a type-mismatch fix, not a
  protocol-behavior change.
- **Generated-file formatting made structurally safe.** `collision_data.rs`
  has been accidentally reformatted into verbose rustfmt style by editor
  auto-format hooks at least three times across prior sessions (Phases 3d,
  3g, 3h), each time caught manually and restored via the generator. Adding
  a `rustfmt.toml` `ignore` entry does not work on stable Rust (this
  toolchain: `rustc`/`rustfmt` 1.9.0-stable) — `ignore` requires nightly.
  `scripts/generate_collision_data.py` now emits `#[rustfmt::skip]` above
  each generated item (the same mechanism `minerider-codegen` already uses
  for its own output), so `cargo fmt --all --check` passes on the generated
  file structurally instead of by convention, and an accidental
  editor-triggered reformat is no longer possible. Verified deterministic:
  regenerating twice produces byte-identical output.
- **Security hardening (bounded task): RSA server-key size validation.**
  `docs/engineering_review.md` §2 flagged this from the Phase 1 audit and it
  was never addressed: `crypto::rsa::encrypt_pkcs1v15` accepted any DER key
  size from the server's Encryption Request with no bound, so a malicious or
  broken server could force several seconds of client CPU per encryption (two
  happen per login) with an oversized key, or hand back a degenerate one.
  Added `validate_key_bits` (512-4096 bits, vanilla's own keys are 1024-bit),
  checked immediately after DER parsing and before any encryption attempt.
  New tests: exact boundary behavior (pure function, no keygen needed) and an
  end-to-end rejection using a real generated 384-bit key.
- **Release hygiene for a public `v0.1.0-alpha`:** `LICENSE-MIT` /
  `LICENSE-APACHE` (canonical texts; the Cargo manifests already declared
  `MIT OR Apache-2.0` but the files didn't exist), `THIRD_PARTY_NOTICES.md`
  for vendored `minecraft-data` (confirmed MIT upstream via the GitHub repo,
  not assumed from memory), `SECURITY.md`, `CONTRIBUTING.md` (generated-file
  rules, testing conventions, acceptable-use, alpha checklist), a rewritten
  `README.md` (accurate phase/feature status, explicit "Lua not implemented
  yet" instead of describing it as available, a Minecraft-branding
  disclaimer, acceptable-use section, no unverified performance numbers),
  a conservative GitHub Actions CI workflow (`fmt`/`test`/`clippy`/codegen
  drift, Linux + Windows for the test job — created locally, not pushed),
  and `Cargo.toml` metadata (`minerider-protocol` path dependency now
  declares `version` so a future `cargo publish` isn't immediately blocked;
  `readme` field added). Repository/homepage URL and legal author identity
  were deliberately left unset — unknown, not guessed.
- **Hygiene:** narrow `.gitignore` rule for local slash-command transcript
  files (`*-local-command*.txt`) instead of a blanket `*.txt` exclusion;
  confirmed no secrets, tokens, `.env` files, or the Microsoft refresh-token
  cache are tracked or untracked-but-unignored.

**Verification:** 251 workspace tests pass, 0 failed (was 249 before this
session's two new RSA tests; the +2 from the prior session's uncommitted
`players`/`event`/inventory work are included once the build was fixed).
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -D
warnings`, and `cargo run -p minerider-codegen -- --check` all clean.

**Problems / honest limitations carried forward:**
- The write-timeout and overall connect-to-play-deadline gaps from
  `docs/engineering_review.md` §2 are still open (only the RSA key-size item
  was picked for this session, per the "exactly one bounded task" rule) —
  `Connection::send_packet`'s `write_all`/`flush` have no timeout, so a
  server that stops reading can still hang a bot indefinitely.
- Repository URL, homepage, and legal copyright identity remain unset in
  `Cargo.toml`/`LICENSE-MIT` (generic "MineRider contributors" used) —
  owner action needed before `cargo publish` or crates.io metadata matters.
- No live end-to-end run against the real Microsoft/Xbox/Mojang services or
  a real vanilla-client reference capture was performed this session (both
  require human/account involvement, unchanged from Phase 3j).

---

## Phase 3l / 4a — resilient long-running authorized client runtime (autonomous)

Closed the two lifecycle gaps left open by the previous session
(`docs/engineering_review.md` §2: no write timeout, no overall
connect-to-play deadline) and added a supervised-reconnect layer for
authorized long-running clients — monitoring, QA/compatibility testing,
authorized load testing. Explicitly **not** anti-AFK-kick or ban-evasion:
reconnect is disabled unless opted in, and never retries after an explicit
server rejection or a permanent auth/protocol error unless the policy is
deliberately configured to.

**Completed:**
- **Write timeout.** `Connection::send_packet`'s `write_all` + `flush` now
  run against one shared deadline (`write_frame_with_timeout`, a small
  helper generic over `AsyncWrite` so it's unit-testable with an in-memory
  fake instead of real, flaky TCP backpressure). Configurable via the new
  `ConnectionTimeouts` struct and `ClientConfig::write_timeout`/
  `with_write_timeout`; default 30s. On timeout the connection is marked
  `write_failed` and every later `send_packet`/`read_packet` call fails fast
  — a partially-sent, possibly-mid-frame write (and, for an encrypted
  connection, a CFB8 keystream that has already advanced past bytes that
  may never have reached the wire) cannot be proven safe to build on, so the
  connection is treated as failed rather than silently reused.
- **Overall connect-to-play deadline.** `Client::connect` now wraps its
  whole TCP-connect → handshake → login → configuration sequence in one
  `tokio::time::timeout` (`ClientConfig::connect_deadline`, default 60s) —
  one shared budget, not reset at each stage, with the existing per-read
  timeout still underneath as defense in depth. An internal `AtomicU8`
  stage tracker (not a `Cell`, so the wrapping future stays `Send` for
  `tokio::spawn` — `src/bin/swarm.rs` spawns `Client::connect` per bot)
  means a deadline timeout names which stage it happened in
  ("... exceeded during login"). The Mojang session-server `join` HTTP call
  deliberately has no timeout of its own: it already runs inside this one
  deadline, so a second independent timeout would just be two clocks racing
  for nothing; documented at the call site in `login.rs`.
- **`core::supervisor::ClientSupervisor`** (new module): owns connection
  lifecycle policy across however many (re)connect attempts a
  `ReconnectPolicy` allows, while `Client` still represents exactly one
  session. `ClientSupervisor::new(cfg, policy)` returns `(Self,
  SupervisorHandle)` — the handle (cloneable, obtained before `run()`
  consumes the supervisor) exposes `events()`, `state()`, `status()` and
  `stop()`, mirroring the existing `Client::control()`/`bot_state()`/
  `events()` shape. `run()`'s loop is strictly sequential — one
  `Client::connect` call site, always awaited to completion before any
  retry — so duplicate simultaneous connection attempts for the same
  account are structurally impossible, not just policy-discouraged.
- **`RetryClass` + centralized classification** (`core::error`): every
  `MineRiderError` maps to `Transient`, `ServerRejected`, `AuthFailure` or
  `ProtocolIncompatible` via one method (`retry_class()`), tested directly —
  not scattered string-matching. `ReconnectPolicy::decision_for(class)` is
  the one place that turns a class into `Retry`/`Stop`.
- **`ReconnectPolicy`**: `enabled` (default `false` — a bare `Client` or a
  disabled-policy supervisor never auto-reconnects), `max_retries`
  (`RetryLimit::Count`/`Unlimited`, default `Count(5)`), exponential backoff
  (`initial_delay`/`max_delay`/`multiplier`, defaults 1s/60s/2.0), optional
  `Jitter::Deterministic(fraction)` (a reproducible function of the attempt
  number, not real randomness, so backoff tests stay deterministic even
  with jitter on), `stable_session_reset` (a session connected at least this
  long resets the backoff counter on its next failure, default 60s), and
  per-class `RetryDecision` — defaults `Retry` only for `Transient`; `Stop`
  for `ServerRejected`/`AuthFailure`/`ProtocolIncompatible` unless the
  policy is explicitly reconfigured otherwise.
- **Cancellation**: `tokio_util::sync::CancellationToken` (new dependency;
  justification below), checked/awaited at every cancellable point — before
  a connect attempt, mid-connect, mid-backoff-sleep, and while relaying a
  live session's traffic — so `SupervisorHandle::stop()` interrupts a 30s
  backoff sleep or a stalled connect attempt immediately rather than
  waiting it out.
- **Unified `BotEvent` stream**: rather than a second, competing event
  type, `core::supervisor` added lifecycle variants directly to the
  existing `BotEvent` enum (`Connecting`, `Connected`, `Disconnected`,
  `ReconnectScheduled`, `RetryAttemptStarted`, `RetriesExhausted`,
  `StoppedByCancellation`) alongside the existing play-session ones
  (`Login`, `Health`, `Chat`, `Kicked`, ...). The supervisor relays a live
  session's events onto its own long-lived channel and interleaves its own
  lifecycle events on the same stream, so one subscription sees both.
- **State staleness**: `SupervisorHandle::state()` is reset to
  `StateSnapshot::default()` the instant a session ends (before any backoff
  sleep), and `SupervisorHandle::status()` is a separate, always-current
  `SupervisorStatus` (`Connecting`/`Connected`/`Disconnected`/
  `ReconnectScheduled`/`Stopped`) — combined, an observer never sees a
  frozen "still connected" snapshot after a disconnect, and never pays for
  cloning world/chunk data on a lifecycle transition (`StateSnapshot` never
  held that to begin with).
- **Account safety**: the supervisor never touches the Microsoft/Xbox
  device-code sign-in flow at all (that already happened once, upstream, in
  `main.rs`, before a `ClientConfig` even exists) — it only ever reuses the
  same already-completed `PremiumSession` across reconnect attempts, exactly
  as a real launcher reconnecting multiple times would. If that session's
  access token expires mid-campaign, the resulting `MineRiderError::Auth`
  classifies as `AuthFailure`, whose default policy is `Stop` — no
  hammering an expired token in a loop. Refreshing a premium session
  automatically is out of scope for this milestone and was deliberately
  left unimplemented rather than half-automated.
- **CLI**: `MINERIDER_WRITE_TIMEOUT_SECS`, `MINERIDER_CONNECT_DEADLINE_SECS`,
  `MINERIDER_RECONNECT=1`, `MINERIDER_MAX_RETRIES=<n|unlimited>`,
  `MINERIDER_INITIAL_BACKOFF_MS`/`MINERIDER_MAX_BACKOFF_MS`. Default
  behavior is unchanged (one-shot connect); reconnect mode prints each
  lifecycle event and maps `SupervisorOutcome` to the process exit code.
  `MINERIDER_TRACE` and the `MINERIDER_WALK_TO`/`MINERIDER_CHAT` manual test
  hooks are not available together with `MINERIDER_RECONNECT` in this
  milestone (documented, not silently ignored) — the supervisor doesn't yet
  forward a control handle for whichever session is currently active.
  Manually verified end-to-end against a closed port: lifecycle events
  printed in the correct order (`Connecting` →
  `ReconnectScheduled`/`RetryAttemptStarted` × 2 → exhausted), correct
  `FAILURE` exit code, no hang, no crash.

**New dependency: `tokio-util` (`sync` feature set, default-features off).**
A hand-rolled `watch::channel<bool>` cancellation flag was tried first (no
new dependency) and rejected: `watch::Receiver::wait_for`'s `Ok` value
borrows the receiver, which conflicted with calling `&self` methods
(`emit`/`set_status`) in the same `tokio::select!` arm (a real
borrow-checker error, not a style preference), and the fallback of matching
on `changed()` directly raises a *busy-loop* risk once the sending handle is
ever dropped without calling `stop()` (a closed `watch` channel's
`changed()` resolves immediately forever after). `tokio_util::sync::
CancellationToken` — the Tokio project's own purpose-built primitive for
exactly this — has neither problem and is a small, official, actively
maintained addition (`futures-sink` is its only extra transitive
dependency).

**Tests added** (all against local mock servers/sockets, no real network,
paused/short real time — no multi-second sleeps): `connection.rs` — write
success within timeout, `write_all`-stage and `flush`-stage timeout
identification (via `tokio::io::duplex` and a `poll_flush`-always-pending
wrapper), poisoning classification; `error.rs` — `retry_class()` for every
error variant; `supervisor.rs` — backoff growth/cap, deterministic-jitter
reproducibility and bounds, disabled-policy-never-retries, enabled-policy
defaults, opt-in override, retry-limit counting;
`tests/connect_deadline.rs` — deadline firing mid-login (naming the stage)
and a healthy connect completing well within a generous deadline;
`tests/supervisor.rs` — full lifecycle-event-order + state-reset-on-
disconnect, transient-disconnect-then-successful-reconnect (a real second
TCP connection, sequentially, not concurrently), max-retries-exhausted
(real connection-refused), cancellation interrupting a 30s backoff in
under 2s, explicit server rejection *not* retried by default, and a
permanent protocol-incompatibility (online-mode required, no premium
session attached) *not* retried by default.

**Verification:** 275 workspace tests pass, 0 failed (was 251; +24: 2 RSA
tests were already counted, +4 connection, +1 error, +6 supervisor unit,
+2 connect_deadline, +6 tests/supervisor — some earlier counts overlap
milestones, see each module's own test output for the authoritative
per-file count). `cargo fmt --all --check`, `cargo clippy --workspace
--all-targets -D warnings`, and `cargo run -p minerider-codegen -- --check`
all clean.

**Honest limitations:**
- The supervisor does not forward a `ControlHandle`/chat/movement API for
  whichever session is currently active — only lifecycle/state/event
  observability. Driving a supervised bot's movement is a follow-up.
- No premium-session refresh/renewal during a long reconnect campaign — an
  expired access token stops the supervisor (`AuthFailure` → default
  `Stop`) rather than being silently re-authenticated.
- `RetryClass::ProtocolIncompatible` and `AuthFailure` are grouped by the
  *shape* of the underlying `MineRiderError` variant, not by inspecting
  message text; a future error variant added without updating
  `retry_class()`'s match would need the match arm added deliberately (the
  match is exhaustive today, so the compiler enforces this for any new
  variant).
- The exact-count assertion in `tests/supervisor.rs`'s reconnect test proves
  attempts happen sequentially for that scenario; there is no separate
  stress test hammering the supervisor with concurrent `stop()`/status
  reads from multiple tasks.
- No live Microsoft/Xbox/Mojang or real vanilla-server run this session
  (unchanged from prior sessions — both require human/account involvement).

---

## Phase 4b — supervised active-session control (autonomous)

The first slice of a larger planned milestone (Lua-ready workflow/action
layer: text/presentation state, scoreboards, inventory transactions,
interaction primitives, navigation). This phase was called out as the
prerequisite for all the others — a stable way to control whichever session
`ClientSupervisor` currently owns — and is implemented and tested on its
own; the remaining phases are unstarted and explicitly not claimed as done
(see "Next unfinished phase" in the session's final report).

**Completed:**
- **`SupervisorHandle::send_command`** (plus `walk_to`/`look`/`set_input`/
  `sprint`/`sneak`/`jump`/`chat`/`stop_movement` convenience wrappers
  mirroring `ControlHandle`'s existing shape) — a bounded
  (`tokio::sync::mpsc`, capacity 64) command channel from the handle into
  the running supervisor, with a one-shot reply per command so the caller
  learns whether it actually reached a live session, not just whether it
  was accepted into a queue.
- **Typed `ControlError`** (`NotConnected`, `SessionReplaced`,
  `Disconnected`, `SupervisorStopped`) — centralized, not a loose string.
- **Offline behavior**: commands are never queued across a reconnect.
  `ClientSupervisor::run`'s connect-attempt loop and `handle_failure`'s
  backoff sleep both now also drain and immediately reject
  (`NotConnected`) any command received while not connected, using the
  same pinned-future-plus-inner-`select!`-loop pattern the connect-deadline
  work established, so a real connect attempt or backoff sleep in progress
  is never restarted just because a command arrived mid-wait.
- **Session generation**: `SupervisorHandle::generation()` returns a `u64`
  that increments by one on every successful connect (published via a
  `watch::channel<u64>`). Exposed for a future multi-step caller (an
  inventory transaction, a workflow step) to snapshot before starting and
  compare afterward, detecting "the session was replaced mid-operation"
  itself — this phase's own simple fire-and-forget commands don't need
  callers to track it themselves (see below).
- **Reconnect safety**: the moment a session ends, anything still sitting
  in the command queue is drained and answered `SessionReplaced` — before
  `handle_failure` even runs — so a command queued for the old session can
  never be silently carried over to whatever replaces it. When the
  supervisor stops for good (cancelled, retries exhausted, or a permanent
  error not retried), a final drain answers anything left with
  `SupervisorStopped` instead of silently dropping the one-shot sender.
- **Single writer**: `run_session` is the only place that ever calls
  `client.control()` for the currently active `Client`; commands are
  matched against the *live* session's control handle, never a stale one,
  and concurrent callers are naturally serialized by the one bounded
  channel feeding one consumer task.
- **Cancellation**: dropping a `send_command` future (e.g. via
  `tokio::time::timeout`) before it resolves cancels cleanly — the
  supervisor still processes the command at most once and just discards a
  reply nobody is listening for anymore; no leaked senders, no panic.

**Honest, deliberately-scoped limitation from this same phase:** a command
that reaches `run_session` right as the underlying TCP connection is dying
can still return `Ok(())` — `ControlHandle::send` (pre-existing, unchanged
semantics) only ever promised "accepted into the play loop's own command
queue," not "confirmed on the wire," and detecting a dead socket is not
instantaneous. This is not a gap introduced here: it is the existing
`ControlHandle` contract, inherited unchanged. What *is* new and
unconditionally guaranteed is that once the supervisor has processed a
session ending, no further command can ever reach it — proven directly in
tests by waiting for `ClientSupervisor::run` itself to return before
sending.

**Tests added** (`tests/supervisor.rs`, +6): commands reach the active
session (a real chat packet observed on the wire); commands fail
immediately (`NotConnected`) while never having connected; a command sent
after the supervisor has fully stopped following a non-retried session end
fails with `SupervisorStopped` (the deterministic version of "stale command
cannot succeed", see above); reconnect increments `generation()` from 0 to
1 to 2 across two sessions, and the *new* session's command path works
(proving commands route to whichever session is current, not a stale
reference); cancellation via `stop()` closes the command path cleanly
post-hoc; 8 concurrent `send_command` calls from separate tasks all land
intact and distinct on the wire (no interleaved/corrupted writes, one
writer).

**Verification:** 276 workspace tests pass, 0 failed (was 270; +6).
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -D
warnings`, and `cargo run -p minerider-codegen -- --check` all clean.

**Next unfinished phase (at the time):** text component foundation
(Phase 4c) — a shared `TextComponent` model for chat/system messages/
titles/boss bars/scoreboards, which every later phase in this milestone
depends on. Not started in that session; see below for its completion.

---

## Phase 4c — TextComponent foundation (autonomous)

One shared, typed text-component model (`src/minecraft/text.rs`) instead of
five separate ad-hoc parsers for chat, titles, boss bars, scoreboards and
tab-list header/footer — the explicit prerequisite for Phase 4d onward.

**Inspected before designing anything:** the generated 1.21.4 struct
definitions (not old wiki packet layouts) for every text-bearing packet —
`PacketSetTitleText`/`Subtitle` (`crate::nbt::Nbt`), `PacketActionBar`
(`Nbt`), `PacketBossBar` (`title: PacketBossBarTitle` — a switch-on-action
enum, still `Nbt` per variant), `PacketSystemChat`/`PacketProfilelessChat`/
`PacketKickDisconnect`/`PacketPlayerlistHeader` (all `Nbt`), and
`PacketPlayerChat` (`network_name`/`network_target_name`: `Nbt`;
`plain_message`: plain `String`; `unsigned_chat_content`: `Option<Nbt>`) —
versus the login-state `PacketDisconnect.reason`, which is
`super::types::String` (a plain wire string whose *content* is
JSON-encoded, predating 1.20.3's switch to network NBT for every other text
field). This confirmed the three input encodings the module needed to
support, and that everything in Play state is network NBT — no packet uses
the old bare-JSON-string chat encoding.

**Completed:**
- `TextComponent { content: Content, style: Style, extra: Vec<TextComponent> }`
  with `Content::{Literal, Translate{key, args: Vec<TextComponent>},
  Unknown{raw: Nbt}}` (score/selector/keybind/nbt-value components and
  anything else not specifically interpreted are preserved, not dropped)
  and a `Style` covering color (named or `#RRGGBB` hex, distinguished),
  bold/italic/underlined/strikethrough/obfuscated, insertion, font, and
  preserved click/hover events.
- **Two parsers, one output model**: `TextComponent::from_nbt` (network
  NBT — every play-state field) and `TextComponent::from_json_str` (the
  login-Disconnect JSON-in-a-string case), both infallible — malformed
  input degrades to best-effort text (worst case an empty or literal-raw
  component) rather than an `Err`, since a garbled component is a display
  concern, not a protocol violation the caller should have to handle.
- **Bounded parsing**: recursion depth (64), total node count (4,096) and
  total literal-text bytes (256 KiB) are all enforced — including through a
  hover event's nested `show_text` component, which shares the same budget
  rather than getting a fresh one (closing what would otherwise be a budget
  bypass). A test deliberately caught the first version of this being
  wrong: an oversized sibling list initially only degraded each excess
  item's *content* to empty while still allocating one `TextComponent` per
  input item — the `Vec` itself wasn't length-bounded. Fixed by checking
  the budget *before* attempting each list item, not just inside the
  recursive parse call.
- **Translation table reused, not duplicated**: `nbt_reason_text`
  (`src/minecraft/mod.rs`, used by `login`/`configuration`/`play` for
  disconnect reasons) is now a one-line wrapper over
  `TextComponent::from_nbt(...).plain_text()`. The old copy of the
  translation-key table and `%s`/`%N$s` placeholder substitution logic that
  used to live in `mod.rs` was deleted, not kept as a parallel
  implementation — `text.rs` is the only place that logic exists now, and
  it's strictly more capable than the old version (translation arguments
  are kept as full structured `TextComponent`s and rendered lazily, instead
  of being pre-flattened to strings before substitution).
- **Honest, documented uncertainty**: click/hover event NBT key casing
  (`clickEvent`/`hoverEvent` vs `click_event`/`hover_event`) has not been
  independently verified against a live 1.21.4 capture, so both spellings
  are checked defensively; an absent/misspelled event is simply not
  populated, never a parse failure. Flagged in the module doc comment
  rather than asserted as fact.

**Tests added** (`src/minecraft/text.rs`, +18 over the removed/superseded
mod.rs test): literal (bare string and `{text:...}`), fully-styled
(color/bold/italic/obfuscated/insertion/click/hover, including a nested
`show_text` hover body), hex vs named color, nested `extra` siblings in
order, a bare-list component-array root, translated components with
structured args (both a known template and an unknown-key fallback),
unknown content kind preserved rather than dropped, malformed `extra` type
ignored without panicking, deeply nested `extra` (4× the depth cap) not
overflowing the stack, an oversized sibling list truncated by the node
budget, oversized literal text truncated by the text budget, the JSON
equivalents of literal/styled/translated/array-root, malformed JSON
degrading to a literal of the raw string, `Display` matching `plain_text`,
and indexed-placeholder substitution directly.

**Verification correction (established by the Phase 4d baseline audit):**
the exact Phase 4c baseline at commit `026c815` is 293 workspace tests, not
297 as this entry originally reported. Phase 4d adds exactly nine tests and
the full workspace then reports 302, which exposed the earlier arithmetic/
reporting error. The Phase 4c format, Clippy, and codegen checks remain clean.

**Honest limitations:**
- Not a full vanilla lang file: `plain_text()`'s translation table covers
  only the handful of keys a bot most often sees in chat (join/leave, chat
  formats, whispers); everything else falls back to `key{arg, arg}` rather
  than claiming a translation MineRider doesn't have.
- Click/hover event key casing is unverified against a live capture (see
  above) — functionally safe (defensive dual-spelling lookup, never
  panics), but not proven correct against real server traffic yet.
- `hover_event`'s `show_item`/`show_entity` payloads are preserved as raw
  NBT/JSON (`RawEvent`), not structurally parsed into item/entity data —
  out of scope for this phase; only `show_text` is interpreted into a
  nested `TextComponent`.

**Next unfinished phase:** Phase 4d — inbound chat/system/action-bar/title/
boss-bar/tab-header-footer state built on this model, plus outbound
chat/commands (folded in from the original Phase 4g).

---

## Phase 4d — inbound presentation and chat state

Built one bounded, snapshot-readable `PresentationState`
(`src/minecraft/presentation.rs`) on the shared Phase 4c text component.
Generated protocol-769 packet structs and ids remain the wire source of
truth; the module projects them into stable public Rust models before state
mutation or event delivery.

**Packet coverage completed:**
- `player_chat`: structured sender/target/display component, registry or
  inline chat decoration, filter status, and raw signature/timestamp/salt/
  previous-message data. Raw signed material is retained but explicitly not
  described as verified.
- `profileless_chat` (disguised chat) and `system_chat`, including the
  system-chat action-bar flag.
- dedicated `action_bar`, `set_title_text`, `set_title_subtitle`,
  `set_title_time`, and `clear_titles`. Clear removes title/subtitle but
  preserves timing; reset also restores vanilla 10/70/20 timing defaults.
- `playerlist_header` updates header/footer atomically.
- all `boss_bar` actions: add, remove, progress, title, style, and flags,
  keyed by stable UUID. Unknown color/overlay values and flag bits are
  preserved; missing/out-of-order updates and removals are typed safe no-ops.
- `kick_disconnect`: the structured reason is stored and published before
  the terminal disconnect error leaves the play loop.

**Architecture and resource bounds:**
- Every play snapshot now includes an independent clone of presentation
  state. A lagged broadcast consumer can therefore recover from the current
  snapshot instead of relying on replay.
- Each applied update emits one ordered `PresentationEvent`; existing
  `Chat`, `SystemChat`, and `Kicked` events remain as compatibility
  projections.
- Chat history is capped at 512 entries and boss bars at 256. Previous
  signed-message references, filter masks, and inline decoration parameters
  have explicit copy bounds with truncation recorded in the public model.
- Event/update payloads use indirection for large text-bearing variants so
  the bounded broadcast channel does not inflate every event allocation.

**Tests added (9):** system/action-bar routing; player-chat raw signature,
unsigned display, target and filter preservation; disguised-chat safety;
title clear versus reset; atomic tab header/footer; complete boss-bar
lifecycle including unknown values; out-of-order boss-bar no-ops; hostile
collection bounds; and structured disconnect state/event.

**Verification:** 302 workspace tests pass, 0 failed (exact baseline 293;
+9). GitHub CI passes `cargo fmt --all --check`,
`cargo test --workspace --locked` on Linux and Windows,
`cargo clippy --workspace --all-targets -- -D warnings`, and
`cargo run -p minerider-codegen -- --check`.
The conformance-document and generated-file drift tests now normalize
checkout CRLF to LF before comparison; their previous byte-for-byte newline
comparisons failed on Windows despite identical generated content.

**Honest limitations:**
- Signed-chat cryptographic verification and acknowledgement state are not
  implemented; signatures are raw untrusted bytes.
- Registry-referenced chat types are retained by id because configuration
  registry resolution for chat decorations is not implemented yet. Inline
  decorations are fully projected.
- Presentation state is headless data only. It does not implement visual
  expiry/animation, execute click/hover actions, or claim renderer parity.
- Compatibility chat events intentionally flatten structured components;
  new consumers should use `PresentationEvent` and the snapshot.

**Next unfinished phase:** Phase 4e — complete scoreboard objectives,
display slots, scores, and teams on the shared text/presentation foundation.

---

## Phase 4e — complete scoreboards and teams

Implemented one bounded, snapshot-readable `ScoreboardState`
(`src/minecraft/scoreboard.rs`) directly from the generated protocol-769
packet definitions. Generated switch enums are projected into stable public
models before state mutation or event delivery.

**Packet coverage completed:**
- `scoreboard_objective`: create, update and remove by stable objective name,
  including structured display text, integer/hearts render type, default,
  blank, styled, fixed and unknown number formats.
- `scoreboard_display_objective`: list/sidebar/below-name/team-color slots,
  explicit empty-name detach and safely preserved unknown slot ids.
- `scoreboard_score` and `reset_score`: create/update, display name, per-score
  number-format override, one-objective removal and all-objectives reset for
  an owner.
- `teams`: create, update and remove; member add/remove and atomic movement
  between teams; display name, prefix, suffix, color, friendly-fire and
  friendly-invisibility bits, name-tag visibility and collision rule.

**Lifecycle, ordering and bounds:**
- Objectives, display slots, scores and teams are `BTreeMap`/`BTreeSet`
  backed for deterministic snapshots. Member-to-team ownership is indexed
  explicitly, so one entry cannot remain in two teams.
- Removing an objective also detaches every referencing display slot and
  removes every score for that objective. Events report the detached/removed
  counts.
- Missing/out-of-order updates, removals and unknown actions are typed safe
  no-ops. Unknown render types, team colors, visibility/collision strings,
  display slots and number-format ids remain inspectable.
- Server-controlled state is capped at 256 objectives, 64 display slots,
  16,384 scores, 1,024 teams and 16,384 unique team members. Capacity rejects
  are observable in ordered `ScoreboardEvent`s.
- Every play snapshot carries an independent scoreboard clone; lagged event
  consumers recover through the snapshot exactly like presentation state.

**Tests added (8):** objective/display/score lifecycle and cascading removal;
scoped/all-objective score reset; complete team/options/member lifecycle;
out-of-order and unknown-action no-ops; unknown number-format preservation;
deterministic ordering and objective/display/score bounds; team/member bounds;
and malformed payload rejection. The lifecycle assertions cover explicit
display detach, styled/fixed formats, shared text components, every team text
field, color, visibility/collision rules and both flag bits.

**Verification:** 310 workspace tests pass, 0 failed (Phase 4d baseline 302;
+8). GitHub CI passes `cargo fmt --all --check`,
`cargo test --workspace --locked` on Linux and Windows,
`cargo clippy --workspace --all-targets -- -D warnings`, and
`cargo run -p minerider-codegen -- --check`.

**Honest limitations:**
- State is headless data: there is no scoreboard/HUD renderer and no claim of
  pixel or animation parity with vanilla.
- Semantics are unit-tested against generated protocol-769 layouts; a real
  vanilla-client trace diff for these five packets is still pending, so the
  conformance matrix reports `PARTIAL`, not `PASS`.
- Styled number-format NBT is retained raw for a future renderer; fixed text
  uses the shared `TextComponent` model immediately.

**Next unfinished phase:** Phase 4f — remaining typed HUD and player-facing
state (abilities, effects, attributes, cooldowns, border, difficulty, spawn
and the gaps in existing health/experience/time/weather/player-list state).

---

## Phase 4f — typed HUD and player-facing state

Added a bounded, snapshot-readable `HudState` and ordered `HudEvent` stream
for protocol-769 player-facing data that previously lived only in partial
compatibility fields or was ignored.

**Packet/state coverage completed:**
- Health, hunger, saturation, experience, game mode, hardcore/previous mode,
  ability flags and flying/walking speeds.
- Selected hotbar slot and held-item projection from direct player-inventory
  updates; bounded cooldown and local-player effect lifecycles.
- Local-player attributes with typed keys/operations and a per-attribute
  modifier bound; structured local death and respawn/dimension context.
- Full world-border initialization and partial updates, safe before initial
  state; world age/day time/ticking, rain/thunder levels, difficulty/lock and
  global spawn position/angle.
- Modern `player_info` fields: account name, game mode, listed state, latency,
  display name, list priority, hat flag and bounded signed-chat-session
  metadata. Player entries are now capped and UUID-ordered.

**Lifecycle, ordering and bounds:** cooldowns, effects, attributes and player
entries use deterministic maps with defensive limits; attribute modifier
vectors are truncated observably. Entity-scoped effects, attributes and death
packets only mutate HUD state when addressed to the local entity. Unknown
game modes, difficulty values and attribute operations remain inspectable;
malformed payloads return protocol errors.

**Tests added (13):** abilities; cooldown/effect add-remove and local-entity
filtering; attribute projection/modifier truncation; structured death; full
and out-of-order world-border updates; difficulty/spawn unknown preservation;
hotbar/held item tracking; typed vitals/experience/time/weather/game mode;
malformed payload rejection; modern player-list fields and clearing; player
bound behavior; and deterministic UUID iteration.

**Verification:** GitHub CI runs format, clippy, generated-protocol drift and
workspace tests on Linux and Windows. Vanilla-client trace capture remains
pending, so these state-only obligations are `PARTIAL`, not `PASS`.

**Honest limitations:**
- HUD state is headless; it does not render, animate or claim pixel parity.
- Status-effect and attribute state is retained for observation but is not
  yet folded into movement physics.
- The direct player-inventory packet currently projects only hotbar/held-item
  HUD data; the complete transactional inventory model belongs to Phase 4h.
- Signed player chat sessions retain only bounded metadata; cryptographic
  chat verification remains unimplemented.

**Next unfinished phase:** Phase 4g — supervised outbound chat and command
actions with explicit packet semantics, validation and typed responses.

---

## Phase 4g — outbound chat and command API

Replaced leading-slash inference with two explicit control actions:
`BotCommand::Chat` always targets protocol 769 `chat_message`, while
`BotCommand::Command` always targets `chat_command` and stores command text
without `/`.

**Validation and packet semantics:**
- Empty chat/command text, command text with a leading slash, chat text that
  looks like a command, and overlength text are rejected before queueing.
- The 256-character protocol limit is measured as Java-compatible UTF-16
  code units, not UTF-8 bytes or Rust scalar values.
- Chat encoding preserves timestamp/salt, an absent signature and the
  protocol-769 three-byte empty acknowledgement window. Command encoding uses
  the dedicated packet and never receives chat-only fields.

**Supervisor behavior:**
- The existing 64-entry bounded queue now uses `try_send`: capacity returns
  typed `ControlError::QueueFull` immediately instead of awaiting space.
- `ControlError::InvalidAction` carries the typed validation reason; offline
  and stopped states still return `NotConnected`/`SupervisorStopped`.
- Every queued action captures the active session generation. `run_session`
  compares it before forwarding, so an old action cannot execute after a
  reconnect even across a narrow status/queue race.

**Tests added (7):** UTF-16/empty/slash/maximum validation; distinct typed
actions; exact chat and command packet encoding; slash rejection without
packet-kind inference; typed validation before connectivity checks; queue
capacity; and captured generation. Existing integration coverage continues
to exercise disconnected sessions, concurrent serialization and successful
commands after a generation-changing reconnect.

**Honest limitations:** outbound player chat is unsigned and carries no
cryptographic message-chain acknowledgement state; secure-chat-enforcing
servers may reject it. This phase does not claim signed-chat conformance.

**Next unfinished phase:** Phase 4h — correct bounded inventory/window
transactions with typed click modes, confirmations and generation safety.

---

## Phase 4h — bounded server-authoritative inventory transactions

Implemented the smallest coherent green Phase 4h slice: protocol-769 generic
container clicks can now be validated, sent and observed through both a live
`ControlHandle` and a reconnecting `SupervisorHandle`, without optimistic
slot mutation or a false success result at queue time.

**State and packet model:**
- `InventoryState` now keeps deterministic bounded window properties, caps a
  window at 1024 slots with an observable truncation flag, and applies direct
  player-inventory updates to the same authoritative state used by snapshots.
- `InventoryClick` models normal pickup/outside clicks, shift-click,
  hotbar/offhand swap, creative middle-click clone, drop, all drag phases and
  double-click. Slot/button/hotbar rules, creative-only modes, drag lifecycle,
  current window id and state id are checked before encoding.
- A validated request encodes the generated `PacketWindowClick` and generated
  serverbound packet id. Every mode has deterministic encode/decode coverage;
  generated protocol files remain untouched.

**Transaction lifecycle and safety:**
- Pending transactions are capped at 64; completed outcomes are retained in a
  deterministic capped map of 128 entries. The model distinguishes queued,
  sent, confirmed by a newer incremental state, corrected by a full sync,
  timed out, window closed and rejected outcomes.
- Clientbound open/close, full-window, slot, cursor, property, selected-hotbar
  and direct-player-inventory packets update snapshots and emit ordered
  `BotEvent::Inventory` events. Closing or replacing a window cancels its
  pending transactions; tick expiry prevents an unbounded wait.
- Click packets intentionally carry an empty `changed_slots` prediction and
  the current authoritative cursor. No local slot is mutated optimistically:
  the next server update is the confirmation or correction source of truth.
- `SupervisorHandle::inventory_click` allocates a transaction id, captures the
  current session generation and state id, submits through the existing
  bounded command queue, then waits for a terminal authoritative outcome.
  `inventory_click_in_generation` lets a multi-step workflow pin an explicit
  generation and timeout. Disconnects, replacements and stale generations
  return typed `InventoryActionError` values and never replay on a new session.

**Tests added (10):** every typed click mode and exact packet fields; generated
packet encode/decode; malformed slot/state/creative requests; drag ordering;
incremental confirmation versus full correction; transaction capacity and
timeout; close cancellation; supervisor confirmation/disconnect/timeout;
stale and changed generation; and the public supervised request lifecycle.
GitHub CI runs format, clippy, codegen drift and workspace tests on Linux and
Windows. The conformance generator classifies the eight strengthened inbound
inventory obligations as `PARTIAL` pending a vanilla-client trace.

**Honest limitations:**
- This slice is generic container transaction plumbing, not a complete
  vanilla screen/menu engine. It does not calculate predicted changed slots,
  recipes, crafting outputs, anvils, merchants or other menu-specific rules.
- An empty `changed_slots` map favors safety and server correction over
  latency; servers or anti-cheat plugins that require exact client prediction
  may reject or resynchronize a click.
- The direct `ControlHandle` is fire-and-observe through snapshots/events;
  only the supervisor convenience API waits and returns a terminal outcome.
- Real vanilla-client trace comparison remains pending, so this phase makes no
  byte/timing parity claim beyond generated layouts and deterministic tests.

**Next unfinished work:** build menu-specific interaction semantics and
higher-level inventory workflows on this bounded transaction foundation.
