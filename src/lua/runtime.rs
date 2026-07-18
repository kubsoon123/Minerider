//! Top-level orchestrator: spawns the fixed worker pool, waits (bounded,
//! failure-atomic) for the coordinator's configuration phase, resolves
//! proxies, spawns every bot's `ClientSupervisor`, bridges each bot's
//! `BotEvent` stream into the dispatcher, and drives graceful shutdown.
//!
//! This is the async, tokio-owned half of the runtime; `crate::lua::worker`
//! is the sync, one-`Lua`-VM-per-OS-thread half. The two meet at
//! [`worker::StartupBarrier`].
//!
//! ## Startup protocol
//!
//! `run_swarm` never waits indefinitely and never leaves partial state
//! behind on failure:
//!
//! 1. Every worker is spawned via `tokio::task::spawn_blocking` (a real,
//!    dedicated OS thread from tokio's blocking pool for that worker's
//!    whole lifetime — the "one `Lua` VM per dedicated thread" invariant is
//!    unaffected), wrapped in `std::panic::catch_unwind` so a worker panic
//!    becomes a normal, attributable `Err` instead of poisoning anything.
//! 2. Every worker reports exactly one of [`worker::WorkerStartupReport::ReachedBarrier`]
//!    (its script ran to `swarm:connect_all()` and is now blocked on the
//!    barrier) or `::Failed` (script/sandbox error) — or its task exits
//!    unexpectedly without reporting either, which is treated the same as
//!    a `Failed`. `run_swarm` collects these under a bounded
//!    `SwarmRuntimeConfig::startup_timeout`.
//! 3. Bots are spawned — `spawn_all_bots` validates every bot's
//!    server/proxy references *before* creating any supervisor, so it is
//!    itself atomic: either every bot is spawned, or none are.
//! 4. On any failure at any of the above steps (a worker failing, a
//!    missing `connect_all`, the timeout elapsing, proxy resolution
//!    failing, or bot spawning failing), [`abort_startup`] runs: signal
//!    shutdown, abort the startup barrier (unblocking any worker still
//!    waiting on it with a typed error instead of hanging), close every
//!    queue, stop any supervisors that (defensively) might already exist,
//!    and join every worker task — before `run_swarm` returns its
//!    `RuntimeError`. No detached threads or tasks survive a failed
//!    `run_swarm` call.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::core::client::ClientConfig;
use crate::core::supervisor::{ClientSupervisor, ReconnectPolicy, SupervisorHandle};
use crate::lua::api::shared::SharedState;
use crate::lua::dispatcher::{DispatcherHandle, WorkerQueue};
use crate::lua::queue::{PriorityQueue, QueueDesign};
use crate::lua::registry::{ProxyProfiles, SwarmRegistry, UnknownProxyProfile};
use crate::lua::sandbox::SandboxConfig;
use crate::lua::worker::{
    run_worker, StartupBarrier, StartupPayload, WorkerConfig, WorkerReport, WorkerStartupReport,
};
use crate::network::socks5::{EnvConfigError, Socks5ProxyConfig};

/// Persistent workers by default — never scales with bot count (see
/// `docs/lua_runtime_benchmark.md`'s recommendation).
pub const DEFAULT_WORKER_COUNT: usize = 4;
/// Safe default for the high-priority (never-silently-dropped) lane, per
/// worker. No single concrete value was specified as mandatory beyond
/// "choose a safe default such as 4096 per worker" — this is that default,
/// chosen from the benchmark's measured peaks (see
/// `docs/lua_runtime_benchmark.md#queues`).
pub const DEFAULT_HIGH_QUEUE_CAPACITY: usize = 4096;
pub const DEFAULT_LOW_QUEUE_CAPACITY: usize = 1024;
/// Safe default bound on the whole startup sequence (every worker loading
/// its script and reaching `swarm:connect_all()`, then bot spawning). A
/// script that never calls `connect_all` — or a worker that hangs before
/// reaching it — fails startup cleanly after this long, rather than
/// blocking `run_swarm` forever.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on cleanup after a failed startup — generous, but finite, so a
/// truly stuck worker thread can't hang `run_swarm`'s error return forever.
const STARTUP_ABORT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct SwarmRuntimeConfig {
    pub worker_count: usize,
    pub sandbox: SandboxConfig,
    pub high_queue_capacity: usize,
    pub low_queue_capacity: usize,
    pub callback_timeout: Duration,
    pub script_body: String,
    /// Host-trusted proxy profiles: profile id → complete, already-resolved
    /// config (including credentials, if any). Never derived from Lua — a
    /// script may only reference a profile id already present here. See
    /// `crate::lua::registry`'s module doc comment for why, and
    /// [`proxy_profiles_from_env`] for the CLI's own way of building this
    /// map from environment variables (never argv).
    pub proxy_profiles: Arc<ProxyProfiles>,
    /// Bound on the whole startup sequence — see the module doc comment.
    pub startup_timeout: Duration,
}

