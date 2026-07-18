//! `LuaGroup`: returned by `swarm:group(name)`/`swarm:groups()`. Routes
//! `chat`/`forward`/`stop_movement`/`disconnect` to each member bot's own
//! assigned worker/supervisor — bots are never moved between workers to
//! satisfy a group operation. Returns one request id per bot (or an
//! immediate validation error for a bot that isn't connected).

use std::rc::Rc;

use mlua::{ObjectLike, Table, UserData, UserDataMethods, Value};

use crate::lua::worker::WorkerState;

#[derive(Clone)]
pub struct LuaGroup {
    pub state: Rc<WorkerState>,
    pub name: String,
    pub bot_ids: Vec<u32>,
}

fn per_bot_request_ids(lua: &mlua::Lua, ids: Vec<(u32, u64)>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (bot_id, request_id) in ids {
        t.set(bot_id, request_id)?;
    }
    Ok(t)
}

impl UserData for LuaGroup {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("name", |_, this, ()| Ok(this.name.clone()));
        methods.add_method("bot_ids", |lua, this, ()| {
            crate::lua::convert::string_array(lua, this.bot_ids.iter().map(|id| id.to_string()))
        });

        methods.add_method("chat", |lua, this, message: String| {
            let mut ids = Vec::new();
            for &bot_id in &this.bot_ids {
                let bot = super::bot::LuaBot {
                    state: this.state.clone(),
                    bot_id,
                };
                let request_id: u64 = lua
                    .create_userdata(bot)?
                    .call_method("chat", message.clone())?;
                ids.push((bot_id, request_id));
            }
            per_bot_request_ids(lua, ids)
        });

        methods.add_method("forward", |lua, this, on: bool| {
            let mut ids = Vec::new();
            for &bot_id in &this.bot_ids {
                let bot = super::bot::LuaBot {
                    state: this.state.clone(),
                    bot_id,
                };
                let request_id: u64 = lua.create_userdata(bot)?.call_method("forward", on)?;
                ids.push((bot_id, request_id));
            }
            per_bot_request_ids(lua, ids)
        });

        methods.add_method("stop_movement", |lua, this, ()| {
            let mut ids = Vec::new();
            for &bot_id in &this.bot_ids {
                let bot = super::bot::LuaBot {
                    state: this.state.clone(),
                    bot_id,
                };
                let request_id: u64 = lua.create_userdata(bot)?.call_method("stop_movement", ())?;
                ids.push((bot_id, request_id));
            }
            per_bot_request_ids(lua, ids)
        });

        methods.add_method("disconnect", |lua, this, ()| {
            for &bot_id in &this.bot_ids {
                if let Some(handle) = this.state.bot_handle(bot_id) {
                    handle.stop();
                }
            }
            let _ = lua;
            Ok(true)
        });
    }
}

pub fn make_group(lua: &mlua::Lua, state: Rc<WorkerState>, name: String, bot_ids: Vec<u32>) -> mlua::Result<Value> {
    lua.pack(LuaGroup { state, name, bot_ids })
}
