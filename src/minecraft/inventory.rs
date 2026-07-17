//! Inventory/container ("GUI") state: the player's own inventory, at most one
//! currently open non-player container, and the item on the cursor.
//!
//! A pure projection of the clientbound container packets, mirroring
//! `entity.rs`/`world.rs`: no network, fully unit-tested. This tracks state
//! only — sending `window_click` to actually interact with a menu is a
//! separate, deliberately unimplemented follow-up (each menu type has its own
//! slot semantics: crafting output, anvil rename+repair-cost, enchanting
//! table levels, ...).

use std::collections::HashMap;

use minerider_protocol::generated::v1_21_4::play::{
    PacketCloseWindow, PacketCraftProgressBar, PacketHeldItemSlot, PacketOpenWindow,
    PacketSetCursorItem, PacketSetSlot, PacketWindowItems,
};
use minerider_protocol::generated::v1_21_4::types::{Slot, SlotValue};
use minerider_protocol::nbt::Nbt;

/// The player's own inventory window id: always open, never closed by the
/// server, and absent from `open_window`/`close_window` traffic.
pub const PLAYER_INVENTORY_WINDOW_ID: i32 = 0;

/// An empty item slot (`item_count: 0`), matching what the wire sends for a
/// slot with nothing in it.
pub fn empty_slot() -> Slot {
    Slot {
        item_count: 0,
        value: SlotValue::V0,
    }
}

/// One open inventory/container window: the player's own inventory (id
/// [`PLAYER_INVENTORY_WINDOW_ID`], always present) or a server-opened menu
/// (chest, furnace, crafting table, anvil, ...).
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub id: i32,
    /// Registry id of the menu type (`minecraft:menu`); meaningless (0) for
    /// the player's own inventory, which has no menu-type packet of its own.
    pub kind: i32,
    pub title: Nbt,
    /// Server's per-window revision counter. Interactions must echo the
    /// latest value to avoid desync; tracked now for when `window_click`
    /// support is added.
    pub state_id: i32,
    pub slots: Vec<Slot>,
    /// Container properties (furnace burn/cook progress, enchanting-table
    /// levels and costs, ...), keyed by the vanilla property index.
    pub properties: HashMap<i16, i16>,
}

impl Window {
    fn player_inventory() -> Self {
        Self {
            id: PLAYER_INVENTORY_WINDOW_ID,
            kind: 0,
            title: Nbt::Compound(vec![]),
            state_id: 0,
            slots: Vec::new(),
            properties: HashMap::new(),
        }
    }

    fn from_open(p: &PacketOpenWindow) -> Self {
        Self {
            id: p.window_id,
            kind: p.inventory_type,
            title: p.window_title.clone(),
            state_id: 0,
            slots: Vec::new(),
            properties: HashMap::new(),
        }
    }

    /// Applies one `set_slot` index update, growing the slot list with empty
    /// slots if the index arrives before a full `window_items` snapshot
    /// (server-driven data, so we grow rather than treat it as fatal).
    fn set_slot(&mut self, index: i16, item: Slot) {
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        if index >= self.slots.len() {
            self.slots.resize(index + 1, empty_slot());
        }
        self.slots[index] = item;
    }
}

/// Aggregate inventory/GUI state: the player's own inventory, at most one
/// open container, and the single shared cursor item.
#[derive(Debug, Clone, PartialEq)]
pub struct InventoryState {
    pub player_inventory: Window,
    /// The currently open non-player container, if any. Opening a new window
    /// implicitly replaces whatever was open, exactly as vanilla does.
    pub open_window: Option<Window>,
    /// The item currently held on the cursor (drag-and-drop), shared across
    /// every window.
    pub cursor: Slot,
    /// The hotbar slot (0-8) the server reports as selected.
    pub selected_hotbar_slot: i32,
}

impl InventoryState {
    pub fn new() -> Self {
        Self {
            player_inventory: Window::player_inventory(),
            open_window: None,
            cursor: empty_slot(),
            selected_hotbar_slot: 0,
        }
    }

