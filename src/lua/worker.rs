//! One persistent Lua worker: one `Lua` VM, one dedicated OS thread, one
//! bounded inbound queue. Never a VM per event, never a task per event,
//! never concurrent Lua execution against the same VM — the loop below
//! processes exactly one envelope's handlers to completion before looking
//! at the next.
//!
//! Worker 0 is the coordinator: only it actually executes the callback
//! passed to `swarm:configure(fn)` (see `crate::lua::registry`). Every
//! worker, coordinator included, registers its own handlers via
//! `swarm:on`/`bot:on` and runs its own copy of the loaded script — Lua
//! globals are therefore worker-local by construction (see
//! `docs/lua_wrapper.md#worker-local-globals`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use mlua::{Lua, RegistryKey};

use crate::core::supervisor::SupervisorHandle;
use crate::lua::dispatcher::{ActionOutcome, DispatcherHandle, WorkerQueue};
use crate::lua::event::WorkItem;
use crate::lua::queue::QueueItem;
use crate::lua::registry::{SwarmRegistry, SwarmRegistryBuilder};
use crate::lua::sandbox::{new_sandboxed_lua, SandboxConfig};

/// Everything the top-level runtime learns only once the coordinator's
/// `configure` callback has run and every bot has been connected — shared,
/// read-only, with every worker after the startup barrier releases.
pub struct StartupPayload {
    pub registry: Arc<SwarmRegistry>,
    pub bot_handles: Arc<HashMap<u32, SupervisorHandle>>,
}

/// A one-time synchronization point: the coordinator publishes the
/// finalized registry + bot handles exactly once; every worker (including
/// the coordinator itself) blocks in `swarm:connect_all()` until it's
/// available. This is the "wait at a startup barrier, then begin
/// connecting" step the two-phase script-loading model requires.
pub struct StartupBarrier {
    state: Mutex<Option<Arc<StartupPayload>>>,
    condvar: Condvar,
}

impl StartupBarrier {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(None),
            condvar: Condvar::new(),
        })
    }

    pub fn wait(&self) -> Arc<StartupPayload> {
        let mut guard = self.state.lock().expect("startup barrier poisoned");
        while guard.is_none() {
            guard = self.condvar.wait(guard).expect("startup barrier poisoned");
        }
        guard.clone().expect("checked Some above")
    }

    /// Non-blocking poll used by `swarm:status()` before the barrier opens.
    pub fn peek(&self) -> Option<Arc<StartupPayload>> {
        self.state.lock().expect("startup barrier poisoned").clone()
    }

    pub fn publish(&self, payload: Arc<StartupPayload>) {
        let mut guard = self.state.lock().expect("startup barrier poisoned");
        *guard = Some(payload);
        self.condvar.notify_all();
    }
}

/// One registered handler. `key` is `Rc`-wrapped since `mlua::RegistryKey`
/// itself is not `Clone`, but callers need to collect a snapshot of
/// `(id, key, once)` tuples while holding only a `Ref`, then drop the
/// borrow before invoking any of them (a handler can register/unregister
/// more handlers, which would otherwise conflict with an outstanding
/// `RefCell` borrow).
pub struct Handler {
    pub id: u64,
    pub key: Rc<RegistryKey>,
    pub once: bool,
}

/// Per-worker handler storage. `swarm:on(name, fn)` registers into
/// `global`; `bot:on(name, fn)` registers into `per_bot`, keyed by the
/// bot's numeric id (only bots assigned to this worker will ever have
/// matching events dispatched here, but nothing stops a script from
/// registering for a bot on the wrong worker — it will simply never fire,
/// since that bot's events never reach this queue).
#[derive(Default)]
pub struct HandlerRegistry {
    pub global: HashMap<&'static str, Vec<Handler>>,
    pub per_bot: HashMap<(u32, &'static str), Vec<Handler>>,
    pub next_id: u64,
}

impl HandlerRegistry {
    pub fn register_global(&mut self, name: &'static str, key: RegistryKey, once: bool) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.global.entry(name).or_default().push(Handler {
            id,
            key: Rc::new(key),
            once,
        });
        id
    }

    pub fn register_bot(
        &mut self,
        bot_id: u32,
        name: &'static str,
        key: RegistryKey,
        once: bool,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.per_bot
            .entry((bot_id, name))
            .or_default()
            .push(Handler {
                id,
                key: Rc::new(key),
                once,
            });
        id
    }

    pub fn remove(&mut self, id: u64) -> bool {
        for handlers in self.global.values_mut() {
            if let Some(pos) = handlers.iter().position(|h| h.id == id) {
                handlers.remove(pos);
                return true;
            }
        }
        for handlers in self.per_bot.values_mut() {
            if let Some(pos) = handlers.iter().position(|h| h.id == id) {
                handlers.remove(pos);
                return true;
            }
        }
        false
    }
}