impl Default for SwarmRuntimeConfig {
    fn default() -> Self {
        Self {
            worker_count: DEFAULT_WORKER_COUNT,
            sandbox: SandboxConfig::default(),
            high_queue_capacity: DEFAULT_HIGH_QUEUE_CAPACITY,
            low_queue_capacity: DEFAULT_LOW_QUEUE_CAPACITY,
            callback_timeout: crate::lua::worker::DEFAULT_CALLBACK_TIMEOUT,
            script_body: String::new(),
            proxy_profiles: Arc::new(ProxyProfiles::new()),
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
        }
    }
}

/// Builds a [`ProxyProfiles`] map for the CLI from environment variables
/// only — the caller passes `profile_id -> ENV_PREFIX` pairs (e.g. from a
/// repeatable `--proxy-profile id=PREFIX` flag), and each profile's actual
/// host/port/credentials are read from `{PREFIX}_HOST`/`_PORT`/
/// `_USERNAME`/`_PASSWORD` via the existing `Socks5ProxyConfig::from_env`.
/// **Never accepts a password through `prefixes` itself** — only prefix
/// *names*, which are operator-chosen and never come from a script or from
/// argv containing the secret itself.
pub fn proxy_profiles_from_env(
    prefixes: impl IntoIterator<Item = (String, String)>,
) -> Result<ProxyProfiles, EnvConfigError> {
    let mut profiles = ProxyProfiles::new();
    for (profile_id, env_prefix) in prefixes {
        match Socks5ProxyConfig::from_env(&env_prefix)? {
            Some(cfg) => {
                profiles.insert(profile_id, Arc::new(cfg));
            }
            None => {
                return Err(EnvConfigError::Missing(format!("{env_prefix}_HOST")));
            }
        }
    }
    Ok(profiles)
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("script error: {0}")]
    Script(String),
    /// A bot referenced a proxy profile id that isn't in
    /// `SwarmRuntimeConfig::proxy_profiles`. This should already be caught
    /// by `crate::lua::registry::SwarmRegistryBuilder::add_bot` (which
    /// validates against the same id set) — reaching this variant means
    /// that invariant was somehow violated, so spawning fails loudly
    /// rather than silently connecting a bot directly (bypassing its
    /// intended proxy would be a real, not merely cosmetic, regression).
    #[error("proxy profile error: {0}")]
    Proxy(#[from] UnknownProxyProfile),
    #[error("worker startup failed: {0}")]
    Worker(String),
    /// A specific worker's script/sandbox failed during startup, or its
    /// task exited unexpectedly before reaching the startup barrier.
    #[error("worker {worker_index} failed during startup: {reason}")]
    WorkerFailed { worker_index: usize, reason: String },
    /// Every worker must reach `swarm:connect_all()` within
    /// `SwarmRuntimeConfig::startup_timeout` — this fires whether the
    /// cause was a hung script, a coordinator that never called
    /// `connect_all`, or anything else that kept a worker from reporting.
    #[error("swarm startup timed out after {0:?}")]
    StartupTimeout(Duration),
}

pub struct RunningSwarm {
    pub dispatcher: DispatcherHandle,
    pub bot_handles: Arc<HashMap<u32, SupervisorHandle>>,
    pub registry: Arc<SwarmRegistry>,
    /// The same cross-worker shared-state store every worker's
    /// `swarm.shared` reads/writes — exposed here so a host (the CLI, or a
    /// test) can observe values scripts publish, e.g. for diagnostics or
    /// black-box correctness assertions.
    pub shared_state: Arc<SharedState>,
    shutdown: Arc<AtomicBool>,
    worker_tasks: tokio::task::JoinSet<(usize, Result<WorkerReport, String>)>,
    supervisor_tasks: tokio::task::JoinSet<()>,
    bridge_tasks: tokio::task::JoinSet<()>,
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "worker thread panicked with a non-string payload".to_string()
    }
}

