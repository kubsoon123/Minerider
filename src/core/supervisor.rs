//! `ClientSupervisor`: connection lifecycle policy for long-running,
//! authorized clients — reconnect-with-backoff, retry classification, and
//! cancellation — layered *on top of* [`Client`] rather than hidden inside
//! it. [`Client`] still represents exactly one active session; this module
//! owns the decision of whether, when and how many times to start another
//! one after a session ends.
//!
//! Intended for authorized monitoring, QA/compatibility testing, and
//! long-running clients on servers you own or are explicitly permitted to
//! use. This is not an anti-AFK-kick or ban-avoidance mechanism: reconnect
//! is disabled by default, never retries after an explicit server rejection
//! or a permanent authentication/protocol error unless the policy is
//! explicitly configured to do so, and never fakes activity of any kind.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::core::client::{Client, ClientConfig};
use crate::core::error::{MineRiderError, RetryClass};
use crate::minecraft::control::{ActionValidationError, BotCommand};
use crate::minecraft::event::{BotEvent, EVENT_CHANNEL_CAPACITY};
use crate::minecraft::inventory::{
    InventoryClick, InventoryClickRequest, InventoryError, InventoryOutcome,
};
use crate::minecraft::play::StateSnapshot;
use crate::minecraft::player::MovementInput;

/// Depth of the bounded command channel from [`SupervisorHandle`] into the
/// running [`ClientSupervisor`]. Deep enough to absorb a burst from a
/// workflow layer issuing several commands back to back without blocking;
/// bounded so a caller that never gets a session can't grow this without
/// limit (offline commands are rejected immediately instead — see
/// [`ControlError::NotConnected`] — so under normal operation this rarely
/// holds more than one or two in-flight commands anyway).
const COMMAND_CHANNEL_CAPACITY: usize = 64;
pub const DEFAULT_INVENTORY_TRANSACTION_TIMEOUT: Duration = Duration::from_secs(5);

/// How many times to retry, if at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryLimit {
    Unlimited,
    Count(u32),
}

/// What a [`ReconnectPolicy`] decides for a given [`RetryClass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    Retry,
    Stop,
}

/// How much randomness to add on top of the exponential backoff delay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Jitter {
    None,
    /// Adds up to `fraction` (clamped to `0.0..=1.0`) of the computed delay
    /// on top of it, as a function of the attempt number rather than real
    /// randomness — so the same attempt number always produces the same
    /// delay and backoff progression stays reproducible in tests even with
    /// jitter enabled.
    Deterministic(f64),
}

/// Typed reconnect policy: whether to reconnect at all, the backoff shape,
/// and — per [`RetryClass`] — whether a given kind of failure is worth
/// retrying. Defaults are deliberately conservative; see [`Default`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReconnectPolicy {
    /// Master switch. `false` (the default) means every failure — of any
    /// class — stops the supervisor after the current attempt; nothing
    /// below matters until this is `true`.
    pub enabled: bool,
    pub max_retries: RetryLimit,
    /// Delay before the first retry.
    pub initial_delay: Duration,
    /// Backoff never grows past this, however many attempts accumulate.
    pub max_delay: Duration,
    /// Backoff growth factor per attempt (`initial_delay * multiplier^n`).
    pub multiplier: f64,
    pub jitter: Jitter,
    /// If a session stays connected at least this long before ending, the
    /// backoff/attempt counter resets to the start on its next failure,
    /// rather than a server that mostly works but blips occasionally
    /// accumulating ever-longer backoff forever.
    pub stable_session_reset: Duration,
    pub on_transient: RetryDecision,
    /// Default `Stop`: an explicit server disconnect (a kick, a ban
    /// message, ...) is exactly the case a covert-reconnect tool would
    /// abuse, so silent auto-retry here must be opt-in.
    pub on_server_rejected: RetryDecision,
    /// Default `Stop`: retrying won't fix bad credentials, a declined
    /// sign-in, or a missing premium session, and hammering an
    /// authentication endpoint on a loop is exactly the account-safety
    /// hazard this design must not create.
    pub on_auth_failure: RetryDecision,
    /// Default `Stop`: the exact same server will produce the exact same
    /// wire-level failure on the next attempt.
    pub on_protocol_incompatible: RetryDecision,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_retries: RetryLimit::Count(5),
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            multiplier: 2.0,
            jitter: Jitter::None,
            stable_session_reset: Duration::from_secs(60),
            on_transient: RetryDecision::Retry,
            on_server_rejected: RetryDecision::Stop,
            on_auth_failure: RetryDecision::Stop,
            on_protocol_incompatible: RetryDecision::Stop,
        }
    }
}

