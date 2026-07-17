//! Minecraft protocol states: handshake, login, configuration, play.

use minerider_protocol::buffer::PacketWriter;
use minerider_protocol::generated::v1_21_4::types::{
    PacketCommonSettings, PacketCommonSettingsParticleStatus,
};
use minerider_protocol::nbt::Nbt;

use crate::core::error::Result;

mod collision_data;
pub mod configuration;
pub mod control;
pub mod coverage;
pub mod entity;
pub mod event;
pub mod handshake;
pub mod inventory;
pub mod login;
pub mod physics;
pub mod play;
pub mod player;
pub mod players;
pub mod text;
pub mod world;

/// The client brand a vanilla client reports on the `minecraft:brand`
/// plugin channel.
pub const CLIENT_BRAND: &str = "vanilla";

/// The `minecraft:brand` plugin-message channel.
pub const BRAND_CHANNEL: &str = "minecraft:brand";

/// Vanilla `ResourcePackStatus.DECLINED`: the client chose not to download an
/// offered pack. MineRider has no renderer to apply a pack to, so declining
/// is the honest response — exactly what a real player who unchecks "Server
/// Resource Packs" in their options does, not a fabricated "loaded" claim.
/// A server that force-kicks players for declining a *required* pack will
/// still kick MineRider, precisely as it would a real player who declines.
pub const RESOURCE_PACK_STATUS_DECLINED: i32 = 1;

/// The out-of-box vanilla render distance (matches a fresh install).
pub const DEFAULT_VIEW_DISTANCE: i8 = 12;

/// The `client_information` (settings) a vanilla 1.21.4 client sends with
/// its default options.
///
/// Values match a fresh vanilla install: `en_us`, chat fully enabled and
/// colored, every skin layer shown, right main hand, no text filtering,
/// server listings allowed, all particles. Locale is a user option in real
/// clients; this is the out-of-box default and the natural knob to align
/// with a reference capture. `view_distance` is also a real, ordinary
/// client setting — a lower value is exactly what a player with a weaker
/// PC or a laggy connection sets, and it directly bounds how many chunks
/// the server streams (and MineRider stores), which matters when running
/// many bots on one machine.
pub fn vanilla_client_information(view_distance: i8) -> PacketCommonSettings {
    PacketCommonSettings {
        locale: "en_us".to_string(),
        view_distance,
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

/// Renders an NBT text component (disconnect reason, chat message, ...) as
/// plain readable text — a thin wrapper over [`text::TextComponent`], the
/// shared model every text-bearing packet (chat, titles, boss bars,
/// scoreboards, tab-list header/footer) is built on. Kept here since
/// existing call sites (`login`, `configuration`, `play`) already import it
/// from this module.
pub(crate) fn nbt_reason_text(component: &Nbt) -> String {
    text::TextComponent::from_nbt(component).plain_text()
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
        let s = vanilla_client_information(DEFAULT_VIEW_DISTANCE);
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

    use minerider_protocol::nbt::NbtList;

    fn text(s: &str) -> Nbt {
        Nbt::Compound(vec![("text".into(), Nbt::String(s.into()))])
    }

    fn list(items: Vec<Nbt>) -> Nbt {
        Nbt::List(NbtList { tag: 10, items })
    }

    #[test]
    fn renders_plain_text_component() {
        assert_eq!(nbt_reason_text(&text("hello")), "hello");
        assert_eq!(nbt_reason_text(&Nbt::String("bare".into())), "bare");
    }

    #[test]
    fn renders_extra_children_in_order() {
        let component = Nbt::Compound(vec![
            ("text".into(), Nbt::String("a".into())),
            (
                "extra".into(),
                list(vec![text("b"), Nbt::String("c".into())]),
            ),
        ]);
        assert_eq!(nbt_reason_text(&component), "abc");
    }

    #[test]
    fn renders_known_translation_with_args() {
        // "<Notch> hi" — the standard player-chat system format.
        let component = Nbt::Compound(vec![
            ("translate".into(), Nbt::String("chat.type.text".into())),
            ("with".into(), list(vec![text("Notch"), text("hi")])),
        ]);
        assert_eq!(nbt_reason_text(&component), "<Notch> hi");
    }

    #[test]
    fn renders_join_message() {
        let component = Nbt::Compound(vec![
            (
                "translate".into(),
                Nbt::String("multiplayer.player.joined".into()),
            ),
            ("with".into(), list(vec![text("Steve")])),
        ]);
        assert_eq!(nbt_reason_text(&component), "Steve joined the game");
    }

    #[test]
    fn unknown_translation_falls_back_to_key_and_args() {
        let component = Nbt::Compound(vec![
            ("translate".into(), Nbt::String("some.unknown.key".into())),
            ("with".into(), list(vec![text("x")])),
        ]);
        assert_eq!(nbt_reason_text(&component), "some.unknown.key{x}");
    }
}
