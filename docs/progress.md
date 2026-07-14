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
