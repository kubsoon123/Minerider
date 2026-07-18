//! The six deterministic Lua scripts every architecture candidate is
//! benchmarked against (see "Lua scripts used by benchmarks"). Each
//! registers a `swarm.on(...)` handler per event name it cares about;
//! `lua_api::install` prepends the `swarm`/`bot` bootstrap before loading
//! whichever of these is selected for a run.

/// Script 1 — no-op: receives the event and returns immediately. The
/// dispatch-overhead floor: whatever this costs over the Rust baseline is
/// pure Lua call/table-construction overhead, not handler logic.
pub const NO_OP: &str = r#"
swarm.on("connected", function(bot_id, event) end)
swarm.on("health", function(bot_id, event) end)
swarm.on("chat", function(bot_id, event) end)
swarm.on("gui_opened", function(bot_id, event) end)
swarm.on("entity_tick", function(bot_id, event) end)
"#;

/// Script 2 — light state: a per-bot counter plus inspecting two or three
/// event fields, no commands emitted.
pub const LIGHT_STATE: &str = r#"
counters = {}
function bump(bot_id)
    counters[bot_id] = (counters[bot_id] or 0) + 1
end
swarm.on("connected", function(bot_id, event) bump(bot_id) end)
swarm.on("health", function(bot_id, event)
    bump(bot_id)
    local ok = event.health > 0 and event.food >= 0
end)
swarm.on("chat", function(bot_id, event)
    bump(bot_id)
    local len = #event.message
end)
swarm.on("gui_opened", function(bot_id, event)
    bump(bot_id)
    local wid = event.window_id
end)
swarm.on("entity_tick", function(bot_id, event) bump(bot_id) end)
"#;

/// Script 3 — realistic behavior: inspects the event, reads the bot id,
/// looks at a small per-bot state object, conditionally emits a command —
/// the mission's own illustrative example, verbatim in shape.
pub const REALISTIC: &str = r#"
state = {}
function get_state(bot_id)
    if state[bot_id] == nil then
        state[bot_id] = { moves = 0, last_window = 0 }
    end
    return state[bot_id]
end

swarm.on("connected", function(bot_id, event)
    get_state(bot_id).moves = 0
end)

swarm.on("chat", function(bot_id, event)
    local s = get_state(bot_id)
    if event.message == "move" then
        s.moves = s.moves + 1
        bot.command(bot_id, "forward", true)
    end
end)

swarm.on("gui_opened", function(bot_id, event)
    local s = get_state(bot_id)
    s.last_window = event.window_id
    local first = event.slots[1]
    if first ~= nil and first.item_id ~= 0 then
        bot.command(bot_id, "click_slot", first.raw_slot, "right")
    end
end)

swarm.on("health", function(bot_id, event)
    if event.health <= 0 then
        bot.command(bot_id, "chat", "help")
    end
end)
"#;

/// Script 4 — moderate table work: maintains a small per-bot table with a
/// handful of keys, doing real (bounded) table churn per event rather than
/// a single counter increment.
pub const TABLE_WORK: &str = r#"
history = {}
function record(bot_id, kind)
    local h = history[bot_id]
    if h == nil then
        h = {}
        history[bot_id] = h
    end
    table.insert(h, kind)
    if #h > 16 then
        table.remove(h, 1)
    end
end

swarm.on("connected", function(bot_id, event) record(bot_id, "connected") end)
swarm.on("health", function(bot_id, event) record(bot_id, "health") end)
swarm.on("chat", function(bot_id, event)
    record(bot_id, "chat")
    local words = {}
    for w in string.gmatch(event.message, "%a+") do
        table.insert(words, w)
    end
end)
swarm.on("gui_opened", function(bot_id, event)
    record(bot_id, "gui_opened")
    local total_items = 0
    for i = 1, #event.slots do
        if event.slots[i].item_id ~= 0 then
            total_items = total_items + 1
        end
    end
end)
swarm.on("entity_tick", function(bot_id, event) record(bot_id, "entity_tick") end)
"#;

/// Script 5 — slow handler: bounded but real CPU work (a fixed-iteration
/// loop), simulating a badly written yet finite handler. Must finish inside
/// the instruction budget; if it doesn't with the benchmark's default
/// budget, that itself is a measured result, not a bug.
pub const SLOW_HANDLER: &str = r#"
swarm.on("health", function(bot_id, event)
    local acc = 0
    for i = 1, 20000 do
        acc = acc + (i % 7) * (i % 13)
    end
end)
swarm.on("chat", function(bot_id, event)
    local acc = 0
    for i = 1, 20000 do
        acc = acc + (i % 7) * (i % 13)
    end
end)
"#;

/// Script 6 — infinite loop: must be aborted by the instruction budget, not
/// hang the worker or the benchmark process.
pub const INFINITE_LOOP: &str = r#"
swarm.on("connected", function(bot_id, event)
    while true do
    end
end)
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptKind {
    NoOp,
    LightState,
    Realistic,
    TableWork,
    SlowHandler,
    InfiniteLoop,
}

