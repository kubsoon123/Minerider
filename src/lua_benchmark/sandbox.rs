//! Benchmark-grade Lua sandbox: the restricted-stdlib / memory-limit /
//! instruction-budget triple from `docs/lua_design.md`'s security model,
//! built for real here so this benchmark's numbers reflect the sandbox the
//! future wrapper will actually ship, not an unrestricted `Lua::new()`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use mlua::{HookTriggers, Lua, LuaOptions, StdLib, VmState};

/// Bounds applied to every Lua VM this benchmark creates, whatever
/// architecture candidate owns it.
#[derive(Debug, Clone, Copy)]
pub struct SandboxConfig {
    pub memory_limit_bytes: usize,
    /// VM instructions between hook checks. Lower = tighter budget
    /// enforcement, higher hook overhead; see "Measure the overhead of
    /// these protections" in the mission and this benchmark's own results.
    pub hook_every_n_instructions: u32,
    /// Hook *firings* allowed for one handler invocation before it is
    /// aborted as runaway (Script 6's `while true do end`) — the real VM
    /// instruction count this bounds is `instruction_budget *
    /// hook_every_n_instructions`. Measured directly: the first default
    /// tried here (2,000,000, i.e. 2 billion real instructions) took 12.8s
    /// wall-clock to abort a tight empty loop in a debug build — nowhere
    /// near `docs/lua_design.md`'s "low-single-digit milliseconds" target.
    /// 2,000 (≈2,000,000 real instructions) lands in the low-single-digit
    /// millisecond range on the same machine; see
    /// `docs/lua_runtime_benchmark.md`'s sandbox-overhead measurements.
    pub instruction_budget: u64,
    /// Consecutive handler errors (including instruction/memory aborts)
    /// before a bot's script execution is disabled — the connection itself
    /// is never affected (see `docs/lua_design.md`'s error model).
    pub consecutive_error_threshold: u32,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            memory_limit_bytes: 4 * 1024 * 1024,
            hook_every_n_instructions: 1000,
            instruction_budget: 2_000,
            consecutive_error_threshold: 10,
        }
    }
}

/// Builds a Lua VM with only `string`/`table`/`math` opened (no `io`, real
/// `os`, `package`, `debug`, or native module loading — see
/// `docs/lua_design.md`'s "Standard libraries"), plus a restricted `os`
/// shim exposing only the read-only `time`/`clock`, and a per-handler-call
/// resettable instruction budget.
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
                return Err(mlua::Error::RuntimeError(format!(
                    "instruction budget of {budget} exceeded"
                )));
            }
            Ok(VmState::Continue)
        },
    )?;

    Ok((lua, counter))
}

/// `os.time()`/`os.clock()` only — no `execute`/`remove`/`rename`/`exit`,
/// no filesystem, no process control. Registered as the global `os` table,
/// replacing (not extending) whatever the real `os` library would have
/// provided — which was never opened in the first place via `StdLib`.
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
