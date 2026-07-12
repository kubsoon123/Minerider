//! Pure wire-protocol primitives for Minecraft Java Edition.
//!
//! This crate contains zero game logic and no async runtime: VarInt/VarLong
//! encoding, packet buffers, length-prefix framing with optional zlib
//! compression, and the login-exchange cryptography (RSA + AES-128-CFB8).
//! Game-state machines and networking live in the `minerider` crate.
//!
//! Hand-written for phase 1. From phase 2 on, packet definitions are
//! produced from minecraft-data and never edited by hand.

pub mod buffer;
pub mod codec;
pub mod compression;
pub mod crypto;
pub mod error;
pub mod packet;
pub mod varint;
pub mod varlong;

pub use error::{ProtocolError, Result};