impl ReconnectPolicy {
    /// [`Default`], but with reconnection turned on — the common starting
    /// point for an authorized long-running client.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    pub fn with_max_retries(mut self, limit: RetryLimit) -> Self {
        self.max_retries = limit;
        self
    }

    pub fn with_initial_delay(mut self, delay: Duration) -> Self {
        self.initial_delay = delay;
        self
    }

    pub fn with_max_delay(mut self, delay: Duration) -> Self {
        self.max_delay = delay;
        self
    }

    pub fn with_multiplier(mut self, multiplier: f64) -> Self {
        self.multiplier = multiplier;
        self
    }

    pub fn with_jitter(mut self, jitter: Jitter) -> Self {
        self.jitter = jitter;
        self
    }

    pub fn with_stable_session_reset(mut self, duration: Duration) -> Self {
        self.stable_session_reset = duration;
        self
    }

    pub fn with_server_rejected_decision(mut self, decision: RetryDecision) -> Self {
        self.on_server_rejected = decision;
        self
    }

    pub fn with_auth_failure_decision(mut self, decision: RetryDecision) -> Self {
        self.on_auth_failure = decision;
        self
    }

    pub fn with_protocol_incompatible_decision(mut self, decision: RetryDecision) -> Self {
        self.on_protocol_incompatible = decision;
        self
    }

    /// What to do after an error of the given class, per this policy. The
    /// one place this decision is made — see [`RetryClass`] for how errors
    /// map to a class.
    pub fn decision_for(&self, class: RetryClass) -> RetryDecision {
        if !self.enabled {
            return RetryDecision::Stop;
        }
        match class {
            RetryClass::Transient => self.on_transient,
            RetryClass::ServerRejected => self.on_server_rejected,
            RetryClass::AuthFailure => self.on_auth_failure,
            RetryClass::ProtocolIncompatible => self.on_protocol_incompatible,
        }
    }

    /// Whether `attempt` (the 1-based count of retries already made) has
    /// reached this policy's limit.
    pub fn retries_exhausted(&self, attempt: u32) -> bool {
        match self.max_retries {
            RetryLimit::Unlimited => false,
            RetryLimit::Count(limit) => attempt >= limit,
        }
    }
}

/// Computes the backoff delay for retry attempt `attempt` (1-based: the
/// first retry is attempt 1). Pure and deterministic — no async, no real
/// time — so the whole progression is unit-testable directly.
pub fn backoff_delay(policy: &ReconnectPolicy, attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1) as i32;
    let base = policy.initial_delay.as_secs_f64() * policy.multiplier.max(0.0).powi(exponent);
    let capped = base.min(policy.max_delay.as_secs_f64().max(0.0));
    let with_jitter = match policy.jitter {
        Jitter::None => capped,
        Jitter::Deterministic(fraction) => {
            // A small, reproducible cycle — not real randomness — so a
            // given attempt number always yields the same delay.
            let cycle = (attempt % 5) as f64 / 5.0;
            capped + capped * fraction.clamp(0.0, 1.0) * cycle
        }
    };
    Duration::from_secs_f64(with_jitter.max(0.0))
}

/// The connectivity state observers see between (re)connect attempts —
/// cheap and always current, unlike [`StateSnapshot`] (which reflects
/// whichever session was last active and is reset to
/// [`StateSnapshot::default`] the moment a session ends; see
/// [`ClientSupervisor::run`]). Answers "is this state I'm looking at even
/// live right now" without diffing timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SupervisorStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    ReconnectScheduled {
        attempt: u32,
    },
    Stopped,
}

/// Why [`ClientSupervisor::run`] returned.
#[derive(Debug)]
pub enum SupervisorOutcome {
    /// [`SupervisorHandle::stop`] was called.
    Cancelled,
    /// The configured retry limit was reached without a successful
    /// reconnect.
    RetriesExhausted { last_error: MineRiderError },
    /// Reconnect was disabled, or the policy decided not to retry this
    /// particular error's [`RetryClass`].
    NotRetried { reason: MineRiderError },
}

/// Why a command sent through [`SupervisorHandle::send_command`] (or one of
/// its convenience wrappers) did not complete. Typed and centralized —
/// never a loose string — so a caller (including the future workflow/action
/// layer) can match on exactly what happened instead of guessing from text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    /// The action was rejected before queueing because its text cannot be
    /// represented as a valid protocol-769 chat/command action.
    #[error(transparent)]
    InvalidAction(ActionValidationError),
    /// The bounded supervisor queue is at capacity. The action was not
    /// queued and may be retried deliberately by the caller.
    #[error("the supervised command queue is full")]
    QueueFull,
    /// No session is currently connected (never connected yet, mid-backoff,
    /// or mid-connect-attempt). The command was rejected immediately, not
    /// queued for whenever a session eventually appears.
    #[error("not connected to any session")]
    NotConnected,
    /// The command was queued while a session was active, but that session
    /// ended before the command was processed — a reconnect may already be
    /// underway, but this command was for the *old* session and was not
    /// carried over to a new one.
    #[error("the session ended before this command completed")]
    SessionReplaced,
    /// The command reached the active session, but delivering it to the
    /// play loop failed (the connection was in the process of tearing
    /// down at that exact moment).
    #[error("disconnected while this command was in flight")]
    Disconnected,
    /// The supervisor itself has stopped (cancelled, retries exhausted, or
    /// a permanent error was not retried); no further commands will ever be
    /// processed by this supervisor.
    #[error("the supervisor has stopped")]
    SupervisorStopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InventoryActionError {
    #[error(transparent)]
    Control(ControlError),
    #[error(transparent)]
    Invalid(InventoryError),
    #[error("inventory transaction timed out")]
    TimedOut,
    #[error("disconnected during the inventory transaction")]
    Disconnected,
    #[error("inventory transaction targeted generation {expected}, current is {current}")]
    StaleGeneration { expected: u64, current: u64 },
}

