//! Minecraft protocol states: handshake, login, configuration, play.

use minerider_protocol::nbt::Nbt;

pub mod configuration;
pub mod coverage;
pub mod entity;
pub mod handshake;
pub mod login;
pub mod play;
pub mod player;

/// Renders an NBT text component (a disconnect reason) as readable text.
///
/// Extracts the `text` entry of a compound component; anything more
/// complex falls back to the NBT's debug representation.
pub(crate) fn nbt_reason_text(reason: &Nbt) -> String {
    match reason.get("text") {
        Some(Nbt::String(text)) => text.clone(),
        _ => format!("{reason:?}"),
    }
}
