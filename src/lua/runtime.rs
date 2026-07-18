//! Top-level orchestrator: spawns the fixed worker pool, waits for the
//! coordinator's configuration phase, resolves proxies, spawns every bot's
//! `ClientSupervisor`, bridges each bot's `BotEvent` stream into the
//! dispatcher, and drives graceful shutdown.
//!
//! This is the async, tokio-owned half of the runtime; `crate::lua::worker`
//! is the sync, one-`Lua`-VM-per-OS-thread half. The two meet at
//! [`worker::StartupBarrier`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::core::client::ClientConfig;
use crate::core::supervisor::{ClientSupervisor, ReconnectPolicy, SupervisorHandle};
use crate::lua::api::shared::SharedState;
use crate::lua::dispatcher::{DispatcherHandle, WorkerQueue};
use crate::lua::queue::{PriorityQueue, QueueDesign};
use crate::lua::registry::SwarmRegistry;
use crate::lua::sandbox::SandboxConfig;
use crate::lua::worker::{run_worker, StartupBarrier, StartupPayload, WorkerConfig, WorkerReport};

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

#[derive(Clone)]
pub struct SwarmRuntimeConfig {
    pub worker_count: usize,
    pub sandbox: SandboxConfig,
    pub high_queue_capacity: usize,
    pub low_queue_capacity: usize,
    pub callback_timeout: Duration,
    pub script_body: String,
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
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("script error: {0}")]
    Script(String),
    #[error("proxy resolution failed: {0}")]
    Proxy(#[from] crate::lua::registry::ProxyResolveError),
    #[error("worker startup failed: {0}")]
    Worker(String),
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
    worker_threads: Vec<std::thread::JoinHandle<Result<WorkerReport, String>>>,
    supervisor_tasks: tokio::task::JoinSet<()>,
    bridge_tasks: tokio::task::JoinSet<()>,
}

/// Spawns the worker pool, waits for the coordinator to finish
/// `swarm:configure(fn)` and call `swarm:connect_all()`, then connects
/// every bot. Must be called from within a tokio runtime.
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

    let mut worker_threads = Vec::with_capacity(worker_count);
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
            config_tx: if is_coordinator { Some(config_tx.clone()) } else { None },
            shutdown: shutdown.clone(),
            callback_timeout: config.callback_timeout,
        };
        let queue = queue.clone();
        let script = config.script_body.clone();
        let handle = std::thread::Builder::new()
            .name(format!("lua-worker-{worker_index}"))
            .spawn(move || run_worker(worker_config, queue, &script))
            .map_err(|e| RuntimeError::Worker(e.to_string()))?;
        worker_threads.push(handle);
    }
    drop(config_tx);

    // The coordinator's `swarm:connect_all()` sends the finalized registry
    // from its own dedicated OS thread; `spawn_blocking` lets this async
    // task wait on that plain `std::sync::mpsc` receiver without blocking
    // a tokio worker thread.
    let registry = tokio::task::spawn_blocking(move || config_rx.recv())
        .await
        .map_err(|e| RuntimeError::Worker(e.to_string()))?
        .map_err(|_| {
            RuntimeError::Script(
                "no worker ever called swarm:connect_all() as the coordinator (worker 0)".to_string(),
            )
        })?;

    let (bot_handles, supervisor_tasks, bridge_tasks) = spawn_all_bots(&registry, &dispatcher).await?;
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
        worker_threads,
        supervisor_tasks,
        bridge_tasks,
    })
}

async fn spawn_all_bots(
    registry: &SwarmRegistry,
    dispatcher: &DispatcherHandle,
) -> Result<
    (
        HashMap<u32, SupervisorHandle>,
        tokio::task::JoinSet<()>,
        tokio::task::JoinSet<()>,
    ),
    RuntimeError,
> {
    let mut resolved_proxies = HashMap::new();
    for (name, proxy) in &registry.proxies {
        resolved_proxies.insert(name.clone(), proxy.resolve()?);
    }

    let mut bot_handles = HashMap::new();
    let mut supervisor_tasks = tokio::task::JoinSet::new();
    let mut bridge_tasks = tokio::task::JoinSet::new();

    for (bot_id, bot_def) in &registry.bots {
        let server = registry.servers.get(&bot_def.server).ok_or_else(|| {
            RuntimeError::Script(format!(
                "bot {bot_id} references unknown server `{}`",
                bot_def.server
            ))
        })?;
        let mut cfg = ClientConfig::new(server.host.clone(), server.port, bot_def.username.clone())
            .with_view_distance(server.view_distance)
            .with_write_timeout(server.write_timeout)
            .with_connect_deadline(server.connect_deadline)
            .with_chunk_sharing(server.shared_chunks);
        if let Some(proxy_name) = &bot_def.proxy {
            if let Some(proxy_cfg) = resolved_proxies.get(proxy_name) {
                cfg = cfg.with_socks5_proxy(proxy_cfg.clone());
            }
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
    /// then join every worker thread.
    pub async fn shutdown(mut self, timeout: Duration) {
        self.shutdown.store(true, Ordering::Release);
        for handle in self.bot_handles.values() {
            handle.stop();
        }
        self.dispatcher.close_all();

        let _ = tokio::time::timeout(timeout, async {
            while self.supervisor_tasks.join_next().await.is_some() {}
            while self.bridge_tasks.join_next().await.is_some() {}
        })
        .await;

        for handle in self.worker_threads {
            let _ = tokio::task::spawn_blocking(move || handle.join()).await;
        }
    }
}
