# Wrapper API readiness

This is **Phase 1.2**: completing and stabilizing the public Rust API, not
the wrapper. There is no Lua, Python, JavaScript, HTTP, WebSocket, FFI, or
dashboard layer in this repository, and none is implemented in this phase.
Nothing here should be read as a claim that a wrapper exists yet.

The purpose of this document is narrower and more durable than a phase
changelog: it is a map, kept up to date as the API evolves, of exactly what
a *future* scripting/application wrapper will be able to call, module by
module, and what in each module is still missing. **The future wrapper is
expected to cover the entire MineRider public API** — not only the surface
this phase added — so this table intentionally also lists modules this
phase did not touch (chat, presentation, world) alongside the ones it did.

Status legend: **Available** (a stable public method/type exists today) ·
**Partial** (something is exposed but with a caveat) · **Not implemented**.

| Wrapper module | Status | Core type/method the wrapper will call | Known limitations |
|---|---|---|---|
| `client` | Available | `core::client::{Client, ClientConfig}`, `core::supervisor::{ClientSupervisor, SupervisorHandle}` | Two entry points exist (bare `Client` for one session, `ClientSupervisor` for reconnect-managed long-running use) — the wrapper should almost certainly wrap `SupervisorHandle`, not `Client`, directly. |
| `connection` | Available | `network::connection::Connection`, `ClientConfig::{write_timeout, connect_deadline}` | Internal to `Client`; a wrapper has no reason to touch `Connection` directly, only the timeout/deadline knobs on `ClientConfig`. |
| `proxy` | Not implemented (on this branch) | — | SOCKS5 transport (`ClientConfig::proxy`, `network::socks5`) exists on the sibling draft PR #3 (`feat/socks5-transport`), not on this branch's base (PR #1) — see "Branch strategy" in the PR description for why. Once merged, `proxy` becomes **Available** with no further core-API work: `ClientConfig::proxy: Option<Arc<Socks5ProxyConfig>>` is already a plain, wrapper-friendly field. |
| `state` | Available | `SupervisorHandle::state() -> watch::Receiver<StateSnapshot>` | `StateSnapshot` is a full, independent clone published after every clientbound packet, every processed command, and every tick — see `PlayState::snapshot`'s doc comment. It deliberately excludes `World`/chunk data (see `world` row below). |
| `player` | Available | `StateSnapshot.player: LocalPlayer` | Every field `pub`; position, yaw/pitch, velocity, on-ground, health/food/saturation, XP, death/respawn context all reachable via `StateSnapshot.player.*`. Game mode/abilities/selected-hotbar-slot live under `StateSnapshot.hud` instead (HUD/player-facing state), not duplicated here. |
| `movement` | Available | `SupervisorHandle::{forward, backward, strafe_left, strafe_right, jump, sneak, sprint, set_input, stop_movement, walk_to}` | Independent per-key state (opposite keys cancel like real vanilla input, exactly like `manual.forward`/`strafe`); `walk_to` is straight-line steering only — no pathfinding, obstacle avoidance, or automatic jumping, and none is planned for this API. |
| `look` | Available | `SupervisorHandle::{look, set_random_look}` | `look` applies immediately; `set_random_look` is bounded, deterministic-when-seeded head rotation driven by the existing per-tick controller — not humanization/anti-detection logic, and explicitly out of scope to become that. |
| `chat` | Available | `SupervisorHandle::{chat, command}` | Chat and commands are distinct protocol packets, never inferred from a leading `/`; UTF-16-length-validated; unsigned (no chat-session message signing yet — servers enforcing secure chat will reject/kick, an existing, pre-Phase-1.2 limitation, see `README.md`). |
| `inventory` | Available | `StateSnapshot.inventory: InventoryState`, `SupervisorHandle::{inventory_click, inventory_click_in_generation, click_inventory_slot}` | Server-authoritative: no optimistic local mutation, ever. A click's outcome is `Sent`/`Confirmed`/`Corrected`/`TimedOut`/`WindowClosed`/`Rejected`, distinguishing "the packet went out" from "the server actually applied it." |
| `gui` | Available | `SupervisorHandle::{open_gui, inventory_view, click_open_gui_slot}`, `minecraft::gui::{GuiView, GuiSlotView}`, `minecraft::inventory::GuiClick` | Raw protocol slot ordering only, never remapped. A missing GUI is a typed `GuiActionError::NoGuiOpen`, never a silent fallback to the player's own inventory. See `items` below for the one real gap (`registry_name` is always `None`). |
| `items` | Partial | `GuiSlotView::{item_id, count, custom_name, lore, enchantments, components}` | **No item-id-to-registry-name mapping.** No `items.json` is vendored in this repository (only `blocks.json`/`blockCollisionShapes.json` are, under `crates/minerider-codegen/vendor/minecraft-data/pc/1.21.4/`), and this phase's mission explicitly forbids parsing a large registry file at runtime or hand-maintaining a partial id table — so `registry_name` and enchantment names stay numeric-id-only, documented at the field. **Follow-up:** vendor the matching `items.json` (same source, same version, alongside the files already there) and have `minerider-codegen` generate a static `id -> &'static str` lookup at build time, the same pattern it already uses for the protocol itself — no runtime parsing, no hand-maintained table, satisfying the same constraint properly. Until then, custom name/lore (already plain-text-decoded from data components) are the reliable identifying signal for a caller that needs to distinguish items without a name table. |
| `entities` | Available (Partial events) | `StateSnapshot.entities: EntityStore` (`get`/`iter`/`len`), `event::BotEvent::{EntitySpawned, EntityRemoved}` | Spawn/remove are event-emitted; **per-tick position/rotation/velocity changes are not** — polling `StateSnapshot.entities` is the intended way to observe continuous movement (emitting one event per moving entity per tick would flood the event channel at network rate with many tracked entities). This was an audited pre-existing gap (no entity events existed at all before this phase); it is now partially closed. |
| `players` | Available | `StateSnapshot.players: PlayerList` (`get`/`iter`/`len`), `event::BotEvent::{PlayerJoined, PlayerLeft}` | Tab-list state; `PlayerChatSession` retains only the length of the signed-chat public key/signature, not the raw bytes (chat-session signing itself is unimplemented, consistent with the `chat` row above). |
| `world` | Partial | `StateSnapshot.dimension: Option<DimensionType>` (new this phase); `minecraft::world::World::{block_state, has_chunk, collision_boxes}` (all `pub`, but unreachable from outside the crate) | **The full `World`/chunk data is intentionally never in `StateSnapshot`** — this is the single largest documented, deliberate limitation in the whole audit, and it is deliberate for a real reason: cloning per-client chunk state every tick would reintroduce exactly the kind of duplication `perf/shared-chunk-store` (PR #2) exists to eliminate. `PlayState`'s own doc comment already flags the fix as "a future on-demand accessor" — i.e. a bounded request/response query (ask for one block at a time, not the whole world), not a bigger snapshot. Not attempted in this phase: a correct bounded query channel (with its own timeout/generation-staleness handling, mirroring the inventory-transaction pattern) is real, separately-testable scope, and this phase already covers nine other missions. `dimension` (min/max Y, coordinate scale, ...) is cheap and is now included, since it costs nothing to clone every tick and is genuinely useful (interpreting block-state ids, understanding build-height limits) even before block queries exist. |
| `hud` | Available | `StateSnapshot.hud: HudState`, `event::BotEvent::Hud` | Vitals, XP, game mode, abilities, hotbar/held item, cooldowns, effects, attributes, death/respawn, world border, time/weather/difficulty, spawn position, bounded modern player list — all present, pre-existing (Phase 1). |
| `presentation` | Available | `StateSnapshot.presentation: PresentationState`, `event::BotEvent::{Chat, SystemChat, Kicked, Presentation}` | Chat/system messages, action bar, titles, tab-list header/footer, boss bars — pre-existing (Phase 1). Signed-chat wire data is retained but not cryptographically verified (pre-existing, documented limitation, unrelated to this phase). |
| `scoreboard` | Available | `StateSnapshot.scoreboard: ScoreboardState`, `event::BotEvent::Scoreboard` | Objectives, display slots, scores, teams — pre-existing (Phase 1), untouched this phase. |
| `events` | Available | `SupervisorHandle::events() -> broadcast::Receiver<BotEvent>` | One unified stream (play-session events interleaved with supervisor lifecycle events in the order they happened); a lagging subscriber gets `RecvError::Lagged` rather than back-pressuring the play loop — recovery is "read a fresh `state()` snapshot", which is why every state-bearing event's doc comment says exactly that. |
| `errors` | Available | `core::error::MineRiderError`, `core::supervisor::{ControlError, InventoryActionError, GuiActionError}`, `minecraft::control::ActionValidationError`, `minecraft::inventory::InventoryError` | Layered, typed, no loose strings: connectivity failures (`ControlError`) are distinct from inventory-transaction failures (`InventoryActionError`) are distinct from GUI-selection failures (`GuiActionError`) are distinct from wire-level protocol failures (`InventoryError`). A wrapper can match on exact variants rather than parsing text. |

## Design choices made with the wrapper specifically in mind

- **Stable semantic names over channel internals.** `SupervisorHandle`
  already hid its `tokio::sync::{watch,broadcast,mpsc}` plumbing behind
  `state()`/`events()`/`status()`/`send_command()`-shaped methods before
  this phase; every method added this phase (`forward`, `open_gui`,
  `click_open_gui_slot`, `use_item`, `swing`, `set_random_look`, ...)
  follows the same shape — a wrapper binding generator sees plain async
  methods returning typed `Result`s, never a raw channel type.
- **Concrete public types over generics.** `GuiView`/`GuiSlotView` are
  concrete structs, not a generic view over `Window`'s internal types —
  deliberately, since a generic-heavy API is exactly the kind of thing a
  Lua/Python/JS binding generator struggles with (see the mission's own
  "avoid generic-heavy public APIs when a concrete public type is clearer").
- **"Sent" vs "confirmed" is a type, not a comment.** `InventoryOutcome`
  and the `use_item`/`swing` doc comments both say explicitly, in the type
  itself where possible, whether an action's `Ok(())` means "the packet
  left this process" or "the server authoritatively applied it" — a
  wrapper surfacing this to script authors needs that distinction to be
  inspectable, not just documented in English.
  `crate::minecraft::gui::GuiSlotView::registry_name`'s outcome ("Ok, but every value happens to be `None` right now") is exactly what that pattern is for — the field exists, is typed, and its absence is documented at the type rather than requiring a version check against this markdown file.
- **No GUI vs player inventory is a different error, not a fallback.**
  `click_open_gui_slot` never silently redirects to window 0 when nothing
  is open; it returns `GuiActionError::NoGuiOpen`. A wrapper exposing both
  as separate script-level calls needs this distinction to not be
  papered over by "well it did *something*."
  `ControlError::{NotConnected, SessionReplaced, Disconnected,
  SupervisorStopped}` similarly keep "never had a session", "had one, it
  ended before this command landed", and "the whole supervisor is done"
  as separate variants rather than one generic "not connected" — a
  wrapper's retry logic needs to be able to tell these apart.
- **No callbacks tied to Rust lifetimes.** Every new method takes owned
  values (`bool`, `Hand`, `GuiClick`, `usize`, `Option<RandomLookConfig>`)
  and returns owned `Result`s — nothing borrows from the caller across an
  `.await` point, and nothing is a closure/trait-object callback a binding
  layer would have to keep a Rust-side lifetime alive for.

## Explicit non-goals restated

Per the mission for this phase: no Lua/Python/JS/HTTP/WebSocket/FFI
bindings, no dashboard/frontend, no pathfinding/A*/obstacle avoidance, no
automatic jumping, no mining/building/combat/targeting AI, no automatic GUI
decision logic, no crafting/recipe/merchant/anvil engine, no full vanilla
menu prediction, no proxy rotation/pools, no anti-AFK or anti-cheat-bypass
behavior. This document does not walk any of that back — it only maps what
already exists so the *next* phase's wrapper work has a single, accurate
starting inventory instead of having to re-derive it from source.
