//! `PresentationState`/`PresentationEvent` → Lua table conversion: chat,
//! action bar, titles/subtitles/timing, tab header/footer, boss bars.

use mlua::{Lua, Table, Value};

use crate::minecraft::presentation::*;

fn chat_kind_to_str(kind: &ChatKind) -> &'static str {
    match kind {
        ChatKind::Player => "player",
        ChatKind::Disguised => "disguised",
        ChatKind::System => "system",
    }
}

fn boss_bar_color_to_str(c: &BossBarColor) -> String {
    match c {
        BossBarColor::Pink => "pink".to_string(),
        BossBarColor::Blue => "blue".to_string(),
        BossBarColor::Red => "red".to_string(),
        BossBarColor::Green => "green".to_string(),
        BossBarColor::Yellow => "yellow".to_string(),
        BossBarColor::Purple => "purple".to_string(),
        BossBarColor::White => "white".to_string(),
        BossBarColor::Unknown(n) => format!("unknown:{n}"),
    }
}

fn boss_bar_overlay_to_str(o: &BossBarOverlay) -> String {
    match o {
        BossBarOverlay::Progress => "progress".to_string(),
        BossBarOverlay::Notched6 => "notched_6".to_string(),
        BossBarOverlay::Notched10 => "notched_10".to_string(),
        BossBarOverlay::Notched12 => "notched_12".to_string(),
        BossBarOverlay::Notched20 => "notched_20".to_string(),
        BossBarOverlay::Unknown(n) => format!("unknown:{n}"),
    }
}

fn boss_bar_action_to_str(a: &BossBarAction) -> String {
    match a {
        BossBarAction::Add => "add".to_string(),
        BossBarAction::Remove => "remove".to_string(),
        BossBarAction::UpdateProgress => "update_progress".to_string(),
        BossBarAction::UpdateTitle => "update_title".to_string(),
        BossBarAction::UpdateStyle => "update_style".to_string(),
        BossBarAction::UpdateFlags => "update_flags".to_string(),
        BossBarAction::Unknown(n) => format!("unknown:{n}"),
    }
}

pub fn chat_message_to_table(lua: &Lua, msg: &ChatMessage) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("sequence", msg.sequence)?;
    t.set("kind", chat_kind_to_str(&msg.kind))?;
    t.set("content", super::text_component(lua, &msg.content)?)?;
    t.set("sender", super::opt_text_component(lua, &msg.sender)?)?;
    t.set("target", super::opt_text_component(lua, &msg.target)?)?;
    t.set("signed", msg.signed.is_some())?;
    Ok(t)
}

pub fn boss_bar_to_table(lua: &Lua, bar: &BossBar) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("id", super::u128_to_hex_string(lua, bar.id)?)?;
    t.set("title", super::text_component(lua, &bar.title)?)?;
    t.set("progress", bar.progress)?;
    t.set("color", boss_bar_color_to_str(&bar.color))?;
    t.set("overlay", boss_bar_overlay_to_str(&bar.overlay))?;
    t.set("darken_sky", bar.flags.darken_sky())?;
    t.set("play_end_music", bar.flags.play_end_music())?;
    t.set("create_world_fog", bar.flags.create_world_fog())?;
    Ok(t)
}

pub fn presentation_state_to_table(lua: &Lua, state: &PresentationState) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let chat = lua.create_table()?;
    for (i, msg) in state.chat.iter().enumerate() {
        chat.set(i + 1, chat_message_to_table(lua, msg)?)?;
    }
    t.set("chat", chat)?;
    t.set(
        "action_bar",
        super::opt_text_component(lua, &state.action_bar)?,
    )?;

    let titles = lua.create_table()?;
    titles.set(
        "title",
        super::opt_text_component(lua, &state.titles.title)?,
    )?;
    titles.set(
        "subtitle",
        super::opt_text_component(lua, &state.titles.subtitle)?,
    )?;
    titles.set("fade_in", state.titles.timing.fade_in)?;
    titles.set("stay", state.titles.timing.stay)?;
    titles.set("fade_out", state.titles.timing.fade_out)?;
    t.set("titles", titles)?;

    let tab_list = lua.create_table()?;
    tab_list.set(
        "header",
        super::opt_text_component(lua, &state.tab_list.header)?,
    )?;
    tab_list.set(
        "footer",
        super::opt_text_component(lua, &state.tab_list.footer)?,
    )?;
    t.set("tab_list", tab_list)?;

    let boss_bars = lua.create_table()?;
    for (id, bar) in &state.boss_bars {
        boss_bars.set(
            super::u128_to_hex_string(lua, *id)?,
            boss_bar_to_table(lua, bar)?,
        )?;
    }
    t.set("boss_bars", boss_bars)?;

    t.set(
        "disconnect_reason",
        super::opt_text_component(lua, &state.disconnect_reason)?,
    )?;
    Ok(t)
}

/// The `PresentationEvent` payload for whichever specific event name it was
/// dispatched under (`presentation`/`action_bar`/`title`/`boss_bar`; see
/// `crate::lua::event::bot_event_name`).
pub fn presentation_event_to_table(lua: &Lua, event: &PresentationEvent) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    match event {
        PresentationEvent::Chat(msg) => {
            t.set("kind", "chat")?;
            t.set("message", chat_message_to_table(lua, msg)?)?;
        }
        PresentationEvent::ActionBarChanged { text } => {
            t.set("kind", "action_bar_changed")?;
            t.set("text", super::text_component(lua, text)?)?;
        }
        PresentationEvent::TitleChanged { text } => {
            t.set("kind", "title_changed")?;
            t.set("text", super::text_component(lua, text)?)?;
        }
        PresentationEvent::SubtitleChanged { text } => {
            t.set("kind", "subtitle_changed")?;
            t.set("text", super::text_component(lua, text)?)?;
        }
        PresentationEvent::TitleTimingChanged { timing } => {
            t.set("kind", "title_timing_changed")?;
            t.set("fade_in", timing.fade_in)?;
            t.set("stay", timing.stay)?;
            t.set("fade_out", timing.fade_out)?;
        }
        PresentationEvent::TitlesCleared { reset } => {
            t.set("kind", "titles_cleared")?;
            t.set("reset", *reset)?;
        }
        PresentationEvent::TabListChanged { header, footer } => {
            t.set("kind", "tab_list_changed")?;
            t.set("header", super::text_component(lua, header)?)?;
            t.set("footer", super::text_component(lua, footer)?)?;
        }
        PresentationEvent::BossBarChanged {
            id,
            action,
            current,
            applied,
        } => {
            t.set("kind", "boss_bar_changed")?;
            t.set("id", super::u128_to_hex_string(lua, *id)?)?;
            t.set("action", boss_bar_action_to_str(action))?;
            t.set(
                "current",
                match current {
                    Some(bar) => Value::Table(boss_bar_to_table(lua, bar)?),
                    None => Value::Nil,
                },
            )?;
            t.set("applied", *applied)?;
        }
        PresentationEvent::Disconnected { reason } => {
            t.set("kind", "disconnected")?;
            t.set("reason", super::text_component(lua, reason)?)?;
        }
    }
    Ok(t)
}
