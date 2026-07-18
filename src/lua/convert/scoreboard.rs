//! `ScoreboardState`/`ScoreboardEvent` → Lua table conversion: objectives,
//! display slots, scores, teams, options, membership, number formats.

use mlua::{Lua, Table, Value};

use crate::minecraft::scoreboard::*;

fn display_slot_to_str(slot: &DisplaySlot) -> String {
    match slot {
        DisplaySlot::List => "list".to_string(),
        DisplaySlot::Sidebar => "sidebar".to_string(),
        DisplaySlot::BelowName => "below_name".to_string(),
        DisplaySlot::Team(color) => format!("team:{}", team_color_to_str(color)),
        DisplaySlot::Unknown(n) => format!("unknown:{n}"),
    }
}

pub fn team_color_to_str(c: &TeamColor) -> String {
    match c {
        TeamColor::Black => "black".to_string(),
        TeamColor::DarkBlue => "dark_blue".to_string(),
        TeamColor::DarkGreen => "dark_green".to_string(),
        TeamColor::DarkAqua => "dark_aqua".to_string(),
        TeamColor::DarkRed => "dark_red".to_string(),
        TeamColor::DarkPurple => "dark_purple".to_string(),
        TeamColor::Gold => "gold".to_string(),
        TeamColor::Gray => "gray".to_string(),
        TeamColor::DarkGray => "dark_gray".to_string(),
        TeamColor::Blue => "blue".to_string(),
        TeamColor::Green => "green".to_string(),
        TeamColor::Aqua => "aqua".to_string(),
        TeamColor::Red => "red".to_string(),
        TeamColor::LightPurple => "light_purple".to_string(),
        TeamColor::Yellow => "yellow".to_string(),
        TeamColor::White => "white".to_string(),
        TeamColor::Reset => "reset".to_string(),
        TeamColor::Unknown(n) => format!("unknown:{n}"),
    }
}

fn objective_render_type_to_str(rt: &ObjectiveRenderType) -> String {
    match rt {
        ObjectiveRenderType::Integer => "integer".to_string(),
        ObjectiveRenderType::Hearts => "hearts".to_string(),
        ObjectiveRenderType::Unknown(n) => format!("unknown:{n}"),
    }
}

fn name_tag_visibility_to_str(v: &NameTagVisibility) -> String {
    match v {
        NameTagVisibility::Always => "always".to_string(),
        NameTagVisibility::Never => "never".to_string(),
        NameTagVisibility::HideForOtherTeams => "hide_for_other_teams".to_string(),
        NameTagVisibility::HideForOwnTeam => "hide_for_own_team".to_string(),
        NameTagVisibility::Unknown(s) => format!("unknown:{s}"),
    }
}

fn collision_rule_to_str(v: &CollisionRule) -> String {
    match v {
        CollisionRule::Always => "always".to_string(),
        CollisionRule::Never => "never".to_string(),
        CollisionRule::PushOtherTeams => "push_other_teams".to_string(),
        CollisionRule::PushOwnTeam => "push_own_team".to_string(),
        CollisionRule::Unknown(s) => format!("unknown:{s}"),
    }
}

fn number_format_to_value(lua: &Lua, fmt: &Option<NumberFormat>) -> mlua::Result<Value> {
    let fmt = match fmt {
        Some(f) => f,
        None => return Ok(Value::Nil),
    };
    let t = lua.create_table()?;
    match fmt {
        NumberFormat::Blank => t.set("kind", "blank")?,
        NumberFormat::Styled(_) => t.set("kind", "styled")?,
        NumberFormat::Fixed(text) => {
            t.set("kind", "fixed")?;
            t.set("text", super::text_component(lua, text)?)?;
        }
        NumberFormat::Unknown { kind, .. } => {
            t.set("kind", "unknown")?;
            t.set("raw_kind", *kind)?;
        }
    }
    Ok(Value::Table(t))
}

pub fn objective_to_table(lua: &Lua, obj: &Objective) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("name", obj.name.as_str())?;
    t.set("display_name", super::text_component(lua, &obj.display_name)?)?;
    t.set("render_type", objective_render_type_to_str(&obj.render_type))?;
    t.set("number_format", number_format_to_value(lua, &obj.number_format)?)?;
    Ok(t)
}

pub fn score_to_table(lua: &Lua, score: &Score) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("owner", score.owner.as_str())?;
    t.set("objective", score.objective.as_str())?;
    t.set("value", score.value)?;
    t.set("display_name", super::opt_text_component(lua, &score.display_name)?)?;
    t.set("number_format", number_format_to_value(lua, &score.number_format)?)?;
    Ok(t)
}