/// One command in flight from a [`SupervisorHandle`] to the running
/// [`ClientSupervisor`], paired with a one-shot reply so the caller learns
/// whether it actually reached a live session.
struct QueuedCommand {
    command: BotCommand,
    generation: u64,
    respond: oneshot::Sender<Result<(), ControlError>>,
}

/// Replies to a queued command that could not be honored, ignoring a
/// caller that already gave up (dropped the receiver, e.g. via a timeout).
fn reject_command(cmd: QueuedCommand, error: ControlError) {
    let _ = cmd.respond.send(Err(error));
}

/// A cloneable handle for observing and stopping a running
/// [`ClientSupervisor`] — the same shape as [`crate::minecraft::control::ControlHandle`]
/// and [`Client::bot_state`]/[`Client::events`], obtained once from
/// [`ClientSupervisor::new`] before [`ClientSupervisor::run`] consumes the
/// supervisor itself.
#[derive(Clone)]
pub struct SupervisorHandle {
    event_tx: broadcast::Sender<BotEvent>,
    state_rx: watch::Receiver<StateSnapshot>,
    status_rx: watch::Receiver<SupervisorStatus>,
    generation_rx: watch::Receiver<u64>,
    command_tx: mpsc::Sender<QueuedCommand>,
    next_inventory_transaction: Arc<AtomicU64>,
    cancel: CancellationToken,
}

impl SupervisorHandle {
    /// Subscribes to the unified event stream: play-session [`BotEvent`]s
    /// (relayed from whichever `Client` is currently active) and
    /// supervisor lifecycle events, interleaved in the order they happened.
    pub fn events(&self) -> broadcast::Receiver<BotEvent> {
        self.event_tx.subscribe()
    }

    /// The latest play-state snapshot, or [`StateSnapshot::default`] if no
    /// session is currently connected — see [`SupervisorStatus`] to tell
    /// the two apart reliably instead of guessing from the snapshot alone.
    pub fn state(&self) -> watch::Receiver<StateSnapshot> {
        self.state_rx.clone()
    }

    /// The current connectivity lifecycle state.
    pub fn status(&self) -> watch::Receiver<SupervisorStatus> {
        self.status_rx.clone()
    }

    /// The current session generation: `0` before the first session ever
    /// connects, incrementing by one on every successful (re)connect. A
    /// multi-step caller (a future inventory transaction or workflow) can
    /// snapshot this before starting and compare it afterward to detect
    /// "the session was replaced mid-operation" itself, rather than relying
    /// only on a single command's own [`ControlError::SessionReplaced`].
    pub fn generation(&self) -> u64 {
        *self.generation_rx.borrow()
    }