/// Spawns the worker pool, waits (bounded, failure-atomic — see the module
/// doc comment) for the coordinator to finish `swarm:configure(fn)` and
/// every worker to call `swarm:connect_all()`, then connects every bot.
/// Must be called from within a tokio runtime.
pub async fn run_swarm(config: SwarmRuntimeConfig) -> Result<RunningSwarm, RuntimeError> {
    let worker_count = config.worker_count.max(1);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shared_state = Arc::new(SharedState::new());
    let startup_barrier = StartupBarrier::new();

    let queues: Vec<Arc<WorkerQueue>> = (0..worker_count)
        .map(|_| {
            WorkerQueue::new(QueueDesign::Priority(PriorityQueue::new(
                config.high_queue_capacity,
                config.low_queue_capacity,
            )))
        })
        .collect();
    let dispatcher = DispatcherHandle::new(queues.clone());
    let runtime_handle = tokio::runtime::Handle::current();
    let (config_tx, config_rx) = std::sync::mpsc::sync_channel(1);
    let (startup_report_tx, mut startup_report_rx) =
        tokio::sync::mpsc::unbounded_channel::<(usize, WorkerStartupReport)>();
    // Only the *names* of the host's registered proxy profiles reach a
    // worker (and therefore the coordinator's `SwarmRegistryBuilder`) — the
    // actual `Arc<Socks5ProxyConfig>` values (with credentials) are only
    // ever read from `config.proxy_profiles` on this async side, in
    // `spawn_all_bots`, never handed to a worker thread.
    let proxy_profile_ids: Arc<BTreeSet<String>> =
        Arc::new(config.proxy_profiles.keys().cloned().collect());

    let mut worker_tasks: tokio::task::JoinSet<(usize, Result<WorkerReport, String>)> =
        tokio::task::JoinSet::new();
    for (worker_index, queue) in queues.iter().enumerate().take(worker_count) {
        let is_coordinator = worker_index == 0;
        let worker_config = WorkerConfig {
            worker_index,
            is_coordinator,
            dispatcher: dispatcher.clone(),
            runtime_handle: runtime_handle.clone(),
            sandbox: config.sandbox,
            startup_barrier: startup_barrier.clone(),
            shared_state: shared_state.clone(),
            proxy_profile_ids: proxy_profile_ids.clone(),
            config_tx: if is_coordinator {
                Some(config_tx.clone())
            } else {
                None
            },
            startup_report_tx: startup_report_tx.clone(),
            shutdown: shutdown.clone(),
            callback_timeout: config.callback_timeout,
        };
        let queue = queue.clone();
        let script = config.script_body.clone();
        let panic_report_tx = startup_report_tx.clone();
        worker_tasks.spawn_blocking(move || {
            // `catch_unwind` needs an `UnwindSafe` closure; `WorkerConfig`
            // and friends aren't unwind-safe by the compiler's
            // conservative default (they contain `RefCell`s reachable
            // transitively), but that's fine here — on panic we discard
            // all of this worker's state immediately and its thread ends,
            // so no other code ever observes a possibly-inconsistent
            // `RefCell`.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_worker(worker_config, queue, &script)
            }));
            let result = match result {
                Ok(r) => r,
                Err(panic_payload) => {
                    let msg = panic_message(&*panic_payload);
                    // `run_worker`'s own error paths already report
                    // `Failed` before returning; a genuine panic bypasses
                    // that, so report it here instead — harmless if a
                    // report for this worker was somehow already sent,
                    // since the collector only acts on the first one.
                    let _ = panic_report_tx
                        .send((worker_index, WorkerStartupReport::Failed(msg.clone())));
                    Err(msg)
                }
            };
            (worker_index, result)
        });
    }
    drop(config_tx);
    drop(startup_report_tx);

    // Phase 1: bounded wait for every worker to either reach the startup
    // barrier or fail — never indefinite, and any single failure (however
    // it manifests) ends the wait immediately rather than waiting out the
    // full timeout.
    let collected = tokio::time::timeout(
        config.startup_timeout,
        collect_startup_reports(worker_count, &mut startup_report_rx, &mut worker_tasks),
    )
    .await;

    let collected = match collected {
        Ok(inner) => inner,
        Err(_elapsed) => Err(RuntimeError::StartupTimeout(config.startup_timeout)),
    };

    if let Err(e) = collected {
        abort_startup(
            &shutdown,
            &startup_barrier,
            &dispatcher,
            worker_tasks,
            None,
            &e,
        )
        .await;
        return Err(e);
    }

    // Phase 2: every worker is now confirmed blocked at the barrier, and
    // the coordinator's `connect_all()` sends its registry strictly before
    // reporting `ReachedBarrier` (same thread, sequential), so it's already
    // sitting in `config_rx`'s buffer.
    let registry = match config_rx.try_recv() {
        Ok(registry) => registry,
        Err(_) => {
            let e = RuntimeError::Script(
                "coordinator reported ready but never sent its registry (internal inconsistency)"
                    .to_string(),
            );
            abort_startup(
                &shutdown,
                &startup_barrier,
                &dispatcher,
                worker_tasks,
                None,
                &e,
            )
            .await;
            return Err(e);
        }
    };

    // Phase 3: `spawn_all_bots` validates every bot before creating any
    // supervisor (see its own doc comment) — it is itself atomic, so a
    // failure here never leaves a partially-spawned swarm to unwind.
    let (bot_handles, supervisor_tasks, bridge_tasks) =
        match spawn_all_bots(&registry, &config.proxy_profiles, &dispatcher).await {
            Ok(v) => v,
            Err(e) => {
                abort_startup(
                    &shutdown,
                    &startup_barrier,
                    &dispatcher,
                    worker_tasks,
                    None,
                    &e,
                )
                .await;
                return Err(e);
            }
        };
    let bot_handles = Arc::new(bot_handles);
    let registry = Arc::new(registry);

    startup_barrier.publish(Arc::new(StartupPayload {
        registry: registry.clone(),
        bot_handles: bot_handles.clone(),
    }));

    Ok(RunningSwarm {
        dispatcher,
        bot_handles,
        registry,
        shared_state,
        shutdown,
        worker_tasks,
        supervisor_tasks,
        bridge_tasks,
    })
}