pub struct PendingCallback {
    pub key: RegistryKey,
    pub bot_id: u32,
    pub registered_at: Instant,
}

#[derive(Default)]
pub struct CallbackRegistry {
    pub pending: HashMap<u64, PendingCallback>,
}

pub const MAX_PENDING_CALLBACKS: usize = 4096;
pub const DEFAULT_CALLBACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything one worker's Lua closures need to reach outside the VM.
/// Confined to a single OS thread by construction, so interior mutability
/// uses `RefCell`, not `Mutex` — the `Arc`-wrapped fields are the only
/// state that legitimately crosses threads (they're handles into
/// cross-worker/async machinery, never the `Lua` VM itself).
pub struct WorkerState {
    pub worker_index: usize,
    pub is_coordinator: bool,
    pub dispatcher: DispatcherHandle,
    pub runtime_handle: tokio::runtime::Handle,
    pub instruction_counter: RefCell<Arc<AtomicU64>>,
    pub sandbox: SandboxConfig,
    pub handlers: RefCell<HandlerRegistry>,
    pub callbacks: RefCell<CallbackRegistry>,
    pub disabled_bots: RefCell<HashSet<u32>>,
    pub consecutive_errors: RefCell<HashMap<u32, u32>>,
    pub config_builder: RefCell<SwarmRegistryBuilder>,
    pub config_tx: RefCell<Option<std::sync::mpsc::SyncSender<SwarmRegistry>>>,
    pub startup_barrier: Arc<StartupBarrier>,
    pub started: RefCell<Option<Arc<StartupPayload>>>,
    pub shared_state: Arc<crate::lua::api::shared::SharedState>,
    pub pubsub: RefCell<HashMap<String, Vec<Handler>>>,
    pub timers: RefCell<crate::lua::api::timers::TimerRegistry>,
    pub shutdown: Arc<std::sync::atomic::AtomicBool>,
    pub callback_timeout: Duration,
}

