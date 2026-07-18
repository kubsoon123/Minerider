//! `HudState`/`HudEvent` → Lua table conversion: health/food/xp/gamemode/
//! abilities/hotbar/cooldowns/effects/attributes/death/respawn/border/
//! time/weather/difficulty/spawn.
//!
//! Closed protocol enums with an `Unknown(raw)` fallback (game mode,
//! difficulty, attribute keys, ...) are rendered as plain lowercase Lua
//! strings, with unrecognized values rendered as `"unknown:<raw>"` rather
//! than a nested table — simpler for scripts to pattern-match, while still
//! preserving the raw value for diagnostics.

use mlua::{Lua, Table, Value};

use crate::lua::convert::items::raw_slot_to_table;
use crate::minecraft::hud::*;

pub fn game_mode_to_str(gm: &GameMode) -> String {
    match gm {
        GameMode::Survival => "survival".to_string(),
        GameMode::Creative => "creative".to_string(),
        GameMode::Adventure => "adventure".to_string(),
        GameMode::Spectator => "spectator".to_string(),
        GameMode::Unknown(n) => format!("unknown:{n}"),
    }
}

fn opt_game_mode(lua: &Lua, gm: &Option<GameMode>) -> mlua::Result<Value> {
    match gm {
        Some(gm) => Ok(Value::String(lua.create_string(game_mode_to_str(gm))?)),
        None => Ok(Value::Nil),
    }
}

fn difficulty_to_str(d: &Difficulty) -> String {
    match d {
        Difficulty::Peaceful => "peaceful".to_string(),
        Difficulty::Easy => "easy".to_string(),
        Difficulty::Normal => "normal".to_string(),
        Difficulty::Hard => "hard".to_string(),
        Difficulty::Unknown(n) => format!("unknown:{n}"),
    }
}

pub fn attribute_key_to_str(k: &AttributeKey) -> &'static str {
    match k {
        AttributeKey::Armor => "armor",
        AttributeKey::ArmorToughness => "armor_toughness",
        AttributeKey::AttackDamage => "attack_damage",
        AttributeKey::AttackKnockback => "attack_knockback",
        AttributeKey::AttackSpeed => "attack_speed",
        AttributeKey::BlockBreakSpeed => "block_break_speed",
        AttributeKey::BlockInteractionRange => "block_interaction_range",
        AttributeKey::EntityInteractionRange => "entity_interaction_range",
        AttributeKey::FallDamageMultiplier => "fall_damage_multiplier",
        AttributeKey::FlyingSpeed => "flying_speed",
        AttributeKey::FollowRange => "follow_range",
        AttributeKey::Gravity => "gravity",
        AttributeKey::JumpStrength => "jump_strength",
        AttributeKey::KnockbackResistance => "knockback_resistance",
        AttributeKey::Luck => "luck",
        AttributeKey::MaxAbsorption => "max_absorption",
        AttributeKey::MaxHealth => "max_health",
        AttributeKey::MovementSpeed => "movement_speed",
        AttributeKey::SafeFallDistance => "safe_fall_distance",
        AttributeKey::Scale => "scale",
        AttributeKey::SpawnReinforcements => "spawn_reinforcements",
        AttributeKey::StepHeight => "step_height",
    }
}

fn attribute_operation_to_str(op: &AttributeOperation) -> String {
    match op {
        AttributeOperation::AddValue => "add_value".to_string(),
        AttributeOperation::AddMultipliedBase => "add_multiplied_base".to_string(),
        AttributeOperation::AddMultipliedTotal => "add_multiplied_total".to_string(),
        AttributeOperation::Unknown(n) => format!("unknown:{n}"),
    }
}

