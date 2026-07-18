//! Benchmark-only: validates the Lua runtime architecture (one shared VM vs.
//! a small fixed worker pool vs. one VM per bot) *before* the real
//! scripting wrapper is built. Not the wrapper itself — see
//! `docs/lua_runtime_benchmark.md` for the full report and
//! `docs/lua_design.md` for the design this measures against.
//!
//! Only compiled with `--features lua-benchmark`; the default build never
//! pulls in `mlua` or compiles the vendored Lua C sources.

#[cfg(test)]
pub mod chunk_proof;
pub mod command;
pub mod dispatch;
pub mod event;
pub mod fake_socks5;
pub mod full_runtime;
pub mod lua_api;
pub mod metrics;
pub mod queue;
pub mod sandbox;
pub mod scripts;
pub mod synthetic;
pub mod worker;
