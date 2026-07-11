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
