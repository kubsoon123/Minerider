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

use std::time::Duration;

use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

use crate::core::client::{Client, ClientConfig};
use crate::core::error::{MineRiderError, RetryClass};
use crate::minecraft::event::{BotEvent, EVENT_CHANNEL_CAPACITY};
use crate::minecraft::play::StateSnapshot;

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

    /// Requests the supervisor stop. Interrupts a backoff sleep or an
    /// in-flight connect/session immediately; does not wait for either to
    /// unwind (the corresponding [`ClientSupervisor::run`] call does that).
    pub fn stop(&self) {
        self.cancel.cancel();
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
        let cancel = CancellationToken::new();
        let supervisor = Self {
            cfg,
            policy,
            event_tx: event_tx.clone(),
            state_tx,
            status_tx,
            cancel: cancel.clone(),
        };
        let handle = SupervisorHandle {
            event_tx,
            state_rx,
            status_rx,
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
    pub async fn run(mut self) -> SupervisorOutcome {
        let mut attempt: u32 = 0;
        loop {
            if self.cancel.is_cancelled() {
                self.set_status(SupervisorStatus::Stopped);
                self.emit(BotEvent::StoppedByCancellation);
                return SupervisorOutcome::Cancelled;
            }

            self.set_status(SupervisorStatus::Connecting);
            self.emit(BotEvent::Connecting);

            let connect_result = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {
                    self.set_status(SupervisorStatus::Stopped);
                    self.emit(BotEvent::StoppedByCancellation);
                    return SupervisorOutcome::Cancelled;
                }
                result = Client::connect(&self.cfg) => result,
            };

            let mut client = match connect_result {
                Ok(client) => client,
                Err(e) => match self.handle_failure(e, &mut attempt).await {
                    Ok(()) => continue,
                    Err(outcome) => return outcome,
                },
            };

            self.set_status(SupervisorStatus::Connected);
            self.emit(BotEvent::Connected);
            let connected_at = tokio::time::Instant::now();

            match self.run_session(&mut client).await {
                SessionEnd::Cancelled => {
                    self.set_status(SupervisorStatus::Stopped);
                    self.emit(BotEvent::StoppedByCancellation);
                    return SupervisorOutcome::Cancelled;
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
                    match self.handle_failure(e, &mut attempt).await {
                        Ok(()) => continue,
                        Err(outcome) => return outcome,
                    }
                }
            }
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

        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                self.set_status(SupervisorStatus::Stopped);
                self.emit(BotEvent::StoppedByCancellation);
                return Err(SupervisorOutcome::Cancelled);
            }
            _ = tokio::time::sleep(delay) => {}
        }
        self.emit(BotEvent::RetryAttemptStarted { attempt: *attempt });
        Ok(())
    }

    /// Runs one connected session, relaying its events and state snapshots
    /// onto the supervisor's own long-lived channels, until it ends or
    /// cancellation is requested. Driven entirely within this one task via
    /// `tokio::select!` — no additional task is spawned, so cancellation
    /// while waiting for play-state traffic takes effect immediately rather
    /// than waiting for a read timeout.
    async fn run_session(&mut self, client: &mut Client) -> SessionEnd {
        let mut events_rx = client.events();
        let mut state_rx = client.bot_state();
        let run_fut = client.run();
        tokio::pin!(run_fut);
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return SessionEnd::Cancelled,
                result = &mut run_fut => {
                    return SessionEnd::Error(result.err().unwrap_or(MineRiderError::ConnectionClosed));
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
}