    fn window_mut(&mut self, window_id: i32) -> Option<&mut Window> {
        if window_id == PLAYER_INVENTORY_WINDOW_ID {
            Some(&mut self.player_inventory)
        } else if self.open_window.as_ref().is_some_and(|w| w.id == window_id) {
            self.open_window.as_mut()
        } else {
            None
        }
    }

    /// Applies `open_window`.
    pub fn open_window(&mut self, p: &PacketOpenWindow) {
        self.open_window = Some(Window::from_open(p));
    }

    /// Applies `close_window`. A close for a window id that is not (or is no
    /// longer) the open one is a stale/no-op, not an error.
    pub fn close_window(&mut self, p: &PacketCloseWindow) {
        if self
            .open_window
            .as_ref()
            .is_some_and(|w| w.id == p.window_id)
        {
            self.open_window = None;
        }
    }

    /// Applies `window_items`: a full slot refresh for one window plus the
    /// cursor item it carries. A refresh for a window id we don't currently
    /// recognize (already closed/replaced) is ignored rather than fatal.
    pub fn window_items(&mut self, p: &PacketWindowItems) {
        if let Some(window) = self.window_mut(p.window_id) {
            window.slots = p.items.clone();
            window.state_id = p.state_id;
        }
        self.cursor = p.carried_item.clone();
    }

    /// Applies `set_slot`. Window id `-1` is the legacy cursor-slot address,
    /// superseded by `set_cursor_item` but still valid wire.
    pub fn set_slot(&mut self, p: &PacketSetSlot) {
        if p.window_id == -1 {
            self.cursor = p.item.clone();
            return;
        }
        if let Some(window) = self.window_mut(p.window_id) {
            window.state_id = p.state_id;
            window.set_slot(p.slot, p.item.clone());
        }
    }

    /// Applies `set_cursor_item`: the authoritative cursor-item update.
    pub fn set_cursor_item(&mut self, p: &PacketSetCursorItem) {
        self.cursor = p.contents.clone();
    }

    /// Applies `craft_progress_bar`: a container property (furnace progress,
    /// enchanting-table levels/costs, ...) on the addressed window.
    pub fn craft_progress_bar(&mut self, p: &PacketCraftProgressBar) {
        if let Some(window) = self.window_mut(p.window_id) {
            window.properties.insert(p.property, p.value);
        }
    }

    /// Applies `held_item_slot`: the server-selected hotbar slot.
    pub fn held_item_slot(&mut self, p: &PacketHeldItemSlot) {
        self.selected_hotbar_slot = p.slot;
    }
}

