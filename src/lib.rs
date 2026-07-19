//! MineRider — a high performance Minecraft Java Edition client engine.
//!
//! Architecture:
//!
//! ```text
//!                   User Bots (Lua, optional — see `lua`)
//!                        |
//!                 Lua API Layer          (`crate::lua`, feature = "lua")
//!                        |
//!               MineRider Engine
//!                        |
//!  ---------------------------------------------
//!  Network | Protocol | World | Physics | Behavior
//!  ---------------------------------------------
//!                        |
//!               Minecraft Server
//! ```
//!
//! Rust owns everything performance critical: TCP, the async runtime, the
//! protocol, encryption, compression, world state, physics and the tick
//! engine. Lua (`crate::lua`, behind the `lua` feature) owns bot
//! orchestration/behavior only — see `docs/lua_wrapper.md`.
//!
//! Wire-protocol primitives (VarInt, framing, compression, crypto) live in
//! the standalone [`minerider_protocol`] crate; this crate adds the async
//! network layer and the game-state machines on top of it.

pub mod auth;
pub mod core;
#[cfg(feature = "lua")]
pub mod lua;
#[cfg(feature = "lua-benchmark")]
pub mod lua_benchmark;
pub mod minecraft;
pub mod network;
pub mod trace;
