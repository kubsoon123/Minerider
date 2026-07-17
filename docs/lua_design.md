# Lua scripting layer — design (not implemented)

**Status: design only.** No `mlua` dependency, no `src/lua/`, no `.lua`
files exist in this repository yet (see `README.md`'s "Not implemented"
section). This document exists so a later, bounded session can implement
Phase 5a without re-deriving these decisions, and so nothing here is
mistaken for a shipped feature.

This design is grounded in the actual Rust APIs as of Phase 4e:
`core::client::Client`, `core::supervisor::ClientSupervisor`,
`minecraft::control::{ControlHandle, BotCommand}`,
`minecraft::event::BotEvent`, `minecraft::play::StateSnapshot`, and the
state types it aggregates (`LocalPlayer`, `EntityStore`, `InventoryState`,
`PlayerList`, `PresentationState`, `ScoreboardState`). Field names below are
copied from those types, not guessed.

## Why Lua sits where it does

Lua scripts must never get direct access to `Connection`, `PlayState`, or
any mutable Rust internals — only the same narrow surfaces an external Rust
caller already gets: push commands through `ControlHandle`, read
`StateSnapshot`, and react to `BotEvent`. This means the Lua layer is
additive: everything it can do, a Rust caller could already do today by
calling `Client::control()`/`bot_state()`/`events()` directly. Lua doesn't
unlock new capability, it makes existing capability scriptable.

## Runtime model

**One Lua state per bot, not shared.** Each bot already runs as its own
tokio task (one `Client`/`ClientSupervisor` per bot; `src/bin/swarm.rs`
spawns one task per bot today). A per-bot `mlua::Lua` instance:
- gives natural isolation — one bot's globals, memory limit, and
  instruction budget can't affect another bot's;
- lets `Lua::set_memory_limit` and the instruction-budget interrupt (below)
  be meaningful per-bot numbers, not a shared pool that one bot could starve;
- matches "no per-bot thread": the VM lives on the bot's *existing* tokio
  task, not a new OS thread.

Cost: one Lua VM's baseline footprint per bot (on the order of a few
hundred KB to low single-digit MB with a restricted standard library,
depending on scripts loaded) — additive to, not a replacement for, the
per-bot Rust-side memory budget discussed in `docs/engineering_review.md`.
This should be measured once implemented, not asserted here.

**Which thread executes Lua, and why `mlua`'s `send` feature.** A Lua VM's
persistent state (globals, loaded functions, the interrupt/budget counters)
must live somewhere for the bot's whole lifetime, which spans many
`.await` points inside that bot's task. On Tokio's multi-threaded runtime, a
spawned task's captured state — including anything stored across an
`.await` — must be `Send`, because the scheduler can move a *suspended*
task to a different worker thread between polls. Plain `mlua::Lua` (default
features) is not `Send`. Two ways to reconcile this:

1. **`mlua` with the `send` feature** (recommended default). Wraps the
   interpreter's internals so `Lua` is `Send` (`Sync` too, if needed) via
   internal locking. The lock is uncontended in the common case (nothing
   else touches a given bot's `Lua` concurrently, since only that bot's own
   task ever calls into it), so the overhead is a small, predictable
   per-call cost, not a design risk.
2. **Pin each bot's task to a single OS thread** (e.g. a
   `tokio::task::LocalSet` per worker, or a small fixed pool of
   current-thread runtimes bots are assigned to round-robin), avoiding the
   `Send` requirement entirely and skipping the lock. More complex (needs a
   scheduling/assignment layer across a fixed thread count as bot count
   grows) and only worth it if profiling later shows the `send`-feature
   lock actually matters. Not the Phase 5a default; noted as the fallback
   if #1's overhead turns out to matter at high bot counts.

Recommendation: start with #1. It's simpler, correct by construction, and
the "no per-bot thread" constraint only rules out a *thread per bot*, not a
small lock inside an otherwise-async design.

**No coroutines / async Lua actions in Phase 5a.** Mapping a Lua coroutine
to a multi-tick asynchronous action (`bot:walk_to(x, z):await()` yielding
across ticks) needs `mlua`'s `async` feature and careful coroutine/executor
integration — real complexity, and exactly the kind of thing that turns a
"quick" scripting layer into an unreviewed pile of edge cases. Phase 5a's
model is deliberately synchronous: event handlers run to completion
(bounded by the instruction budget below) and issue **instantaneous or
persistent commands** through the existing non-blocking `ControlHandle`
(already just an unbounded-channel push — never blocks). A script that
wants "walk there, then do X" implements a tiny state machine itself across
multiple `on("tick", ...)` calls (see Proposed API) — more primitive than
`:await()` sugar, but correct and reviewable. Async coroutine actions are
explicitly future work (Phase 5b+), not this design's job to fully solve.