impl WorkerState {
    pub fn my_bot_ids(&self) -> Vec<u32> {
        match self.started.borrow().as_ref() {
            Some(payload) => payload
                .registry
                .bots
                .keys()
                .copied()
                .filter(|id| self.dispatcher.worker_index_for(*id) == self.worker_index)
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn bot_handle(&self, bot_id: u32) -> Option<SupervisorHandle> {
        self.started
            .borrow()
            .as_ref()
            .and_then(|p| p.bot_handles.get(&bot_id).cloned())
    }

    pub fn registry(&self) -> Option<Arc<SwarmRegistry>> {
        self.started.borrow().as_ref().map(|p| p.registry.clone())
    }
}

pub struct WorkerReport {
    pub events_processed: u64,
    pub handlers_run: u64,
    pub handler_errors: u64,
    pub bots_disabled_by_consecutive_errors: u64,
}

/// Everything needed to build a [`WorkerState`], as plain `Send` data —
/// `Rc<WorkerState>` itself cannot cross the `std::thread::spawn` boundary
/// (`Rc` is not `Send`), so each worker thread builds its own `WorkerState`
/// from one of these immediately after starting, rather than receiving an
/// already-constructed one.
pub struct WorkerConfig {
    pub worker_index: usize,
    pub is_coordinator: bool,
    pub dispatcher: DispatcherHandle,
    pub runtime_handle: tokio::runtime::Handle,
    pub sandbox: SandboxConfig,
    pub startup_barrier: Arc<StartupBarrier>,
    pub shared_state: Arc<crate::lua::api::shared::SharedState>,
    pub config_tx: Option<std::sync::mpsc::SyncSender<SwarmRegistry>>,
    pub shutdown: Arc<std::sync::atomic::AtomicBool>,
    pub callback_timeout: Duration,
}

/// Runs one worker to completion: loads `script_body` (defining `configure`
/// on the coordinator only, and `on`-handlers on every worker), then loops
/// on its queue until closed.
pub fn run_worker(
    config: WorkerConfig,
    queue: Arc<WorkerQueue>,
    script_body: &str,
) -> Result<WorkerReport, String> {
    let (lua, counter) =
        new_sandboxed_lua(&config.sandbox).map_err(|e| format!("sandbox init failed: {e}"))?;

    let state = Rc::new(WorkerState {
        worker_index: config.worker_index,
        is_coordinator: config.is_coordinator,
        dispatcher: config.dispatcher,
        runtime_handle: config.runtime_handle,
        instruction_counter: RefCell::new(counter.clone()),
        sandbox: config.sandbox,
        handlers: RefCell::new(HandlerRegistry::default()),
        callbacks: RefCell::new(CallbackRegistry::default()),
        disabled_bots: RefCell::new(HashSet::new()),
        consecutive_errors: RefCell::new(HashMap::new()),
        config_builder: RefCell::new(SwarmRegistryBuilder::new()),
        config_tx: RefCell::new(config.config_tx),
        startup_barrier: config.startup_barrier,
        started: RefCell::new(None),
        shared_state: config.shared_state,
        pubsub: RefCell::new(HashMap::new()),
        timers: RefCell::new(crate::lua::api::timers::TimerRegistry::default()),
        shutdown: config.shutdown,
        callback_timeout: config.callback_timeout,
    });

    crate::lua::api::install(&lua, state.clone())
        .map_err(|e| format!("api install failed: {e}"))?;

    lua.load(script_body)
        .set_name("swarm_script")
        .exec()
        .map_err(|e| format!("script load/top-level exec failed: {e}"))?;

    let mut report = WorkerReport {
        events_processed: 0,
        handlers_run: 0,
        handler_errors: 0,
        bots_disabled_by_consecutive_errors: 0,
    };

    loop {
        if state.shutdown.load(Ordering::Acquire) {
            queue.close();
        }
        let batch = queue.wait_for_batch();
        if batch.is_empty() && state.shutdown.load(Ordering::Acquire) {
            break;
        }
        for envelope in batch {
            report.events_processed += 1;
            counter.store(0, Ordering::Relaxed);
            dispatch_one(&lua, &state, envelope, &mut report);
        }
        sweep_callback_timeouts(&lua, &state);
        crate::lua::api::timers::fire_due(&lua, &state, &mut report);
    }

    Ok(report)
}

fn dispatch_one(
    lua: &Lua,
    state: &Rc<WorkerState>,
    envelope: crate::lua::queue::Envelope<WorkItem>,
    report: &mut WorkerReport,
) {
    let bot_id = envelope.bot_id.0;
    let name = envelope.event.name();

    match envelope.event {
        WorkItem::Bot(event) => {
            if state.disabled_bots.borrow().contains(&bot_id) {
                return;
            }
            let event_table = match crate::lua::convert::events::event_to_table(lua, &event) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(worker = state.worker_index, %bot_id, error = %e, "failed to convert event to Lua table");
                    return;
                }
            };
            let bot_obj = crate::lua::api::bot::make_bot(lua, state.clone(), bot_id);
            let bot_value = match bot_obj {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(worker = state.worker_index, %bot_id, error = %e, "failed to build bot object");
                    return;
                }
            };
            run_handlers_for(
                lua,
                state,
                bot_id,
                name,
                bot_value,
                mlua::Value::Table(event_table),
                report,
            );
        }
        WorkItem::ActionResult(result) => {
            let table = crate::lua::convert::events::action_result_to_table(lua, &result);
            if let Ok(table) = table {
                if let Some(callback) = state
                    .callbacks
                    .borrow_mut()
                    .pending
                    .remove(&result.request_id)
                {
                    if let Ok(func) = lua.registry_value::<mlua::Function>(&callback.key) {
                        if let Err(e) = func.call::<()>(table.clone()) {
                            tracing::warn!(worker = state.worker_index, error = %e, "action_result callback failed");
                        }
                    }
                    let _ = lua.remove_registry_value(callback.key);
                }
                let bot_obj = crate::lua::api::bot::make_bot(lua, state.clone(), bot_id).ok();
                if let Some(bot_value) = bot_obj {
                    run_handlers_for(
                        lua,
                        state,
                        bot_id,
                        "action_result",
                        bot_value,
                        mlua::Value::Table(table),
                        report,
                    );
                }
            }
        }
        WorkItem::Message { topic, payload } => {
            let funcs: Vec<mlua::Function> = {
                let reg = state.pubsub.borrow();
                reg.get(&topic)
                    .map(|hs| {
                        hs.iter()
                            .filter_map(|h| lua.registry_value::<mlua::Function>(&h.key).ok())
                            .collect()
                    })
                    .unwrap_or_default()
            };
            if funcs.is_empty() {
                return;
            }
            if let Ok(value) = crate::lua::api::shared::shared_value_to_lua(lua, &payload) {
                for func in funcs {
                    report.handlers_run += 1;
                    if let Err(e) = func.call::<()>((topic.clone(), value.clone())) {
                        report.handler_errors += 1;
                        tracing::warn!(worker = state.worker_index, %topic, error = %e, "on_message handler failed");
                    }
                }
            }
        }
        WorkItem::TimerFired { timer_id } => {
            crate::lua::api::timers::invoke(lua, state, timer_id, report);
        }
        WorkItem::ScriptError {
            event_name,
            message,
        } => {
            tracing::warn!(worker = state.worker_index, %bot_id, event = event_name, %message, "script_error");
        }
        WorkItem::ScriptDisabled { reason } => {
            tracing::warn!(worker = state.worker_index, %bot_id, %reason, "script_disabled");
        }
        WorkItem::WorkerOverloaded => {
            tracing::error!(
                worker = state.worker_index,
                "worker_overload: critical queue saturated"
            );
        }
    }
}

