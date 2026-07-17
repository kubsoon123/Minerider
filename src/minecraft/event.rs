//! Bot events: a broadcast stream of things that happen to the bot, the
//! push counterpart to the pull-based [`crate::minecraft::play::StateSnapshot`]
//! and the equivalent of Mineflayer's `bot.on(...)`.
//!
//! The play loop emits a [`BotEvent`] as each notable packet is handled;
//! callers subscribe via [`crate::core::client::Client::events`]. Delivery is
//! best-effort: the channel is bounded, and a subscriber that falls behind
//! receives [`tokio::sync::broadcast::error::RecvError::Lagged`] rather than
//! stalling the play loop, so slow observers can never back-pressure the
//! network.

/// The buffer depth of the event broadcast channel. Deep enough that a
/// consumer polling at a human or per-tick cadence never lags on a normal
/// packet burst, small enough to bound memory.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Something the bot observed. Cloneable so it can fan out to every
/// subscriber of the broadcast channel.
#[derive(Debug, Clone, PartialEq)]
pub enum BotEvent {
    /// Entered the play state; the server assigned this entity id.
    Login { entity_id: i32 },
    /// Finished the initial world-load handshake (`player_loaded` sent). The
    /// bot is now "in the world" and moving.
    Spawned,
    /// Health/food/saturation changed.
    Health {
        health: f32,
        food: i32,
        saturation: f32,
    },
    /// The bot died (health reached zero). Auto-respawn is already in flight.
    Death,
    /// A player chat message. `sender` is the rendered display name.
    Chat { sender: String, message: String },
    /// A system chat message (join/leave notices, `/say`, plugin output).
    SystemChat { message: String },
    /// A player appeared in the tab list (joined, or was first seen).
    PlayerJoined { uuid: u128, name: String },
    /// A player left the tab list.
    PlayerLeft { uuid: u128 },
    /// The world time advanced/changed (`time_of_day` in ticks, 0..24000).
    Time { time_of_day: i64 },
    /// Rain started or stopped.
    Weather { raining: bool },
    /// The server kicked the bot; the play loop ends after this.
    Kicked { reason: String },

    // ---- Supervisor lifecycle events ------------------------------------
    // Emitted by `crate::core::supervisor::ClientSupervisor` around
    // (re)connect attempts, on the same event stream rather than a second,
    // competing event type — a Lua/observer layer subscribes once and sees
    // both play-session events (above) and connection lifecycle (below).
    // These never fire outside a supervisor; a bare `Client` used directly
    // only ever emits the play-session variants.
    /// A connect attempt is starting.
    Connecting,
    /// TCP connect, handshake, login and configuration all completed; the
    /// connection has reached Play state. (Distinct from the play-session
    /// [`BotEvent::Login`], which additionally carries the entity id from
    /// the play-state `login` packet.)
    Connected,
    /// The active session ended, for any reason (transport error, protocol
    /// error, or an explicit server kick — [`BotEvent::Kicked`] fires first
    /// in that specific case). `reason` is this error's `Display` text.
    Disconnected { reason: String },
    /// A reconnect attempt was scheduled after this delay, following the
    /// given (1-based) attempt count.
    ReconnectScheduled {
        attempt: u32,
        delay: std::time::Duration,
    },
    /// A new connect attempt is beginning after backoff.
    RetryAttemptStarted { attempt: u32 },
    /// The configured retry limit was reached without a successful
    /// reconnect; the supervisor has stopped.
    RetriesExhausted,
    /// `SupervisorHandle::stop` was called; the supervisor has stopped.
    StoppedByCancellation,
}
