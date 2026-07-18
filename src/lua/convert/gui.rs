//! `GuiView` → Lua table conversion, used by `bot:open_gui()`/
//! `bot:inventory()` and the `gui_opened` event payload.

use mlua::{Lua, Table, Value};

use crate::minecraft::gui::GuiView;

pub fn gui_view_to_table(lua: &Lua, view: &GuiView) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("window_id", view.window_id)?;
    t.set("menu_type", view.menu_type)?;
    t.set("title", view.title_text.as_str())?;
    t.set("state_id", view.state_id)?;
    let slots = lua.create_table()?;
    for (i, slot) in view.slots.iter().enumerate() {
        slots.set(i + 1, super::items::gui_slot_view_to_table(lua, slot)?)?;
    }
    t.set("slots", slots)?;
    t.set(
        "cursor",
        super::items::gui_slot_view_to_table(lua, &view.cursor)?,
    )?;
    let properties = lua.create_table()?;
    for (k, v) in &view.properties {
        properties.set(*k, *v)?;
    }
    t.set("properties", properties)?;
    t.set("slots_truncated", view.slots_truncated)?;
    Ok(t)
}

pub fn opt_gui_view_to_value(lua: &Lua, view: Option<&GuiView>) -> mlua::Result<Value> {
    match view {
        Some(v) => Ok(Value::Table(gui_view_to_table(lua, v)?)),
        None => Ok(Value::Nil),
    }
}
