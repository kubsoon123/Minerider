//! The production Lua sandbox: restricted stdlib, a real memory limit, and a
//! real per-handler-invocation instruction budget. One worker (see
//! `worker.rs`) owns exactly one of these for its whole lifetime.
//!
//! This is the same design validated in `docs/lua_runtime_benchmark.md` —
//! promoted here rather than duplicated; `src/lua_benchmark/sandbox.rs` now
//! just re-exports this module so the benchmark harness measures the exact
//! sandbox the production runtime ships.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use mlua::{HookTriggers, Lua, LuaOptions, StdLib, VmState};

/// Raised by the instruction-count hook below when a single top-level
/// invocation's budget is exceeded. Wrapped as an `mlua::Error::external`
/// (rather than a bare `RuntimeError(String)`) specifically so callers can
/// reliably classify "this was a sandbox abort" via `err.downcast_ref`
/// instead of pattern-matching an error message — see
/// `crate::lua::error::classify_sandbox_abort`, the only place that reads
/// this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SandboxAbort {
    #[error("instruction budget of {0} exceeded")]
    InstructionLimit(u64),
}

/// Bounds applied to every worker's Lua VM.
#[derive(Debug, Clone, Copy)]
pub struct SandboxConfig {
    pub memory_limit_bytes: usize,
    /// VM instructions between hook checks. Lower = tighter budget
    /// enforcement, higher hook overhead.
    pub hook_every_n_instructions: u32,
    /// Hook *firings* allowed for one handler invocation before it is
    /// aborted as runaway — the real VM instruction count this bounds is
    /// `instruction_budget * hook_every_n_instructions`. Measured directly
    /// in the architecture benchmark: the first default tried (2,000,000
    /// firings, i.e. 2 billion real instructions) took 12.8s wall-clock to
    /// abort a tight empty loop in a debug build. `2,000` firings (≈2
    /// million real instructions, matching the mission's "around two
    /// million Lua instructions per handler invocation" target) lands in
    /// the low-single-digit millisecond range on the same machine; see
    /// `docs/lua_runtime_benchmark.md`'s sandbox-overhead measurements.
    pub instruction_budget: u64,
    /// Consecutive handler errors (including instruction/memory aborts) for
    /// one bot before that bot's script execution is disabled. The
    /// underlying Minecraft connection is never affected — only the Lua
    /// handler invocations for that bot stop running.
    pub consecutive_error_threshold: u32,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            // ~16 MiB per worker: no exact production value was specified
            // by the mission beyond "choose a conservative value such as
            // 16 MiB per worker", so that's what ships as the default. The
            // benchmark harness (much higher event volume, single process)
            // uses a tighter 4 MiB default; production workers run far
            // fewer concurrent handler invocations per instant.
            memory_limit_bytes: 16 * 1024 * 1024,
            hook_every_n_instructions: 1000,
            instruction_budget: 2_000,
            consecutive_error_threshold: 10,
        }
    }
}

/// Builds a Lua VM with only `string`/`table`/`math` opened (no `io`, real
/// `os`, `package`, `debug`, or native module loading), plus a restricted
/// `os` shim exposing only the read-only `time`/`clock`, and a
/// per-handler-call resettable instruction budget.
///
/// Returns the `Lua` and the `Arc<AtomicU64>` instruction counter so the
/// caller can reset it to zero before each handler invocation (a
/// process-lifetime-cumulative counter would eventually trip on a VM that
/// has simply run for a long time, not a runaway single call).
pub fn new_sandboxed_lua(config: &SandboxConfig) -> mlua::Result<(Lua, Arc<AtomicU64>)> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH,
        LuaOptions::new(),
    )?;
    lua.set_memory_limit(config.memory_limit_bytes)?;

    install_restricted_os_shim(&lua)?;

    let counter = Arc::new(AtomicU64::new(0));
    let hook_counter = counter.clone();
    let budget = config.instruction_budget;
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(config.hook_every_n_instructions),
        move |_lua, _debug| {
            let seen = hook_counter.fetch_add(1, Ordering::Relaxed);
            if seen > budget {
                return Err(mlua::Error::external(SandboxAbort::InstructionLimit(
                    budget,
                )));
            }
            Ok(VmState::Continue)
        },
    )?;

    Ok((lua, counter))
}

