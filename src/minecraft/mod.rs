//! Minecraft protocol states: handshake, login, configuration, play.

use minerider_protocol::buffer::PacketWriter;
use minerider_protocol::generated::v1_21_4::types::{
    PacketCommonSettings, PacketCommonSettingsParticleStatus,
};
use minerider_protocol::nbt::Nbt;

use crate::core::error::Result;

pub mod configuration;
pub mod coverage;
pub mod entity;
pub mod handshake;
pub mod login;
pub mod play;
pub mod player;

/// The client brand a vanilla client reports on the `minecraft:brand`
/// plugin channel.
pub const CLIENT_BRAND: &str = "vanilla";

/// The `minecraft:brand` plugin-message channel.
pub const BRAND_CHANNEL: &str = "minecraft:brand";

/// The `client_information` (settings) a vanilla 1.21.4 client sends with
/// its default options.
///
/// Values match a fresh vanilla install: `en_us`, render distance 12, chat
/// fully enabled and colored, every skin layer shown, right main hand, no
/// text filtering, server listings allowed, all particles. Render distance
/// and locale are user options in real clients; these are the out-of-box
/// defaults and are the natural knobs to align with a reference capture.
pub fn vanilla_client_information() -> PacketCommonSettings {
    PacketCommonSettings {
        locale: "en_us".to_string(),
        view_distance: 12,
        chat_flags: 0, // 0 = chat enabled (1 = commands only, 2 = hidden)
        chat_colors: true,
        skin_parts: 0x7f, // all seven skin layers shown
        main_hand: 1,     // 0 = left, 1 = right
        enable_text_filtering: false,
        enable_server_listing: true,
        particle_status: PacketCommonSettingsParticleStatus::All,
    }
}

/// The payload bytes of the client's `minecraft:brand` plugin message: the
/// brand string written as a length-prefixed Minecraft string (the custom
/// payload `data` field is a raw rest-buffer, so no extra framing).
pub fn brand_payload() -> Result<Vec<u8>> {
    let mut w = PacketWriter::new();
    w.put_string(CLIENT_BRAND)?;
    Ok(w.into_inner().to_vec())
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_payload_is_length_prefixed_vanilla() {
        // writeUtf("vanilla") == varint(7) + b"vanilla".
        assert_eq!(brand_payload().unwrap(), b"\x07vanilla");
    }

    #[test]
    fn vanilla_settings_match_default_options() {
        let s = vanilla_client_information();
        assert_eq!(s.locale, "en_us");
        assert_eq!(s.view_distance, 12);
        assert_eq!(s.chat_flags, 0);
        assert!(s.chat_colors);
        assert_eq!(s.skin_parts, 0x7f);
        assert_eq!(s.main_hand, 1);
        assert!(!s.enable_text_filtering);
        assert!(s.enable_server_listing);
        assert_eq!(s.particle_status, PacketCommonSettingsParticleStatus::All);
    }
}