pub fn hud_state_to_table(lua: &Lua, hud: &HudState) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("entity_id", super::state::opt_i32(hud.entity_id))?;

    let vitals = lua.create_table()?;
    vitals.set("health", hud.vitals.health)?;
    vitals.set("food", hud.vitals.food)?;
    vitals.set("saturation", hud.vitals.saturation)?;
    t.set("vitals", vitals)?;

    let xp = lua.create_table()?;
    xp.set("bar", hud.experience.bar)?;
    xp.set("level", hud.experience.level)?;
    xp.set("total", hud.experience.total)?;
    t.set("experience", xp)?;

    t.set("game_mode", game_mode_to_str(&hud.game_mode))?;
    t.set("previous_game_mode", opt_game_mode(lua, &hud.previous_game_mode)?)?;
    t.set("hardcore", hud.hardcore)?;

    let abilities = lua.create_table()?;
    abilities.set("invulnerable", hud.abilities.invulnerable())?;
    abilities.set("flying", hud.abilities.flying())?;
    abilities.set("may_fly", hud.abilities.may_fly())?;
    abilities.set("instant_build", hud.abilities.instant_build())?;
    abilities.set("flying_speed", hud.abilities.flying_speed)?;
    abilities.set("walking_speed", hud.abilities.walking_speed)?;
    t.set("abilities", abilities)?;

    t.set("selected_hotbar_slot", hud.selected_hotbar_slot)?;
    t.set("held_item", raw_slot_to_table(lua, 0, &hud.held_item)?)?;

    let hotbar = lua.create_table()?;
    for (slot, item) in &hud.hotbar {
        hotbar.set(*slot + 1, raw_slot_to_table(lua, *slot as usize, item)?)?;
    }
    t.set("hotbar", hotbar)?;

    let cooldowns = lua.create_table()?;
    for (group, ticks) in &hud.cooldowns {
        cooldowns.set(group.as_str(), *ticks)?;
    }
    t.set("cooldowns", cooldowns)?;

    let effects = lua.create_table()?;
    for (id, effect) in &hud.active_effects {
        let e = lua.create_table()?;
        e.set("id", effect.id)?;
        e.set("amplifier", effect.amplifier)?;
        e.set("duration_ticks", effect.duration_ticks)?;
        e.set("ambient", effect.flags.ambient())?;
        e.set("show_particles", effect.flags.show_particles())?;
        e.set("show_icon", effect.flags.show_icon())?;
        effects.set(*id, e)?;
    }
    t.set("active_effects", effects)?;

    let attributes = lua.create_table()?;
    for (key, attr) in &hud.attributes {
        let a = lua.create_table()?;
        a.set("base_value", attr.base_value)?;
        let modifiers = lua.create_table()?;
        for (i, m) in attr.modifiers.iter().enumerate() {
            let mt = lua.create_table()?;
            mt.set("id", m.id.as_str())?;
            mt.set("amount", m.amount)?;
            mt.set("operation", attribute_operation_to_str(&m.operation))?;
            modifiers.set(i + 1, mt)?;
        }
        a.set("modifiers", modifiers)?;
        a.set("modifiers_truncated", attr.modifiers_truncated)?;
        attributes.set(attribute_key_to_str(key), a)?;
    }
    t.set("attributes", attributes)?;

    t.set(
        "death",
        match &hud.death {
            Some(d) => {
                let dt = lua.create_table()?;
                dt.set("player_id", d.player_id)?;
                dt.set("message", super::text_component(lua, &d.message)?)?;
                Value::Table(dt)
            }
            None => Value::Nil,
        },
    )?;

    let respawn = lua.create_table()?;
    respawn.set("count", hud.respawn.count)?;
    respawn.set("game_mode", game_mode_to_str(&hud.respawn.game_mode))?;
    respawn.set("previous_game_mode", opt_game_mode(lua, &hud.respawn.previous_game_mode)?)?;
    respawn.set("dimension_name", hud.respawn.dimension_name.as_str())?;
    respawn.set(
        "last_death_location",
        match &hud.respawn.last_death_location {
            Some(loc) => {
                let lt = lua.create_table()?;
                lt.set("dimension_name", loc.dimension_name.as_str())?;
                lt.set("x", loc.position.x)?;
                lt.set("y", loc.position.y)?;
                lt.set("z", loc.position.z)?;
                Value::Table(lt)
            }
            None => Value::Nil,
        },
    )?;
    respawn.set("portal_cooldown", hud.respawn.portal_cooldown)?;
    respawn.set("sea_level", hud.respawn.sea_level)?;
    t.set("respawn", respawn)?;

    t.set(
        "world_border",
        match &hud.world_border {
            Some(b) => {
                let bt = lua.create_table()?;
                bt.set("center_x", b.center_x)?;
                bt.set("center_z", b.center_z)?;
                bt.set("old_diameter", b.old_diameter)?;
                bt.set("new_diameter", b.new_diameter)?;
                bt.set("lerp_millis", b.lerp_millis)?;
                bt.set("warning_blocks", b.warning_blocks)?;
                bt.set("warning_time", b.warning_time)?;
                Value::Table(bt)
            }
            None => Value::Nil,
        },
    )?;

    let time = lua.create_table()?;
    time.set("age", hud.time.age)?;
    time.set("day_time", hud.time.day_time)?;
    time.set("ticking", hud.time.ticking)?;
    t.set("time", time)?;

    let weather = lua.create_table()?;
    weather.set("raining", hud.weather.raining)?;
    weather.set("rain_level", hud.weather.rain_level)?;
    weather.set("thunder_level", hud.weather.thunder_level)?;
    t.set("weather", weather)?;

    let difficulty = lua.create_table()?;
    difficulty.set("difficulty", difficulty_to_str(&hud.difficulty.difficulty))?;
    difficulty.set("locked", hud.difficulty.locked)?;
    t.set("difficulty", difficulty)?;

    t.set(
        "spawn_position",
        match &hud.spawn_position {
            Some(sp) => {
                let st = lua.create_table()?;
                st.set("x", sp.position.x)?;
                st.set("y", sp.position.y)?;
                st.set("z", sp.position.z)?;
                st.set("angle", sp.angle)?;
                Value::Table(st)
            }
            None => Value::Nil,
        },
    )?;

    Ok(t)
}