pub fn team_to_table(lua: &Lua, team: &Team) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("name", team.name.as_str())?;
    t.set("display_name", super::text_component(lua, &team.display_name)?)?;
    t.set("allows_friendly_fire", team.friendly_fire.allows_friendly_fire())?;
    t.set("see_friendly_invisibles", team.friendly_fire.see_friendly_invisibles())?;
    t.set("name_tag_visibility", name_tag_visibility_to_str(&team.name_tag_visibility))?;
    t.set("collision_rule", collision_rule_to_str(&team.collision_rule))?;
    t.set("color", team_color_to_str(&team.color))?;
    t.set("prefix", super::text_component(lua, &team.prefix)?)?;
    t.set("suffix", super::text_component(lua, &team.suffix)?)?;
    t.set("members", super::string_array(lua, team.members.iter())?)?;
    Ok(t)
}

pub fn scoreboard_state_to_table(lua: &Lua, state: &ScoreboardState) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let objectives = lua.create_table()?;
    for (name, obj) in &state.objectives {
        objectives.set(name.as_str(), objective_to_table(lua, obj)?)?;
    }
    t.set("objectives", objectives)?;

    let display_slots = lua.create_table()?;
    for (slot, objective) in &state.display_slots {
        display_slots.set(display_slot_to_str(slot), objective.as_str())?;
    }
    t.set("display_slots", display_slots)?;

    let scores = lua.create_table()?;
    for (key, score) in &state.scores {
        let composite = format!("{}\u{1}{}", key.objective, key.owner);
        scores.set(composite, score_to_table(lua, score)?)?;
    }
    t.set("scores", scores)?;

    let teams = lua.create_table()?;
    for (name, team) in &state.teams {
        teams.set(name.as_str(), team_to_table(lua, team)?)?;
    }
    t.set("teams", teams)?;

    Ok(t)
}

pub fn scoreboard_event_to_table(lua: &Lua, event: &ScoreboardEvent) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    match event {
        ScoreboardEvent::ObjectiveChanged {
            name,
            action,
            current,
            detached_slots,
            removed_scores,
            applied,
        } => {
            t.set("kind", "objective_changed")?;
            t.set("name", name.as_str())?;
            t.set(
                "action",
                match action {
                    ObjectiveAction::Create => "create",
                    ObjectiveAction::Remove => "remove",
                    ObjectiveAction::Update => "update",
                    ObjectiveAction::Unknown(_) => "unknown",
                },
            )?;
            t.set(
                "current",
                match current {
                    Some(obj) => Value::Table(objective_to_table(lua, obj)?),
                    None => Value::Nil,
                },
            )?;
            t.set("detached_slots", *detached_slots)?;
            t.set("removed_scores", *removed_scores)?;
            t.set("applied", *applied)?;
        }
        ScoreboardEvent::DisplaySlotChanged {
            slot,
            objective,
            applied,
        } => {
            t.set("kind", "display_slot_changed")?;
            t.set("slot", display_slot_to_str(slot))?;
            t.set("objective", super::opt_string(lua, objective)?)?;
            t.set("applied", *applied)?;
        }
        ScoreboardEvent::ScoreChanged {
            owner,
            objective,
            action,
            current,
            affected,
            applied,
        } => {
            t.set("kind", "score_changed")?;
            t.set("owner", owner.as_str())?;
            t.set("objective", super::opt_string(lua, objective)?)?;
            t.set(
                "action",
                match action {
                    ScoreAction::Set => "set",
                    ScoreAction::Reset => "reset",
                },
            )?;
            t.set(
                "current",
                match current {
                    Some(score) => Value::Table(score_to_table(lua, score)?),
                    None => Value::Nil,
                },
            )?;
            t.set("affected", *affected)?;
            t.set("applied", *applied)?;
        }
        ScoreboardEvent::TeamChanged {
            name,
            action,
            current,
            affected_members,
            rejected_members,
            applied,
        } => {
            t.set("kind", "team_changed")?;
            t.set("name", name.as_str())?;
            t.set(
                "action",
                match action {
                    TeamAction::Create => "create",
                    TeamAction::Remove => "remove",
                    TeamAction::Update => "update",
                    TeamAction::AddMembers => "add_members",
                    TeamAction::RemoveMembers => "remove_members",
                    TeamAction::Unknown(_) => "unknown",
                },
            )?;
            t.set(
                "current",
                match current {
                    Some(team) => Value::Table(team_to_table(lua, team)?),
                    None => Value::Nil,
                },
            )?;
            t.set("affected_members", *affected_members)?;
            t.set("rejected_members", *rejected_members)?;
            t.set("applied", *applied)?;
        }
    }
    Ok(t)
}
