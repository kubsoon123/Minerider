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

        methods.add_method("add_proxy", |lua, this, table: Table| {
            let def = super::config::parse_proxy_def(&table)?;
            match this.state.config_builder.borrow_mut().add_proxy(def) {
                Ok(()) => Ok((Value::Boolean(true), Value::Nil)),
                Err(e) => super::errors::err_pair(lua, e.into()),
            }
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
            let count: u32 = table.get("count").unwrap_or(0);
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
                match this.state.config_builder.borrow_mut().add_bot(spec) {
                    Ok(id) => ids.push(id),
                    Err(e) => return super::errors::err_pair(lua, e.into()),
                }
            }
            match this.state.config_builder.borrow_mut().add_group(name, ids) {
                Ok(()) => Ok((Value::Boolean(true), Value::Nil)),
                Err(e) => super::errors::err_pair(lua, e.into()),
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
            let payload = this.state.startup_barrier.wait();
            *this.state.started.borrow_mut() = Some(payload);
            Ok(())
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
            Ok(this.state.handlers.borrow_mut().remove(id))
        });

        // ---- Bounded cross-worker pub/sub --------------------------------
        methods.add_method("publish", |lua, this, (topic, payload): (String, Value)| {
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
