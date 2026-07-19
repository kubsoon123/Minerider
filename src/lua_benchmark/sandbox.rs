//! Re-exports the production sandbox (`crate::lua::sandbox`) so this
//! benchmark measures the exact restricted-stdlib / memory-limit /
//! instruction-budget sandbox the production runtime ships, rather than a
//! second parallel implementation. See `crate::lua::sandbox` for the actual
//! implementation and `docs/lua_runtime_benchmark.md` for the measurements
//! that produced its defaults.

pub use crate::lua::sandbox::{new_sandboxed_lua, SandboxConfig};
