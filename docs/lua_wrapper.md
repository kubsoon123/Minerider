# Lua scripting wrapper — architecture

Production embedded Lua 5.4 scripting for controlling one Rust-owned
swarm of Minecraft bots. Lives in `src/lua/`, behind the `lua` Cargo
feature, shipped as the `minerider-lua` binary. For the full method-by-
method API, see `docs/lua_api_reference.md`. For the measurements this
design is based on, see `docs/lua_runtime_benchmark.md`. The old
one-VM-per-bot proposal in `docs/lua_design.md` is superseded — see that
document's banner.

## Status

Implemented and tested (342+ tests across `src/lua/*` and
`src/lua_benchmark/full_runtime.rs::production_smoke`, the latter driving
the real production runtime against a local mock Minecraft server and
local fake SOCKS5 relays — see "Testing" below). Not released, not
published, `feat/lua-wrapper` stays a draft PR.

## Trust model

Two distinct parties, two distinct trust levels — every design decision
in this document follows from keeping them separate:

- **The host** is whoever embeds this wrapper: the `minerider-lua` CLI's
  operator, or any other Rust code calling `crate::lua::runtime::run_swarm`
  directly. The host is **fully trusted**. It chooses the worker count,
  sandbox limits (`SandboxConfig`), startup timeout, and — critically —
  the complete set of proxy profiles (`SwarmRuntimeConfig::proxy_profiles`)
  a run will ever have access to, all *before* any script executes. None
  of this is reachable or overridable from Lua.
- **The script** (the `.lua` file the host points the CLI at, or the
  `script_body` string an embedder supplies) is treated as **untrusted —
  potentially buggy, and potentially actively adversarial** — even though
  in the common case it's written by the same person operating the CLI.
  Every bound documented in this file (sandbox stdlib allow-list, memory
  limit, per-invocation instruction budget, consecutive-error disabling,
  bounded queues/shared-state/pub-sub, bounded concurrent action tasks —
  see `crate::lua::worker::MAX_CONCURRENT_ACTION_TASKS`) exists
  specifically to bound what a script can do to the process, to other
  bots, and to other workers, regardless of whether the misbehavior was
  intentional.

Concretely, a script — no matter how it's written — can **never**:

