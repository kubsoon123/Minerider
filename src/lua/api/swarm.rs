//! `LuaSwarm`: the `swarm` global every worker's script sees.
//!
//! `configure`/`add_server`/`add_proxy`/`add_bot`/`add_group` only take
//! effect on the coordinator (worker 0) — see the module doc comment on
//! `crate::lua::worker`. `connect_all` is the two-phase model's explicit
//! synchronization point: on the coordinator it finalizes the registry and
//! hands it to the async orchestrator (`crate::lua::runtime`); on every
//! worker (coordinator included) it then blocks on the startup barrier
//! until that orchestrator has spawned every bot's supervisor and
//! published the shared bot-handle map.

use std::rc::Rc;

use mlua::{Lua, Table, UserData, UserDataFields, UserDataMethods, Value};

use crate::lua::error::ScriptError;
use crate::lua::registry::BotSpec;
use crate::lua::worker::WorkerState;

/// Maximum bytes for one pub/sub topic name — `publish`/`on_message`
/// otherwise accept an unbounded `String` directly as a `HashMap` key
/// (`WorkerState::pubsub`) with no length check at all.
const MAX_TOPIC_LEN: usize = 256;

#[derive(Clone)]
pub struct LuaSwarm {
    pub state: Rc<WorkerState>,
}

pub fn install(lua: &Lua, state: Rc<WorkerState>) -> mlua::Result<()> {
    lua.globals().set("swarm", LuaSwarm { state })?;
    Ok(())
}

