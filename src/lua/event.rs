//! The production dispatch item type, and the exhaustive
//! `BotEvent` → `(name, Priority)` classification that drives
//! `crate::lua::queue`'s priority lanes.
//!
//! Every match below is written without a wildcard arm on purpose: adding a
//! new variant to `crate::minecraft::event::BotEvent` (or any of the event
//! enums it boxes) must force a compile error here, not a silently
//! unhandled event. See `docs/lua_api_reference.md#events` for the full
//! name table this mirrors.

use crate::lua::queue::{Priority, QueueItem};
use crate::minecraft::event::BotEvent;
use crate::minecraft::hud::HudEvent;
use crate::minecraft::inventory::InventoryEvent;
use crate::minecraft::presentation::PresentationEvent;
use crate::minecraft::scoreboard::ScoreboardEvent;

/// Everything that can arrive in one worker's inbound queue.
#[derive(Debug, Clone)]
pub enum WorkItem {
    /// A Minecraft event for one of this worker's bots.
    Bot(BotEvent),
    /// Completion of a previously-enqueued action, delivered to the bot's
    /// *owning* worker so its `action_result` handlers fire (see
    /// `crate::lua::dispatcher::ActionResult`).
    ActionResult(crate::lua::dispatcher::ActionResult),
    /// The same completion, delivered instead (or additionally, when the
    /// owning worker differs) to the worker whose Lua VM actually
    /// registered the one-shot callback for this action — see
    /// `crate::lua::dispatcher::DispatcherHandle::dispatch_action_result`'s
    /// doc comment. A one-shot callback closure lives in the *calling*
    /// worker's registry (it can only be invoked on the VM that created
    /// it), which can differ from the bot's owning worker whenever a
    /// script calls `swarm:bot(id)` for a bot it doesn't own.
    CallbackCompletion(crate::lua::dispatcher::ActionResult),
    /// A cross-worker pub/sub message delivered to this worker. The
    /// payload is cloned directly into every worker's queue at publish
    /// time (see `crate::lua::dispatcher::DispatcherHandle::broadcast_message`)
    /// — there is no shared "latest message" slot to race over.
    Message {
        topic: String,
        payload: crate::lua::shared_value::SharedValue,
    },
    /// A bot-scoped or global timer fired.
    TimerFired { timer_id: u64 },
    /// A script handler raised an error (including sandbox aborts).
    ScriptError {
        event_name: &'static str,
        message: String,
    },
    /// A bot's script execution was disabled after too many consecutive
    /// handler errors.
    ScriptDisabled { reason: String },
    /// This worker's critical queue is saturated.
    WorkerOverloaded,
}

impl QueueItem for WorkItem {
    fn priority(&self) -> Priority {
        match self {
            WorkItem::Bot(event) => bot_event_priority(event),
            WorkItem::ActionResult(_) => Priority::High,
            WorkItem::CallbackCompletion(_) => Priority::High,
            WorkItem::Message { .. } => Priority::Low,
            WorkItem::TimerFired { .. } => Priority::Low,
            WorkItem::ScriptError { .. } => Priority::High,
            WorkItem::ScriptDisabled { .. } => Priority::High,
            WorkItem::WorkerOverloaded => Priority::High,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            WorkItem::Bot(event) => bot_event_name(event),
            WorkItem::ActionResult(_) => "action_result",
            WorkItem::CallbackCompletion(_) => "callback_completion",
            WorkItem::Message { .. } => "message",
            WorkItem::TimerFired { .. } => "timer",
            WorkItem::ScriptError { .. } => "script_error",
            WorkItem::ScriptDisabled { .. } => "script_disabled",
            WorkItem::WorkerOverloaded => "worker_overload",
        }
    }
}