**Script lifecycle across reconnects.** The Lua VM's lifetime spans the
*supervisor's* lifetime, not any individual `Client` session — it is not
recreated on reconnect. Consequences, which a script author needs to know:
- Script-global variables (counters, config the script computed) **survive**
  a reconnect.
- Anything that lived in the *previous* session's Rust-side state does
  **not** — a `Controller` (and thus any `walk_to` goal) belongs to one
  `Client`'s play loop and is fresh on every new `Client`, exactly as it is
  today for direct Rust callers. A script should treat
  `on("connected", ...)` as the place to re-issue goals it cares about,
  not assume a goal survives a disconnect.
- `bot:state()` reflects `SupervisorHandle::state()`, which is reset to
  `StateSnapshot::default()` the instant a session ends (see Phase 3l/4a) —
  a script reading state between `disconnected` and the next `connected`
  sees defaults, not stale data.

**Hot reload.** Out of scope for Phase 5a itself, but the intended shape:
an explicit reload (a file-watch or a control command, not automatic) that
(a) builds a *new* `Lua` instance, loads the script fresh, and only after
it succeeds (b) atomically swaps it in for future event dispatches — never
mid-handler. The old VM is simply dropped: reload does **not** preserve
Lua-side globals across a reload (only across a reconnect, per above). If
carrying state across a reload is ever wanted, that's an explicit future
serialization step, not a default.

## Security model

Every item below is a concrete, checkable property, not an aspiration:

- **Standard libraries**: an explicit allow-list, not "everything minus a
  few". Allowed: `string`, `table`, `math`. A restricted `os` shim exposing
  only `os.time`/`os.clock` (read-only), not the real `os` library.
  Everything else — `io`, the real `os` (`execute`/`remove`/`rename`/
  `exit`), `package`, `debug`, `require`/`dofile`/`loadfile`,
  `collectgarbage` — is not loaded into the global table at all. `mlua`
  supports constructing a `Lua` with a specific `StdLib` bitflag selection
  (`Lua::unsafe_new_with` / the safe equivalent once available for the
  chosen Lua version) precisely for this.
- **Filesystem**: disabled by construction (no `io` library loaded) — not
  "disabled by convention", there is no code path into the filesystem from
  a script at all.
- **Network access**: disabled by construction — `reqwest`/`tokio::net` are
  never registered into the Lua global table; the only way out of the
  sandbox is the curated `bot` API, which never accepts an arbitrary
  URL/address.
- **Process execution**: unavailable (`os.execute` and friends excluded, as
  above).
- **Module loading**: Phase 5a is single-file only — no `require`, no
  `dofile`. A later multi-file feature (if ever wanted) would be a
  narrow, explicit `bot:require(name)` resolving only inside a designated
  scripts directory, sandboxed the same way — not stock Lua module loading.
- **Memory limit**: `Lua::set_memory_limit(bytes)` (a real `mlua` API) set
  at VM creation, default TBD-but-small (e.g. low single-digit MB) and
  configurable per bot. An allocation past the limit raises a normal Lua
  error, handled like any other handler error (below) — it does not crash
  the bot's connection.
- **Instruction/fuel budget**: `mlua::Lua::set_interrupt` installs a
  callback the VM calls periodically during execution; the callback
  increments a per-invocation counter and returns an error once a
  configured budget is exceeded, aborting a runaway script (e.g. an
  infinite loop) without needing a thread to forcibly kill — there is no
  such thread to kill in this design, so this cooperative mechanism is not
  optional, it's the only mechanism available.
- **Maximum event-queue length**: reused, not reinvented — `BotEvent`
  already flows over a bounded `tokio::sync::broadcast` channel
  (`EVENT_CHANNEL_CAPACITY = 256`) with lag-drop semantics. A slow Lua
  consumer sees `RecvError::Lagged(n)` (surfaced to the script as a distinct
  signal, not silently swallowed) instead of the channel growing without
  bound.