- Read or exfiltrate a proxy password, or connect through a proxy
  endpoint of its own choosing. It may only *reference* a host-registered
  profile id (`add_bot`/`add_group`'s `proxy` field); the profile ↔
  credential mapping is resolved entirely on the host side. See "Proxy
  grouping and credentials" below.
- Read an arbitrary environment variable, the filesystem, or the network
  directly, or load a native/`require`d module. See "Sandbox" below.
- Exceed its memory limit, run past its per-invocation instruction
  budget, or starve other bots'/workers' progress by monopolizing one
  worker's dispatch loop indefinitely — a runaway handler is aborted, not
  the process.
- Force unbounded Rust-side allocation from a single number/string it
  supplies (bot/group counts, name/username/label lengths, shared-state
  keys, pub/sub topics are all explicitly bounded — see
  `crate::lua::registry`'s `MAX_*` constants).
- Move a `Lua` value, function, or VM state across a worker boundary —
  each worker owns one OS thread and one `Lua` VM for its whole lifetime;
  cross-worker communication only ever happens through plain, deep-copied
  Rust data (`crate::lua::shared_value::SharedValue`,
  `crate::lua::dispatcher::ActionResult`), never a shared reference.
- Crash a worker via a raised error propagating out of a handler — every
  top-level handler invocation is caught and converted into a typed,
  logged failure (see "Error model" below), not an unhandled panic.

What a script legitimately *can* do — control every bot's movement/
look/chat/inventory/GUI actions, read all published game state, register
handlers for any event, use the bounded cross-worker shared-state and
pub/sub primitives, and log — is the entire rest of this document and
`docs/lua_api_reference.md`.

## Why a fixed pool of 4 Lua workers

`docs/lua_runtime_benchmark.md` measured four candidate architectures —
one shared VM, small fixed worker pools (2/4/8), and one VM per bot — up
to 800 bots. Findings that drove this design:

- A single shared VM meets every target under normal load, **but** a
  merely-slow (not broken) handler causes real head-of-line blocking:
  `enqueue_to_start` p50 jumps to 46.7 ms for every bot behind it.
- A 4-worker pool brings that same case to 12.7 ms; 8 workers only adds
  another 30% improvement for double the fixed per-worker memory.
- One VM per bot avoids head-of-line blocking entirely but costs ≈70 KB of
  Lua-only memory per bot (≈56 MiB at 800 bots) — real money that a fixed
  pool doesn't spend, for a swarm-orchestration workload that doesn't need
  per-bot Lua isolation in the first place.

**Default: exactly 4 persistent workers.** Configurable (the
`minerider-lua` CLI's `--lua-workers <N>` flag, or
`SwarmRuntimeConfig::worker_count` directly for an embedder calling
`run_swarm` programmatically) for testing or advanced use, but the
default must not scale with bot count, and nothing in this codebase
scales it automatically. There is no Lua-callable `create_swarm`
function — worker count, like every other host-level setting, is fixed
before a script ever runs (see "Trust model" below).

## Two halves: `worker` (sync) and `runtime` (async)

- `crate::lua::worker`: one dedicated `std::thread` per worker, one
  `mlua::Lua` VM for that thread's whole lifetime. Never a VM or OS thread
  per bot or per event. `Lua` never crosses a thread boundary and never
  crosses an `.await` — this is why `mlua`'s `send` feature is **not**
  enabled (`Cargo.toml`'s `mlua` dependency has no `send` feature, matching
  every architecture candidate the benchmark measured).
- `crate::lua::runtime`: the async, tokio-owned orchestrator. Spawns the
  worker threads, waits for the coordinator's configuration phase, resolves
  proxy credentials, spawns every bot's `ClientSupervisor`, bridges each
  bot's `BotEvent` stream into the right worker's queue, and drives
  graceful shutdown.

The two halves meet at exactly one synchronization point:
`crate::lua::worker::StartupBarrier` (see "Two-phase script loading" below).

## Deterministic worker assignment

`worker_index = bot_numeric_id % worker_count` (`crate::lua::registry::SwarmRegistry::worker_index`,
also `crate::lua::dispatcher::DispatcherHandle::worker_index_for`). A bot's
numeric `id` is assigned once, at registration time (explicit via
`swarm:add_bot({id = ...})`, or auto-incremented if omitted), and never
changes — not across reconnect, not across a username change (usernames
and ids are entirely independent), not across proxy reassignment (proxies
are also assigned once, at registration). This assignment rule is a pure
function with no mutable state, so it's trivially stable by construction;
`crate::lua::dispatcher::tests::worker_index_is_deterministic_and_stable`
and `production_smoke::default_four_workers_assign_bots_deterministically_by_id`
verify it directly (the latter through the real runtime).

## Two-phase script loading

The **same script text** is loaded into every worker's Lua VM — there is
one script file, not one per worker. What differs is which parts of it
actually take effect on a given worker:

1. **Configuration phase.** `swarm:configure(function() ... end)` — the
   callback is only *invoked* on the coordinator (worker 0;
   `WorkerState::is_coordinator`). On every other worker, `configure` is a
   no-op: the callback is never called, so `swarm:add_server`/`add_proxy`/
   `add_bot`/`add_group` calls inside it never execute a second (or third,
   or fourth) time. This is what "configuration must occur only once"
   means concretely — it's enforced by *not calling the function* on
   non-coordinator workers, not by locking or deduplicating writes.

   The coordinator accumulates calls into a plain Rust
   `crate::lua::registry::SwarmRegistryBuilder` (no `mlua` involved in
   `registry.rs` at all — see its module doc comment). Validation
   (duplicate names/ids, unknown server/proxy references, empty fields) is
   checked immediately on each `add_*` call, surfaced back to Lua as
   `(nil, error_table)` per `crate::lua::api::errors`.

2. **Handler registration phase.** `swarm:on(name, fn)` / `bot:on(name, fn)`
   run identically on **every** worker, since every worker executes the
   same script top to bottom. This is intentional: **ordinary Lua globals
   are worker-local.** A script-level variable declared outside any
   handler is a separate value in each of the 4 VMs. `swarm.shared` (see
   below) is the only thing genuinely shared across workers.

3. **`swarm:connect_all()`** is the explicit synchronization point. On the
   coordinator, it finalizes the registry (`SwarmRegistryBuilder::build()`),
   sends it to the async orchestrator over a one-shot channel, and then —
   like every other worker — blocks on `StartupBarrier::wait()`. The
   orchestrator (`crate::lua::runtime::run_swarm`) receives the registry,
   resolves every referenced proxy's credentials, spawns every bot's
   `ClientSupervisor`, builds the shared `bot_handles` map, and publishes
   both (wrapped in `Arc`) to the barrier. Every worker's `connect_all()`
   call then unblocks with the same data.

4. **`swarm:run()`** is a no-op marker. The script's top-level chunk
   finishes executing shortly after; from then on, each worker's own
   persistent dispatch loop (`crate::lua::worker::run_worker`'s `loop`)
   drives everything — invoking registered handlers as events for that
   worker's bots arrive, sweeping expired action callbacks, firing due
   timers — until shutdown.

Dynamic bot creation after startup (adding a bot mid-run, not at
`configure` time) is not implemented — `swarm:add_bot`/`add_group` are only
meaningful inside the `configure` callback, before `connect_all()`. This is
a deliberate scope decision (the mission's own "initial config must work
first" ordering), not an oversight; it is a natural extension point if ever
needed (the registry-building machinery doesn't inherently require the
startup-barrier framing).

## Cross-worker shared state

`swarm.shared:get(key)` / `:set(key, value)` / `:update(key, fn)` —
Rust-owned (`crate::lua::api::shared::SharedState`, one instance shared via
`Arc` across all 4 workers), bounded, deep-copied.

Only [`crate::lua::shared_value::SharedValue`](../src/lua/shared_value.rs)
-representable data crosses the boundary: nil, bool, finite number, a
bounded string (≤4096 bytes), a bounded array (≤256 elements), or a
bounded string-keyed table (≤256 keys) — nested up to 8 levels deep, capped
at ≈64 KiB total approximate size. No functions, userdata, threads, or
non-finite numbers. A self-referential Lua table is rejected by the depth
bound, not separate cycle detection (bounded recursion can't loop forever).
Every `get`/`set`/`update` call **deep-copies** through `SharedValue` — a
value written by one worker and read by another is never the same Lua
table instance; mutating what you read back never affects the store.

`update(key, fn)`'s implementation is the one place this subsystem
deliberately does something non-obvious: it reads a clone of the current
value under the store's mutex, **releases the mutex**, then calls `fn`
(arbitrary Lua) with that clone, then re-acquires the mutex only to
compare-and-swap against the version it originally read — retrying (up to
`MAX_UPDATE_RETRIES = 8` times) if another worker wrote in between. **The
shared-state mutex is never held while Lua executes.** This matters because
Lua execution can be arbitrarily slow (subject only to the instruction
budget) — holding a process-wide mutex across that would let one worker's
slow updater stall every other worker's unrelated `shared:get`/`:set` calls.

Bounded cross-worker pub/sub complements this: `swarm:publish(topic, payload)`
converts `payload` through the same `SharedValue` bounds and clones it once
per worker into that worker's own queue as a `WorkItem::Message` (never a
single shared "latest message" slot — every worker gets an independent
copy, so concurrent publishes to the same topic can never race one worker
against another). `swarm:on_message(topic, fn)` registers a handler; there
is no cycle-prevention beyond the normal bounded-queue drop-under-load
behavior other traffic gets (a script that publishes from inside its own
`on_message` handler in an unbounded loop will eventually see drops on its
own low-priority lane, not an unbounded memory/CPU spiral).

## Event model

Every `BotEvent` variant (`src/minecraft/event.rs`) is converted to a
named Lua event exhaustively — `crate::lua::event::bot_event_name` and
`crate::lua::convert::events::event_to_table` are both written as `match`
statements **without a wildcard arm**, so adding a new `BotEvent` variant
(or a new variant to any event type it boxes: `InventoryEvent`,
`PresentationEvent`, `ScoreboardEvent`, `HudEvent`) is a compile error here
until it's explicitly classified and converted. See
`docs/lua_api_reference.md#events` for the full name table.

**Priority / coalescing.** Reuses the benchmark's recommended design
(`crate::lua::queue::PriorityQueue`, promoted out of `src/lua_benchmark/`
rather than duplicated): a high-priority lane that's never silently
dropped (lifecycle events, action results, GUI open/close, inventory
transaction outcomes, script errors), a `(bot, event kind)`-coalesced lane
for repeated state (health/time/weather/HUD snapshots — only the latest
value is ever delivered), and a low-priority best-effort lane for
everything else (chat, player join/leave, entity spawn/remove,
presentation, scoreboard). See `crate::lua::event::bot_event_priority`'s
doc comment for the exact classification and rationale.

**Queue capacity.** `DEFAULT_HIGH_QUEUE_CAPACITY = 4096`,
`DEFAULT_LOW_QUEUE_CAPACITY = 1024`, per worker
(`crate::lua::runtime::SwarmRuntimeConfig`). The benchmark report didn't
specify one mandatory value; 4096 is the "safe default" it suggested,
chosen from its measured peaks rather than picked arbitrarily — see
`docs/lua_runtime_benchmark.md#queues`. This is a genuine finite-capacity
guarantee, not unconditional delivery: if the high-priority lane still
somehow fills, the *new* item is rejected outright (`PushOutcome::CriticalOverflow`)
— already-queued critical items are never evicted to make room — and this
is never silent:
- A dedicated `critical_overflow` counter (surfaced as
  `swarm:stats().queue_critical_overflow_total`, separate from the
  combined `queue_dropped_total`) is incremented.
- A `worker_overload` event fires (a real `swarm:on("worker_overload", fn)`-dispatchable
  event, delivered through the low-priority lane specifically so
  reporting the high lane's saturation can never itself be lost to that
  same saturation).
- If the lost item was an `action_result`/callback completion, its
  pending one-shot callback (if any) is resolved with a typed
  `worker_overloaded` error instead of being left to silently expire via
  the callback-timeout sweep. The same mechanism resolves a callback with
  a typed `shutdown` error if the push was instead rejected because the
  queue had already been closed (`PushOutcome::Closed` — pushing into a
  closed queue is always rejected, never silently accepted).

**Handler dispatch order.** `bot:on` handlers run before `swarm:on`
handlers for the same event; multiple handlers registered for the same
name run in registration order; one handler raising an error does not
prevent the others in the list from still running for that same dispatch
(`crate::lua::worker::run_handlers_for`).

## Action execution model

Every action a script can take (movement, look, chat/command, hand
actions, GUI/inventory clicks) follows one uniform, non-blocking shape:

1. Validate arguments (Lua-level type/shape checks; the underlying Rust
   core additionally validates semantically — e.g. a command starting with
   `/` — and that validation is preserved, not duplicated).
2. Allocate a request id (`DispatcherHandle::allocate_request_id`, a
   process-wide monotonic counter).
3. Spawn the actual async `SupervisorHandle` call onto the shared tokio
   runtime (`crate::lua::api::bot::spawn_action`) — the Lua worker thread
   is never blocked, not even for a microsecond-scale channel round trip.
4. Return the request id immediately to Lua (`local request_id = bot:forward(true)`)
   — or, if the bot has no active connection handle yet, an immediate
   `not_connected` error is still delivered asynchronously via the same
   path (never a synchronous Lua error for this expected case).

Completion arrives later via the worker's `action_result` event
(`bot:on("action_result", fn)` / `swarm:on("action_result", fn)`), and —
if one was passed as the action call's last argument — a one-shot
callback (`bot:click_gui(13, "right", function(result) ... end)`).
Callbacks are stored per-worker, keyed by request id
(`crate::lua::worker::CallbackRegistry`), bounded at
`MAX_PENDING_CALLBACKS = 4096` (oldest evicted on overflow), with a
configurable timeout (`DEFAULT_CALLBACK_TIMEOUT = 30s`) swept once per
dispatch-loop iteration (`worker::sweep_callback_timeouts`).

Outcomes are distinguished, never conflated: `delivered_sent` (the packet
was handed to the connection layer — **never** a claim the server
acknowledged it — this is the terminal outcome for movement/look/chat/
hand actions, which the Rust core doesn't itself track further),
`confirmed`/`corrected` (server-authoritative outcomes for GUI/inventory
clicks, carrying the new `state_id`), or an error result carrying one of
the stable codes in `docs/lua_api_reference.md#errors` (`queue_full`,
`not_connected`, `session_replaced`, `disconnected`, `supervisor_stopped`,
`invalid_action`, `invalid_gui_slot`, `no_gui_open`, `stale_generation`,
`inventory_timeout`, `inventory_rejected`). `use_item` is never reported as
"confirmed" — the Rust core has no server-acknowledgment signal for it, so
the wrapper doesn't fabricate one.

## Reconnect semantics

The full `ReconnectPolicy` from `crate::core::supervisor` is exposed via
each bot's/group's `reconnect = {...}` table
(`crate::lua::api::config::parse_reconnect_policy`), defaulting to
`ReconnectPolicy::default()` (disabled) unless the script overrides
`enabled = true` — matching the Rust core's own default exactly, not a
Lua-side reinterpretation. Statuses (`disconnected`/`connecting`/
`connected`/`reconnect_scheduled:<attempt>`/`stopped`) and `generation`
(`bot:status()`, `bot:generation()`) are exposed as plain synchronous reads
of the supervisor's own `watch` channels — no polling delay, no extra
round trip.

What survives a reconnect and what doesn't (all inherited directly from
`ClientSupervisor`'s existing, tested contract — this wrapper adds no new
reconnect logic of its own):

- **Survives:** the Lua VM and its globals (a worker's script-level state
  is untouched by any bot's reconnect), worker assignment (pure function of
  the immutable bot id), proxy assignment (`ClientSupervisor::new`'s `cfg`,
  including its `proxy: Option<Arc<Socks5ProxyConfig>>`, is reused
  unchanged for every reconnect attempt — see
  `production_smoke::reconnect_preserves_proxy_assignment_in_the_production_runtime`
  and `full_runtime::tests::proxy_assignment_persists_across_a_reconnect`
  for both a production-runtime and a lower-level proof of this).
- **Resets:** the Rust-side `StateSnapshot` (back to
  `StateSnapshot::default()` the instant a session ends), and any
  persistent movement/controller goal (a `walk_to` target lives in that
  session's `Controller`, fresh per `Client`) — a script that wants a goal
  to survive reconnect must re-issue it from a `connected` handler, exactly
  as a direct Rust caller already must.
- **Old-generation actions never execute in the new session:** the
  underlying `SupervisorHandle` command path is generation-checked
  (`ControlError::SessionReplaced`); a pending action callback from the
  old session completes with that typed error rather than either silently
  vanishing or firing against the new session. Inventory transactions
  additionally bind to an explicit expected generation
  (`InventoryActionError::StaleGeneration`) and never replay across a
  reconnect.

## Proxy grouping and credentials

**Proxy endpoints and credentials are never Lua-constructible.** There is
no `swarm:add_proxy` that takes a host/port — calling it at all returns a
typed `invalid_configuration` error. Instead, the **host** (the CLI, or
any other embedder calling `crate::lua::runtime::run_swarm`) supplies a
fixed `crate::lua::registry::ProxyProfiles` map — profile id →
already-resolved `Arc<Socks5ProxyConfig>` — via
`SwarmRuntimeConfig::proxy_profiles`, *before* the script ever runs. A
script may only reference a profile by its id
(`swarm:add_bot({proxy = "profile_id"})`); the id is validated against
that host-supplied set at `add_bot`/`add_group` time
(`crate::lua::registry::SwarmRegistryBuilder`, which is constructed with
the id set and has no `add_proxy` method at all), and an unregistered id
returns `unknown_proxy` — an error that deliberately carries only the id
the script asked for, never any host/port/credential detail
(`crate::lua::registry::UnknownProxyProfile`).

This closes a real exfiltration path an earlier version of this wrapper
had: when scripts could supply `username_env`/`password_env` themselves,
a sandboxed script could name *any* environment variable already present
in the process (not necessarily a proxy credential at all) and *any*
destination host, and Rust would faithfully read that variable and send
it to that script-chosen endpoint as SOCKS5 auth — without ever needing
the sandboxed `os.getenv` (which was never exposed, but was never the
actual gap; see
`production_smoke::a_script_cannot_choose_an_arbitrary_proxy_endpoint_or_env_var`
for the regression test). Proxy references are now a pure lookup key into
host-owned data, nothing more.

The CLI's own way of building a `ProxyProfiles` map is
`crate::lua::runtime::proxy_profiles_from_env`, driven by repeatable
`--proxy-profile <id>=<ENV_PREFIX>` flags: argv carries only the profile
id and an environment-variable-name *prefix* (operator-chosen, not a
secret), and the actual host/port/username/password are read from
`{PREFIX}_HOST`/`_PORT`/`_USERNAME`/`_PASSWORD` (reusing the existing
`Socks5ProxyConfig::from_env`) — **never from argv**. No-auth proxies are
preserved (omit `_USERNAME`/`_PASSWORD` entirely). `Socks5ProxyConfig`/
`Socks5Credentials`'s existing `Debug` impls redact usernames and
passwords unconditionally, so even an incidental `{:?}` of a resolved
profile can never leak one.

No proxy rotation, fallback, or auto-switching is implemented — a bot's
proxy assignment is fixed at registration and reused unchanged for its
whole lifetime, including every reconnect.

## Chunk sharing

Unaffected by any of the Lua layer's own design — `ServerDef.shared_chunks`
maps directly to the existing `ClientConfig::with_chunk_sharing`, and
sharing scope (`ServerIdentity`) is derived only from a server's
`host`/`port`, never from proxy configuration (this was already true and
already tested in `src/lua_benchmark/chunk_proof.rs`; nothing in the
wrapper's proxy-grouping code touches or could touch it). Two bots on
different proxies but the same server still share chunk payloads exactly
as two bots with no proxy at all would.

## GUI / inventory conventions

Every slot table (`crate::lua::convert::items`) carries **both**
`slot.raw_slot` (the true, zero-based protocol slot index — never remapped)
and `slot.lua_index` (`raw_slot + 1`, for natural 1-based Lua iteration).
`bot:click_gui`/`bot:click_inventory` take the **raw** (zero-based) slot
index as their first argument, matching `raw_slot`, not `lua_index` — this
is deliberate (it's the value the server's own protocol addresses, and the
value `bot:open_gui()`'s slots already expose as `raw_slot`), and is
called out explicitly here because it's the one place a 1-based-language
convention and a 0-based-protocol convention sit side by side. All 15
`GuiClick` modes are supported via `"mode"` or `"mode:param"` strings — see
`docs/lua_api_reference.md#gui-click-modes` for the complete list.
`click_gui` always targets the currently open non-player GUI
(`GuiActionError::NoGuiOpen` if none is open — never silently redirected to
the player inventory); `click_inventory` always targets window 0 (the
player's own inventory), regardless of whether a GUI happens to be open.

## Sandbox

Curated allow-list, unchanged in spirit from the benchmark's validated
design (`crate::lua::sandbox`, promoted rather than duplicated): `string`,
`table`, `math`, plus a restricted `os` shim exposing only `time`/`clock`.
Everything else — `io`, the real `os`, `package`, `debug`, `require`/
`dofile`/`loadfile`, native module loading, raw Rust pointers — is never
loaded into the global table at all, not merely hidden.

- **Memory limit:** 16 MiB per worker by default
  (`SandboxConfig::default().memory_limit_bytes`) — no single mandatory
  value was specified beyond "choose a conservative value"; this is that
  value, configurable per deployment.
- **Instruction budget:** ≈2 million real Lua instructions per handler
  invocation (`instruction_budget: 2_000` hook firings ×
  `hook_every_n_instructions: 1000`), reused unchanged from the
  benchmark's corrected default — the *first* value tried there
  (2,000,000 firings, i.e. 2 *billion* real instructions) took **12.8
  seconds** wall-clock to abort a tight infinite loop in a debug build;
  this corrected value aborts the same loop in the low-single-digit-
  millisecond range. See `crate::lua::sandbox::SandboxConfig`'s doc
  comment and `docs/lua_runtime_benchmark.md`'s sandbox-overhead
  measurements.
- **Consecutive-error disabling:** `consecutive_error_threshold: 10` by
  default. A bot whose handlers error 10 times in a row (instruction/
  memory aborts count as errors) has further script execution disabled
  for that bot specifically (`WorkerState::disabled_bots`) — the
  underlying Minecraft connection, and every other bot on every worker,
  keeps running unaffected. A single success resets the counter to zero
  (`crate::lua::worker::tests::a_successful_handler_resets_the_consecutive_error_counter`).

## Timers

`swarm:set_timeout`/`set_interval`/`clear_timer` (global — see below),
`bot:set_timeout`/`set_interval`/`clear_timer` (bot-scoped, run on that
bot's own worker). One scheduler per worker (`crate::lua::api::timers`,
`WorkerState::timers`) — never a task or thread per timer. Bounded at
`MAX_TIMERS_PER_WORKER = 1024`, minimum interval `MIN_TIMER_INTERVAL = 10ms`.
Checked at least once per `WorkerQueue::wait_for_batch` wake-up, which
polls every 25 ms even when the queue is otherwise idle specifically so
timers are checked promptly without needing a dedicated thread — this is
also the origin of this wrapper's only drift guarantee: a timer's actual
firing time is "as soon as reasonably possible after it's due," bounded by
that ~25 ms polling granularity plus however long the current batch takes
to process, never a hard real-time guarantee.

**`swarm:set_timeout`/`set_interval` only actually arm on the coordinator
(worker 0).** Since every worker runs the same script, a naive
implementation would fire a "global" timer once per worker instead of
exactly once; `LuaSwarm`'s timer methods check `is_coordinator` before
calling `timers::schedule` (a non-coordinator call still returns a
plausible id so scripts don't need worker-aware branching, it simply never
fires). Verified directly by
`production_smoke::global_swarm_timer_fires_exactly_once_across_all_workers`.
Bot-scoped timers have no such restriction — a bot's timers run on
whichever single worker owns that bot, which is exactly where its `on`
handlers run too.

## Logging

`minerider.log(level, message)` / `.info`/`.warn`/`.error(message)` — routed
through `tracing`, prefixed with the worker id, length-capped at 2048
bytes (`crate::lua::api::log_line`). Never a full state-snapshot dump by
default. Proxy passwords can never appear in a log line because they are
never representable in Lua in the first place (see "Proxy grouping" above)
— there is no redaction step because there is nothing to redact.

## Error model

Every wrapped operation that can fail returns a stable
`{code, message, retryable, bot_id, generation}` table
(`crate::lua::error::ScriptError`, `crate::lua::convert::events::script_error_to_table`).
Every existing typed Rust error (`ControlError`, `GuiActionError`,
`InventoryActionError`, `InventoryError`, `crate::lua::registry::RegistryError`)
has a `From` conversion into this one type, so the mapping lives in one
place (`src/lua/error.rs`) rather than scattered across call sites. Scripts
should key logic off `code` (a fixed, documented, small string set — see
`docs/lua_api_reference.md#errors`), never off `message` (human-readable,
not a stable contract).

## Graceful shutdown

`swarm:stop()` (from Lua) or `RunningSwarm::shutdown(timeout)` (from the
CLI's Ctrl+C handler) both: stop every bot's supervisor
(`SupervisorHandle::stop()`), close every worker's queue (unblocking its
dispatch loop's next `wait_for_batch`), wait — bounded by `timeout` — for
every event-bridge and supervisor task to finish, then join every worker
thread. `minerider-lua`'s Ctrl+C handling: the first Ctrl+C starts this
graceful sequence; a second Ctrl+C forces immediate process exit rather
than waiting on a shutdown that might be stuck — the OS reclaims every
thread on process exit regardless, so this is safe. See
`production_smoke::graceful_shutdown_completes_within_the_bound_and_stops_every_bot`
for an end-to-end proof this actually completes (not just compiles).

## Performance expectations

No per-bot Lua VM or OS thread; no task or event-queue-entry explosion
under normal load (bounded queues, bounded callback/timer counts); no full
world/chunk payload ever copied into Lua (`bot:state()` and friends return
bounded, already-tracked state — never chunk data); command routing never
blocks core packet processing (every action is a `tokio::spawn`, never an
inline await on the Lua thread); Lua worker count has no effect on
process-wide chunk sharing (`SharedChunkStore` is keyed by `ServerIdentity`
alone). See `docs/lua_runtime_benchmark.md` for the architecture-level
numbers this design is based on; a dedicated production-wrapper overhead
benchmark comparing this real implementation against the prototype is
tracked separately (see the final report for what was and wasn't run in
this session, and why).

## Testing

- `src/lua/*`'s own unit tests (sandbox, queue, registry, error mapping,
  event classification, shared-state bounds/CAS, timer scheduling, CLI
  argument parsing) — fast, no network, run under plain `--features lua`.
- `src/lua_benchmark/full_runtime.rs::production_smoke` — integration
  tests driving the *real* `crate::lua::runtime::run_swarm` against a
  local mock Minecraft server and local fake SOCKS5 relays (reusing the
  benchmark's existing fixtures rather than a second test harness): event
  → handler → action → callback round trips, every targeted state view,
  GUI clicks, deterministic worker assignment, reconnect-preserves-proxy,
  bot- and swarm-scoped timers, one-bad-handler isolation, graceful
  shutdown, and — loaded via `include_str!` so it can't silently drift
  from the shipped file — `examples/lua/swarm.lua` itself, running both
  full proxy groups end to end. Requires `--features lua-benchmark`.

## Limitations

Carried over honestly from what the underlying Rust core does and doesn't
support, not invented by this wrapper:

- No pathfinding, obstacle avoidance, auto-jump, or parkour — `walk_to` is
  the same straight-line `Controller` goal a direct Rust caller already
  gets.
- `use_item` success means "packet sent," never server confirmation — the
  core has no acknowledgment signal for it.
- `item.registry_name` is always `nil` — no item-id-to-name registry is
  vendored in this repository (only block data is); see
  `crate::minecraft::gui::GuiSlotView::registry_name`'s doc comment.
- Text components (chat, titles, boss bars, scoreboard display names, tab
  list header/footer, ...) are exposed as their best-effort plain-text
  rendering only, not the full color/style/click-event tree.
- World block queries are not exposed — `crate::minecraft::world::World::block_state`
  exists in the core, but no Lua binding was added for it in this pass (a
  narrow, safe addition, not scope creep into a world-query API, would be
  a natural small follow-up if a script genuinely needs it).
- Dynamic bot creation after `swarm:connect_all()` is not implemented (see
  "Two-phase script loading" above).
- No Node.js/JavaScript/TypeScript/N-API/HTTP/WebSocket binding of any
  kind — Lua only, embedded directly in this process.
