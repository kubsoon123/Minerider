# Lua scripting API reference

Complete method-by-method reference for the `lua` feature's scripting API
(`src/lua/api/`). For architecture/rationale, see `docs/lua_wrapper.md`.
Every method listed here is implemented and tested — this is not an
aspirational API proposal.

Two globals are injected into every worker's Lua VM: `minerider` (runtime
utilities) and `swarm` (this worker's swarm handle). No `require`, no
module loading — see the sandbox section of `docs/lua_wrapper.md`.

## `minerider`

| Call | Description |
|---|---|
| `minerider.log(level, message)` | `level` is `"info"`/`"warn"`/`"error"` (anything else logs at info). Length-capped at 2048 bytes, prefixed with the worker id. |
| `minerider.info(message)` / `.warn(message)` / `.error(message)` | Shorthand for the above. |

There is no `minerider.create_swarm(...)` callable from *inside* a script
— the swarm is already created and running by the time your script loads;
`minerider-lua <script.lua>` (the CLI) is what constructs it, from
`crate::lua::runtime::run_swarm`.

## `swarm`

### Configuration (coordinator-only; see `docs/lua_wrapper.md#two-phase-script-loading`)

```lua
swarm:configure(function()
  swarm:add_server({...})
  swarm:add_bot({..., proxy = "profile_id"})   -- profile_id is host-registered, not defined here
  swarm:add_group({...})
end)
```

**`swarm:add_server(table) -> ok, err`**

| Field | Type | Default |
|---|---|---|
| `name` | string (required, unique) | — |
| `host` | string (required) | — |
| `port` | number | `25565` |
| `view_distance` | number, `2..=32` | `12` |
| `write_timeout_ms` | number | `10000` |
| `connect_deadline_ms` | number | `15000` |
| `shared_chunks` | boolean | `true` |

Returns `(true, nil)` on success or `(nil, error_table)` — see
[Errors](#errors). `shared_chunks` maps directly to
`ClientConfig::with_chunk_sharing`; sharing scope is always derived from
`host`/`port` only, regardless of any bot's proxy.

**There is no `swarm:add_proxy`.** Proxy endpoints and credentials are
never Lua-constructible — calling `swarm:add_proxy(table)` (kept callable
only so old scripts get a clear error instead of "attempt to call a nil
value") always returns `(nil, {code = "invalid_configuration", ...})`.
Proxies are **host-trusted profiles**: the host (the CLI, via repeatable
`--proxy-profile <id>=<ENV_PREFIX>` flags, or any embedder calling
`crate::lua::runtime::run_swarm` directly) registers a fixed
`profile_id -> Socks5ProxyConfig` map *before* the script runs
(`SwarmRuntimeConfig::proxy_profiles`). A script may only *reference* a
profile by id via `add_bot`/`add_group`'s `proxy` field below — it can
never choose an arbitrary host/port, and it can never choose which
environment variable a credential is read from. Referencing an id the
host never registered returns `unknown_proxy`, carrying only the id the
script asked for, never any host/port/credential detail. See
`docs/lua_wrapper.md#proxy-grouping-and-credentials` for the full
security writeup.

**`swarm:add_bot(table) -> bot_id, err`**

| Field | Type | Default |
|---|---|---|
| `id` | number or nil | auto-assigned (next free integer) |
| `username` | string (required, unique) | — |
| `server` | string (required, must reference a registered server) | — |
| `proxy` | string or nil (must reference a host-registered proxy profile id — see above) | nil (direct connection) |
| `reconnect` | table or nil | see [Reconnect policy](#reconnect-policy-table) |

Returns the assigned numeric `bot_id` on success, or `(nil, error_table)`.
`bot_id` is what `worker_index = bot_id % worker_count` uses — it's
independent of `username` and never changes.

**`swarm:add_group(table) -> ok, err`**

| Field | Type | Default |
|---|---|---|
| `name` | string (required, unique) | — |
| `count` | number (required) | — |
| `id_prefix` | string or nil | nil — cosmetic label only, e.g. for logs; never used for worker assignment |
| `username_prefix` | string | `""` |
| `server` | string (required) | — |
| `proxy` | string or nil | nil |
| `reconnect` | table or nil | see below |

Creates `count` bots with usernames `username_prefix .. i` for
`i = 0, 1, ..., count - 1`, each with auto-assigned ids, all sharing the
given `server`/`proxy`/`reconnect`. Equivalent to `count` individual
`add_bot` calls followed by grouping their resulting ids under `name`.

#### Reconnect policy table

Every field is optional; omitted fields keep
`ReconnectPolicy::default()`'s value (which means `enabled = false` unless
you set it — this matches the Rust core's own default exactly).

| Field | Type | Meaning |
|---|---|---|
| `enabled` | boolean | Master on/off switch. |
| `max_retries` | number or `"unlimited"` | Count, or unbounded. |
| `initial_delay_ms` | number | First retry delay. |
| `max_delay_ms` | number | Backoff cap. |
| `multiplier` | number | Backoff multiplier per attempt. |
| `jitter` | `{type = "none"\|"deterministic", fraction = number}` | Jitter applied to each delay. |
| `stable_session_reset_ms` | number | How long a session must stay up before the attempt counter resets. |
| `on_transient` | `"retry"` or `"stop"` | Decision for transient errors (I/O, timeout). |
| `on_server_rejected` | `"retry"` or `"stop"` | Decision when the server explicitly disconnects. |
| `on_auth_failure` | `"retry"` or `"stop"` | Decision for auth failures. |
| `on_protocol_incompatible` | `"retry"` or `"stop"` | Decision for protocol/wire errors. |

### Lookups

| Call | Returns |
|---|---|
| `swarm:bot(id) -> bot \| nil` | The bot userdata for numeric `id`, or `nil` if unknown. |
| `swarm:bots() -> {bot, ...}` | Every registered bot (array, 1-based). |
| `swarm:group(name) -> group \| nil` | The named group, or `nil`. |
| `swarm:groups() -> {group, ...}` | Every registered group. |

Any worker can look up any bot/group — the registry is shared. Calling an
action on a bot owned by a *different* worker still works (it's routed
through the shared `SupervisorHandle` map), but registering `bot:on(...)`
for a bot not assigned to the calling worker is a no-op that will simply
never fire (that bot's events never reach this worker's queue).

### Lifecycle

| Call | Description |
|---|---|
| `swarm:connect_all()` | The startup barrier. Coordinator: finalizes config and hands it to the runtime. Every worker: blocks until every bot's supervisor is spawned. |
| `swarm:disconnect_all()` | Stops every bot's supervisor (`SupervisorHandle::stop()` for each). |
| `swarm:stop()` | Same as `disconnect_all()`, plus signals every worker to shut down its dispatch loop. |
| `swarm:run()` | No-op marker — the persistent per-worker dispatch loop takes over once the script's top-level chunk returns. |
| `swarm:status() -> table` | `{worker_count, worker_index, is_coordinator, started, bot_count}`. |
| `swarm:stats() -> table` | `{queue_depth_total, queue_peak_depth_total, queue_dropped_total, queue_critical_overflow_total}` across all workers. `queue_dropped_total` is the combined high+low-lane drop count; `queue_critical_overflow_total` is high-lane (critical) drops alone — see [Queue capacity](../docs/lua_wrapper.md#queue-capacity). |

### Events

| Call | Description |
|---|---|
| `swarm:on(name, fn) -> handler_id` | Registers a handler for every bot's `name` events on this worker. |
| `swarm:once(name, fn) -> handler_id` | Same, removed after firing once. |
| `swarm:off(handler_id) -> removed` | Unregisters by id (works for `on`/`once`/`on_message` ids). |

See [Events](#events) for the complete name list and payload shapes.

### Shared state and pub/sub

| Call | Description |
|---|---|
| `swarm.shared:get(key) -> value` | Reads the current value (`nil` if unset). |
| `swarm.shared:set(key, value) -> ok, err` | Writes unconditionally. |
| `swarm.shared:update(key, fn) -> new_value, err` | Atomic read-modify-write; `fn(current_or_nil) -> new_value`. See `docs/lua_wrapper.md#cross-worker-shared-state`. |
| `swarm:publish(topic, payload) -> ok, err` | Delivers `payload` to every worker's `on_message` handlers for `topic`. |
| `swarm:on_message(topic, fn) -> handler_id` | `fn(topic, payload)`. |

Only nil/bool/finite-number/bounded-string/bounded-array/bounded
string-keyed-table values are representable — see
`docs/lua_wrapper.md#cross-worker-shared-state` for exact bounds.

### Timers (global — see `docs/lua_wrapper.md#timers`)

| Call | Description |
|---|---|
| `swarm:set_timeout(delay_ms, fn) -> timer_id` | Fires once. Only actually arms on the coordinator. |
| `swarm:set_interval(interval_ms, fn) -> timer_id` | Fires repeatedly. Coordinator-only, same as above. |
| `swarm:clear_timer(timer_id) -> cancelled` | Cancels; only meaningful on the coordinator. |

## `bot`

Every event handler receives a `bot` as its first argument; `swarm:bot(id)`/
`swarm:bots()` return the same userdata type.

### Identity / metadata (synchronous)

| Call | Returns |
|---|---|
| `bot:id() -> number` | Stable numeric id. |
| `bot:worker_id() -> number` | Which worker (`0..worker_count-1`) owns this bot. |
| `bot:username() -> string \| nil` | From the registry. |
| `bot:server() -> string \| nil` | Registered server name. |
| `bot:proxy() -> string \| nil` | Registered proxy name, or `nil` if direct. |
| `bot:groups() -> {string, ...}` | Names of every group this bot belongs to. |
| `bot:status() -> string \| nil` | `"disconnected"`/`"connecting"`/`"connected"`/`"reconnect_scheduled:<attempt>"`/`"stopped"`. |
| `bot:generation() -> number \| nil` | Current session generation (increments by 1 on every successful connect). |

### Lifecycle

| Call | Returns |
|---|---|
| `bot:disconnect() -> ok, err` | Stops this bot's supervisor. |
| `bot:stop() -> ok, err` | Same as `disconnect()`. |
| `bot:connect() -> nil, err` | Always returns `invalid_action` — bots connect automatically at swarm startup; there is no manual (re)connect for an already-stopped bot in this version. |

### Movement (async action; returns `request_id`)

| Call |
|---|
| `bot:forward(on) -> request_id` |
| `bot:backward(on) -> request_id` |
| `bot:strafe_left(on) -> request_id` |
| `bot:strafe_right(on) -> request_id` |
| `bot:jump(on) -> request_id` |
| `bot:sneak(on) -> request_id` |
| `bot:sprint(on) -> request_id` |
| `bot:set_input({forward, strafe, jump, sprint, sneak}) -> request_id` |
| `bot:stop_movement() -> request_id` |
| `bot:walk_to(x, z) -> request_id` |

No pathfinding/obstacle avoidance — `walk_to` is a straight-line
`Controller` goal, same as a direct Rust caller gets.

### Looking (async action)

| Call |
|---|
| `bot:look(yaw, pitch) -> request_id` |
| `bot:set_random_look(nil) -> request_id` — disables random look |
| `bot:set_random_look({min_interval_ms, max_interval_ms, max_yaw_delta, min_pitch, max_pitch, seed}) -> request_id` |

### Chat / command (async action, kept separate — no `/`-inference)

| Call |
|---|
| `bot:chat(message) -> request_id` |
| `bot:command(command) -> request_id` — `command` must not include a leading `/` |

### Hand actions (async action)

| Call |
|---|
| `bot:use_item(hand) -> request_id` — `hand` is `"main"` or `"off"`; success means the packet was sent, never server confirmation |
| `bot:swing(hand) -> request_id` |
| `bot:select_hotbar_slot(slot) -> request_id` — `slot` is `0..=8`; sends `held_item_slot` and updates the tracked selection so a following `use_item`/`swing` acts on the newly held item. A slot outside `0..=8` raises immediately. |

### State (synchronous, read-only, detached — mutating the returned table never affects Rust state)

| Call | Returns |
|---|---|
| `bot:state() -> table \| nil` | Full `StateSnapshot`: `tick`, `player`, `entities`, `inventory`, `players`, `presentation`, `scoreboard`, `hud`, `world_time`, `raining`, `dimension`. `nil` if the bot has no connection handle yet. |
| `bot:player() -> table \| nil` | Just the `player` section (position, health, food, saturation, xp, velocity, input, on_ground, ...). |
| `bot:entities() -> table \| nil` | `{[entity_id] = {id, uuid, kind, x, y, z, yaw, pitch, head_yaw, vx, vy, vz, on_ground}}` — a bounded snapshot, never a per-tick/per-movement event stream. |
| `bot:players() -> table \| nil` | `{[uuid_hex] = {uuid, name, gamemode, latency, listed, display_name, list_priority, show_hat, chat_session}}` — no secret auth data. |
| `bot:inventory() -> table \| nil` | `{player_inventory, open_window, cursor, selected_hotbar_slot, pending_transaction_ids}`. |
| `bot:hud() -> table \| nil` | `{entity_id, vitals, experience, game_mode, previous_game_mode, hardcore, abilities, selected_hotbar_slot, held_item, hotbar, cooldowns, active_effects, attributes, death, respawn, world_border, time, weather, difficulty, spawn_position}`. |
| `bot:presentation() -> table \| nil` | `{chat, action_bar, titles, tab_list, boss_bars, disconnect_reason}`. |
| `bot:scoreboard() -> table \| nil` | `{objectives, display_slots, scores, teams}`. |

`uuid` fields are 32-character lowercase hex strings (not Lua numbers —
Minecraft UUIDs don't fit in a Lua number without precision loss).

### GUI inspection and clicking

| Call | Returns |
|---|---|
| `bot:open_gui() -> table \| nil` | `{window_id, menu_type, title, state_id, slots, cursor, properties, slots_truncated}`, or `nil` if no non-player GUI is open. |
| `bot:click_gui(raw_slot, mode, callback?) -> request_id` | Clicks a slot in the currently open non-player GUI. `NoGuiOpen` error if none is open — never silently redirected to the player inventory. |
| `bot:click_inventory(raw_slot, mode, callback?) -> request_id` | Clicks a slot in window 0 (the player's own inventory), regardless of any open GUI. |

Every slot table (in `open_gui()`'s `slots`, and elsewhere items appear —
`hud().held_item`, `hud().hotbar[n]`, `inventory().cursor`, window slot
arrays) has this shape:

| Field | Type |
|---|---|
| `raw_slot` | number — the true zero-based protocol slot index; pass this to `click_gui`/`click_inventory` |
| `lua_index` | number — `raw_slot + 1`, for natural 1-based iteration |
| `empty` | boolean |
| `item_id` | number or nil |
| `registry_name` | always `nil` (no item registry vendored — see `docs/lua_wrapper.md#limitations`) |
| `count` | number |
| `custom_name` | string or nil |
| `lore` | array of strings, or nil |
| `enchantments` | array of `{id, level}`, or nil |
| `components` | array of `{type, data}` — every raw data component, best-effort string `data` for ones not specially decoded above |

#### GUI click modes

Pass as `"mode"` or `"mode:param"`:

| Mode | Param | Vanilla equivalent |
|---|---|---|
| `"left"` | — | Left-click |
| `"right"` | — | Right-click |
| `"shift_left"` | — | Shift+left-click |
| `"shift_right"` | — | Shift+right-click |
| `"hotbar_swap"` | `0..=8` | Number-key swap |
| `"offhand_swap"` | — | F key |
| `"throw_one"` | — | Q |
| `"throw_stack"` | — | Ctrl+Q |
| `"double_click"` | — | Double-click collect |
| `"outside_left"` | — | Left-click outside any slot |
| `"outside_right"` | — | Right-click outside any slot |
| `"drag_start"` | `"left"\|"right"\|"middle"` | Begin drag |
| `"drag_add_slot"` | `"left"\|"right"\|"middle"` | Add slot to drag |
| `"drag_end"` | `"left"\|"right"\|"middle"` | End drag |
| `"creative_clone"` | — | Middle-click clone (creative only) |

Example: `bot:click_gui(3, "hotbar_swap:2")`, `bot:click_gui(0, "drag_start:left")`.

### Timers (bot-scoped, run on this bot's own worker)

| Call |
|---|
| `bot:set_timeout(delay_ms, fn) -> timer_id` |
| `bot:set_interval(interval_ms, fn) -> timer_id` |
| `bot:clear_timer(timer_id) -> cancelled` |

### Events

| Call |
|---|
| `bot:on(name, fn) -> handler_id` |
| `bot:once(name, fn) -> handler_id` |
| `bot:off(handler_id) -> removed` |

## `group`

Returned by `swarm:group(name)` / `swarm:groups()`.

| Call | Returns |
|---|---|
| `group:name() -> string` | |
| `group:bot_ids() -> {string, ...}` | Member bot ids, as strings. |
| `group:chat(message) -> {[bot_id] = request_id, ...}` | Routes to each member's own worker/supervisor — bots are never moved between workers. |
| `group:forward(on) -> {[bot_id] = request_id, ...}` | |
| `group:stop_movement() -> {[bot_id] = request_id, ...}` | |
| `group:disconnect() -> true` | Stops every member's supervisor. |

## Events

Every table has a `name` field (the dispatch name below) so a single
handler registered for multiple related sub-kinds (e.g. everything under
`"hud"`) can branch on the payload's `kind` field. `bot`/`event` are passed
as the handler's two arguments: `function(bot, event) ... end`.

| Name | Fires for | Payload (beyond `name`) |
|---|---|---|
| `connecting` | Every attempt starting | — |
| `connected` | A session reaching Play state | — |
| `disconnected` | Session end | `reason` |
| `reconnect_scheduled` | A retry being scheduled | `attempt`, `delay_ms` |
| `retry_started` | A retry attempt beginning | `attempt` |
| `retries_exhausted` | Policy gives up | — |
| `stopped` | Supervisor cancelled | — |
| `kicked` | Server-issued kick | `reason` |
| `login` | Login packet | `entity_id` |
| `spawned` | Player spawned | — |
| `death` | Player death (both the top-level and HUD-sourced variants use this name) | `player_id`, `message` (HUD variant only) |
| `health` | Health/food/saturation change (coalesced) | `health`, `food`, `saturation` |
| `chat` | Player chat message | `sender`, `message` |
| `system_chat` | System chat message | `message` |
| `player_joined` | Tab-list entry added | `uuid`, `name` |
| `player_left` | Tab-list entry removed | `uuid` |
| `player_updated` | Tab-list bulk change | `updated_count`, `removed_count`, `rejected` |
| `entity_spawned` | Nearby entity appears | `entity_id`, `uuid`, `kind` |
| `entity_removed` | Nearby entity disappears | `entity_id` |
| `time` | Time-of-day change (coalesced) | `time_of_day` (top-level) or `age`/`day_time`/`ticking` (HUD variant) |
| `weather` | Weather change (coalesced) | `raining` (top-level) or `raining`/`rain_level`/`thunder_level` (HUD variant) |
| `presentation` | Chat-via-presentation, tab list, disconnect-reason changes | `kind` (`"chat"`/`"tab_list_changed"`/`"disconnected"`), plus kind-specific fields |
| `action_bar` | Action bar text change | `kind = "action_bar_changed"`, `text` |
| `title` | Title/subtitle/timing changes | `kind` (`"title_changed"`/`"subtitle_changed"`/`"title_timing_changed"`/`"titles_cleared"`), plus kind-specific fields |
| `boss_bar` | Boss bar add/remove/update | `id`, `action`, `current`, `applied` |
| `scoreboard` | Objective/score/display-slot changes | `kind` (`"objective_changed"`/`"display_slot_changed"`/`"score_changed"`), plus kind-specific fields |
| `team` | Team create/remove/update/membership | `kind = "team_changed"`, `name`, `action`, `current`, `affected_members`, `rejected_members`, `applied` |
| `hud` | Every other HUD sub-change (vitals, xp, game mode, abilities, hotbar, cooldowns, effects, attributes, respawn, world border, difficulty, spawn position) | `kind` names each sub-variant, e.g. `"vitals_changed"`, `"hotbar_changed"` |
| `gui_opened` | A non-player window is ready to interact with — fires **exactly once** per opened window, on its *first* full slot synchronization (never on the earlier slot-less `open_window` arrival, never on later refresh corrections, never for the player's own inventory) | `window_id`, `state_id`, `first_sync = true` |
| `gui_closed` | A non-player window closes | `window_id` |
| `inventory` | Slot/cursor/property/hotbar-selection/transaction-lifecycle changes not covered above — including the raw `open_window` arrival (`kind = "window_opened"`, slots not yet populated) and non-first full refreshes (`kind = "window_synchronized"`, `first_sync = false`) | `kind` names each sub-variant |
| `action_result` | Completion of a previously-enqueued action | See [Action results](#action-results) |
| `worker_overload` | This worker's critical queue is saturated | — |
| `script_error` | A script handler raised an error | (currently log-only; see `docs/lua_wrapper.md#sandbox`) |
| `script_disabled` | A bot's script execution was disabled after too many consecutive errors | (currently log-only) |

## Action results

`bot:on("action_result", function(bot, event) ... end)`:

| Field | Type |
|---|---|
| `request_id` | number — matches the id an action call returned |
| `bot_id` | number |
| `outcome` | `"delivered_sent"` / `"confirmed"` / `"corrected"` / an [error code](#errors) |
| `ok` | boolean |
| `state_id` | number (only present for `"confirmed"`/`"corrected"`) |
| `error` | error table (only present when `ok == false`) — see [Errors](#errors) |

A one-shot callback, if given as the action call's last argument, receives
the same table and is then automatically unregistered.

## Errors

Every fallible call returns `(nil, error_table)` on failure, or the error
table is the `error` field of an `action_result` event. Shape:

```lua
{code = "...", message = "...", retryable = true|false, bot_id = number|nil, generation = number|nil}
```

Scripts should key logic off `code` — the complete, stable set:

| Code | Meaning |
|---|---|
| `invalid_configuration` | A `swarm:add_*`/`shared:*` call's arguments were invalid. |
| `duplicate_id` | A server/proxy/bot-id/username/group name was already registered. |
| `unknown_proxy` | A bot/group referenced a proxy name that isn't registered. |
| `unknown_server` | A bot/group referenced a server name that isn't registered. |
| `queue_full` | The bounded command queue was full. |
| `not_connected` | The bot has no active connection right now. |
| `session_replaced` | The action's session generation is no longer current. |
| `disconnected` | The bot's connection ended. |
| `supervisor_stopped` | The bot's supervisor has stopped. |
| `invalid_action` | Argument validation failed at the Rust core level (e.g. chat/command starting with `/`), or an action with no Rust-side equivalent was attempted (e.g. `bot:connect()` on an already-stopped bot). |
| `invalid_gui_slot` | GUI slot index out of range. |
| `no_gui_open` | `click_gui` with no non-player GUI currently open. |
| `stale_generation` | An inventory transaction's expected generation no longer matches. |
| `inventory_timeout` | An inventory/GUI transaction timed out waiting for a server response. |
| `inventory_rejected` | The server rejected an inventory transaction. |
| `script_memory_limit` | A handler exceeded the worker's Lua memory limit. |
| `script_instruction_limit` | A handler exceeded the per-invocation instruction budget. |
| `script_disabled` | This bot's script execution has been disabled after too many consecutive errors. **Currently log-only** (same as the `script_disabled` event above) — a disabled bot's *event handlers* stop running, but its underlying connection and any actions issued against it are deliberately unaffected (see `docs/lua_wrapper.md#sandbox`), so no action currently returns this code; it is emitted as a `tracing::warn!` log line only. |
| `worker_overloaded` | A worker's critical (high-priority) queue is saturated — either a direct outcome of an action whose result couldn't be delivered, or (rarely) a semaphore-exhausted action rejected before it was even attempted. |
| `shutdown` | The swarm is shutting down; a pending action/callback was resolved with this instead of being silently dropped. |

`swarm:bot(id)`/`swarm:group(name)` deliberately do **not** have
corresponding `unknown_bot`/`unknown_group` codes — an unknown id/name
returns plain `nil` (see the [Configuration](#configuration) table above),
never a raised or returned error, so there is nothing for those codes to
carry.

## Sandbox reference

See `docs/lua_wrapper.md#sandbox` for the full rationale. Quick reference:

- Available: `string`, `table`, `math`, a restricted `os` (`time`/`clock`
  only — no `getenv`/`execute`/filesystem/process control).
- Not available at all (not merely hidden): `io`, the real `os`,
  `package`, `debug`, `require`/`dofile`/`loadfile`, native module
  loading.
- Memory limit: 16 MiB per worker by default.
- Instruction budget: ≈2,000,000 real instructions per handler
  invocation.
- Consecutive-error threshold: 10 (per bot, resets on any success).
