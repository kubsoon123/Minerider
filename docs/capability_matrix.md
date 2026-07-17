# Core API capability matrix (Phase 1.2)

A systematic audit of existing MineRider functionality against public
access, produced before any code in this phase was written (see the PR
description's "Verified starting state") and updated as gaps were closed.
For every capability: where it lives internally, what was already public,
what this phase added, whether it is sync or async, whether it is
snapshot-based (poll `SupervisorHandle::state()`) or event-based (subscribe
`SupervisorHandle::events()`), how it behaves across reconnect, and known
limitations.

Legend for "Reconnect behavior": **Resets** (state/config returns to a
default on every new session) · **Isolated** (per-session, e.g. a counter
that starts fresh but isn't "reset" in a visible way) · **N/A** (stateless
query/action).

## Lifecycle

| Capability | Internal location | Public access | Sync/Async | Snapshot/Event | Reconnect behavior | Limitations |
|---|---|---|---|---|---|---|
| Create client configuration | `core::client::ClientConfig` | Available (pre-existing) | Sync (builder) | — | N/A | — |
| Connect (single session) | `core::client::Client::connect` | Available (pre-existing) | Async | — | N/A | No auto-reconnect; use `ClientSupervisor` for that. |
| Connect (managed, reconnecting) | `core::supervisor::ClientSupervisor::{new, run}` | Available (pre-existing) | Async (`run` drives until stopped) | — | Resets per attempt | `cfg.premium` (if set) is reused unchanged across every attempt — no re-sign-in. |
| Disconnect / stop | `SupervisorHandle::stop` | Available (pre-existing) | Sync (cancels; `run()`'s caller awaits the actual unwind) | — | N/A | — |
| Connection status | `SupervisorHandle::status` | Available (pre-existing) | Sync | Snapshot (`watch`) | Resets to `Disconnected`/`Stopped` | `SupervisorStatus` is the reliable "is this live right now" signal — `StateSnapshot` alone can't distinguish "no session yet" from "session ended", see its own doc comment. |
| Session generation | `SupervisorHandle::generation` | Available (pre-existing) | Sync | Snapshot (`watch`) | Increments on every reconnect | Used to detect "this multi-step operation crossed a reconnect" without relying on a single command's own error. |
| Reconnect status | `SupervisorHandle::status` (`ReconnectScheduled{attempt}`) + `BotEvent::{ReconnectScheduled, RetryAttemptStarted}` | Available (pre-existing) | Sync/Event | Both | N/A | — |
| Terminal failure reason | `SupervisorOutcome::{RetriesExhausted, NotRetried}` (return value of `ClientSupervisor::run`) | Available (pre-existing) | Async (awaited once) | — | N/A | Only observable by the task that called `.run()`; not itself broadcast as an event (the `Disconnected`/`RetriesExhausted`/`StoppedByCancellation` *events* narrate the same story for a live subscriber). |

## Player

| Capability | Internal location | Public access | Sync/Async | Snapshot/Event | Reconnect behavior | Limitations |
|---|---|---|---|---|---|---|
| Entity ID | `LocalPlayer.entity_id` | Available (pre-existing) | — | Snapshot | Resets to `None` | — |
| UUID | `Client::uuid` | Available (pre-existing) | — | — | N/A (per-`Client`, not per-`StateSnapshot`) | Not on `SupervisorHandle`/`StateSnapshot` — only the single-session `Client` facade exposes it directly. Documented gap, not fixed this phase (would need threading through the supervisor's session-relay path). |
| Position, yaw, pitch | `LocalPlayer.position: PlayerPosition` | Available (pre-existing) | — | Snapshot | Resets to origin | — |
| Velocity | `LocalPlayer.velocity: Vec3` | Available (pre-existing) | — | Snapshot | Resets | — |
| On-ground state | `LocalPlayer.on_ground` | Available (pre-existing) | — | Snapshot | Resets | — |
| Health | `LocalPlayer.health` | Available (pre-existing) | — | Snapshot + `BotEvent::Health`/`Death` | Resets to spawn default | — |
| Food | `LocalPlayer.food` | Available (pre-existing) | — | Snapshot + `BotEvent::Health` | Resets | — |
| Saturation | `LocalPlayer.saturation` | Available (pre-existing) | — | Snapshot + `BotEvent::Health` | Resets | — |
| Experience | `LocalPlayer.{xp_bar, xp_level, total_experience}` | Available (pre-existing) | — | Snapshot | Resets | — |
| Game mode | `HudState.game_mode` | Available (pre-existing) | — | Snapshot + `HudEvent::GameModeChanged` | Resets | — |
| Abilities | `HudState.abilities: Abilities` | Available (pre-existing) | — | Snapshot + `HudEvent::AbilitiesChanged` | Resets | — |
| Selected hotbar slot | `HudState.selected_hotbar_slot` / `InventoryState.selected_hotbar_slot` | Available (pre-existing) | — | Snapshot + events | Resets | Tracked in two places (HUD's copy from `held_item_slot`'s HUD projection, inventory's own copy) — both already existed; not unified this phase since neither is wrong, just two valid views of the same server field. |
| Active effects | `HudState.active_effects` | Available (pre-existing) | — | Snapshot + `HudEvent::EffectChanged` | Resets | — |
| Attributes | `HudState.attributes` | Available (pre-existing) | — | Snapshot + `HudEvent::AttributesChanged` | Resets | — |
| Death/respawn state | `HudState.{death, respawn}` | Available (pre-existing) | — | Snapshot + `HudEvent::{Death, Respawned}` + `BotEvent::Death` | Resets | Auto-respawn is already automatic (sends the respawn request itself, Mineflayer-style); no manual respawn API is exposed or needed. |

## Movement and control

| Capability | Internal location | Public access before this phase | Added this phase | Sync/Async | Reconnect behavior | Limitations |
|---|---|---|---|---|---|---|
| Forward | `Controller.held_forward` / `manual.forward` | **Missing** (only full `set_input`) | `SupervisorHandle::forward`/`ControlHandle::forward` | Async/Sync | Resets (fresh `Controller` per session) | Cancels with `backward`, exactly like vanilla W+S. |
| Backward | same | **Missing** | `SupervisorHandle::backward` | Async/Sync | Resets | — |
| Strafe left | `Controller.held_left` / `manual.strafe` | **Missing** | `SupervisorHandle::strafe_left` | Async/Sync | Resets | Cancels with `strafe_right`. |
| Strafe right | same | **Missing** | `SupervisorHandle::strafe_right` | Async/Sync | Resets | — |
| Jump | `Controller.manual.jump` | Available (pre-existing) | — | Async/Sync | Resets | — |
| Sneak | `Controller.manual.sneak` | Available (pre-existing) | — | Async/Sync | Resets | — |
| Sprint | `Controller.manual.sprint` | Available (pre-existing) | — | Async/Sync | Resets | Never auto-enabled by any other action. |
| Explicit movement input | `BotCommand::SetInput` | Available (pre-existing) | Best-effort held-key resync added so later individual toggles behave predictably afterward | Async/Sync | Resets | Bypasses the independent-key model; documented as the deliberate escape hatch for full manual control. |
| Look | `BotCommand::Look` / `Controller::apply_look` | Available (pre-existing) | — | Async/Sync | Resets | Applies immediately (not deferred to next tick). |
| Random look | `Controller.random_look` | **Missing** | `SupervisorHandle::set_random_look` / `RandomLookConfig` | Async/Sync | Resets (disabled) | Bounded random walk from current yaw, absolute pitch range; driven by the existing per-tick `Controller::drive`, no separate task. Explicit `look()` applies immediately and does not disable it. |
| Stop movement | `BotCommand::Stop` | Available (pre-existing) | Directional held-flags now also cleared by `Stop` | Async/Sync | N/A | `Stop` intentionally does not disable random look (a facing behavior, not movement input) — disable it explicitly. |
| `walk_to` | `BotCommand::WalkTo` | Available (pre-existing) | — | Async/Sync | Resets (goal cleared) | Straight-line steering; no pathfinding/obstacle avoidance, and none planned. |

## Communication

| Capability | Internal location | Public access | Sync/Async | Snapshot/Event | Reconnect behavior | Limitations |
|---|---|---|---|---|---|---|
| Chat (send) | `BotCommand::Chat` | Available (pre-existing) | Async/Sync | Event (own send has no event; received chat does, see below) | Not replayed across reconnect (queued commands for an ended session are rejected, never carried to a new one) | UTF-16-length-validated; never inferred from `/`. |
| Commands (send) | `BotCommand::Command` | Available (pre-existing) | Async/Sync | — | Not replayed | Distinct protocol packet from chat, proven by test. |
| System chat (receive) | `PresentationState`, `PresentationEvent::Chat` | Available (pre-existing) | — | Snapshot + `BotEvent::SystemChat` | Resets | — |
| Player chat (receive) | same | Available (pre-existing) | — | Snapshot + `BotEvent::Chat` | Resets | Signed-chat data retained, not verified. |
| Action bar | `PresentationState.action_bar` | Available (pre-existing) | — | Snapshot + `PresentationEvent::ActionBarChanged` | Resets | — |
| Title/subtitle | `PresentationState.titles: TitleState` | Available (pre-existing) | — | Snapshot + `PresentationEvent::{TitleChanged, SubtitleChanged, TitleTimingChanged, TitlesCleared}` | Resets | — |
| Tab header/footer | `PresentationState.tab_list` | Available (pre-existing) | — | Snapshot + `PresentationEvent::TabListChanged` | Resets | — |
| Boss bars | `PresentationState.boss_bars` | Available (pre-existing) | — | Snapshot + `PresentationEvent::BossBarChanged` | Resets | — |

## Inventory and GUI

| Capability | Internal location | Public access before this phase | Added this phase | Sync/Async | Reconnect behavior | Limitations |
|---|---|---|---|---|---|---|
| Player inventory | `InventoryState.player_inventory: Window` | Available (pre-existing, raw `Window`) | `SupervisorHandle::inventory_view() -> GuiView` convenience | Sync | Resets | — |
| Currently open GUI | `InventoryState.open_window: Option<Window>` | Available (pre-existing, raw `Window`) | `SupervisorHandle::open_gui() -> Option<GuiView>` | Sync | Resets | — |
| Cursor item | `InventoryState.cursor: Slot` | Available (pre-existing, raw `Slot`) | Surfaced as `GuiView.cursor: GuiSlotView` | Sync | Resets | — |
| All slots | `Window.slots: Vec<Slot>` | Available (pre-existing) | `GuiView.slots: Vec<GuiSlotView>`, decoded (custom name/lore/enchantments best-effort, raw components preserved) | Sync | Resets | Raw protocol order, never remapped — see `docs/wrapper_api_readiness.md`'s `items`/`gui` rows for the registry-name limitation. |
| State ID | `Window.state_id` | Available (pre-existing) | Surfaced on `GuiView.state_id`; already used internally for click validation | Sync | Resets | — |
| Window ID | `Window.id` | Available (pre-existing) | Surfaced on `GuiView.window_id` | Sync | Resets | — |
| Menu type | `Window.kind` | Available (pre-existing, raw registry id) | Surfaced on `GuiView.menu_type` | Sync | Resets | Numeric only — same registry-name limitation as items. |
| Properties | `Window.properties` | Available (pre-existing) | Surfaced on `GuiView.properties` | Sync | Resets | — |
| Slot updates | `InventoryState::{set_slot, window_items, set_cursor_item}` | Available (pre-existing) | — | Event (`InventoryEvent::{SlotUpdated, WindowSynchronized, CursorUpdated}`) | Resets | — |
| Click transactions | `SupervisorHandle::inventory_click` | Available (pre-existing) | Convenience layer added: `GuiClick` + `click_open_gui_slot`/`click_inventory_slot` | Async | Not replayed across reconnect (stale-generation rejected) | Server-authoritative throughout; no optimistic mutation, unchanged this phase. |
| Transaction outcomes | `InventoryOutcome` | Available (pre-existing) | — | Async return value + `InventoryEvent::TransactionFinished` | N/A | `Sent`/`Confirmed`/`Corrected`/`TimedOut`/`WindowClosed`/`Rejected` — explicit, never conflated. |

## World and entities

| Capability | Internal location | Public access | Sync/Async | Snapshot/Event | Reconnect behavior | Limitations |
|---|---|---|---|---|---|---|
| Entities | `EntityStore` (`get`/`iter`/`len`) | Available (pre-existing) | — | Snapshot | Resets | — |
| Player list | `PlayerList` (`get`/`iter`/`len`) | Available (pre-existing) | — | Snapshot + `BotEvent::{PlayerJoined, PlayerLeft}` | Resets | — |
| Chunks already retained | `World` (`has_chunk`, `block_state`, `collision_boxes` — all `pub`) | **Unreachable from outside the crate** (`PlayState.world` is a private field, excluded from `StateSnapshot` by design) | Not changed this phase (see `docs/wrapper_api_readiness.md`'s `world` row for why and what the real follow-up is) | — | — | Documented, deliberate: a full `World` in every snapshot would reintroduce the per-client chunk duplication PR #2 exists to remove. A bounded on-demand query channel is the correct fix and is out of scope for this phase. |
| Block queries | `World::block_state` (`pub`, but see above) | Same as above | Same as above | — | — | Same as above. |
| Dimension | `PlayState.dimension: Option<DimensionType>` (private) | **Missing from `StateSnapshot`** | `StateSnapshot.dimension: Option<DimensionType>` | — | Snapshot | Cheap (a handful of scalar fields), unlike full `World`; safe to add without the chunk-duplication concern above. |
| Time | `PlayState.world_time` | Available (pre-existing) | — | Snapshot + `BotEvent::Time` | Resets | — |
| Weather | `PlayState.raining` | Available (pre-existing) | — | Snapshot + `BotEvent::Weather` | Resets | — |
| Border | `HudState.world_border` | Available (pre-existing) | — | Snapshot + `HudEvent::WorldBorderChanged` | Resets | — |
| Scoreboard | `ScoreboardState` | Available (pre-existing) | — | Snapshot + `BotEvent::Scoreboard` | Resets | — |
| Teams | `ScoreboardState.teams`, `team_for_member` | Available (pre-existing) | — | Snapshot + `ScoreboardEvent::TeamChanged` | Resets | — |

## Events

| Capability | Internal location | Public access before this phase | Added this phase | Limitations |
|---|---|---|---|---|
| Connection lifecycle events | `BotEvent::{Connecting, Connected, Disconnected, ReconnectScheduled, RetryAttemptStarted, RetriesExhausted, StoppedByCancellation}` | Available (pre-existing) | — | Supervisor-only; a bare `Client` never emits these. |
| Player-state events | `BotEvent::{Login, Spawned, Health, Death}` | Available (pre-existing), **except `Spawned` was declared but never emitted** | `Spawned` now fires at the same point the client sends `player_loaded` (world-load handshake complete) | — |
| Inventory events | `BotEvent::Inventory(Box<InventoryEvent>)` | Available (pre-existing) | — | — |
| GUI events | Same as inventory events — a "GUI" is just the currently-open non-player `Window` | Available (pre-existing, via `InventoryEvent::{WindowOpened, WindowClosed}`) | — | No dedicated `GuiEvent` type was introduced; `InventoryEvent` already covers window open/close, and duplicating it under a new name would violate "do not duplicate Minerider logic into future-wrapper-specific structures." |
| Entity events | — | **Missing entirely** (confirmed by audit: `spawn_entity`/`entity_destroy`/movement packets called straight into `EntityStore` with no `state.emit(...)` anywhere) | `BotEvent::{EntitySpawned, EntityRemoved}` added | Deliberately does **not** add per-tick move/rotate/velocity events — see `docs/wrapper_api_readiness.md`'s `entities` row for why (event-channel flood risk with many tracked entities); poll `StateSnapshot.entities` for continuous movement. |
| Presentation events | `BotEvent::Presentation(Box<PresentationEvent>)` | Available (pre-existing) | — | — |
| HUD events | `BotEvent::Hud(Box<HudEvent>)` | Available (pre-existing) | — | — |
| Scoreboard events | `BotEvent::Scoreboard(Box<ScoreboardEvent>)` | Available (pre-existing) | — | — |
| Errors and disconnects | `BotEvent::{Kicked, Disconnected}`, `SupervisorOutcome` | Available (pre-existing) | — | — |

## Summary of what this phase actually changed vs. audited-and-left-alone

**Added:** independent forward/backward/strafe-left/strafe-right controls;
bounded random look; held-item use (`use_item`) and arm swing (`swing`);
`GuiView`/`GuiSlotView` read model with best-effort custom-name/lore/
enchantment decoding and full raw-component preservation; `GuiClick` +
`click_open_gui_slot`/`click_inventory_slot` convenience API over the
unchanged server-authoritative transaction system; `StateSnapshot.dimension`;
`BotEvent::Spawned` (was declared, never emitted — now is);
`BotEvent::{EntitySpawned, EntityRemoved}` (did not exist at all).

**Audited and confirmed already correct, left unchanged:** chat/command
separation and validation, HUD/presentation/scoreboard/player-list state and
events, the inventory transaction system's server-authoritative design,
reconnect/generation semantics, `PlayerList`/`EntityStore`'s existing
iterator-based (not raw-map) public access.

**Audited and explicitly deferred, with a stated reason and a stated
follow-up:** item/block/enchantment registry names (needs vendoring new
data, out of scope for a phase that must not parse large registry files at
runtime or hand-maintain a partial table); `World`/block queries reachable
from outside the crate (needs a bounded request/response query channel, not
a bigger snapshot — see the `world` row above); per-tick entity movement
events (deliberately not added, event-flood risk); a bare `Client`'s own
`uuid` field not being mirrored onto `SupervisorHandle`/`StateSnapshot`.
