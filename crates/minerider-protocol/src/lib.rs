//! Pure wire-protocol primitives for Minecraft Java Edition.
//!
//! This crate contains zero game logic and no async runtime: VarInt/VarLong
//! encoding, packet buffers, length-prefix framing with optional zlib
//! compression, and the login-exchange cryptography (RSA + AES-128-CFB8).
//! Game-state machines and networking live in the `minerider` crate.
//!
//! Hand-written for phase 1. From phase 2 on, packet definitions live in
//! [`generated`] and are produced from minecraft-data by the
//! `minerider-codegen` crate — never edited by hand.

#![forbid(unsafe_code)]

pub mod buffer;
pub mod codec;
pub mod compression;
pub mod crypto;
pub mod error;
pub mod generated;
pub mod holder;
pub mod nbt;
pub mod packet;
pub mod traits;
pub mod varint;
pub mod varlong;

pub use error::{ProtocolError, Result};
pub use traits::{Decode, Encode};