impl Default for InventoryState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(item_id: i32, count: i32) -> Slot {
        Slot {
            item_count: count,
            value: SlotValue::Default(
                minerider_protocol::generated::v1_21_4::types::SlotValueDefault {
                    item_id,
                    added_component_count: 0,
                    removed_component_count: 0,
                    components: vec![],
                    remove_components: vec![],
                },
            ),
        }
    }

    #[test]
    fn player_inventory_exists_from_the_start() {
        let inv = InventoryState::new();
        assert_eq!(inv.player_inventory.id, PLAYER_INVENTORY_WINDOW_ID);
        assert!(inv.open_window.is_none());
        assert_eq!(inv.cursor, empty_slot());
    }

    #[test]
    fn window_items_fills_player_inventory() {
        let mut inv = InventoryState::new();
        inv.window_items(&PacketWindowItems {
            window_id: 0,
            state_id: 3,
            items: vec![item(1, 5), empty_slot()],
            carried_item: empty_slot(),
        });
        assert_eq!(inv.player_inventory.state_id, 3);
        assert_eq!(inv.player_inventory.slots.len(), 2);
        assert_eq!(inv.player_inventory.slots[0], item(1, 5));
    }

    #[test]
    fn open_window_replaces_any_previous_container() {
        let mut inv = InventoryState::new();
        inv.open_window(&PacketOpenWindow {
            window_id: 5,
            inventory_type: 2,
            window_title: Nbt::Compound(vec![]),
        });
        assert_eq!(inv.open_window.as_ref().unwrap().id, 5);

        inv.open_window(&PacketOpenWindow {
            window_id: 7,
            inventory_type: 9,
            window_title: Nbt::Compound(vec![]),
        });
        assert_eq!(
            inv.open_window.as_ref().unwrap().id,
            7,
            "opening a new window replaces the old one"
        );
    }

    #[test]
    fn close_window_only_clears_matching_id() {
        let mut inv = InventoryState::new();
        inv.open_window(&PacketOpenWindow {
            window_id: 5,
            inventory_type: 2,
            window_title: Nbt::Compound(vec![]),
        });
        // A stale close for a window we've already moved past is a no-op.
        inv.close_window(&PacketCloseWindow { window_id: 4 });
        assert!(inv.open_window.is_some());

        inv.close_window(&PacketCloseWindow { window_id: 5 });
        assert!(inv.open_window.is_none());
    }

    #[test]
    fn set_slot_updates_one_index_and_grows_as_needed() {
        let mut inv = InventoryState::new();
        inv.window_items(&PacketWindowItems {
            window_id: 0,
            state_id: 1,
            items: vec![empty_slot(); 3],
            carried_item: empty_slot(),
        });
        inv.set_slot(&PacketSetSlot {
            window_id: 0,
            state_id: 2,
            slot: 1,
            item: item(64, 1),
        });
        assert_eq!(inv.player_inventory.slots[1], item(64, 1));
        assert_eq!(inv.player_inventory.state_id, 2);

        // An index past the known length grows the window instead of panicking.
        inv.set_slot(&PacketSetSlot {
            window_id: 0,
            state_id: 3,
            slot: 10,
            item: item(2, 1),
        });
        assert_eq!(inv.player_inventory.slots.len(), 11);
        assert_eq!(inv.player_inventory.slots[10], item(2, 1));
    }

    #[test]
    fn set_slot_for_unknown_window_is_ignored() {
        let mut inv = InventoryState::new();
        inv.set_slot(&PacketSetSlot {
            window_id: 9,
            state_id: 1,
            slot: 0,
            item: item(1, 1),
        });
        assert!(
            inv.open_window.is_none(),
            "no window was created out of thin air"
        );
    }

    #[test]
    fn set_slot_legacy_cursor_address_updates_cursor() {
        let mut inv = InventoryState::new();
        inv.set_slot(&PacketSetSlot {
            window_id: -1,
            state_id: 0,
            slot: -1,
            item: item(3, 2),
        });
        assert_eq!(inv.cursor, item(3, 2));
    }

    #[test]
    fn set_cursor_item_updates_cursor() {
        let mut inv = InventoryState::new();
        inv.set_cursor_item(&PacketSetCursorItem {
            contents: item(9, 1),
        });
        assert_eq!(inv.cursor, item(9, 1));
    }

    #[test]
    fn craft_progress_bar_tracks_properties_on_open_window() {
        let mut inv = InventoryState::new();
        inv.open_window(&PacketOpenWindow {
            window_id: 5,
            inventory_type: 13, // furnace
            window_title: Nbt::Compound(vec![]),
        });
        inv.craft_progress_bar(&PacketCraftProgressBar {
            window_id: 5,
            property: 0,
            value: 150,
        });
        assert_eq!(inv.open_window.unwrap().properties.get(&0), Some(&150));
    }

    #[test]
    fn craft_progress_bar_for_unopened_window_is_ignored() {
        let mut inv = InventoryState::new();
        inv.craft_progress_bar(&PacketCraftProgressBar {
            window_id: 5,
            property: 0,
            value: 150,
        });
        assert!(inv.open_window.is_none());
    }

    #[test]
    fn held_item_slot_tracks_selection() {
        let mut inv = InventoryState::new();
        inv.held_item_slot(&PacketHeldItemSlot { slot: 4 });
        assert_eq!(inv.selected_hotbar_slot, 4);
    }
}
