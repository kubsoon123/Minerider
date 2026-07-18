//! Production embedded Lua scripting: one Rust-owned swarm of bots
//! controlled by a fixed pool of persistent Lua workers.
//!
//! See `docs/lua_wrapper.md` for the architecture and `docs/lua_api_reference.md`
//! for the full scripting API. The design here is the one validated by
//! `docs/lua_runtime_benchmark.md` (via `src/lua_benchmark/`) — 4 persistent
//! workers by default, deterministic `bot_id % worker_count` assignment,
//! never a VM or OS thread per bot or per event.
//!
//! Only compiled with `--features lua`; the default build never pulls in
//! `mlua` or compiles the vendored Lua C sources.

pub mod api;
pub mod convert;
pub mod dispatcher;
pub mod error;
pub mod event;
pub mod queue;
pub mod registry;
pub mod runtime;
pub mod sandbox;
pub mod shared_value;
pub mod worker;