    /// Requests the supervisor stop. Interrupts a backoff sleep or an
    /// in-flight connect/session immediately; does not wait for either to
    /// unwind (the corresponding [`ClientSupervisor::run`] call does that).
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// Sends one command to whichever session is currently active.
    ///
    /// Never queued across a reconnect: if no session is connected right
    /// now, this fails immediately with [`ControlError::NotConnected`]
    /// rather than waiting for one to appear (see the module's "Offline
    /// command behavior" — movement/chat/interaction commands are not
    /// silently buffered). If a session was active when this was called but
    /// ends before the command is actually processed, it fails with
    /// [`ControlError::SessionReplaced`] instead of running against
    /// whatever session (if any) replaces it.
    ///
    /// Dropping this future (e.g. via [`tokio::time::timeout`]) before it
    /// resolves cleanly cancels the wait: the supervisor still processes
    /// the command exactly once and simply discards the reply.
    pub async fn send_command(&self, command: BotCommand) -> Result<(), ControlError> {
        command.validate().map_err(ControlError::InvalidAction)?;
        match *self.status_rx.borrow() {
            SupervisorStatus::Connected => {}
            SupervisorStatus::Stopped => return Err(ControlError::SupervisorStopped),
            _ => return Err(ControlError::NotConnected),
        }
        let generation = self.generation();
        if let BotCommand::InventoryClick(request) = &command {
            if request.generation != generation {
                return Err(ControlError::SessionReplaced);
            }
        }
        let (respond, receive) = oneshot::channel();
        let queued = QueuedCommand {
            command,
            generation,
            respond,
        };
        match self.command_tx.try_send(queued) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => return Err(ControlError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(ControlError::SupervisorStopped);
            }
        }
        receive.await.map_err(|_| ControlError::SupervisorStopped)?
    }

    /// Walks to a horizontal target, steering automatically (see
    /// [`crate::minecraft::control::ControlHandle::walk_to`]).
    pub async fn walk_to(&self, x: f64, z: f64) -> Result<(), ControlError> {
        self.send_command(BotCommand::WalkTo { x, z }).await
    }

    /// Faces an absolute yaw/pitch in degrees.
    pub async fn look(&self, yaw: f32, pitch: f32) -> Result<(), ControlError> {
        self.send_command(BotCommand::Look { yaw, pitch }).await
    }

    /// Replaces the manual movement overlay.
    pub async fn set_input(&self, input: MovementInput) -> Result<(), ControlError> {
        self.send_command(BotCommand::SetInput(input)).await
    }

    /// Enables or disables sprint.
    pub async fn sprint(&self, on: bool) -> Result<(), ControlError> {
        self.send_command(BotCommand::Sprint(on)).await
    }

    /// Enables or disables sneak.
    pub async fn sneak(&self, on: bool) -> Result<(), ControlError> {
        self.send_command(BotCommand::Sneak(on)).await
    }

    /// Holds or releases the jump key.
    pub async fn jump(&self, on: bool) -> Result<(), ControlError> {
        self.send_command(BotCommand::Jump(on)).await
    }

    /// Sends an ordinary chat message. Commands are never inferred from `/`.
    pub async fn chat(&self, message: impl Into<String>) -> Result<(), ControlError> {
        self.send_command(BotCommand::Chat(message.into())).await
    }

    /// Runs a command using the protocol's dedicated command packet. The
    /// text excludes the leading slash.
    pub async fn command(&self, command: impl Into<String>) -> Result<(), ControlError> {
        self.send_command(BotCommand::Command(command.into())).await
    }

    pub async fn inventory_click(
        &self,
        window_id: i32,
        click: InventoryClick,
    ) -> Result<InventoryOutcome, InventoryActionError> {
        self.inventory_click_in_generation(
            self.generation(),
            window_id,
            click,
            DEFAULT_INVENTORY_TRANSACTION_TIMEOUT,
        )
        .await
    }

    /// Submits and observes one server-authoritative inventory transaction in
    /// an explicitly selected session generation. This is the building block
    /// for multi-step workflows that must never cross a reconnect.
    pub async fn inventory_click_in_generation(
        &self,
        expected_generation: u64,
        window_id: i32,
        click: InventoryClick,
        timeout: Duration,
    ) -> Result<InventoryOutcome, InventoryActionError> {
        let current = self.generation();
        if current != expected_generation {
            return Err(InventoryActionError::StaleGeneration {
                expected: expected_generation,
                current,
            });
        }
        let state_id = self
            .state_rx
            .borrow()
            .inventory
            .window(window_id)
            .ok_or(InventoryActionError::Invalid(
                InventoryError::UnknownWindow { window_id },
            ))?
            .state_id;
        let transaction_id = self
            .next_inventory_transaction
            .fetch_add(1, Ordering::Relaxed);
        let request = InventoryClickRequest {
            transaction_id,
            generation: expected_generation,
            window_id,
            state_id,
            click,
        };
        self.send_command(BotCommand::InventoryClick(request))
            .await
            .map_err(InventoryActionError::Control)?;
        self.wait_for_inventory_outcome(transaction_id, expected_generation, timeout)
            .await
    }

    async fn wait_for_inventory_outcome(
        &self,
        transaction_id: u64,
        expected_generation: u64,
        timeout: Duration,
    ) -> Result<InventoryOutcome, InventoryActionError> {
        let mut state = self.state_rx.clone();
        let mut status = self.status_rx.clone();
        let mut generation = self.generation_rx.clone();
        let wait = async {
            loop {
                if let Some(outcome) = state
                    .borrow()
                    .inventory
                    .completed_transactions
                    .get(&transaction_id)
                    .copied()
                {
                    return match outcome {
                        InventoryOutcome::Rejected(error) => {
                            Err(InventoryActionError::Invalid(error))
                        }
                        other => Ok(other),
                    };
                }
                let current = *generation.borrow();
                if current != expected_generation {
                    return Err(InventoryActionError::StaleGeneration {
                        expected: expected_generation,
                        current,
                    });
                }
                if *status.borrow() != SupervisorStatus::Connected {
                    return Err(InventoryActionError::Disconnected);
                }
                tokio::select! {
                    changed = state.changed() => {
                        if changed.is_err() {
                            return Err(InventoryActionError::Disconnected);
                        }
                    }
                    changed = status.changed() => {
                        if changed.is_err() {
                            return Err(InventoryActionError::Disconnected);
                        }
                    }
                    changed = generation.changed() => {
                        if changed.is_err() {
                            return Err(InventoryActionError::Disconnected);
                        }
                    }
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| InventoryActionError::TimedOut)?
    }

    /// Clears the walk goal and stops all movement — named `stop_movement`
    /// (not `stop`) so it can't be confused with [`Self::stop`], which
    /// shuts down the whole supervisor.
    pub async fn stop_movement(&self) -> Result<(), ControlError> {
        self.send_command(BotCommand::Stop).await
    }
}

/// Owns connection lifecycle policy for one account/config across however
/// many (re)connect attempts the policy allows, while each individual
/// session is a completely ordinary [`Client`]. Not `Clone` — obtain a
/// [`SupervisorHandle`] from [`Self::new`] before calling [`Self::run`],
/// which consumes `self`.
pub struct ClientSupervisor {
    cfg: ClientConfig,
    policy: ReconnectPolicy,
    event_tx: broadcast::Sender<BotEvent>,
    state_tx: watch::Sender<StateSnapshot>,
    status_tx: watch::Sender<SupervisorStatus>,
    generation_tx: watch::Sender<u64>,
    generation: u64,
    command_rx: mpsc::Receiver<QueuedCommand>,
    cancel: CancellationToken,
}

/// Internal: why [`ClientSupervisor::run_session`] returned.
enum SessionEnd {
    Cancelled,
    Error(MineRiderError),
}

impl ClientSupervisor {
    /// Builds a supervisor for `cfg` under `policy`, returning it paired
    /// with the [`SupervisorHandle`] used to observe and stop it. `cfg` is
    /// reused unchanged for every (re)connect attempt — this does not
    /// refresh or re-run any Microsoft/Xbox sign-in; if `cfg.premium` is
    /// set, every attempt reuses that same already-completed session.
    pub fn new(cfg: ClientConfig, policy: ReconnectPolicy) -> (Self, SupervisorHandle) {
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (state_tx, state_rx) = watch::channel(StateSnapshot::default());
        let (status_tx, status_rx) = watch::channel(SupervisorStatus::default());
        let (generation_tx, generation_rx) = watch::channel(0u64);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let cancel = CancellationToken::new();
        let supervisor = Self {
            cfg,
            policy,
            event_tx: event_tx.clone(),
            state_tx,
            status_tx,
            generation_tx,
            generation: 0,
            command_rx,
            cancel: cancel.clone(),
        };
        let handle = SupervisorHandle {
            event_tx,
            state_rx,
            status_rx,
            generation_rx,
            command_tx,
            next_inventory_transaction: Arc::new(AtomicU64::new(1)),
            cancel,
        };
        (supervisor, handle)
    }

    fn emit(&self, event: BotEvent) {
        let _ = self.event_tx.send(event);
    }

    fn set_status(&self, status: SupervisorStatus) {
        let _ = self.status_tx.send(status);
    }

    /// Runs the supervised connection lifecycle until cancelled or the
    /// policy gives up. Never retried automatically beyond what `policy`
    /// itself specifies, and never spawns a second concurrent connect
    /// attempt — the loop below is strictly sequential.
    ///
    /// Commands sent through the paired [`SupervisorHandle`] are drained and
    /// answered at every point in this loop, not only while a session is
    /// active: while connecting or in backoff they fail immediately with
    /// [`ControlError::NotConnected`] (see "Offline command behavior") so a
    /// caller never has a command silently sitting in the queue waiting for
    /// a session that may be a long time coming, or never comes.
    pub async fn run(mut self) -> SupervisorOutcome {
        let mut attempt: u32 = 0;
        let outcome = 'outer: loop {
            if self.cancel.is_cancelled() {
                self.set_status(SupervisorStatus::Stopped);
                self.emit(BotEvent::StoppedByCancellation);
                break SupervisorOutcome::Cancelled;
            }

            self.reject_queued_commands(ControlError::NotConnected);
            self.set_status(SupervisorStatus::Connecting);
            self.emit(BotEvent::Connecting);

            // Scoped so `connect_fut` (and its borrow of `self.cfg`) is
            // dropped before `self` is borrowed mutably again below.
            let connect_result = {
                let connect_fut = Client::connect(&self.cfg);
                tokio::pin!(connect_fut);
                loop {
                    tokio::select! {
                        biased;
                        _ = self.cancel.cancelled() => {
                            self.set_status(SupervisorStatus::Stopped);
                            self.emit(BotEvent::StoppedByCancellation);
                            break 'outer SupervisorOutcome::Cancelled;
                        }
                        Some(cmd) = self.command_rx.recv() => {
                            reject_command(cmd, ControlError::NotConnected);
                        }
                        result = &mut connect_fut => break result,
                    }
                }
            };

            let mut client = match connect_result {
                Ok(client) => client,
                Err(e) => match self.handle_failure(e, &mut attempt).await {
                    Ok(()) => continue,
                    Err(outcome) => break outcome,
                },
            };

            self.generation += 1;
            let _ = self.generation_tx.send(self.generation);
            self.set_status(SupervisorStatus::Connected);
            self.emit(BotEvent::Connected);
            let connected_at = tokio::time::Instant::now();

            match self.run_session(&mut client).await {
                SessionEnd::Cancelled => {
                    self.set_status(SupervisorStatus::Stopped);
                    self.emit(BotEvent::StoppedByCancellation);
                    break SupervisorOutcome::Cancelled;
                }
                SessionEnd::Error(e) => {
                    if connected_at.elapsed() >= self.policy.stable_session_reset {
                        attempt = 0;
                    }
                    self.set_status(SupervisorStatus::Disconnected);
                    // Reset to a default snapshot immediately, rather than
                    // leaving the last-known (now stale) state visible —
                    // cheap, since StateSnapshot never holds world/chunk
                    // data (see PlayState::snapshot).
                    let _ = self.state_tx.send(StateSnapshot::default());
                    self.emit(BotEvent::Disconnected {
                        reason: e.to_string(),
                    });
                    // Anything still queued was submitted for the session
                    // that just ended; it must not run against whatever
                    // (if anything) replaces it.
                    self.reject_queued_commands(ControlError::SessionReplaced);
                    match self.handle_failure(e, &mut attempt).await {
                        Ok(()) => continue,
                        Err(outcome) => break outcome,
                    }
                }
            }
        };

        // The supervisor is done for good past this point; anything still
        // queued gets a clean typed answer instead of a silently-dropped
        // one-shot sender.
        self.reject_queued_commands(ControlError::SupervisorStopped);
        outcome
    }

    /// Answers every command currently sitting in the queue with `error`,
    /// without blocking — used at every "not connected" boundary.
    fn reject_queued_commands(&mut self, error: ControlError) {
        while let Ok(cmd) = self.command_rx.try_recv() {
            reject_command(cmd, error);
        }
    }

    /// Classifies `error` and either schedules a retry (sleeping for the
    /// backoff delay, cancellably, then returning `Ok(())` so the caller's
    /// loop continues) or returns the terminal [`SupervisorOutcome`].
    async fn handle_failure(
        &mut self,
        error: MineRiderError,
        attempt: &mut u32,
    ) -> std::result::Result<(), SupervisorOutcome> {
        let class = error.retry_class();
        if self.policy.decision_for(class) == RetryDecision::Stop {
            self.set_status(SupervisorStatus::Stopped);
            return Err(SupervisorOutcome::NotRetried { reason: error });
        }
        if self.policy.retries_exhausted(*attempt) {
            self.set_status(SupervisorStatus::Stopped);
            return Err(SupervisorOutcome::RetriesExhausted { last_error: error });
        }
        *attempt += 1;
        let delay = backoff_delay(&self.policy, *attempt);
        self.set_status(SupervisorStatus::ReconnectScheduled { attempt: *attempt });
        self.emit(BotEvent::ReconnectScheduled {
            attempt: *attempt,
            delay,
        });

        let sleep_fut = tokio::time::sleep(delay);
        tokio::pin!(sleep_fut);
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {
                    self.set_status(SupervisorStatus::Stopped);
                    self.emit(BotEvent::StoppedByCancellation);
                    return Err(SupervisorOutcome::Cancelled);
                }
                Some(cmd) = self.command_rx.recv() => {
                    reject_command(cmd, ControlError::NotConnected);
                }
                _ = &mut sleep_fut => break,
            }
        }
        self.emit(BotEvent::RetryAttemptStarted { attempt: *attempt });
        Ok(())
    }

    /// Runs one connected session, relaying its events and state snapshots
    /// onto the supervisor's own long-lived channels, until it ends or
    /// cancellation is requested. Driven entirely within this one task via
    /// `tokio::select!` — no additional task is spawned, so cancellation
    /// while waiting for play-state traffic takes effect immediately rather
    /// than waiting for a read timeout. Commands are forwarded to this
    /// session's own [`crate::minecraft::control::ControlHandle`] as they
    /// arrive — the single point where packets actually get written for
    /// this session, so concurrent callers never race each other onto the
    /// wire.
    async fn run_session(&mut self, client: &mut Client) -> SessionEnd {
        let mut events_rx = client.events();
        let mut state_rx = client.bot_state();
        let control = client.control();
        let run_fut = client.run();
        tokio::pin!(run_fut);
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return SessionEnd::Cancelled,
                result = &mut run_fut => {
                    return SessionEnd::Error(result.err().unwrap_or(MineRiderError::ConnectionClosed));
                }
                Some(cmd) = self.command_rx.recv() => {
                    let outcome = if cmd.generation != self.generation {
                        Err(ControlError::SessionReplaced)
                    } else {
                        control
                            .send(cmd.command)
                            .map_err(|_| ControlError::Disconnected)
                    };
                    let _ = cmd.respond.send(outcome);
                }
                event = events_rx.recv() => {
                    if let Ok(event) = event {
                        self.emit(event);
                    }
                    // A `Lagged` error just means some events weren't
                    // relayed; the play loop itself is never slowed by it.
                }
                changed = state_rx.changed() => {
                    if changed.is_ok() {
                        let snapshot = state_rx.borrow_and_update().clone();
                        let _ = self.state_tx.send(snapshot);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle_for_queue_test(
        status: SupervisorStatus,
        generation: u64,
    ) -> (SupervisorHandle, mpsc::Receiver<QueuedCommand>) {
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (_, state_rx) = watch::channel(StateSnapshot::default());
        let (_, status_rx) = watch::channel(status);
        let (_, generation_rx) = watch::channel(generation);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        (
            SupervisorHandle {
                event_tx,
                state_rx,
                status_rx,
                generation_rx,
                command_tx,
                next_inventory_transaction: Arc::new(AtomicU64::new(1)),
                cancel: CancellationToken::new(),
            },
            command_rx,
        )
    }

    fn handle_for_inventory_wait_test() -> (
        SupervisorHandle,
        watch::Sender<StateSnapshot>,
        watch::Sender<SupervisorStatus>,
        watch::Sender<u64>,
    ) {
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (state_tx, state_rx) = watch::channel(StateSnapshot::default());
        let (status_tx, status_rx) = watch::channel(SupervisorStatus::Connected);
        let (generation_tx, generation_rx) = watch::channel(7);
        let (command_tx, _command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        (
            SupervisorHandle {
                event_tx,
                state_rx,
                status_rx,
                generation_rx,
                command_tx,
                next_inventory_transaction: Arc::new(AtomicU64::new(1)),
                cancel: CancellationToken::new(),
            },
            state_tx,
            status_tx,
            generation_tx,
        )
    }

    fn policy() -> ReconnectPolicy {
        ReconnectPolicy::enabled()
            .with_initial_delay(Duration::from_secs(1))
            .with_max_delay(Duration::from_secs(60))
            .with_multiplier(2.0)
    }

    #[test]
    fn backoff_grows_exponentially_then_caps() {
        let p = policy();
        assert_eq!(backoff_delay(&p, 1), Duration::from_secs(1));
        assert_eq!(backoff_delay(&p, 2), Duration::from_secs(2));
        assert_eq!(backoff_delay(&p, 3), Duration::from_secs(4));
        assert_eq!(backoff_delay(&p, 4), Duration::from_secs(8));
        assert_eq!(backoff_delay(&p, 5), Duration::from_secs(16));
        assert_eq!(backoff_delay(&p, 6), Duration::from_secs(32));
        // 64s would be next uncapped; the policy's max_delay is 60s.
        assert_eq!(backoff_delay(&p, 7), Duration::from_secs(60));
        assert_eq!(backoff_delay(&p, 20), Duration::from_secs(60));
    }

    #[test]
    fn deterministic_jitter_is_reproducible_and_bounded() {
        let p = policy().with_jitter(Jitter::Deterministic(0.5));
        // Same attempt number must always produce the same delay.
        assert_eq!(backoff_delay(&p, 3), backoff_delay(&p, 3));
        // Jitter never pushes the delay below the un-jittered base or more
        // than `fraction` above it.
        let base = Duration::from_secs(4); // attempt 3 = 1s * 2^2
        let jittered = backoff_delay(&p, 3);
        assert!(jittered >= base);
        assert!(jittered <= base.mul_f64(1.5));
    }

    #[test]
    fn disabled_policy_never_retries_regardless_of_class() {
        let p = ReconnectPolicy::default(); // enabled: false
        for class in [
            RetryClass::Transient,
            RetryClass::ServerRejected,
            RetryClass::AuthFailure,
            RetryClass::ProtocolIncompatible,
        ] {
            assert_eq!(p.decision_for(class), RetryDecision::Stop);
        }
    }

    #[test]
    fn enabled_policy_defaults_retry_transient_only() {
        let p = ReconnectPolicy::enabled();
        assert_eq!(p.decision_for(RetryClass::Transient), RetryDecision::Retry);
        assert_eq!(
            p.decision_for(RetryClass::ServerRejected),
            RetryDecision::Stop,
            "no silent reconnect after an explicit server rejection by default"
        );
        assert_eq!(
            p.decision_for(RetryClass::AuthFailure),
            RetryDecision::Stop,
            "no reconnect after a permanent auth failure by default"
        );
        assert_eq!(
            p.decision_for(RetryClass::ProtocolIncompatible),
            RetryDecision::Stop,
            "no reconnect after a protocol incompatibility by default"
        );
    }

    #[test]
    fn policy_can_opt_into_retrying_server_rejections() {
        let p = ReconnectPolicy::enabled().with_server_rejected_decision(RetryDecision::Retry);
        assert_eq!(
            p.decision_for(RetryClass::ServerRejected),
            RetryDecision::Retry
        );
    }

    #[test]
    fn retry_limit_counts_and_unlimited_never_exhausts() {
        let p = ReconnectPolicy::enabled().with_max_retries(RetryLimit::Count(3));
        assert!(!p.retries_exhausted(0));
        assert!(!p.retries_exhausted(2));
        assert!(p.retries_exhausted(3));
        assert!(p.retries_exhausted(100));

        let unlimited = ReconnectPolicy::enabled().with_max_retries(RetryLimit::Unlimited);
        assert!(!unlimited.retries_exhausted(u32::MAX));
    }

    #[tokio::test]
    async fn invalid_actions_are_typed_before_connectivity_checks() {
        let (handle, _rx) = handle_for_queue_test(SupervisorStatus::Disconnected, 0);
        assert_eq!(
            handle.chat("").await,
            Err(ControlError::InvalidAction(
                ActionValidationError::EmptyChat
            ))
        );
        assert_eq!(handle.chat("hello").await, Err(ControlError::NotConnected));
    }

    #[tokio::test]
    async fn queue_capacity_returns_typed_error_without_waiting() {
        let (handle, _rx) = handle_for_queue_test(SupervisorStatus::Connected, 7);
        let mut pending_replies = Vec::new();
        for _ in 0..COMMAND_CHANNEL_CAPACITY {
            let (respond, receive) = oneshot::channel();
            let queued = handle.command_tx.try_send(QueuedCommand {
                command: BotCommand::Stop,
                generation: 7,
                respond,
            });
            assert!(queued.is_ok(), "fill bounded queue");
            pending_replies.push(receive);
        }
        assert_eq!(handle.chat("hello").await, Err(ControlError::QueueFull));
    }

    #[tokio::test]
    async fn queued_action_captures_current_generation() {
        let (handle, mut rx) = handle_for_queue_test(SupervisorStatus::Connected, 7);
        let task = tokio::spawn(async move { handle.command("say hello").await });
        let queued = rx.recv().await.expect("queued action");
        assert_eq!(queued.generation, 7);
        assert_eq!(queued.command, BotCommand::Command("say hello".into()));
        queued.respond.send(Ok(())).unwrap();
        assert_eq!(task.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn inventory_wait_observes_confirmation_disconnect_and_timeout() {
        let (handle, state_tx, status_tx, _generation_tx) = handle_for_inventory_wait_test();
        let confirmed = {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .wait_for_inventory_outcome(1, 7, Duration::from_secs(1))
                    .await
            })
        };
        let mut snapshot = StateSnapshot::default();
        snapshot
            .inventory
            .completed_transactions
            .insert(1, InventoryOutcome::Confirmed { state_id: 2 });
        state_tx.send(snapshot).unwrap();
        assert_eq!(
            confirmed.await.unwrap(),
            Ok(InventoryOutcome::Confirmed { state_id: 2 })
        );

        let disconnected = {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .wait_for_inventory_outcome(2, 7, Duration::from_secs(1))
                    .await
            })
        };
        status_tx.send(SupervisorStatus::Disconnected).unwrap();
        assert_eq!(
            disconnected.await.unwrap(),
            Err(InventoryActionError::Disconnected)
        );

        assert_eq!(
            handle
                .wait_for_inventory_outcome(3, 7, Duration::from_millis(1))
                .await,
            Err(InventoryActionError::Disconnected),
            "disconnected status wins before timeout"
        );
    }

    #[tokio::test]
    async fn inventory_wait_rejects_stale_and_changed_generation() {
        let (handle, _state_tx, _status_tx, generation_tx) = handle_for_inventory_wait_test();
        assert_eq!(
            handle
                .inventory_click_in_generation(
                    6,
                    0,
                    InventoryClick::PickupAll { slot: 0 },
                    Duration::from_secs(1),
                )
                .await,
            Err(InventoryActionError::StaleGeneration {
                expected: 6,
                current: 7,
            })
        );

        assert_eq!(
            handle
                .send_command(BotCommand::InventoryClick(InventoryClickRequest {
                    transaction_id: 99,
                    generation: 6,
                    window_id: 0,
                    state_id: 0,
                    click: InventoryClick::PickupAll { slot: 0 },
                }))
                .await,
            Err(ControlError::SessionReplaced)
        );

        let changed = {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .wait_for_inventory_outcome(4, 7, Duration::from_secs(1))
                    .await
            })
        };
        generation_tx.send(8).unwrap();
        assert_eq!(
            changed.await.unwrap(),
            Err(InventoryActionError::StaleGeneration {
                expected: 7,
                current: 8,
            })
        );
    }

    #[tokio::test]
    async fn inventory_wait_times_out_while_session_stays_connected() {
        let (handle, _state_tx, _status_tx, _generation_tx) = handle_for_inventory_wait_test();
        assert_eq!(
            handle
                .wait_for_inventory_outcome(5, 7, Duration::from_millis(1))
                .await,
            Err(InventoryActionError::TimedOut)
        );
    }

    #[tokio::test]
    async fn inventory_api_queues_state_and_generation_bound_request() {
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let mut snapshot = StateSnapshot::default();
        snapshot.inventory.player_inventory.state_id = 7;
        let (state_tx, state_rx) = watch::channel(snapshot);
        let (_, status_rx) = watch::channel(SupervisorStatus::Connected);
        let (_, generation_rx) = watch::channel(3);
        let (command_tx, mut command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let handle = SupervisorHandle {
            event_tx,
            state_rx,
            status_rx,
            generation_rx,
            command_tx,
            next_inventory_transaction: Arc::new(AtomicU64::new(1)),
            cancel: CancellationToken::new(),
        };
        let task = {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .inventory_click_in_generation(
                        3,
                        0,
                        InventoryClick::QuickMove { slot: 0 },
                        Duration::from_secs(1),
                    )
                    .await
            })
        };
        let queued = command_rx.recv().await.expect("queued inventory command");
        let request = match queued.command {
            BotCommand::InventoryClick(request) => request,
            other => panic!("expected inventory click, got {other:?}"),
        };
        assert_eq!(request.transaction_id, 1);
        assert_eq!(request.generation, 3);
        assert_eq!(request.window_id, 0);
        assert_eq!(request.state_id, 7);
        queued.respond.send(Ok(())).unwrap();

        let mut completed = state_tx.borrow().clone();
        completed
            .inventory
            .completed_transactions
            .insert(1, InventoryOutcome::Confirmed { state_id: 8 });
        state_tx.send(completed).unwrap();
        assert_eq!(
            task.await.unwrap(),
            Ok(InventoryOutcome::Confirmed { state_id: 8 })
        );
    }
}