impl UserData for LuaSwarm {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("shared", |_, this| {
            Ok(super::shared::LuaSharedHandle {
                shared: this.state.shared_state.clone(),
            })
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // ---- Configuration phase (coordinator-only; see module docs) --
        methods.add_method("configure", |_, this, func: mlua::Function| {
            if this.state.is_coordinator {
                func.call::<()>(())?;
            }
            Ok(())
        });

        methods.add_method("add_server", |lua, this, table: Table| {
            let def = super::config::parse_server_def(&table)?;
            match this.state.config_builder.borrow_mut().add_server(def) {
                Ok(()) => Ok((Value::Boolean(true), Value::Nil)),
                Err(e) => super::errors::err_pair(lua, e.into()),
            }
        });

        // There is deliberately no way for a script to define a proxy:
        // proxy endpoints and credentials are host-supplied profiles (see
        // `crate::lua::registry`'s module doc comment) — a script may only
        // *reference* a profile id via `add_bot`/`add_group`'s `proxy`
        // field. `add_proxy` is kept as a callable method (rather than
        // simply absent) so a script written against the old API gets a
        // clear, typed explanation instead of a raw "attempt to call a nil
        // value".
        methods.add_method("add_proxy", |lua, _this, _table: Table| {
            super::errors::err_pair(
                lua,
                ScriptError::new(
                    "invalid_configuration",
                    "proxies are configured by the host, not by scripts; reference a profile id \
                     via add_bot/add_group's `proxy` field instead",
                ),
            )
        });

        methods.add_method("add_bot", |lua, this, table: Table| {
            let spec = super::config::parse_bot_spec(&table)?;
            match this.state.config_builder.borrow_mut().add_bot(spec) {
                Ok(id) => Ok((Value::Integer(id as i64), Value::Nil)),
                Err(e) => super::errors::err_pair(lua, e.into()),
            }
        });

        methods.add_method("add_group", |lua, this, table: Table| {
            let name: String = table.get("name").unwrap_or_default();
            // Checked *before* any allocation sized off it: mlua's own
            // `u32` conversion already rejects a negative/out-of-range
            // Lua number (see `crate::lua::api::config::checked_u32_from_value`'s
            // doc comment for the general pattern), but a merely
            // *huge-but-valid* `u32` (billions) must still be rejected
            // here rather than reaching `Vec::with_capacity` — the same
            // `MAX_BOTS_PER_GROUP` bound `add_group` itself enforces, just
            // checked early enough to also bound the allocation.
            let count: u32 = table.get("count").unwrap_or(0);
            if count as usize > crate::lua::registry::MAX_BOTS_PER_GROUP {
                return super::errors::err_pair(
                    lua,
                    ScriptError::new(
                        "invalid_configuration",
                        format!(
                            "group `{name}` requested {count} bots, exceeding the {} limit",
                            crate::lua::registry::MAX_BOTS_PER_GROUP
                        ),
                    ),
                );
            }
            let id_prefix: Option<String> = table.get("id_prefix").ok();
            let username_prefix: String = table.get("username_prefix").unwrap_or_default();
            let server: String = table.get("server").unwrap_or_default();
            let proxy: Option<String> = table.get("proxy").ok();
            let reconnect_table = match table.get::<Value>("reconnect")? {
                Value::Table(t) => Some(t),
                _ => None,
            };
            let reconnect = super::config::parse_reconnect_policy(reconnect_table.as_ref())?;

            let mut ids = Vec::with_capacity(count as usize);
            for i in 0..count {
                let username = format!("{username_prefix}{i}");
                let label = id_prefix.as_ref().map(|p| format!("{p}{i}"));
                let spec = BotSpec {
                    id: None,
                    username,
                    server: server.clone(),
                    proxy: proxy.clone(),
                    reconnect: reconnect.clone(),
                    label,
                };
                // Bound to a `let` first, not matched directly on the
                // `borrow_mut()` expression: a temporary created in a
                // `match` scrutinee stays alive for the *whole* match
                // (every arm's body), so matching directly here would
                // leave `config_builder` borrowed while the `Err` arm
                // below tries to borrow it again to roll back — a runtime
                // "already borrowed" panic, not a compile error.
                let add_result = this.state.config_builder.borrow_mut().add_bot(spec);
                match add_result {
                    Ok(id) => ids.push(id),
                    Err(e) => {
                        // All-or-nothing: a bot that failed partway
                        // through this batch must never leave the
                        // successfully-added ones behind as orphaned,
                        // ungrouped bots — roll every one of them back
                        // before reporting the error.
                        let mut builder = this.state.config_builder.borrow_mut();
                        for id in &ids {
                            builder.remove_bot(*id);
                        }
                        drop(builder);
                        return super::errors::err_pair(lua, e.into());
                    }
                }
            }
            let group_result = this
                .state
                .config_builder
                .borrow_mut()
                .add_group(name, ids.clone());
            match group_result {
                Ok(()) => Ok((Value::Boolean(true), Value::Nil)),
                Err(e) => {
                    // `add_group` itself can still fail (duplicate group
                    // name, too many groups) after every bot was
                    // successfully added — roll those back too, for the
                    // same all-or-nothing guarantee.
                    let mut builder = this.state.config_builder.borrow_mut();
                    for id in &ids {
                        builder.remove_bot(*id);
                    }
                    drop(builder);
                    super::errors::err_pair(lua, e.into())
                }
            }
        });

        // ---- Lookups (registry is shared/global; any worker can look up
        // any bot/group, but events for a bot only ever reach *its own*
        // assigned worker's handlers) ------------------------------------
        methods.add_method("bot", |lua, this, id: u32| match this.state.registry() {
            Some(r) if r.bots.contains_key(&id) => {
                super::bot::make_bot(lua, this.state.clone(), id)
            }
            _ => Ok(Value::Nil),
        });
        methods.add_method("bots", |lua, this, ()| {
            let t = lua.create_table()?;
            if let Some(registry) = this.state.registry() {
                for (i, id) in registry.bots.keys().enumerate() {
                    t.set(i + 1, super::bot::make_bot(lua, this.state.clone(), *id)?)?;
                }
            }
            Ok(t)
        });
        methods.add_method("group", |lua, this, name: String| {
            match this.state.registry() {
                Some(r) => match r.groups.get(&name) {
                    Some(g) => super::group::make_group(
                        lua,
                        this.state.clone(),
                        g.name.clone(),
                        g.bot_ids.clone(),
                    ),
                    None => Ok(Value::Nil),
                },
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("groups", |lua, this, ()| {
            let t = lua.create_table()?;
            if let Some(registry) = this.state.registry() {
                for (i, g) in registry.groups.values().enumerate() {
                    t.set(
                        i + 1,
                        super::group::make_group(
                            lua,
                            this.state.clone(),
                            g.name.clone(),
                            g.bot_ids.clone(),
                        )?,
                    )?;
                }
            }
            Ok(t)
        });

        // ---- Startup barrier --------------------------------------------
        methods.add_method("connect_all", |_, this, ()| {
            if this.state.is_coordinator {
                let builder = std::mem::take(&mut *this.state.config_builder.borrow_mut());
                let registry = builder.build();
                if let Some(tx) = this.state.config_tx.borrow_mut().take() {
                    let _ = tx.send(registry);
                }
            }
            // Tell the async orchestrator this worker has reached the
            // barrier *before* blocking on it — this is what lets
            // `run_swarm` detect "the coordinator's script finished
            // without ever calling connect_all" and every other startup
            // failure mode promptly instead of waiting forever (see
            // `crate::lua::worker::WorkerStartupReport`).
            let _ = this.state.startup_report_tx.send((
                this.state.worker_index,
                crate::lua::worker::WorkerStartupReport::ReachedBarrier,
            ));
            match this.state.startup_barrier.wait() {
                crate::lua::worker::StartupOutcome::Started(payload) => {
                    *this.state.started.borrow_mut() = Some(payload);
                    Ok(())
                }
                crate::lua::worker::StartupOutcome::Aborted(reason) => Err(
                    mlua::Error::RuntimeError(format!("swarm startup aborted: {reason}")),
                ),
            }
        });

        methods.add_method("disconnect_all", |_, this, ()| {
            if let Some(payload) = this.state.started.borrow().as_ref() {
                for handle in payload.bot_handles.values() {
                    handle.stop();
                }
            }
            Ok(())
        });

        methods.add_method("stop", |_, this, ()| {
            if let Some(payload) = this.state.started.borrow().as_ref() {
                for handle in payload.bot_handles.values() {
                    handle.stop();
                }
            }
            this.state
                .shutdown
                .store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        });

        // No-op marker: the persistent per-worker dispatch loop (see
        // `crate::lua::worker::run_worker`) takes over once the top-level
        // script chunk returns — `run()` doesn't need to block Lua itself.
        methods.add_method("run", |_, _this, ()| Ok(()));

        methods.add_method("status", |lua, this, ()| {
            let t = lua.create_table()?;
            t.set("worker_count", this.state.dispatcher.worker_count())?;
            t.set("worker_index", this.state.worker_index)?;
            t.set("is_coordinator", this.state.is_coordinator)?;
            let started = this.state.started.borrow();
            t.set("started", started.is_some())?;
            t.set(
                "bot_count",
                started.as_ref().map(|p| p.registry.bots.len()).unwrap_or(0),
            )?;
            Ok(t)
        });
        methods.add_method("stats", |lua, this, ()| {
            let t = lua.create_table()?;
            t.set(
                "queue_depth_total",
                this.state.dispatcher.queue_depth_total(),
            )?;
            t.set(
                "queue_peak_depth_total",
                this.state.dispatcher.queue_peak_depth_total(),
            )?;
            t.set(
                "queue_dropped_total",
                this.state.dispatcher.queue_dropped_total(),
            )?;
            t.set(
                "queue_critical_overflow_total",
                this.state.dispatcher.queue_critical_overflow_total(),
            )?;
            Ok(t)
        });

        // ---- Events (worker-global; every worker registers its own copy
        // by running this same script — see `docs/lua_wrapper.md`) --------
        methods.add_method("on", |lua, this, (name, func): (String, mlua::Function)| {
            let name = super::intern_event_name(&name)?;
            let key = lua.create_registry_value(func)?;
            Ok(this
                .state
                .handlers
                .borrow_mut()
                .register_global(name, key, false))
        });
        methods.add_method(
            "once",
            |lua, this, (name, func): (String, mlua::Function)| {
                let name = super::intern_event_name(&name)?;
                let key = lua.create_registry_value(func)?;
                Ok(this
                    .state
                    .handlers
                    .borrow_mut()
                    .register_global(name, key, true))
            },
        );
        methods.add_method("off", |_, this, id: u64| {
            Ok(this.state.remove_handler_or_subscription(id))
        });

        // ---- Bounded cross-worker pub/sub --------------------------------
        methods.add_method("publish", |lua, this, (topic, payload): (String, Value)| {
            if topic.len() > MAX_TOPIC_LEN {
                return super::errors::err_pair(
                    lua,
                    ScriptError::new(
                        "invalid_configuration",
                        format!("topic exceeds the {MAX_TOPIC_LEN}-byte limit"),
                    ),
                );
            }
            match super::shared::lua_value_to_shared(&payload) {
                Ok(shared) => {
                    this.state.dispatcher.broadcast_message(topic, shared);
                    Ok((Value::Boolean(true), Value::Nil))
                }
                Err(e) => super::errors::err_pair(
                    lua,
                    ScriptError::new("invalid_configuration", e.to_string()),
                ),
            }
        });
        methods.add_method(
            "on_message",
            |lua, this, (topic, func): (String, mlua::Function)| {
                if topic.len() > MAX_TOPIC_LEN {
                    return Err(mlua::Error::RuntimeError(format!(
                        "topic exceeds the {MAX_TOPIC_LEN}-byte limit"
                    )));
                }
                let key = lua.create_registry_value(func)?;
                let id = {
                    let mut handlers = this.state.handlers.borrow_mut();
                    handlers.next_id += 1;
                    handlers.next_id
                };
                this.state
                    .pubsub
                    .borrow_mut()
                    .entry(topic)
                    .or_default()
                    .push(crate::lua::worker::Handler {
                        id,
                        key: key.into(),
                        once: false,
                    });
                Ok(id)
            },
        );

        // ---- Coordinator/global timers ----------------------------------
        // `swarm:set_timeout`/`set_interval` only actually arm on the
        // coordinator (worker 0): every worker runs this same script, so
        // without this gate a "global" timer would fire once per worker
        // instead of exactly once. On a non-coordinator worker this still
        // returns a valid, unique-looking id (so scripts don't need
        // worker-aware branching) but never schedules anything to fire.
        methods.add_method(
            "set_timeout",
            |lua, this, (delay_ms, func): (u64, mlua::Function)| {
                if !this.state.is_coordinator {
                    return Ok(0);
                }
                crate::lua::api::timers::schedule(
                    &this.state,
                    lua,
                    func,
                    std::time::Duration::from_millis(delay_ms),
                    None,
                    None,
                )
                .map_err(|e| mlua::Error::RuntimeError(e.to_string()))
            },
        );
        methods.add_method(
            "set_interval",
            |lua, this, (interval_ms, func): (u64, mlua::Function)| {
                if !this.state.is_coordinator {
                    return Ok(0);
                }
                crate::lua::api::timers::schedule(
                    &this.state,
                    lua,
                    func,
                    std::time::Duration::from_millis(interval_ms),
                    Some(std::time::Duration::from_millis(interval_ms)),
                    None,
                )
                .map_err(|e| mlua::Error::RuntimeError(e.to_string()))
            },
        );
        methods.add_method("clear_timer", |lua, this, id: u64| {
            if !this.state.is_coordinator {
                return Ok(false);
            }
            Ok(crate::lua::api::timers::clear(&this.state, lua, id))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua::dispatcher::{DispatcherHandle, WorkerQueue};
    use crate::lua::queue::{PriorityQueue, QueueDesign};
    use crate::lua::sandbox::{new_sandboxed_lua, SandboxConfig};
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::Arc;

    fn test_state() -> Rc<WorkerState> {
        let queue = WorkerQueue::new(QueueDesign::Priority(PriorityQueue::new(64, 64)));
        let dispatcher = DispatcherHandle::new(vec![queue]);
        Rc::new(WorkerState {
            worker_index: 0,
            is_coordinator: true,
            dispatcher,
            runtime_handle: tokio::runtime::Handle::current(),
            instruction_counter: RefCell::new(Arc::new(AtomicU64::new(0))),
            sandbox: SandboxConfig::default(),
            handlers: RefCell::new(crate::lua::worker::HandlerRegistry::default()),
            callbacks: RefCell::new(crate::lua::worker::CallbackRegistry::default()),
            disabled_bots: RefCell::new(HashSet::new()),
            consecutive_errors: RefCell::new(HashMap::new()),
            config_builder: RefCell::new(crate::lua::registry::SwarmRegistryBuilder::default()),
            config_tx: RefCell::new(None),
            startup_barrier: crate::lua::worker::StartupBarrier::new(),
            startup_report_tx: tokio::sync::mpsc::unbounded_channel().0,
            started: RefCell::new(None),
            shared_state: Arc::new(crate::lua::api::shared::SharedState::new()),
            pubsub: RefCell::new(HashMap::new()),
            timers: RefCell::new(crate::lua::api::timers::TimerRegistry::default()),
            shutdown: Arc::new(AtomicBool::new(false)),
            callback_timeout: crate::lua::worker::DEFAULT_CALLBACK_TIMEOUT,
            action_task_semaphore: Arc::new(tokio::sync::Semaphore::new(
                crate::lua::worker::MAX_CONCURRENT_ACTION_TASKS,
            )),
        })
    }

    fn install(state: &Rc<WorkerState>) -> mlua::Lua {
        let (lua, counter) = new_sandboxed_lua(&state.sandbox).unwrap();
        *state.instruction_counter.borrow_mut() = counter;
        crate::lua::api::install(&lua, state.clone()).unwrap();
        lua.load(r#"swarm:add_server({name = "main", host = "127.0.0.1", port = 25565})"#)
            .exec()
            .unwrap();
        lua
    }

    /// The core Fix 6 regression: `add_group`'s bulk bot creation must be
    /// all-or-nothing. A pre-existing bot named "grunt2" is set up to
    /// collide with the third bot `add_group`'s batch would otherwise
    /// generate, forcing the batch to fail partway through after two bots
    /// ("grunt0"/"grunt1") already succeeded — both must be rolled back,
    /// not left behind as orphaned, ungrouped bots.
    #[tokio::test]
    async fn add_group_rolls_back_every_bot_it_added_when_a_later_one_in_the_batch_fails() {
        let state = test_state();
        let lua = install(&state);
        lua.load(r#"swarm:add_bot({username = "grunt2", server = "main"})"#)
            .exec()
            .unwrap();

        let ok: bool = lua
            .load(
                r#"
                local ok, err = swarm:add_group({
                    name = "grunts",
                    count = 5,
                    username_prefix = "grunt",
                    server = "main",
                })
                return ok == true
                "#,
            )
            .eval()
            .unwrap();
        assert!(
            !ok,
            "the batch must fail (grunt2 collides with the pre-existing bot)"
        );

        // If "grunt0"/"grunt1" were genuinely rolled back (not merely
        // left registered under a group that was never created), each
        // must be freely re-addable as a brand new bot.
        for username in ["grunt0", "grunt1"] {
            let readded: bool = lua
                .load(format!(
                    r#"local id = swarm:add_bot({{username = "{username}", server = "main"}}) return id ~= nil"#
                ))
                .eval()
                .unwrap();
            assert!(
                readded,
                "{username} must have been rolled back by the failed batch"
            );
        }
    }

    #[tokio::test]
    async fn add_group_succeeds_normally_and_creates_every_requested_bot() {
        let state = test_state();
        let lua = install(&state);

        let ok: bool = lua
            .load(
                r#"
                local ok, err = swarm:add_group({
                    name = "grunts",
                    count = 5,
                    username_prefix = "grunt",
                    server = "main",
                })
                return ok == true
                "#,
            )
            .eval()
            .unwrap();
        assert!(ok);

        // Every generated username must now be taken — attempting to
        // re-add any of them must fail as a duplicate.
        for i in 0..5 {
            let duplicate_rejected: bool = lua
                .load(format!(
                    r#"local id, err = swarm:add_bot({{username = "grunt{i}", server = "main"}}) return id == nil"#
                ))
                .eval()
                .unwrap();
            assert!(
                duplicate_rejected,
                "grunt{i} must have actually been created"
            );
        }
    }

    #[tokio::test]
    async fn add_group_rejects_an_oversized_count_before_allocating_anything() {
        let state = test_state();
        let lua = install(&state);

        let ok: bool = lua
            .load(format!(
                r#"
                local ok, err = swarm:add_group({{
                    name = "huge",
                    count = {},
                    username_prefix = "b",
                    server = "main",
                }})
                return ok == true
                "#,
                crate::lua::registry::MAX_BOTS_PER_GROUP + 1
            ))
            .eval()
            .unwrap();
        assert!(
            !ok,
            "a count exceeding MAX_BOTS_PER_GROUP must be rejected outright, before any allocation"
        );
    }

    /// `swarm:off(id)` previously only searched `HandlerRegistry`
    /// (`global`/`per_bot`), never `pubsub` — an `on_message` subscription
    /// id was silently never found, so `off()` returned `false` and the
    /// subscription kept running forever with no way to remove it.
    #[tokio::test]
    async fn off_also_removes_an_on_message_subscription() {
        let state = test_state();
        let lua = install(&state);

        let id: u64 = lua
            .load(r#"return swarm:on_message("topic1", function(topic, payload) end)"#)
            .eval()
            .unwrap();
        assert_eq!(
            state
                .pubsub
                .borrow()
                .get("topic1")
                .map(Vec::len)
                .unwrap_or(0),
            1,
            "the subscription must actually be registered before off() is tested"
        );

        let removed: bool = lua.load(format!("return swarm:off({id})")).eval().unwrap();
        assert!(
            removed,
            "off() must report the subscription was found and removed"
        );
        assert_eq!(
            state
                .pubsub
                .borrow()
                .get("topic1")
                .map(Vec::len)
                .unwrap_or(0),
            0,
            "the on_message subscription must actually be gone from pubsub, not just \
             unrelated HandlerRegistry state"
        );
    }
}
