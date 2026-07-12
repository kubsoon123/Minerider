# Paper / vanilla 1.21.4 real-server validation report

Date: 2026-07-12. MineRider commit range: `b8ba1e5` → HEAD (validation session).
Verdict: **MineRider connects, logs in, enters play, answers all mandatory
obligations observed on both servers, and idles without kicks. Two P0
protocol obligations were found via traces, fixed, and re-validated.**

## 1. Environment

| Item | Value |
|---|---|
| OS | Windows 11, Git Bash + Windows PowerShell 5.1 |
| Rust | 1.97.0 (2026-07-07) |
| System Java | 1.8.0_491 (too old — unused) |
| Test JDK | Eclipse Temurin 21 JRE, local at `.test-servers/jdk-21/` |
| Paper | 1.21.4 build 232, sha256 `5ee4f542…4eaffc`, from fill.papermc.io (v3 API) |
| Vanilla | 1.21.4 dedicated server, sha1 `4707d00e…8c001`, from piston-meta.mojang.com |
| Protocol | 769 |
| Binding | both servers `127.0.0.1` only (Paper 25565/RCON 25575, vanilla 25566/RCON 25576), offline mode |

## 2. Server setup

- `.test-servers/paper-1.21.4/` and `.test-servers/vanilla-1.21.4/` (git-ignored);
  `server-metadata.json` in each records source, build, checksum, timestamp.
- Scripts in `scripts/test-server/`: setup, start, stop, reset-world,
  validate-ready, run-minerider-validation, rcon (localhost-only RCON client).
- Fixed seed `1234567890123`, creative/peaceful, view-distance 6,
  simulation-distance 4, compression threshold 256.

Notable setup fixes: PaperMC's v2 API is sunset (410) — setup uses the
official **fill.papermc.io v3** API. Paper's RCON resets connections when
the length prefix arrives in its own TCP segment — the RCON script sends
length+body as one buffer (documented in the script).

## 3. First real connection (Paper, 60 s)

Command: `cargo run --release -- 127.0.0.1 25565 MineRiderTest` with
`MINERIDER_TRACE` (trace: `.test-servers/traces/minerider-20260712T184616Z.jsonl`,
18840 events).

State transitions: TCP → handshake → login start → set compression (256) →
login success → login acknowledged → configuration (registry data, feature
flags, tags, known packs echo) → finish configuration → **play**. Server
log: `MineRiderTest joined the game`, disconnected only by the harness
after exactly 60 s. No kick, no protocol error, 3/3 keep-alive echoes.

## 4. Divergences found (from traces, not guesses)

1. **`position` (play 0x42) received without `teleport_confirm`** —
   mandatory Strict obligation. Paper tolerated it; vanilla parity requires
   the response. → fixed.
2. **`chunk_batch_finished` never observed on Paper** — Paper 1.21.4's
   chunk sender (moonrise) did not emit batch markers even for a 162-chunk
   teleport burst, so `chunk_batch_received` could not be exercised there.
   Implemented per the vanilla obligation, then validated against the
   vanilla server (below).
3. **Log flood** — per-packet "not handled" warnings for high-frequency
   ignored packets exceeded 16 MiB in 6 minutes of soak. → fixed
   (warn once per packet id, then debug).

Unknown packets: none. All 40 distinct clientbound packets seen on Paper
decoded successfully via generated definitions.

## 5. Fixes (one commit each)

| Commit | Change | Tests |
|---|---|---|
| `fix(client): confirm server position synchronizations` | decode `position`, apply relative flags into `PlayerPosition`, echo `teleport_confirm` | mock requires confirm; conformance scenario 4 PASS + fixture |
| `fix(client): acknowledge completed chunk batches` | decode `chunk_batch_finished`, answer `chunk_batch_received` (chunks-per-tick = batch size) | mock requires ack; conformance scenario 3 PASS + fixture |
| `fix(client): rate-limit unhandled-packet warnings` | bounded once-per-id warning set | full workspace suite |

Workspace after fixes: 146+ tests green, clippy `-D warnings`, rustfmt,
codegen drift and matrix drift gates all pass.

## 6. Scenario validation

### Teleport correction (Paper + vanilla)

RCON `tp MineRiderTest 10 100 10` then `tp … 12 101 12` (Paper), and a
5000-block teleport (vanilla). Trace: every `position` (join id 1,
then ids 2, 3) followed within ~0.1 ms by `teleport_confirm` echoing the
id. Server logs show the teleports succeeded, no complaints. **PASS vs
both servers** (PARTIAL in the matrix: no vanilla-client reference
capture yet).

### Chunk batch acknowledgement (vanilla)

Paper: not triggerable (see §4.2). Vanilla 1.21.4: join + 5000-block
teleport produced **21 chunk batches / 49 chunks; 21/21
`chunk_batch_received` acknowledgements, each immediately after its
`chunk_batch_finished`**, server continued streaming, no disconnect.
**PASS vs vanilla server**, mock-covered, blocked-item closed.

### Keep-alive

Paper 60 s soak: 3/3 echoes. Vanilla: 4/4 echoes. No keep-alive kick on
either server. **PASS vs both servers.**

### Idle soak

- 60 s: PASS (§3).
- 5 min (Paper): PASS — RSS 7.8 MB at join and 7.8 MB at 5 min, ~2 s
  total CPU, 102k traced packets, no disconnect. (Run truncated at ~6.4 min
  by the harness output limit, which motivated the log fix.)
- 30 min (Paper): running at report time; results appended below.

## 7. Resource baseline (single bot, debug-logging off)

| Metric | Value |
|---|---|
| RSS at play entry | 7.8 MB |
| RSS after 5 min idle | 7.8 MB (no growth) |
| CPU (idle, cumulative) | ~2 s over 6 min (<1%) |
| Packets observed | ~280/s burst at join, ~170/s steady (entity/world spam from the existing test world) |
| Unknown packets | 0 |
| Trace buffering | unbounded-memory risk: none (per-event file flush; RSS flat) |

Caveat: one process, includes tokio runtime overhead — not a per-bot
production number.

## 8. Remaining blockers

Before full vanilla-client reference parity:
- **Vanilla-client reference capture** (procedure in
  `docs/real_server_validation.md` §13): needs a manual vanilla client run
  against the local servers; no credentials are intercepted.
- Vanilla-default client behavior not yet sent: `client information`
  (settings), brand `custom_payload`, play `pong` reply — never demanded
  by either server; candidates for the fidelity pass, not blockers.
- `cookie_request`/`cookie_response`, resource-pack responses, `transfer`,
  `start_configuration` — classified NOT IMPLEMENTED, not exercised by
  either local server.

Before Phase 3: none from this validation. Phase 3 (tick engine, player
state) may proceed; position state already exists in `play.rs`.

## 9. Reproduction

```powershell
scripts\test-server\setup-paper-1.21.4.ps1
scripts\test-server\start-paper-1.21.4.ps1
scripts\test-server\validate-server-ready.ps1
scripts\test-server\run-minerider-validation.ps1 -DurationSeconds 60
scripts\test-server\rcon.ps1 -Command "tp MineRiderTest 10 100 10"
scripts\test-server\stop-paper-1.21.4.ps1
```

Vanilla server setup is documented in `docs/real_server_validation.md`
(official Mojang jar, sha1-verified, port 25566).
