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
- Play-state keep-alive ids (0x26/0x18) are hardcoded until the phase 2
  minecraft-data generator makes all ids data-driven.
- SelectKnownPacks is echoed verbatim (claims all packs known) — matches
  vanilla clients but should be revisited with real registry handling.

**Next step:** Phase 2 — minecraft-data packet pipeline: generator producing
`src/protocol/generated/` (packet ids, structs, serializers/deserializers)
plus encode/decode round-trip tests. Real-server validation of Phase 1
should happen in parallel with that.

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
(`src/protocol/generated/`, encode/decode round-trip tests).