/// The Lua-facing event name for a `BotEvent`, e.g. for `bot:on(name, fn)`
/// registration. Stable — never changes without a documented, deliberate
/// API change (see `docs/lua_api_reference.md#events`).
pub fn bot_event_name(event: &BotEvent) -> &'static str {
    match event {
        BotEvent::Login { .. } => "login",
        BotEvent::Spawned => "spawned",
        BotEvent::Health { .. } => "health",
        BotEvent::Death => "death",
        BotEvent::Chat { .. } => "chat",
        BotEvent::SystemChat { .. } => "system_chat",
        BotEvent::PlayerJoined { .. } => "player_joined",
        BotEvent::PlayerLeft { .. } => "player_left",
        BotEvent::EntitySpawned { .. } => "entity_spawned",
        BotEvent::EntityRemoved { .. } => "entity_removed",
        BotEvent::Time { .. } => "time",
        BotEvent::Weather { .. } => "weather",
        BotEvent::Kicked { .. } => "kicked",
        BotEvent::Presentation(inner) => presentation_event_name(inner),
        BotEvent::Scoreboard(inner) => scoreboard_event_name(inner),
        BotEvent::Hud(inner) => hud_event_name(inner),
        BotEvent::Inventory(inner) => inventory_event_name(inner),
        BotEvent::Connecting => "connecting",
        BotEvent::Connected => "connected",
        BotEvent::Disconnected { .. } => "disconnected",
        BotEvent::ReconnectScheduled { .. } => "reconnect_scheduled",
        BotEvent::RetryAttemptStarted { .. } => "retry_started",
        BotEvent::RetriesExhausted => "retries_exhausted",
        BotEvent::StoppedByCancellation => "stopped",
    }
}

fn presentation_event_name(event: &PresentationEvent) -> &'static str {
    match event {
        PresentationEvent::Chat(_) => "presentation",
        PresentationEvent::ActionBarChanged { .. } => "action_bar",
        PresentationEvent::TitleChanged { .. } => "title",
        PresentationEvent::SubtitleChanged { .. } => "title",
        PresentationEvent::TitleTimingChanged { .. } => "title",
        PresentationEvent::TitlesCleared { .. } => "title",
        PresentationEvent::TabListChanged { .. } => "presentation",
        PresentationEvent::BossBarChanged { .. } => "boss_bar",
        PresentationEvent::Disconnected { .. } => "presentation",
    }
}

fn scoreboard_event_name(event: &ScoreboardEvent) -> &'static str {
    match event {
        ScoreboardEvent::ObjectiveChanged { .. } => "scoreboard",
        ScoreboardEvent::DisplaySlotChanged { .. } => "scoreboard",
        ScoreboardEvent::ScoreChanged { .. } => "scoreboard",
        ScoreboardEvent::TeamChanged { .. } => "team",
    }
}

fn hud_event_name(event: &HudEvent) -> &'static str {
    match event {
        HudEvent::VitalsChanged { .. } => "hud",
        HudEvent::ExperienceChanged { .. } => "hud",
        HudEvent::GameModeChanged { .. } => "hud",
        HudEvent::AbilitiesChanged { .. } => "hud",
        HudEvent::HotbarChanged { .. } => "hud",
        HudEvent::CooldownChanged { .. } => "hud",
        HudEvent::EffectChanged { .. } => "hud",
        HudEvent::AttributesChanged { .. } => "hud",
        HudEvent::Death { .. } => "death",
        HudEvent::Respawned { .. } => "hud",
        HudEvent::WorldBorderChanged { .. } => "hud",
        HudEvent::TimeChanged { .. } => "time",
        HudEvent::WeatherChanged { .. } => "weather",
        HudEvent::DifficultyChanged { .. } => "hud",
        HudEvent::SpawnPositionChanged { .. } => "hud",
        HudEvent::PlayerListChanged { .. } => "player_updated",
    }
}

fn inventory_event_name(event: &InventoryEvent) -> &'static str {
    match event {
        InventoryEvent::WindowOpened { .. } => "gui_opened",
        InventoryEvent::WindowClosed { .. } => "gui_closed",
        InventoryEvent::WindowSynchronized { .. } => "gui_opened",
        InventoryEvent::SlotUpdated { .. } => "inventory",
        InventoryEvent::CursorUpdated => "inventory",
        InventoryEvent::PropertyUpdated { .. } => "inventory",
        InventoryEvent::SelectedHotbarChanged { .. } => "inventory",
        InventoryEvent::TransactionQueued { .. } => "inventory",
        InventoryEvent::TransactionSent { .. } => "inventory",
        InventoryEvent::TransactionFinished { .. } => "inventory",
        InventoryEvent::TransactionRejected { .. } => "inventory",
    }
}

