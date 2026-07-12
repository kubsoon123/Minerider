# MineRider — Engineering Review (7-Agent Audit)

Seven specialized audits (architecture, performance, protocol correctness,
testing, security, open-source readiness, roadmap) over the Phase 1 codebase.
Duplicates merged; findings ranked. Two connection-fatal protocol bugs and
three security issues were found and **fixed in
`fix: correct 1.21.4 packet layouts and harden decoder against DoS`** —
everything else is a recommendation only.

## 1. Critical issues (fixed)

| # | Issue | Evidence | Fix |
|---|-------|----------|-----|
| C1 | **Play keep-alive ids were 1.21.1's** (0x26/0x18). Real 1.21.4: clientbound `0x27`, serverbound `0x1a`. Consequence: real keep-alives ignored → vanilla kicks after 15 s; the client *sent* 0x18 = `use_entity` → DecoderException kick. | minecraft-data `data/pc/1.21.4/protocol.json` | `play.rs` ids corrected; mock updated |
| C2 | **Login Success parsed a phantom `strict_error_handling` bool** — exists in 1.20.5–1.21.1, removed in 1.21.2. Consequence: every real login fails with `BufferUnderflow` after properties. | minecraft-data 1.21.4 (fields: uuid, username, properties only) | bool read removed from `login.rs`; mock updated |
| C3 | **Decompressed-size DoS**: a ≤2 MiB frame could declare `data_length` up to 64 MiB → 32× inflate amplification, repeatable inside the login/config/play loops. | `codec.rs` (checked only `data_length < 0`) | reject `data_length > MAX_FRAME_SIZE` (2 MiB), vanilla behavior |
| C4 | **`read_array` allocation abort**: `Vec::with_capacity(len)` on a server-controlled VarInt up to i32::MAX (51 GiB request for `String` elements). Latent today (unused on read paths), one-packet kill once Phase 2 codegen uses it. | `buffer.rs` | reject `len > remaining()` before allocating |
| C5 | **Config/play disconnect reason is NBT, not String** (only *login* disconnect is a JSON String) — misparse turned clean disconnects into confusing errors. | minecraft-data: `reason: anonymousNbt` | read raw payload, lossy-render; full NBT decode lands in Phase 2 |

Root cause of C1/C2: BAC's notes were protocol-767 (1.21.1) vintage for these
two items. `docs/bac_analysis.md` now carries a corrections note. The tests
codified the same wrong layouts (self-consistent mock) — **the suite could
not catch wire-format drift against a real server**; see §4 and the v0.2
roadmap (real-server CI gate).

## 2. Important improvements

**Security (untrusted-server model):**
- No write timeout — a server that stops reading parks `write_all` forever.
  Add `tokio::time::timeout` around send/flush (`connection.rs`).
- No overall connect→play deadline — a server trickling 1 byte / 29 s hangs
  login indefinitely (per-read timeout resets per byte). Wrap
  `Client::connect` in one deadline (~60 s).
- RSA key accepted without size sanity — a ~2 Mbit modulus = seconds of CPU
  per encryption (×2). Enforce 512–4096 bits after parsing.

**Architecture:**
- `send_packet(id, &[u8])` double-copies every payload (callers already hold
  a `PacketWriter` buffer). Take `&RawPacket`/hand over `BytesMut` — Phase 2
  serializers get a no-copy path for free.
- Test-only items in the public API: `Connection::from_tcp_stream`,
  `build_known_packs_payload`, `TcpTransport`'s public fields →
  `#[doc(hidden)]` / `pub(crate)`; delete the dead duplicate
  `MineRiderError::Crypto` variant (only the mock constructs it).
- `Connection` owns `ConnectionState` with unvalidated `set_state`; document
  as informational; transition enforcement belongs to `Client` later.

**Testing:**
- Login disconnect, plugin-request, properties-parsing, unknown-packet, and
  packet-limit branches are **never exercised**; `MineRiderError::Disconnected`
  / `Io` / `Crypto` never asserted. MockServer needs a scripted builder with
  "send disconnect / send plugin request" steps.
- Wrap every `Client::connect` in tests with a 10 s overall timeout (current
  worst case: hours).
- Un-awaited mock peers can die silently — add a `spawn_peer` helper joined
  under timeout.

**Open source (blocks publishing):**
- No `LICENSE-MIT` / `LICENSE-APACHE` files despite the SPDX declaration.
- `minerider-protocol` path dep lacks `version` → `cargo publish` rejects it;
  add `[workspace.package]` inheritance; both manifests need `repository`.
- README claims env-gated live tests that don't exist — implement or remove.
- No CI at all (build/test/clippy/fmt matrix incl. **windows-latest** —
  development is on Windows).

## 3. Nice-to-have improvements

- `#![warn(missing_docs)]` both crates; doc the error variants and
  `ConnectionState`.
