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
/// plain readable text, recursively resolving `text`, `translate` (+`with`
/// argument substitution) and `extra` child components. Formatting/color
/// fields are dropped — this is for logging, not display.
pub(crate) fn nbt_reason_text(component: &Nbt) -> String {
    let mut out = String::new();
    render_component(component, &mut out);
    out
}

fn render_component(component: &Nbt, out: &mut String) {
    match component {
        // A bare string is a literal text component.
        Nbt::String(text) => out.push_str(text),
        // A list is a component array: the first is the base, the rest are
        // appended (some servers send chat this way).
        Nbt::List(list) => {
            for item in &list.items {
                render_component(item, out);
            }
        }
        Nbt::Compound(_) => {
            if let Some(Nbt::String(text)) = component.get("text") {
                out.push_str(text);
            } else if let Some(Nbt::String(key)) = component.get("translate") {
                let args = translate_args(component);
                render_translation(key, &args, out);
            }
            // Any component can carry `extra` children, appended in order.
            if let Some(Nbt::List(extra)) = component.get("extra") {
                for child in &extra.items {
                    render_component(child, out);
                }
            }
        }
        _ => {}
    }
}

/// Collects the already-rendered `with` argument components of a `translate`
/// component, in order.
fn translate_args(component: &Nbt) -> Vec<String> {
    match component.get("with") {
        Some(Nbt::List(list)) => list
            .items
            .iter()
            .map(|arg| {
                let mut s = String::new();
                render_component(arg, &mut s);
                s
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Renders a `translate` key by substituting `%s` / `%N$s` placeholders in
/// the known template, or falling back to the key plus its args when the
/// template is not one MineRider ships (we don't bundle the full lang file).
fn render_translation(key: &str, args: &[String], out: &mut String) {
    if let Some(template) = translation_template(key) {
        substitute_placeholders(template, args, out);
    } else if args.is_empty() {
        out.push_str(key);
    } else {
        // Best effort for an unknown key: e.g. "key{a, b}".
        out.push_str(key);
        out.push('{');
        out.push_str(&args.join(", "));
        out.push('}');
    }
}

/// A small table of the translation keys a bot most often sees in chat/system
/// messages. Not the full vanilla lang file — unknown keys fall back to
/// showing the key and its args.
fn translation_template(key: &str) -> Option<&'static str> {
    Some(match key {
        "chat.type.text" => "<%s> %s",
        "chat.type.announcement" => "[%s] %s",
        "chat.type.emote" => "* %s %s",
        "chat.type.team.text" => "%s <%s> %s",
        "multiplayer.player.joined" => "%s joined the game",
        "multiplayer.player.joined.renamed" => "%s (formerly known as %s) joined the game",
        "multiplayer.player.left" => "%s left the game",
        "commands.message.display.incoming" => "%s whispers to you: %s",
        "commands.message.display.outgoing" => "You whisper to %s: %s",
        _ => return None,
    })
}

/// Substitutes `%s` (sequential) and `%N$s` (indexed) placeholders in a
/// vanilla translation template with the supplied arguments.
fn substitute_placeholders(template: &str, args: &[String], out: &mut String) {
    let mut chars = template.chars().peekable();
    let mut next_seq = 0usize;
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('%') => {
                chars.next();
                out.push('%');
            }
            Some('s') => {
                chars.next();
                if let Some(arg) = args.get(next_seq) {
                    out.push_str(arg);
                }
                next_seq += 1;
            }
            Some(d) if d.is_ascii_digit() => {
                // Indexed form: %N$s.
                let mut index = 0usize;
                while let Some(d) = chars.peek().filter(|c| c.is_ascii_digit()) {
                    index = index * 10 + (*d as usize - '0' as usize);
                    chars.next();
                }
                // Consume the "$s" tail if present.
                if chars.peek() == Some(&'$') {
                    chars.next();
                    if chars.peek() == Some(&'s') {
                        chars.next();
                    }
                }
                if let Some(arg) = index.checked_sub(1).and_then(|i| args.get(i)) {
                    out.push_str(arg);
                }
            }
            _ => out.push('%'),
        }
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

    #[test]
    fn substitutes_indexed_placeholders() {
        // Death messages use indexed args like "%1$s was slain by %2$s".
        let mut out = String::new();
        substitute_placeholders(
            "%1$s was slain by %2$s",
            &["Steve".into(), "Zombie".into()],
            &mut out,
        );
        assert_eq!(out, "Steve was slain by Zombie");
    }
}
