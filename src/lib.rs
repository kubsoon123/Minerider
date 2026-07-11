//! MineRider — a high performance Minecraft Java Edition client engine.
//!
//! Architecture:
//!
//! ```text
//!                   User Bots (Lua)
//!                        |
//!                 Lua API Layer          (phase 5)
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
//! engine. Lua (phase 5) will own bot logic only.

pub mod compression;
pub mod core;
pub mod crypto;
pub mod minecraft;
pub mod network;
pub mod protocol;