/// Drives phase 1 of startup: loops until either every worker has reported
/// `ReachedBarrier`, or any worker reports `Failed`, or any worker's task
/// exits without ever reporting either (treated identically to `Failed`).
async fn collect_startup_reports(
    worker_count: usize,
    startup_report_rx: &mut tokio::sync::mpsc::UnboundedReceiver<(usize, WorkerStartupReport)>,
    worker_tasks: &mut tokio::task::JoinSet<(usize, Result<WorkerReport, String>)>,
) -> Result<(), RuntimeError> {
    let mut reached = HashSet::with_capacity(worker_count);
    loop {
        tokio::select! {
            report = startup_report_rx.recv() => {
                match report {
                    Some((idx, WorkerStartupReport::ReachedBarrier)) => {
                        reached.insert(idx);
                        if reached.len() >= worker_count {
                            return Ok(());
                        }
                    }
                    Some((idx, WorkerStartupReport::Failed(reason))) => {
                        return Err(RuntimeError::WorkerFailed { worker_index: idx, reason });
                    }
                    None => {
                        return Err(RuntimeError::Worker(
                            "every worker's startup-report channel closed before startup completed"
                                .to_string(),
                        ));
                    }
                }
            }
            joined = worker_tasks.join_next() => {
                match joined {
                    Some(Ok((idx, Ok(_report)))) => {
                        // A worker's whole lifecycle ended (its dispatch
                        // loop returned) before startup ever completed —
                        // can only mean it never reached (or never stayed
                        // at) the barrier correctly.
                        return Err(RuntimeError::WorkerFailed {
                            worker_index: idx,
                            reason: "worker exited before startup completed".to_string(),
                        });
                    }
                    Some(Ok((idx, Err(reason)))) => {
                        return Err(RuntimeError::WorkerFailed { worker_index: idx, reason });
                    }
                    Some(Err(join_error)) => {
                        return Err(RuntimeError::Worker(format!(
                            "a worker task could not be joined: {join_error}"
                        )));
                    }
                    None => {
                        // No tasks left at all (worker_count == 0 in
                        // practice never happens — `run_swarm` clamps to
                        // at least 1) — keep waiting on reports rather than
                        // looping tightly.
                        std::future::pending::<()>().await;
                    }
                }
            }
        }
    }
}