- `PacketWriter::put_string` counts chars twice; start `PacketWriter` at 32 B.
- `PacketReader` mixes `get_*`/`read_*` naming; three buffer types
  (`Vec<u8>`, `BytesMut`, `Bytes`) across the wire API — standardize on
  `BytesMut`.
- Zip-bomb cap (64 MiB) vs frame cap (2 MiB) — with C3 fixed the 64 MiB cap
  is unreachable defense-in-depth; fine to keep.
- Non-minimal VarInt encodings: decoder silently accepts (vanilla does too) —
  pin the decision with a test.
- Secret zeroization (`zeroize` crate) for the shared secret / AES state —
  forensic hygiene, not wire-exploitable.
- Reject duplicate Encryption Request mid-login (re-enabling resets CFB8
  registers; self-DoS only).
- progress.md → CHANGELOG.md (Keep a Changelog); CONTRIBUTING.md,
  `rust-toolchain.toml` (pin 1.85), examples/, issue/PR templates,
  SECURITY.md, cargo-deny license job.
- Doctests for `minerider-protocol` public modules (free tests on docs.rs).

## 4. Performance opportunities (ranked)

1. **Single-copy packet pipeline** — send path currently 3 allocs / 3 copies
   per packet (uncompressed), 5–6 allocs / 4 copies (compressed); receive
   path copies the payload once where `advance()` on the owned `BytesMut`
   gives zero-copy. Per-connection scratch buffer for encode + advance-based
   payload split removes ~5 of 6 copies on the hottest code in the engine.
2. **Per-bot RAM diet** — the 8 KiB read chunk lives inside the
   `read_packet` future (resident per task); read directly into `read_buf`
   spare capacity and decrypt the tail in place. `read_buf` never shrinks:
   one 2 MiB frame inflates a bot forever → shrink-when-empty policy.
   Halves per-bot RAM (~30–45 KiB → ~15–25 KiB idle) and bounds the worst
   case; **this is what makes 1000 bots/machine comfortable**.
3. **Pooled zlib state** — per-packet `ZlibEncoder`/`Decoder` allocate
   ~100–150 KiB of state each call. A num_cpus-sized pool of
   `flate2::Compress`/`Decompress` with `reset()` kills the churn without
   per-bot memory cost (per-connection persistence would cost 150–300 MiB at
   1000 bots — rejected).
4. Timer registration per socket read → hoist the `Sleep`, `reset()` after
   each read.