- **Scripts that repeatedly error**: a per-bot consecutive-handler-error
  counter. After a threshold (e.g. 10 in a row), further script execution
  for that bot is disabled (an event/log records this), while the
  underlying Minecraft connection keeps running unaffected — a broken
  script degrades to "no scripted behavior", not "bot disconnects" or
  "runtime wedges".
- **One bot exhausting the whole runtime**: the combination above — per-bot
  memory limit, per-invocation instruction budget, and execution confined
  to that bot's own tokio task — bounds the *worst case* per script call to
  a small, deliberately-tuned amount of CPU and memory. The budget should be
  tuned so one invocation finishes in low-single-digit milliseconds range;
  because each event/tick dispatch is a separate call (not one giant loop),
  the surrounding async loop still yields to the scheduler between
  invocations, so other bots' tasks on the same worker thread aren't
  starved by one script even without true OS-level preemption.

## Proposed API

Names are illustrative, not final — but every one below maps to a Rust
type/field that already exists.

```lua
-- Events: multiple handlers per name allowed, invoked in registration
-- order; one handler erroring does not stop the others (see Error behavior).
bot:on("login", function(e) end)             -- BotEvent::Login { entity_id }
bot:on("spawned", function(e) end)            -- BotEvent::Spawned
bot:on("health", function(e) end)              -- BotEvent::Health { health, food, saturation }
bot:on("death", function(e) end)               -- BotEvent::Death
bot:on("chat", function(e) end)                -- BotEvent::Chat { sender, message }
bot:on("system_chat", function(e) end)         -- BotEvent::SystemChat { message }
bot:on("player_joined", function(e) end)       -- BotEvent::PlayerJoined { uuid, name }
bot:on("player_left", function(e) end)         -- BotEvent::PlayerLeft { uuid }
bot:on("time", function(e) end)                -- BotEvent::Time { time_of_day }
bot:on("weather", function(e) end)             -- BotEvent::Weather { raining }
bot:on("kicked", function(e) end)              -- BotEvent::Kicked { reason }
bot:on("scoreboard", function(e) end)          -- BotEvent::Scoreboard(ScoreboardEvent)
-- Supervisor lifecycle (only fire when run under a ClientSupervisor):
bot:on("connecting", function() end)
bot:on("connected", function() end)
bot:on("disconnected", function(e) end)        -- { reason }
bot:on("reconnect_scheduled", function(e) end) -- { attempt, delay_ms }
bot:on("retries_exhausted", function() end)
-- A script/runtime error in another handler (see Error behavior):
bot:on("script_error", function(e) end)        -- { message } — proposed new BotEvent variant

-- State: a read-only snapshot table, populated from the latest
-- StateSnapshot (SupervisorHandle::state() / Client::bot_state()).
local s = bot:state()
s.tick                                   -- StateSnapshot::tick
s.player.x, s.player.y, s.player.z       -- LocalPlayer::position (PlayerPosition)
s.player.yaw, s.player.pitch
s.player.health, s.player.food, s.player.saturation
s.world_time, s.raining
-- s.entities: array of { id, uuid, kind, x, y, z, yaw, pitch, head_yaw }  (from EntityStore/Entity)
-- s.inventory.player_inventory.slots, s.inventory.open_window, s.inventory.cursor  (from InventoryState/Window)
-- s.players: array of { uuid, name, gamemode, latency, listed }           (from PlayerList/PlayerEntry)
-- s.presentation: bounded structured chat, action bar, titles, tab-list
--                 header/footer, boss bars, and disconnect reason
-- s.scoreboard: bounded objectives, display slots, scores, teams and members

-- Persistent controls (map directly to BotCommand via ControlHandle; all
-- fire-and-forget, never block):
bot:walk_to(x, z)          -- BotCommand::WalkTo
bot:look(yaw, pitch)       -- BotCommand::Look
bot:set_input(forward, strafe, jump, sprint, sneak)  -- BotCommand::SetInput
bot:sprint(on)             -- BotCommand::Sprint
bot:sneak(on)              -- BotCommand::Sneak
bot:jump(on)               -- BotCommand::Jump
bot:stop()                 -- BotCommand::Stop
bot:chat(message)          -- BotCommand::Chat

-- Supervisor control, if running under one:
bot:disconnect()           -- SupervisorHandle::stop()
```

