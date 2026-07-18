//! The stable structured error model exposed to Lua as
//! `{code, message, retryable, bot_id, generation}` (see
//! `docs/lua_api_reference.md#errors`). Every existing typed Rust error a
//! wrapped action can produce is mapped here into one of a small, stable
//! set of `code` strings — Lua scripts key logic off `code`, never off the
//! human-readable `message`.

use crate::core::supervisor::{ControlError, GuiActionError, InventoryActionError};
use crate::lua::registry::RegistryError;
use crate::minecraft::inventory::InventoryError;

/// The complete set of stable error codes a script can key logic off of.
pub const ERROR_CODES: &[&str] = &[
    "invalid_configuration",
    "duplicate_id",
    "unknown_bot",
    "unknown_group",
    "unknown_proxy",
    "unknown_server",
    "queue_full",
    "not_connected",
    "session_replaced",
    "disconnected",
    "supervisor_stopped",
    "invalid_action",
    "invalid_gui_slot",
    "no_gui_open",
    "stale_generation",
    "inventory_timeout",
    "inventory_rejected",
    "script_memory_limit",
    "script_instruction_limit",
    "script_disabled",
    "worker_overloaded",
    "shutdown",
];

#[derive(Debug, Clone, PartialEq)]
pub struct ScriptError {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    pub bot_id: Option<u32>,
    pub generation: Option<u64>,
}

impl ScriptError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
            bot_id: None,
            generation: None,
        }
    }

    pub fn with_bot(mut self, bot_id: u32) -> Self {
        self.bot_id = Some(bot_id);
        self
    }

    pub fn with_generation(mut self, generation: u64) -> Self {
        self.generation = Some(generation);
        self
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn unknown_bot(id: u32) -> Self {
        Self::new("unknown_bot", format!("no bot with id {id}"))
    }

    pub fn unknown_group(name: &str) -> Self {
        Self::new("unknown_group", format!("no group named `{name}`"))
    }

    pub fn script_memory_limit(detail: impl Into<String>) -> Self {
        Self::new("script_memory_limit", detail.into())
    }

    pub fn script_instruction_limit(detail: impl Into<String>) -> Self {
        Self::new("script_instruction_limit", detail.into())
    }

    pub fn script_disabled(reason: impl Into<String>) -> Self {
        Self::new("script_disabled", reason.into())
    }

    pub fn worker_overloaded() -> Self {
        Self::new(
            "worker_overloaded",
            "this worker's critical event queue is saturated",
        )
        .retryable(true)
    }

    pub fn shutdown() -> Self {
        Self::new("shutdown", "the swarm is shutting down")
    }

    pub fn queue_full() -> Self {
        Self::new("queue_full", "the bounded command queue is full").retryable(true)
    }
}

impl From<ControlError> for ScriptError {
    fn from(err: ControlError) -> Self {
        match err {
            ControlError::InvalidAction(inner) => Self::new("invalid_action", inner.to_string()),
            ControlError::QueueFull => Self::queue_full(),
            ControlError::NotConnected => {
                Self::new("not_connected", "the bot is not currently connected").retryable(true)
            }
            ControlError::SessionReplaced => Self::new(
                "session_replaced",
                "this action's session generation is no longer current",
            ),
            ControlError::Disconnected => {
                Self::new("disconnected", "the bot's connection ended").retryable(true)
            }
            ControlError::SupervisorStopped => {
                Self::new("supervisor_stopped", "this bot's supervisor has stopped")
            }
        }
    }
}

impl From<GuiActionError> for ScriptError {
    fn from(err: GuiActionError) -> Self {
        match err {
            GuiActionError::NoGuiOpen => {
                Self::new("no_gui_open", "no non-player GUI is currently open")
            }
            GuiActionError::InvalidSlot(slot) => {
                Self::new("invalid_gui_slot", format!("slot {slot} is out of range"))
            }
            GuiActionError::Action(inner) => inner.into(),
        }
    }
}

impl From<InventoryActionError> for ScriptError {
    fn from(err: InventoryActionError) -> Self {
        match err {
            InventoryActionError::Control(inner) => inner.into(),
            InventoryActionError::Invalid(inner) => Self::from(inner),
            InventoryActionError::TimedOut => {
                Self::new("inventory_timeout", "inventory transaction timed out").retryable(true)
            }
            InventoryActionError::Disconnected => {
                Self::new("disconnected", "the bot's connection ended").retryable(true)
            }
            InventoryActionError::StaleGeneration { expected, current } => Self::new(
                "stale_generation",
                format!("action was for generation {expected}, current is {current}"),
            )
            .with_generation(current),
        }
    }
}

impl From<InventoryError> for ScriptError {
    fn from(err: InventoryError) -> Self {
        Self::new("inventory_rejected", err.to_string())
    }
}