fn run_handlers_for(
    lua: &Lua,
    state: &Rc<WorkerState>,
    bot_id: u32,
    name: &'static str,
    bot_value: mlua::Value,
    event_value: mlua::Value,
    report: &mut WorkerReport,
) {
    let mut to_remove = Vec::new();
    let per_bot_keys: Vec<(u64, Rc<RegistryKey>, bool)> = {
        let reg = state.handlers.borrow();
        reg.per_bot
            .get(&(bot_id, name))
            .map(|hs| hs.iter().map(|h| (h.id, h.key.clone(), h.once)).collect())
            .unwrap_or_default()
    };
    let global_keys: Vec<(u64, Rc<RegistryKey>, bool)> = {
        let reg = state.handlers.borrow();
        reg.global
            .get(name)
            .map(|hs| hs.iter().map(|h| (h.id, h.key.clone(), h.once)).collect())
            .unwrap_or_default()
    };

    let mut had_error = false;
    for (id, key, once) in per_bot_keys.into_iter().chain(global_keys) {
        if let Ok(func) = lua.registry_value::<mlua::Function>(&key) {
            report.handlers_run += 1;
            if let Err(e) = func.call::<()>((bot_value.clone(), event_value.clone())) {
                report.handler_errors += 1;
                had_error = true;
                tracing::warn!(worker = state.worker_index, %bot_id, event = name, error = %e, "handler error");
            }
        }
        if once {
            to_remove.push(id);
        }
    }
    if !to_remove.is_empty() {
        let mut reg = state.handlers.borrow_mut();
        for id in to_remove {
            reg.remove(id);
        }
    }

    if had_error {
        let mut errors = state.consecutive_errors.borrow_mut();
        let count = errors.entry(bot_id).or_insert(0);
        *count += 1;
        if *count >= state.sandbox.consecutive_error_threshold {
            state.disabled_bots.borrow_mut().insert(bot_id);
            report.bots_disabled_by_consecutive_errors += 1;
            tracing::error!(worker = state.worker_index, %bot_id, "script disabled after {count} consecutive handler errors");
        }
    } else {
        state.consecutive_errors.borrow_mut().remove(&bot_id);
    }
}

fn sweep_callback_timeouts(lua: &Lua, state: &Rc<WorkerState>) {
    let now = Instant::now();
    let timeout = state.callback_timeout;
    let expired: Vec<u64> = {
        let pending = &state.callbacks.borrow().pending;
        pending
            .iter()
            .filter(|(_, cb)| now.duration_since(cb.registered_at) > timeout)
            .map(|(id, _)| *id)
            .collect()
    };
    for request_id in expired {
        if let Some(cb) = state.callbacks.borrow_mut().pending.remove(&request_id) {
            if let Ok(func) = lua.registry_value::<mlua::Function>(&cb.key) {
                let outcome = ActionOutcome::Error(crate::lua::error::ScriptError::new(
                    "inventory_timeout",
                    "action callback timed out waiting for a result",
                ));
                let result = crate::lua::dispatcher::ActionResult {
                    request_id,
                    bot_id: cb.bot_id,
                    outcome,
                };
                if let Ok(table) = crate::lua::convert::events::action_result_to_table(lua, &result)
                {
                    let _ = func.call::<()>(table);
                }
            }
            let _ = lua.remove_registry_value(cb.key);
        }
    }
}
