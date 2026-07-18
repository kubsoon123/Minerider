//! The synthetic/proxy event model this benchmark dispatches to Lua.
//!
//! Deliberately small and bounded: every payload here is what a realistic
//! per-event `bot`/`event` table would actually carry (see
//! `docs/lua_runtime_benchmark.md`'s "Event model"), never a clone of the
//! whole world or every chunk. `GuiOpened` caps its slot array at
//! [`GUI_OPENED_MAX_SLOTS`] for exactly that reason.

// `BotId`/`Priority`/`QueueItem` are the production queue's shared types
// (`crate::lua::queue`), promoted out of this module so the benchmark and
// the production dispatcher use one queue implementation, not two.
pub use crate::lua::queue::{BotId, Priority, QueueItem};

/// How large a realistic payload for this event is, driving which of the
/// benchmark's three payload-size tiers a scenario is exercising.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeClass {
    Small,
    Medium,
    Large,
}

/// One realistic slot inside a `GuiOpened` payload — enough for a script to
/// inspect item identity and act on it, not a full `GuiSlotView`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BenchGuiSlot {
    pub raw_slot: u16,
    pub item_id: i32,
    pub count: i32,
}

/// Slots beyond this index are dropped before the event is built — a real
/// GUI view is bounded by the window's own slot count (at most a double
/// chest, 90 slots including the player inventory), but this benchmark caps
/// lower to keep the "large event" tier realistic without modelling a
/// specific window layout.
pub const GUI_OPENED_MAX_SLOTS: usize = 27;

#[derive(Debug, Clone, PartialEq)]
pub enum BenchEvent {
    // ---- Small ---------------------------------------------------------
    Connected,
    Disconnected {
        reason_len: u16,
    },
    ReconnectScheduled {
        attempt: u32,
        delay_ms: u32,
    },
    Health {
        health: f32,
        food: i32,
    },
    // ---- Medium ---------------------------------------------------------
    Chat {
        sender_len: u8,
        message: String,
    },
    PlayerJoined {
        name_len: u8,
    },
    InventorySlotUpdate {
        slot: u16,
        item_id: i32,
        count: i32,
    },
    // ---- Large ------------------------------------------------------------
    GuiOpened {
        window_id: i32,
        slots: Vec<BenchGuiSlot>,
    },
    StateSummary {
        entity_count: u16,
        hud_effects: u8,
        selected_entity_id: Option<i32>,
    },
    // ---- Low-priority / poll-only in the recommended design -------------
    EntityTick {
        entity_id: i32,
        x: f32,
        y: f32,
        z: f32,
    },
}

impl BenchEvent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disconnected { .. } => "disconnected",
            Self::ReconnectScheduled { .. } => "reconnect_scheduled",
            Self::Health { .. } => "health",
            Self::Chat { .. } => "chat",
            Self::PlayerJoined { .. } => "player_joined",
            Self::InventorySlotUpdate { .. } => "inventory_slot_update",
            Self::GuiOpened { .. } => "gui_opened",
            Self::StateSummary { .. } => "state_summary",
            Self::EntityTick { .. } => "entity_tick",
        }
    }

    pub fn priority(&self) -> Priority {
        match self {
            Self::Connected | Self::Disconnected { .. } | Self::ReconnectScheduled { .. } => {
                Priority::High
            }
            Self::GuiOpened { .. } => Priority::High,
            Self::Health { .. } => Priority::Coalescible,
            Self::Chat { .. } | Self::PlayerJoined { .. } | Self::InventorySlotUpdate { .. } => {
                Priority::Coalescible
            }
            Self::StateSummary { .. } => Priority::Coalescible,
            Self::EntityTick { .. } => Priority::Low,
        }
    }

    pub fn size_class(&self) -> SizeClass {
        match self {
            Self::Connected
            | Self::Disconnected { .. }
            | Self::ReconnectScheduled { .. }
            | Self::Health { .. } => SizeClass::Small,
            Self::Chat { .. } | Self::PlayerJoined { .. } | Self::InventorySlotUpdate { .. } => {
                SizeClass::Medium
            }
            Self::GuiOpened { .. } | Self::StateSummary { .. } => SizeClass::Large,
            Self::EntityTick { .. } => SizeClass::Small,
        }
    }

    /// A deterministic small/medium/large synthetic event for `bot_id`,
    /// varied by `seq` so repeated calls are not byte-identical (matching
    /// real traffic) while staying fully reproducible under a fixed seed.
    pub fn synthetic(size: SizeClass, bot_id: BotId, seq: u64) -> Self {
        match size {
            SizeClass::Small => Self::Health {
                health: 20.0 - (seq % 20) as f32,
                food: 20 - (seq % 5) as i32,
            },
            SizeClass::Medium => Self::Chat {
                sender_len: 6,
                message: if seq % 7 == 0 {
                    "move".to_string()
                } else {
                    format!("bot{} says hi #{seq}", bot_id.0)
                },
            },
            SizeClass::Large => {
                let slots = (0..GUI_OPENED_MAX_SLOTS)
                    .map(|i| BenchGuiSlot {
                        raw_slot: i as u16,
                        item_id: if i % 5 == 0 { 1 + (i as i32) } else { 0 },
                        count: if i % 5 == 0 { 1 } else { 0 },
                    })
                    .collect();
                Self::GuiOpened {
                    window_id: 1 + (seq % 4) as i32,
                    slots,
                }
            }
        }
    }
}

impl QueueItem for BenchEvent {
    fn priority(&self) -> Priority {
        BenchEvent::priority(self)
    }

    fn name(&self) -> &'static str {
        BenchEvent::name(self)
    }
}

/// One event on its way to a worker: the payload plus the timestamp used
/// for enqueue-to-* latency measurements. A type alias over the shared
/// generic `Envelope<E>` (see `crate::lua::queue`) so existing struct-literal
/// call sites (`Envelope { bot_id, event, .. }`) keep working unchanged.
pub type Envelope = crate::lua::queue::Envelope<BenchEvent>;