Explicit categories (mission's ask to distinguish these):
- **Instantaneous commands**: `sprint`/`sneak`/`jump`/`look` — apply
  immediately to the next tick's input, no persistence beyond "until
  changed again".
- **Persistent controls**: `walk_to`/`set_input` — persist across many
  ticks until reached/replaced/`stop()`ped, exactly like `Controller`
  already behaves for a direct Rust caller.
- **Asynchronous actions**: none in Phase 5a (see Runtime model above).
- **State snapshots**: `bot:state()` — a point-in-time copy, not a live
  reference; calling it twice in the same handler returns the same values
  both times.
- **Events**: `bot:on(name, fn)` — push-based, called by the runtime, never
  polled by the script.

No raw Rust internals are ever handed to Lua: `bot:state()`'s return value
is a plain table built by converting the current `StateSnapshot` (or the
relevant sub-struct) into Lua values — not a userdata wrapping `&PlayState`
or similar. Commands are plain function calls that construct a
`BotCommand` and push it through the same `ControlHandle` a Rust caller
would use.

## Error behavior

- **Rust errors reaching Lua**: a `bot:*()` call that fails at the Rust
  level (e.g. `ControlHandle::send` returning `Err` because the play loop
  ended) surfaces as a normal Lua error at the call site — `mlua` converts
  a `Result::Err` return from a registered function into a raised Lua
  error automatically. A script can `pcall` it or let it propagate.
- **Lua errors becoming events/logs**: an event handler that errors (a
  runtime error, an explicit `error(...)`, a memory-limit or
  instruction-budget abort) is caught at the dispatch boundary (`mlua`
  function calls return `Result`), logged via `tracing::warn!` naming the
  bot and the handler, and counted toward the consecutive-error threshold.
  The proposed `BotEvent::ScriptError { message }` (not yet added — this is
  a design proposal) surfaces it on the same unified event stream other
  code already observes, rather than only in logs.
- **Do other handlers still run after one fails**: yes. Each registered
  handler for an event (and unrelated events) is invoked independently;
  one failing does not deregister it or block others. Only crossing the
  consecutive-error threshold disables further script execution for that
  bot.
- **After script timeout or memory exhaustion**: treated identically to
  any other handler error (logged, counted) — the Minecraft connection is
  completely unaffected, since the Lua VM is a passenger, not the driver,
  of the connection.
- **How reconnect affects pending Lua actions**: there are no pending
  *asynchronous* actions in Phase 5a to worry about (see Runtime model).
  Persistent controls (`walk_to`) live in the Rust-side `Controller`,
  which is fresh per `Client`; a script relying on a goal surviving a
  reconnect must re-issue it from an `on("connected", ...)` handler.

## Phase 5a implementation scope

A later, bounded session could implement exactly this, no more:

- New `src/lua/` module in the `minerider` crate only (protocol/codegen
  crates stay Lua-free, preserving the existing crate-independence rule).
  `mlua` with `lua54` (or whichever Lua version is decided at
  implementation time) + `vendored` + `send` features.
- A `LuaBot` type: one `mlua::Lua` (sandboxed per the security model
  above), one `ControlHandle` clone, the latest `StateSnapshot` (refreshed
  from `bot_state()`/`SupervisorHandle::state()`), a consecutive-error
  counter, and the interrupt/memory-limit configuration.
- Loads exactly one script file (path from a CLI flag/env var, e.g.
  `MINERIDER_LUA_SCRIPT`), registers the `bot` table, and drains
  `events()` into the registered handlers.
- Wired in as an *optional* layer over `Client` or `ClientSupervisor` — if
  no script path is given, behavior is byte-for-byte what it is today.
- Tests: event dispatch and multi-handler ordering; each sandbox
  restriction actually rejected (`os.execute`, `io.open`, `require` each
  fail cleanly, asserted directly, not just "believed to be off");
  memory-limit and instruction-budget enforcement (a script that tries to
  balloon a table, and a script with `while true do end`, both abort
  instead of hanging/OOMing the process); consecutive-error auto-disable;
  a control command issued from a script reaching the same `Controller`
  state a direct Rust test already asserts against.

**Explicitly out of scope for 5a** (per this session's instructions, and
because the underlying Rust capability doesn't exist yet either):
pathfinding/obstacle avoidance, inventory automation (`window_click`
doesn't exist at the Rust level yet — see README's "Partial or
unverified"), any form of networking from Lua, async/coroutine bot actions,
hot reload, multi-file scripts/`require`, and any thread-pool-sharing
optimization for many bots' Lua VMs (the single-thread-per-bot-task model
above is the Phase 5a baseline; revisit only if profiling shows it matters).