5. Syscall per packet on send → batch encode, flush once per loop iteration
   (matters in Phase 3's movement/chunk-ack bursts).

**Measured, not guessed:** AES-128-CFB8 at ~62 MiB/s is the hardware bound
(serial AES-NI latency chain, ~16 ns/byte) and is **never the per-connection
bottleneck** (play traffic peaks 1–2 MiB/s; 1000 bots × 200 KiB/s ≈ 3–4
cores aggregate). The only remaining micro-opt (u128 shift register, ~10%)
is benchmark-first. Leave AES alone otherwise.

**Verified clean:** zero `unwrap`/`expect` outside tests; no reachable panic
from server input (other than the fixed C4 abort); VarInt/VarLong/Position
arithmetic overflow-free; frame cap checked before allocation; string/
identifier validation strict; async model (one runtime, one task per bot,
no Arc/Mutex/singletons) is exactly right.

## 5. Memory optimizations

- Idle bot today: **~30–45 KiB RSS** (16 KiB `read_buf` + 8 KiB
  future-resident chunk + cipher + structs). With §4.2: ~15–25 KiB.
  1000 idle bots ≈ 35–50 MiB user-space today → ~20 MiB after the diet
  (kernel socket buffers extra, as traffic flows).
- Worst case today: unshrinkable `read_buf` → 2 MiB/bot → 2 GiB at 1000
  bots. The shrink policy removes this.
- Future (per roadmap): generated registries as `&'static`/phf tables —
  zero heap, zero parse-at-startup; per-bot budget ≤2 MB without world,
  ≤8–16 MB with bounded chunk cache.

## 6. API improvements

- `send_packet` should consume `RawPacket`/`BytesMut`, not `&[u8]` (C-adjacent
  copy elimination; Phase 2 calls it per packet).
- `read_string` returning `&'a str` is good; `read_identifier` should return
  `Cow<'a, str>` (double String alloc today; hot after Phase 2).
- `Handshake<'a>` borrow pattern is the right precedent — apply to other
  packet structs as they appear.
- Promote `parse_login_success`/`parse_encryption_request` to `pub(crate)`
  for cheap unit tests and fuzz targets.
- Error ergonomics: one error type per crate (done), but merge the
  crypto-variant duplication; downstream users currently see two
  indistinguishable crypto errors.

## 7. Roadmap for v0.2 — "Data-driven protocol + proof"

1. **`crates/minerider-codegen` + xtask** — minecraft-data 1.21.4 pinned
   (submodule/tarball); `protocol.json` → intermediate model. Exit: model
   round-trips.
2. **Emission → `crates/minerider-protocol/src/generated/`** (path decided by
   the audit: wire definitions belong to the protocol crate) — packet-id
   enums, typed structs, **one encoder + one decoder from the same schema**
   (kills BAC's encode/decode drift). Exit: `cargo xtask generate`
   reproduces committed output byte-identically.
3. **Rewire client to generated ids** — delete every hardcoded id
   (keep-alive, login, configuration); dispatch via generated `match`.
   Round-trip tests for every generated packet.
4. **Drift test + CI** — structure-hash against upstream data; CI matrix
   (win/linux/mac × stable/beta + MSRV 1.85), clippy `-D warnings`, fmt.
5. **Real-server validation** — pay off the owed Paper 1.21.4 check in CI
   (dockerized offline Paper, env-gated). *This is the test that would have
   caught C1/C2.*
6. **Security follow-ups** — write timeout, connect deadline, RSA key-size
   bounds (all from §2).
7. **Swarm bench v1** — 100 concurrent clients vs MockServer on one runtime;
   publish bots/core + RSS/bot in `docs/benchmarks.md`.

Definition of done: 1.21.4 fully data-driven, zero hardcoded ids, drift CI
green, live-server gate green, first published density numbers.

## 8. Roadmap for v0.5 — "Mineflayer-core parity"

- **M1 World** — native chunk decoding (sections, palettes, heightmaps;
  chunk-unload Z-first quirk), bounded LRU chunk cache (cost-based
  eviction). Bench: MB/bot with 8-radius cache; chunk-burst packets/s.
- **M2 Tick engine + physics** — 20 Hz loop, vanilla constants (from
  `bac_analysis.md`), teleport confirm, movement sync, bounded entity store
  (slotmap, ~4 096 cap). Bench: zero steady-state allocs/tick (dhat);
  vanilla-replay movement test.
- **M3 Inventory** — windows, 1.21.4 slot/component layout, stateId/
  action-number/changed-slots tracking, dig/place/use. Parity gate:
  mineflayer `digger`/`chest` examples ported.
- **M4 Second protocol version** — generator validated on 2 versions;
  version auto-select at connect; per-version live tests.
- **M5 Behaviors + pathfinder v1** — behavior stack trait, A* over the
  chunk cache, tick-budgeted search (never blocks other bots — structural
  win over Node's event loop).
- **M6 `minerider-lua` v1** — per-bot mlua states, event queue drained per
  tick, coroutine-style actions, instruction/memory budgets, sandbox,
  hot-reload. Parity gate: mineflayer `echo`/`guard` in Lua.

Honest scope: 2 versions vs Mineflayer's ~20; depth on recent versions is
the segment where per-bot cost matters.

## 9. Roadmap for v1.0 — "Surpass", measurably

| Goal | Target | Baseline to beat |
|------|--------|------------------|
| Idle RSS per bot (no world) | ≤2 MB | Mineflayer ~200–400 MB (user report, not controlled — measure a fair Node baseline ourselves) |
| RSS per bot with world cache | ≤16 MB | same |
| Bots per core (idle, p99 tick <5 ms) | ≥500 | ~10/process practical ceiling |
| Concurrent joins without timeouts | ≥200 in one process | failures ~60 reported |
| Steady-state allocations | 0/tick (dhat-verified) | V8 GC pauses |
| Memory stability | flat RSS, 24 h bot-churn soak | leak-class reports |
| Version coverage | ≥6 recent versions; new-version turnaround ≤1 day via codegen | full 1.8+ (accepted gap) |
| Distribution | single static binary; `minerider-protocol` WASM build | Node ≥18 + npm tree |
| Fidelity | vanilla replay traces match | known-detection complaints |

v1.0 milestones: **M7** online-mode auth (Microsoft/Mojang session — blocks
parity on non-cracked servers), **M8** remaining interaction surface
(combat/vehicles), **M9** JPS pathfinding + long-range navigation,
**M10** hardening: decoder fuzzing, soak tests, published Node-vs-Rust
harness in-repo, docs site with migration guide.

Ecosystem strategy: Mineflayer-compatible event names where semantics match,
side-by-side migration table, 1:1 ports of their examples, a reproducible
"100 bots on a $5 VPS" benchmark.

---

### Appendix — what the audits verified as already solid

- Crate split genuinely clean: no game logic in `minerider-protocol`, no
  wire logic in the client; no dependency cycles; no tokio in protocol.
- Crypto correct: AES-CFB8 feedback direction verified byte-by-byte and
  openssl-KAT-pinned; RSA PKCS#1 v1.5 usage matches vanilla; no client-side
  padding oracle (client only encrypts).
- Protocol primitives byte-exact: VarInt/VarLong/Position/UUID/String
  vectors match vanilla docs.
- 71 tests green, clippy `-D warnings` and rustfmt clean.