/// Priority classification. High = never silently dropped by the queue.
/// Coalescible = only the latest value per `(bot, kind)` is delivered.
/// Low = best-effort, bounded, may be dropped under sustained overload.
///
/// The mission's critical-delivery list — connected, disconnected,
/// reconnect_scheduled, retries_exhausted, stopped, kicked, death,
/// gui_opened/closed, action_result, inventory transaction outcomes, and
/// script_error/disabled — is exactly the `High` set below, plus the
/// synthetic `WorkItem` variants that are always `High` (see
/// `QueueItem::priority` above).
fn bot_event_priority(event: &BotEvent) -> Priority {
    match event {
        BotEvent::Connecting
        | BotEvent::Connected
        | BotEvent::Disconnected { .. }
        | BotEvent::ReconnectScheduled { .. }
        | BotEvent::RetryAttemptStarted { .. }
        | BotEvent::RetriesExhausted
        | BotEvent::StoppedByCancellation
        | BotEvent::Kicked { .. }
        | BotEvent::Death
        | BotEvent::Login { .. }
        | BotEvent::Spawned => Priority::High,

        BotEvent::Health { .. } | BotEvent::Time { .. } | BotEvent::Weather { .. } => {
            Priority::Coalescible
        }

        BotEvent::Hud(inner) => match **inner {
            HudEvent::Death { .. } | HudEvent::Respawned { .. } => Priority::High,
            _ => Priority::Coalescible,
        },

        BotEvent::Inventory(inner) => match **inner {
            InventoryEvent::WindowOpened { .. }
            | InventoryEvent::WindowClosed { .. }
            | InventoryEvent::WindowSynchronized { .. }
            | InventoryEvent::TransactionFinished { .. }
            | InventoryEvent::TransactionRejected { .. } => Priority::High,
            _ => Priority::Low,
        },

        BotEvent::Chat { .. }
        | BotEvent::SystemChat { .. }
        | BotEvent::PlayerJoined { .. }
        | BotEvent::PlayerLeft { .. }
        | BotEvent::EntitySpawned { .. }
        | BotEvent::EntityRemoved { .. }
        | BotEvent::Presentation(_)
        | BotEvent::Scoreboard(_) => Priority::Low,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn every_supervisor_lifecycle_event_is_high_priority() {
        let events = [
            BotEvent::Connecting,
            BotEvent::Connected,
            BotEvent::Disconnected { reason: "x".into() },
            BotEvent::ReconnectScheduled {
                attempt: 1,
                delay: Duration::from_secs(1),
            },
            BotEvent::RetryAttemptStarted { attempt: 1 },
            BotEvent::RetriesExhausted,
            BotEvent::StoppedByCancellation,
            BotEvent::Kicked { reason: "x".into() },
            BotEvent::Death,
        ];
        for event in events {
            assert_eq!(
                bot_event_priority(&event),
                Priority::High,
                "{event:?} must be high priority"
            );
        }
    }

    #[test]
    fn health_time_weather_are_coalescible() {
        assert_eq!(
            bot_event_priority(&BotEvent::Health {
                health: 20.0,
                food: 20,
                saturation: 5.0
            }),
            Priority::Coalescible
        );
        assert_eq!(
            bot_event_priority(&BotEvent::Time { time_of_day: 0 }),
            Priority::Coalescible
        );
        assert_eq!(
            bot_event_priority(&BotEvent::Weather { raining: false }),
            Priority::Coalescible
        );
    }

    #[test]
    fn event_names_match_the_documented_table_for_representative_variants() {
        assert_eq!(bot_event_name(&BotEvent::Connected), "connected");
        assert_eq!(bot_event_name(&BotEvent::Spawned), "spawned");
        assert_eq!(
            bot_event_name(&BotEvent::Kicked { reason: "x".into() }),
            "kicked"
        );
        assert_eq!(
            bot_event_name(&BotEvent::Inventory(Box::new(
                InventoryEvent::WindowOpened { window_id: 1 }
            ))),
            "gui_opened"
        );
        assert_eq!(
            bot_event_name(&BotEvent::Inventory(Box::new(
                InventoryEvent::WindowClosed { window_id: 1 }
            ))),
            "gui_closed"
        );
    }
}
