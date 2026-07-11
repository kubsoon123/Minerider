//! Wire protocol primitives: VarInt/VarLong, buffer handling, packet
//! framing and the compression-aware codec.
//!
//! Hand-written for phase 1. From phase 2 on, packet definitions under
//! `generated/` are produced from minecraft-data and never edited by hand.

pub mod buffer;
pub mod codec;
pub mod packet;
pub mod varint;
pub mod varlong;