/// Runs on any startup failure: signals shutdown, aborts the startup
/// barrier (releasing any worker still blocked in `connect_all()` with a
/// typed error instead of leaving it hanging), closes every queue, stops
/// any supervisors that (defensively) might already exist, and joins every
/// worker task — bounded by [`STARTUP_ABORT_CLEANUP_TIMEOUT`] so a truly
/// stuck worker can't hang the error return forever.
async fn abort_startup(
    shutdown: &Arc<AtomicBool>,
    startup_barrier: &Arc<StartupBarrier>,
    dispatcher: &DispatcherHandle,
    mut worker_tasks: tokio::task::JoinSet<(usize, Result<WorkerReport, String>)>,
    partial_bot_handles: Option<&HashMap<u32, SupervisorHandle>>,
    reason: &RuntimeError,
) {
    shutdown.store(true, Ordering::Release);
    startup_barrier.abort(reason.to_string());
    dispatcher.close_all();
    if let Some(handles) = partial_bot_handles {
        for handle in handles.values() {
            handle.stop();
        }
    }
    let _ = tokio::time::timeout(STARTUP_ABORT_CLEANUP_TIMEOUT, async {
        while worker_tasks.join_next().await.is_some() {}
    })
    .await;
}

/// Spawns every bot's `ClientSupervisor` and event bridge. **Atomic**: a
/// first pass validates every bot's server/proxy references against
/// `registry`/`proxy_profiles` before a second pass creates anything, so a
/// single invalid reference can never leave some bots spawned and others
/// not — either the whole registry is valid and every bot is spawned, or
/// none are.
async fn spawn_all_bots(
    registry: &SwarmRegistry,
    proxy_profiles: &ProxyProfiles,
    dispatcher: &DispatcherHandle,
) -> Result<
    (
        HashMap<u32, SupervisorHandle>,
        tokio::task::JoinSet<()>,
        tokio::task::JoinSet<()>,
    ),
    RuntimeError,