impl ScriptKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "no-op" | "noop" => Some(Self::NoOp),
            "light-state" | "light" => Some(Self::LightState),
            "realistic" => Some(Self::Realistic),
            "table-work" | "table" => Some(Self::TableWork),
            "slow-handler" | "slow" => Some(Self::SlowHandler),
            "infinite-loop" | "infinite" => Some(Self::InfiniteLoop),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::NoOp => "no-op",
            Self::LightState => "light-state",
            Self::Realistic => "realistic",
            Self::TableWork => "table-work",
            Self::SlowHandler => "slow-handler",
            Self::InfiniteLoop => "infinite-loop",
        }
    }

    pub fn source(self) -> &'static str {
        match self {
            Self::NoOp => NO_OP,
            Self::LightState => LIGHT_STATE,
            Self::Realistic => REALISTIC,
            Self::TableWork => TABLE_WORK,
            Self::SlowHandler => SLOW_HANDLER,
            Self::InfiniteLoop => INFINITE_LOOP,
        }
    }

    pub const ALL: [ScriptKind; 6] = [
        Self::NoOp,
        Self::LightState,
        Self::Realistic,
        Self::TableWork,
        Self::SlowHandler,
        Self::InfiniteLoop,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua_benchmark::command::CommandSink;
    use crate::lua_benchmark::event::{BenchEvent, BotId};
    use crate::lua_benchmark::lua_api::{event_to_table, handlers_for, install, test_context};
    use crate::lua_benchmark::sandbox::{new_sandboxed_lua, SandboxConfig};

    #[test]
    fn every_non_infinite_script_loads_and_registers_handlers() {
        for kind in [
            ScriptKind::NoOp,
            ScriptKind::LightState,
            ScriptKind::Realistic,
            ScriptKind::TableWork,
            ScriptKind::SlowHandler,
        ] {
            let (lua, _c) = new_sandboxed_lua(&SandboxConfig::default()).unwrap();
            let (sink, _rx) = CommandSink::bounded(8);
            let (ctx, lat) = test_context("connected");
            install(&lua, sink, ctx, lat, kind.source())
                .unwrap_or_else(|e| panic!("script {} failed to load: {e}", kind.name()));
        }
    }

    #[test]
    fn light_state_script_increments_a_real_per_bot_counter() {
        let (lua, _c) = new_sandboxed_lua(&SandboxConfig::default()).unwrap();
        let (sink, _rx) = CommandSink::bounded(8);
        let (ctx, lat) = test_context("connected");
        install(&lua, sink, ctx, lat, ScriptKind::LightState.source()).unwrap();
        let table = event_to_table(&lua, &BenchEvent::Connected).unwrap();
        for handler in handlers_for(&lua, "connected").unwrap() {
            handler.call::<()>((BotId(5).0, table.clone())).unwrap();
        }
        for handler in handlers_for(&lua, "connected").unwrap() {
            handler.call::<()>((BotId(5).0, table.clone())).unwrap();
        }
        let counters: mlua::Table = lua.globals().get("counters").unwrap();
        let value: i64 = counters.get(5u32).unwrap();
        assert_eq!(value, 2);
    }

    #[test]
    fn realistic_script_emits_a_command_only_on_the_move_message() {
        let (lua, _c) = new_sandboxed_lua(&SandboxConfig::default()).unwrap();
        let (sink, rx) = CommandSink::bounded(8);
        let (ctx, lat) = test_context("chat");
        install(&lua, sink, ctx, lat, ScriptKind::Realistic.source()).unwrap();
        let not_move = BenchEvent::Chat {
            sender_len: 1,
            message: "hello".to_string(),
        };
        let table = event_to_table(&lua, &not_move).unwrap();
        for handler in handlers_for(&lua, "chat").unwrap() {
            handler.call::<()>((BotId(1).0, table.clone())).unwrap();
        }
        assert!(
            rx.try_recv().is_err(),
            "no command expected for non-move chat"
        );

        let mv = BenchEvent::Chat {
            sender_len: 1,
            message: "move".to_string(),
        };
        let table = event_to_table(&lua, &mv).unwrap();
        for handler in handlers_for(&lua, "chat").unwrap() {
            handler.call::<()>((BotId(1).0, table.clone())).unwrap();
        }
        assert!(rx.try_recv().is_ok(), "a command is expected for move chat");
    }

    #[test]
    fn infinite_loop_script_is_aborted_by_the_instruction_budget() {
        let config = SandboxConfig {
            instruction_budget: 10_000,
            hook_every_n_instructions: 100,
            ..SandboxConfig::default()
        };
        let (lua, counter) = new_sandboxed_lua(&config).unwrap();
        let (sink, _rx) = CommandSink::bounded(8);
        let (ctx, lat) = test_context("connected");
        install(&lua, sink, ctx, lat, ScriptKind::InfiniteLoop.source()).unwrap();
        let table = event_to_table(&lua, &BenchEvent::Connected).unwrap();
        let handlers = handlers_for(&lua, "connected").unwrap();
        counter.store(0, std::sync::atomic::Ordering::Relaxed);
        let result: mlua::Result<()> = handlers[0].call((BotId(0).0, table));
        assert!(result.is_err(), "infinite loop handler must be aborted");
    }
}
