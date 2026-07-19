//! `GuiSlotView`/raw `Slot` → Lua table conversion.
//!
//! Every slot table exposes both `slot.lua_index` (1-based, for natural Lua
//! iteration) and `slot.raw_slot` (the true zero-based protocol slot index
//! — never silently shifted). `registry_name` stays `nil`: no item-id
//! registry is vendored in this repository (see
//! `crate::minecraft::gui::GuiSlotView::registry_name`'s doc comment) —
//! this wrapper does not add hand-maintained partial data or parse a
//! registry file at runtime, consistent with the mission's constraints.

use mlua::{Lua, Table, Value};

use minerider_protocol::generated::v1_21_4::types::{Slot, SlotComponent, SlotComponentData};

use crate::minecraft::gui::GuiSlotView;

/// Converts a raw protocol `Slot` (e.g. `HudState.held_item`, one entry of
/// `HudState.hotbar`) into the same enriched table shape a `GuiView` slot
/// gets, reusing `GuiSlotView::from_slot` rather than re-decoding
/// components by hand.
pub fn raw_slot_to_table(lua: &Lua, index: usize, slot: &Slot) -> mlua::Result<Table> {
    let view = GuiSlotView::from_slot(index, slot);
    gui_slot_view_to_table(lua, &view)
}

pub fn gui_slot_view_to_table(lua: &Lua, view: &GuiSlotView) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("raw_slot", view.index as i64)?;
    t.set("lua_index", view.index as i64 + 1)?;
    t.set("empty", view.empty)?;
    t.set("item_id", opt_i64(view.item_id.map(|v| v as i64)))?;
    t.set("registry_name", Value::Nil)?;
    t.set("count", view.count)?;
    t.set(
        "custom_name",
        match &view.custom_name {
            Some(s) => Value::String(lua.create_string(s)?),
            None => Value::Nil,
        },
    )?;
    match &view.lore {
        Some(lines) => {
            let arr = lua.create_table()?;
            for (i, line) in lines.iter().enumerate() {
                arr.set(i + 1, line.as_str())?;
            }
            t.set("lore", arr)?;
        }
        None => t.set("lore", Value::Nil)?,
    }
    match &view.enchantments {
        Some(list) => {
            let arr = lua.create_table()?;
            for (i, (id, level)) in list.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("id", *id)?;
                entry.set("level", *level)?;
                arr.set(i + 1, entry)?;
            }
            t.set("enchantments", arr)?;
        }
        None => t.set("enchantments", Value::Nil)?,
    }
    t.set("components", components_to_table(lua, &view.components)?)?;
    Ok(t)
}

/// Every raw data component, verbatim — a bounded safe form for components
/// not specially decoded into `custom_name`/`lore`/`enchantments` above.
/// Each entry is `{type = "<ComponentType>", data = <best-effort string>}`;
/// unknown/complex payloads fall back to their `Debug` representation
/// rather than being dropped, so scripts can at least detect presence.
fn components_to_table(lua: &Lua, components: &[SlotComponent]) -> mlua::Result<Table> {
    let arr = lua.create_table()?;
    for (i, component) in components.iter().enumerate() {
        let entry = lua.create_table()?;
        entry.set("type", format!("{:?}", component.r#type))?;
        entry.set("data", component_data_summary(&component.data))?;
        arr.set(i + 1, entry)?;
    }
    Ok(arr)
}

fn component_data_summary(data: &SlotComponentData) -> String {
    match data {
        SlotComponentData::Damage(v) => v.to_string(),
        SlotComponentData::MaxDamage(v) => v.to_string(),
        SlotComponentData::MaxStackSize(v) => v.to_string(),
        SlotComponentData::CustomName(_) | SlotComponentData::ItemName(_) => {
            "<text component>".to_string()
        }
        SlotComponentData::Lore(lines) => format!("<{} lore lines>", lines.len()),
        SlotComponentData::Enchantments(e) => format!("<{} enchantments>", e.enchantments.len()),
        SlotComponentData::StoredEnchantments(e) => {
            format!("<{} stored enchantments>", e.enchantments.len())
        }
        other => format!("{other:?}"),
    }
}

fn opt_i64(v: Option<i64>) -> Value {
    match v {
        Some(v) => Value::Integer(v),
        None => Value::Nil,
    }
}