> {
    for (bot_id, bot_def) in &registry.bots {
        if !registry.servers.contains_key(&bot_def.server) {
            return Err(RuntimeError::Script(format!(
                "bot {bot_id} references unknown server `{}`",
                bot_def.server
            )));
        }
        if let Some(profile_id) = &bot_def.proxy {
            if !proxy_profiles.contains_key(profile_id) {
                // Must already be a valid key —
                // `SwarmRegistryBuilder::add_bot` validated it against
                // this exact same profile-id set. Treated as a hard error
                // rather than silently falling back to a direct
                // connection: a bot silently skipping its intended proxy
                // would be a real behavioral/security regression, not a
                // cosmetic one.
                return Err(UnknownProxyProfile(profile_id.clone()).into());
            }
        }
    }

    let mut bot_handles = HashMap::new();
    let mut supervisor_tasks = tokio::task::JoinSet::new();
    let mut bridge_tasks = tokio::task::JoinSet::new();

    for (bot_id, bot_def) in &registry.bots {
        let server = registry
            .servers
            .get(&bot_def.server)
            .expect("validated above");
        let mut cfg = ClientConfig::new(server.host.clone(), server.port, bot_def.username.clone())
            .with_view_distance(server.view_distance)
            .with_write_timeout(server.write_timeout)
            .with_connect_deadline(server.connect_deadline)
            .with_chunk_sharing(server.shared_chunks);
        if let Some(profile_id) = &bot_def.proxy {
            let proxy_cfg = proxy_profiles
                .get(profile_id)
                .cloned()
                .expect("validated above");
            cfg = cfg.with_socks5_proxy(proxy_cfg);
        }
        let policy: ReconnectPolicy = bot_def.reconnect.clone();
        let (supervisor, handle) = ClientSupervisor::new(cfg, policy);
        bot_handles.insert(*bot_id, handle.clone());

        supervisor_tasks.spawn(async move {
            let _ = supervisor.run().await;
        });

        let bot_id_val = *bot_id;
        let dispatcher = dispatcher.clone();
        let mut events = handle.events();
        bridge_tasks.spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        dispatcher.dispatch_bot_event(bot_id_val, event);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    Ok((bot_handles, supervisor_tasks, bridge_tasks))
}

impl RunningSwarm {
    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// Graceful shutdown: stop every bot's supervisor, close every
    /// worker's queue (unblocking its dispatch loop), wait (bounded by
    /// `timeout`) for the event-bridge and supervisor tasks to finish,
    /// then join every worker task.
    pub async fn shutdown(mut self, timeout: Duration) {
        self.shutdown.store(true, Ordering::Release);
        for handle in self.bot_handles.values() {
            handle.stop();
        }
        self.dispatcher.close_all();

        let _ = tokio::time::timeout(timeout, async {
            while self.supervisor_tasks.join_next().await.is_some() {}
            while self.bridge_tasks.join_next().await.is_some() {}
            while self.worker_tasks.join_next().await.is_some() {}
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::registry::{BotDef, ServerDef};

    fn quick_config(script_body: String, startup_timeout: Duration) -> SwarmRuntimeConfig {
        SwarmRuntimeConfig {
            worker_count: 2,
            sandbox: SandboxConfig::default(),
            high_queue_capacity: 64,
            low_queue_capacity: 32,
            callback_timeout: Duration::from_secs(5),
            script_body,
            proxy_profiles: Arc::new(ProxyProfiles::new()),
            startup_timeout,
        }
    }

    fn empty_report() -> WorkerReport {
        WorkerReport {
            events_processed: 0,
            handlers_run: 0,
            handler_errors: 0,
            bots_disabled_by_consecutive_errors: 0,
        }
    }

    // ---- End-to-end, through the real `run_swarm`, no network needed
    // (every failure mode below is decided before `spawn_all_bots` would
    // ever attempt a connection) -------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_connect_all_times_out_rather_than_hanging_forever() {
        let script = r#"
            swarm:configure(function()
                swarm:add_server({name = "main", host = "127.0.0.1", port = 1})
                swarm:add_bot({username = "Bot0", server = "main"})
            end)
            -- Deliberately never calls swarm:connect_all().
        "#
        .to_string();
        let timeout = Duration::from_millis(300);
        let start = std::time::Instant::now();
        let result = run_swarm(quick_config(script, timeout)).await;
        let elapsed = start.elapsed();
        match result {
            Err(RuntimeError::StartupTimeout(configured)) => assert_eq!(configured, timeout),
            Ok(_) => panic!("expected StartupTimeout, got Ok"),
            Err(other) => panic!("expected StartupTimeout, got {other}"),
        }
        assert!(
            elapsed < Duration::from_secs(5),
            "must not hang well past the timeout, took {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn coordinator_script_failure_is_reported_promptly_not_after_the_timeout() {
        // Must fail on the coordinator *only* — an unconditional `error(...)`
        // here would fail on every worker simultaneously, making it a race
        // which worker's `Failed` report `collect_startup_reports` happens
        // to see first (this previously flaked under load: worker 1's
        // report occasionally won the race and failed the `worker_index ==
        // 0` assertion below, even though the underlying startup-abort
        // logic was correct either way).
        let script = r#"
            if swarm:status().is_coordinator then
                error("boom from the coordinator")
            end
            swarm:configure(function()
                swarm:add_server({name = "main", host = "127.0.0.1", port = 1})
            end)
            swarm:connect_all()
        "#
        .to_string();
        let start = std::time::Instant::now();
        let result = run_swarm(quick_config(script, Duration::from_secs(30))).await;
        let elapsed = start.elapsed();
        match result {
            Err(RuntimeError::WorkerFailed {
                worker_index,
                reason,
            }) => {
                assert_eq!(worker_index, 0, "the coordinator is always worker 0");
                assert!(
                    reason.contains("boom from the coordinator"),
                    "reason was: {reason}"
                );
            }
            Ok(_) => panic!("expected WorkerFailed, got Ok"),
            Err(other) => panic!("expected WorkerFailed, got {other}"),
        }
        assert!(
            elapsed < Duration::from_secs(5),
            "must fail promptly, not wait out the 30s timeout: took {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_coordinator_script_failure_is_reported_promptly() {
        let script = r#"
            if not swarm:status().is_coordinator then
                error("boom from a non-coordinator worker")
            end
            swarm:configure(function()
                swarm:add_server({name = "main", host = "127.0.0.1", port = 1})
            end)
            swarm:connect_all()
        "#
        .to_string();
        let start = std::time::Instant::now();
        let result = run_swarm(quick_config(script, Duration::from_secs(30))).await;
        let elapsed = start.elapsed();
        match result {
            Err(RuntimeError::WorkerFailed {
                worker_index,
                reason,
            }) => {
                assert_ne!(
                    worker_index, 0,
                    "the failure must be attributed to the non-coordinator worker, not worker 0"
                );
                assert!(
                    reason.contains("boom from a non-coordinator worker"),
                    "reason was: {reason}"
                );
            }
            Ok(_) => panic!("expected WorkerFailed, got Ok"),
            Err(other) => panic!("expected WorkerFailed, got {other}"),
        }
        assert!(
            elapsed < Duration::from_secs(5),
            "must fail promptly: took {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_failed_startups_each_return_promptly_and_do_not_accumulate_state() {
        let script = r#"
            swarm:configure(function()
                swarm:add_server({name = "main", host = "127.0.0.1", port = 1})
            end)
        "#
        .to_string();
        let mut durations = Vec::new();
        for _ in 0..10 {
            let start = std::time::Instant::now();
            let result = run_swarm(quick_config(script.clone(), Duration::from_millis(150))).await;
            durations.push(start.elapsed());
            assert!(matches!(result, Err(RuntimeError::StartupTimeout(_))));
        }
        // `abort_startup` joins every worker task before `run_swarm`
        // returns on *every* error path (see its doc comment) — so there
        // is no leaked-thread state to accumulate by construction. This is
        // a black-box confirmation of that: if worker tasks/threads were
        // piling up across iterations, later calls would tend to get
        // slower under the growing resource pressure. Generous margin —
        // this is a leak smoke check, not a precise benchmark.
        let first = durations.first().copied().unwrap();
        let last = durations.last().copied().unwrap();
        assert!(
            last < first * 3 + Duration::from_secs(1),
            "iterations should not progressively slow down (leak smoke check): first={first:?} last={last:?} all={durations:?}"
        );
    }

    // ---- `collect_startup_reports` unit tests: no Lua, no network -------

    /// `collect_startup_reports`'s `worker_tasks.join_next() => None` arm
    /// intentionally awaits `std::future::pending()` — safe in production
    /// only because `run_swarm` always spawns `worker_count` real tasks
    /// into the `JoinSet` before calling this function (so `join_next()`
    /// never sees a genuinely empty set until a task has already resolved
    /// and returned) and wraps the whole call in an outer
    /// `tokio::time::timeout`. A unit test calling this function directly
    /// with a still-empty `JoinSet` would hit that `None` arm immediately
    /// and hang forever, bypassing both guarantees — so every test below
    /// spawns one never-resolving placeholder task per worker (mirroring a
    /// real worker thread that is still running) instead of leaving the
    /// set empty.
    fn spawn_placeholder_worker_tasks(
        tasks: &mut tokio::task::JoinSet<(usize, Result<WorkerReport, String>)>,
        worker_count: usize,
    ) {
        for _ in 0..worker_count {
            tasks.spawn(std::future::pending());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collect_startup_reports_succeeds_once_every_worker_reports_reached_barrier() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tasks: tokio::task::JoinSet<(usize, Result<WorkerReport, String>)> =
            tokio::task::JoinSet::new();
        spawn_placeholder_worker_tasks(&mut tasks, 2);
        tx.send((0, WorkerStartupReport::ReachedBarrier)).unwrap();
        tx.send((1, WorkerStartupReport::ReachedBarrier)).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            collect_startup_reports(2, &mut rx, &mut tasks),
        )
        .await
        .expect("must not hang");
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collect_startup_reports_fails_immediately_on_a_failed_report() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tasks: tokio::task::JoinSet<(usize, Result<WorkerReport, String>)> =
            tokio::task::JoinSet::new();
        spawn_placeholder_worker_tasks(&mut tasks, 2);
        tx.send((0, WorkerStartupReport::ReachedBarrier)).unwrap();
        tx.send((1, WorkerStartupReport::Failed("boom".to_string())))
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            collect_startup_reports(2, &mut rx, &mut tasks),
        )
        .await
        .expect("must not hang");
        match result {
            Err(RuntimeError::WorkerFailed {
                worker_index,
                reason,
            }) => {
                assert_eq!(worker_index, 1);
                assert_eq!(reason, "boom");
            }
            other => panic!("expected WorkerFailed, got {other:?}"),
        }
    }

    /// Proves a genuine Rust panic inside a worker task — not just a
    /// script/sandbox `Err` — is caught and treated as a startup failure,
    /// never left to hang `collect_startup_reports` forever.
    #[tokio::test(flavor = "multi_thread")]
    async fn collect_startup_reports_treats_a_panicking_worker_task_as_failed() {
        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(usize, WorkerStartupReport)>();
        let mut tasks: tokio::task::JoinSet<(usize, Result<WorkerReport, String>)> =
            tokio::task::JoinSet::new();
        tasks.spawn(async { panic!("simulated worker panic") });
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            collect_startup_reports(1, &mut rx, &mut tasks),
        )
        .await
        .expect("must not hang");
        match result {
            Err(RuntimeError::Worker(reason)) => {
                assert!(
                    reason.contains("could not be joined"),
                    "reason was: {reason}"
                );
            }
            other => panic!("expected Worker(..), got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collect_startup_reports_treats_an_early_clean_exit_as_failed() {
        let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(usize, WorkerStartupReport)>();
        let mut tasks: tokio::task::JoinSet<(usize, Result<WorkerReport, String>)> =
            tokio::task::JoinSet::new();
        tasks.spawn(async { (0usize, Ok(empty_report())) });
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            collect_startup_reports(1, &mut rx, &mut tasks),
        )
        .await
        .expect("must not hang");
        match result {
            Err(RuntimeError::WorkerFailed { worker_index, .. }) => assert_eq!(worker_index, 0),
            other => panic!("expected WorkerFailed, got {other:?}"),
        }
    }

    #[test]
    fn panic_message_extracts_str_and_string_payloads_and_falls_back_otherwise() {
        let str_payload: Box<dyn std::any::Any + Send> = Box::new("static str panic");
        assert_eq!(panic_message(&*str_payload), "static str panic");
        let string_payload: Box<dyn std::any::Any + Send> =
            Box::new("owned string panic".to_string());
        assert_eq!(panic_message(&*string_payload), "owned string panic");
        let other_payload: Box<dyn std::any::Any + Send> = Box::new(42i32);
        assert_eq!(
            panic_message(&*other_payload),
            "worker thread panicked with a non-string payload"
        );
    }

    // ---- `spawn_all_bots` proxy-resolution-failure unit test -------------

    /// Simulates the registry/proxy_profiles invariant being violated
    /// (should never happen through the normal Lua-driven path, since
    /// `SwarmRegistryBuilder` validates every bot's `proxy` field against
    /// this exact same profile-id set at `add_bot` time) — this proves
    /// `spawn_all_bots` itself defends against it directly, atomically
    /// (no supervisor is created for *any* bot when validation fails for
    /// one).
    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_all_bots_fails_atomically_on_an_unresolvable_proxy_profile() {
        let mut registry = SwarmRegistry::default();
        registry.servers.insert(
            "main".to_string(),
            ServerDef {
                name: "main".to_string(),
                host: "127.0.0.1".to_string(),
                port: 1,
                ..ServerDef::default()
            },
        );
        registry.bots.insert(
            0,
            BotDef {
                id: 0,
                username: "Bot0".to_string(),
                server: "main".to_string(),
                proxy: Some("ghost".to_string()),
                reconnect: ReconnectPolicy::default(),
                label: None,
            },
        );
        let proxy_profiles = ProxyProfiles::new(); // "ghost" is not registered
        let queue = WorkerQueue::new(QueueDesign::Priority(PriorityQueue::new(8, 8)));
        let dispatcher = DispatcherHandle::new(vec![queue]);

        let result = spawn_all_bots(&registry, &proxy_profiles, &dispatcher).await;
        match result {
            Err(RuntimeError::Proxy(UnknownProxyProfile(id))) => assert_eq!(id, "ghost"),
            Ok((handles, _, _)) => panic!(
                "expected Proxy(UnknownProxyProfile), got Ok with {} bot handle(s)",
                handles.len()
            ),
            Err(other) => panic!("expected Proxy(UnknownProxyProfile), got {other}"),
        }
    }
}