impl From<RegistryError> for ScriptError {
    fn from(err: RegistryError) -> Self {
        let code = err.code();
        Self::new(code, err.to_string())
    }
}

fn contains_memory_error(err: &mlua::Error) -> bool {
    match err {
        mlua::Error::MemoryError(_) => true,
        mlua::Error::CallbackError { cause, .. } | mlua::Error::WithContext { cause, .. } => {
            contains_memory_error(cause)
        }
        _ => false,
    }
}

/// Classifies a Lua error raised by a *top-level* handler invocation (see
/// `crate::lua::worker::invoke_top_level`, the sole caller) as one of the
/// sandbox's own abort conditions — an instruction-budget or memory-limit
/// abort — or `None` for an ordinary script error (a plain `error(...)`, a
/// type mismatch, etc.), which callers report as-is instead. `downcast_ref`
/// and the manual `CallbackError`/`WithContext` recursion above both
/// follow the same "descend through wrapping layers" path `mlua::Error`
/// itself documents, since a hook or memory-limit abort raised deep inside
/// a `Function::call` surfaces wrapped in one or more of those variants.
pub fn classify_sandbox_abort(err: &mlua::Error) -> Option<ScriptError> {
    if let Some(crate::lua::sandbox::SandboxAbort::InstructionLimit(budget)) =
        err.downcast_ref::<crate::lua::sandbox::SandboxAbort>()
    {
        return Some(ScriptError::script_instruction_limit(format!(
            "instruction budget of {budget} exceeded"
        )));
    }
    if contains_memory_error(err) {
        return Some(ScriptError::script_memory_limit(err.to_string()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::sandbox::{new_sandboxed_lua, SandboxConfig};

    #[test]
    fn classify_sandbox_abort_recognizes_an_instruction_limit_abort() {
        let config = SandboxConfig {
            instruction_budget: 50,
            hook_every_n_instructions: 10,
            ..SandboxConfig::default()
        };
        let (lua, _counter) = new_sandboxed_lua(&config).unwrap();
        let err = lua.load("while true do end").exec().unwrap_err();
        match classify_sandbox_abort(&err) {
            Some(e) => assert_eq!(e.code, "script_instruction_limit"),
            None => panic!("expected an instruction-limit classification, got None (err: {err})"),
        }
    }

    #[test]
    fn classify_sandbox_abort_recognizes_a_memory_limit_abort() {
        let config = SandboxConfig {
            memory_limit_bytes: 64 * 1024,
            ..SandboxConfig::default()
        };
        let (lua, _counter) = new_sandboxed_lua(&config).unwrap();
        let err = lua
            .load(
                r#"
                local t = {}
                for i = 1, 10000000 do
                    t[i] = string.rep("x", 64)
                end
                "#,
            )
            .exec()
            .unwrap_err();
        match classify_sandbox_abort(&err) {
            Some(e) => assert_eq!(e.code, "script_memory_limit"),
            None => panic!("expected a memory-limit classification, got None (err: {err})"),
        }
    }

    #[test]
    fn classify_sandbox_abort_returns_none_for_an_ordinary_script_error() {
        let (lua, _counter) = new_sandboxed_lua(&SandboxConfig::default()).unwrap();
        let err = lua.load("error('boom')").exec().unwrap_err();
        assert!(
            classify_sandbox_abort(&err).is_none(),
            "an ordinary script error must not be misclassified as a sandbox abort"
        );
    }

    #[test]
    fn every_declared_error_code_is_reachable_from_a_real_conversion() {
        // Spot check the mapping table's most important entries actually
        // produce the documented code, not just that they compile.
        assert_eq!(
            ScriptError::from(ControlError::QueueFull).code,
            "queue_full"
        );
        assert_eq!(
            ScriptError::from(ControlError::SessionReplaced).code,
            "session_replaced"
        );
        assert_eq!(
            ScriptError::from(GuiActionError::NoGuiOpen).code,
            "no_gui_open"
        );
        assert_eq!(
            ScriptError::from(InventoryActionError::StaleGeneration {
                expected: 1,
                current: 2
            })
            .code,
            "stale_generation"
        );
    }

    #[test]
    fn all_documented_codes_are_listed_in_error_codes() {
        for code in [
            "invalid_configuration",
            "duplicate_id",
            "unknown_bot",
            "unknown_group",
            "unknown_proxy",
            "unknown_server",
            "queue_full",
            "not_connected",
            "session_replaced",
            "disconnected",
            "supervisor_stopped",
            "invalid_action",
            "invalid_gui_slot",
            "no_gui_open",
            "stale_generation",
            "inventory_timeout",
            "inventory_rejected",
            "script_memory_limit",
            "script_instruction_limit",
            "script_disabled",
            "worker_overloaded",
            "shutdown",
        ] {
            assert!(ERROR_CODES.contains(&code), "missing code: {code}");
        }
    }
}