/// `os.time()`/`os.clock()` only — no `execute`/`remove`/`rename`/`exit`, no
/// filesystem, no process control, no `os.getenv` (proxy credentials must
/// never be readable from Lua; see `crate::lua::api::proxy`). Registered as
/// the global `os` table, replacing (not extending) whatever the real `os`
/// library would have provided — which was never opened via `StdLib`.
fn install_restricted_os_shim(lua: &Lua) -> mlua::Result<()> {
    let os_table = lua.create_table()?;
    os_table.set(
        "time",
        lua.create_function(|_, ()| {
            Ok(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0))
        })?,
    )?;
    os_table.set(
        "clock",
        lua.create_function(|_, ()| {
            Ok(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0))
        })?,
    )?;
    lua.globals().set("os", os_table)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricted_stdlib_rejects_io_and_real_os_and_require() {
        let (lua, _counter) = new_sandboxed_lua(&SandboxConfig::default()).unwrap();
        assert!(
            lua.load("return io").eval::<mlua::Value>().is_err()
                || matches!(
                    lua.load("return io").eval::<mlua::Value>().unwrap(),
                    mlua::Value::Nil
                )
        );
        assert!(lua
            .load("return os.execute")
            .eval::<mlua::Value>()
            .map(|v| matches!(v, mlua::Value::Nil))
            .unwrap_or(true));
        assert!(lua
            .load("return os.getenv")
            .eval::<mlua::Value>()
            .map(|v| matches!(v, mlua::Value::Nil))
            .unwrap_or(true));
        assert!(
            lua.load("return require").eval::<mlua::Value>().is_err()
                || matches!(
                    lua.load("return require").eval::<mlua::Value>().unwrap(),
                    mlua::Value::Nil
                )
        );
        assert!(
            lua.load("return package").eval::<mlua::Value>().is_err()
                || matches!(
                    lua.load("return package").eval::<mlua::Value>().unwrap(),
                    mlua::Value::Nil
                )
        );
        assert!(
            lua.load("return debug").eval::<mlua::Value>().is_err()
                || matches!(
                    lua.load("return debug").eval::<mlua::Value>().unwrap(),
                    mlua::Value::Nil
                )
        );
    }

    #[test]
    fn restricted_os_shim_exposes_only_time_and_clock() {
        let (lua, _counter) = new_sandboxed_lua(&SandboxConfig::default()).unwrap();
        let t: f64 = lua.load("return os.clock()").eval().unwrap();
        assert!(t >= 0.0);
        let secs: u64 = lua.load("return os.time()").eval().unwrap();
        assert!(secs > 0);
        assert!(matches!(
            lua.load("return os.execute").eval::<mlua::Value>().unwrap(),
            mlua::Value::Nil
        ));
    }

    #[test]
    fn string_table_math_are_available() {
        let (lua, _counter) = new_sandboxed_lua(&SandboxConfig::default()).unwrap();
        let v: i64 = lua.load("return math.floor(3.7)").eval().unwrap();
        assert_eq!(v, 3);
        let s: String = lua.load("return string.upper('a')").eval().unwrap();
        assert_eq!(s, "A");
        let n: i64 = lua.load("local t = {1,2,3} return #t").eval().unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn instruction_budget_aborts_an_infinite_loop() {
        let config = SandboxConfig {
            instruction_budget: 5_000,
            hook_every_n_instructions: 100,
            ..SandboxConfig::default()
        };
        let (lua, _counter) = new_sandboxed_lua(&config).unwrap();
        let result: mlua::Result<()> = lua.load("while true do end").exec();
        assert!(result.is_err(), "infinite loop must be aborted, not hang");
    }

    #[test]
    fn memory_limit_rejects_unbounded_table_growth() {
        let config = SandboxConfig {
            memory_limit_bytes: 64 * 1024,
            ..SandboxConfig::default()
        };
        let (lua, _counter) = new_sandboxed_lua(&config).unwrap();
        let result: mlua::Result<()> = lua
            .load(
                r#"
                local t = {}
                for i = 1, 10000000 do
                    t[i] = string.rep("x", 64)
                end
                "#,
            )
            .exec();
        assert!(
            result.is_err(),
            "unbounded growth must hit the memory limit"
        );
    }

    #[test]
    fn a_bounded_finite_script_completes_within_budget() {
        let (lua, counter) = new_sandboxed_lua(&SandboxConfig::default()).unwrap();
        let v: i64 = lua
            .load("local sum = 0 for i = 1, 100 do sum = sum + i end return sum")
            .eval()
            .unwrap();
        assert_eq!(v, 5050);
        assert!(counter.load(Ordering::Relaxed) < SandboxConfig::default().instruction_budget);
    }
}
