//! A caller-friendly, read-only view of the currently open inventory/GUI
//! window — the public counterpart to [`crate::minecraft::inventory::Window`]
//! for external code that wants to *read* a menu's contents without pulling
//! in `minerider_protocol` types directly.
//!
//! Slot ordering is always raw protocol ordering (index `0` is whatever the
//! server's `window_items`/`set_slot` traffic calls index `0` — for a
//! chest-like window, upper-left, left-to-right then top-to-bottom, with the
//! player's own inventory slots following later in the same window); this
//! module never remaps indexes.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use minerider_protocol::generated::v1_21_4::types::{
    Slot, SlotComponent, SlotComponentData, SlotValue,
};
use minerider_protocol::nbt::Nbt;

use crate::minecraft::inventory::{InventoryState, Window};
use crate::minecraft::text::TextComponent;

/// One slot in a [`GuiView`], in raw zero-based protocol order.
#[derive(Debug, Clone, PartialEq)]
pub struct GuiSlotView {
    /// Raw zero-based index into the window's slot array — never remapped.
    pub index: usize,
    pub empty: bool,
    /// Numeric protocol item id, or `None` for an empty slot.
    pub item_id: Option<i32>,
    /// Registry name (e.g. `"minecraft:diamond_sword"`) for `item_id`.
    ///
    /// Always `None` in this phase: no item-id-to-name registry is vendored
    /// in this repository (only `blocks.json`/`blockCollisionShapes.json`
    /// are, under `crates/minerider-codegen/vendor/minecraft-data/pc/1.21.4/`
    /// — there is no `items.json`), and the mission that added this API
    /// explicitly forbids parsing a large registry file at runtime or
    /// hand-maintaining a partial id table. Vendoring the matching
    /// `items.json` and generating a static lookup at build time — the same
    /// pattern `minerider-codegen` already uses for the protocol itself —
    /// is the documented follow-up; see `docs/wrapper_api_readiness.md`.
    pub registry_name: Option<&'static str>,
    pub count: i32,
    /// Best-effort plain-text custom name, decoded from the `custom_name`
    /// data component when present.
    pub custom_name: Option<String>,
    /// Best-effort plain-text lore lines, decoded from the `lore` data
    /// component when present.
    pub lore: Option<Vec<String>>,
    /// Best-effort `(enchantment_id, level)` pairs, decoded from whichever
    /// of the `enchantments`/`stored_enchantments` components is present.
    /// Numeric ids only, for the same reason as `registry_name`.
    pub enchantments: Option<Vec<(i32, i32)>>,
    /// Every data component this slot carries, verbatim — including ones
    /// not specially decoded above (`custom_name`/`lore`/`enchantments` are
    /// convenience projections of a subset of this list, not a replacement
    /// for it) and any this generator doesn't specifically model.
    pub components: Vec<SlotComponent>,
}

impl GuiSlotView {
    /// `pub(crate)` (not private) so `crate::lua::convert::items` can build
    /// the same enriched view for `HudState.held_item`/`hotbar` (raw
    /// protocol `Slot`s, not `GuiSlotView`s) instead of duplicating this
    /// decode logic.
    pub(crate) fn from_slot(index: usize, slot: &Slot) -> Self {
        let SlotValue::Default(data) = &slot.value else {
            return GuiSlotView {
                index,
                empty: true,
                item_id: None,
                registry_name: None,
                count: slot.item_count,
                custom_name: None,
                lore: None,
                enchantments: None,
                components: Vec::new(),
            };
        };
        let mut custom_name = None;
        let mut lore: Option<Vec<String>> = None;
        let mut enchantments: Vec<(i32, i32)> = Vec::new();
        for component in &data.components {
            match &component.data {
                SlotComponentData::CustomName(nbt) => {
                    custom_name = Some(TextComponent::from_nbt(nbt).plain_text());
                }
                SlotComponentData::Lore(lines) => {
                    lore = Some(
                        lines
                            .iter()
                            .filter_map(|line| line.as_ref())
                            .map(|nbt| TextComponent::from_nbt(nbt).plain_text())
                            .collect(),
                    );
                }
                SlotComponentData::Enchantments(e) => {
                    enchantments.extend(e.enchantments.iter().map(|item| (item.id, item.level)));
                }
                SlotComponentData::StoredEnchantments(e) => {
                    enchantments.extend(e.enchantments.iter().map(|item| (item.id, item.level)));
                }
                _ => {}
            }
        }
        GuiSlotView {
            index,
            empty: false,
            item_id: Some(data.item_id),
            registry_name: None,
            count: slot.item_count,
            custom_name,
            lore,
            enchantments: (!enchantments.is_empty()).then_some(enchantments),
            components: data.components.clone(),
        }
    }
}

/// A caller-friendly, read-only view of one open window (the currently open
/// non-player container, or the player's own inventory when built via
/// [`crate::core::supervisor::SupervisorHandle::inventory_view`]).
#[derive(Debug, Clone, PartialEq)]
pub struct GuiView {
    pub window_id: i32,
    /// Raw registry id of the menu type (`minecraft:menu`); `0` for the
    /// player's own inventory, which has no menu-type packet of its own.
    pub menu_type: i32,
    /// The window title exactly as the server sent it (network NBT text
    /// component).
    pub title_raw: Nbt,
    /// The window title's flattened plain text.
    pub title_text: String,
    pub state_id: i32,
    /// Every slot in the window, in raw protocol order — see the module
    /// documentation for exactly what index `0` means for a chest-like
    /// window.
    pub slots: Vec<GuiSlotView>,
    /// The item on the shared cursor (drag-and-drop), not part of this
    /// window's own slot array.
    pub cursor: GuiSlotView,
    /// Container properties (furnace burn/cook progress, enchanting-table
    /// levels/costs, ...), keyed by the vanilla property index.
    pub properties: BTreeMap<i16, i16>,
    /// `true` if the server reported more slots than this view retained
    /// (see [`crate::minecraft::inventory::MAX_WINDOW_SLOTS`]).
    pub slots_truncated: bool,
}

impl GuiView {
    pub(crate) fn from_window(window: &Window, cursor: &Slot) -> Self {
        GuiView {
            window_id: window.id,
            menu_type: window.kind,
            title_raw: window.title.clone(),
            title_text: TextComponent::from_nbt(&window.title).plain_text(),
            state_id: window.state_id,
            slots: window
                .slots
                .iter()
                .enumerate()
                .map(|(index, slot)| GuiSlotView::from_slot(index, slot))
                .collect(),
            cursor: GuiSlotView::from_slot(0, cursor),
            properties: window.properties.clone(),
            slots_truncated: window.slots_truncated,
        }
    }

    /// A human-readable multi-line dump: one summary line, then one line per
    /// slot, then the cursor and any properties. Intended for interactive
    /// inspection/debugging, not machine parsing — the structured fields
    /// above are the stable contract.
    pub fn dump_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "window_id={} menu_type={} title={:?} state_id={} slots={} truncated={}",
            self.window_id,
            self.menu_type,
            self.title_text,
            self.state_id,
            self.slots.len(),
            self.slots_truncated,
        );
        for slot in &self.slots {
            let _ = writeln!(
                out,
                "  slot={} empty={} item_id={:?} registry_name={:?} count={} custom_name={:?} lore={:?} enchantments={:?}",
                slot.index,
                slot.empty,
                slot.item_id,
                slot.registry_name,
                slot.count,
                slot.custom_name,
                slot.lore,
                slot.enchantments,
            );
        }
        let _ = writeln!(
            out,
            "cursor: empty={} item_id={:?} count={}",
            self.cursor.empty, self.cursor.item_id, self.cursor.count
        );
        if !self.properties.is_empty() {
            let _ = writeln!(out, "properties: {:?}", self.properties);
        }
        out
    }
}

/// Builds a [`GuiView`] of the currently open non-player window, or `None`
/// if no container is open right now.
pub(crate) fn open_window_view(inventory: &InventoryState) -> Option<GuiView> {
    inventory
        .open_window
        .as_ref()
        .map(|window| GuiView::from_window(window, &inventory.cursor))
}

/// Builds a [`GuiView`] of the player's own inventory window (id
/// [`crate::minecraft::inventory::PLAYER_INVENTORY_WINDOW_ID`]), which is
/// always present.
pub(crate) fn player_inventory_view(inventory: &InventoryState) -> GuiView {
    GuiView::from_window(&inventory.player_inventory, &inventory.cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minecraft::inventory::{empty_slot, PLAYER_INVENTORY_WINDOW_ID};
    use minerider_protocol::generated::v1_21_4::play::{PacketOpenWindow, PacketWindowItems};
    use minerider_protocol::generated::v1_21_4::types::SlotValueDefault;

    fn item(item_id: i32, count: i32, components: Vec<SlotComponent>) -> Slot {
        Slot {
            item_count: count,
            value: SlotValue::Default(SlotValueDefault {
                item_id,
                added_component_count: components.len() as i32,
                removed_component_count: 0,
                components,
                remove_components: vec![],
            }),
        }
    }

    fn text_component_nbt(text: &str) -> Nbt {
        Nbt::Compound(vec![("text".to_string(), Nbt::String(text.to_string()))])
    }

    #[test]
    fn no_open_window_returns_none() {
        let inventory = InventoryState::new();
        assert!(open_window_view(&inventory).is_none());
    }

    #[test]
    fn open_window_view_preserves_raw_slot_order_and_zero_index() {
        let mut inventory = InventoryState::new();
        inventory.open_window(&PacketOpenWindow {
            window_id: 5,
            inventory_type: 2,
            window_title: text_component_nbt("Chest"),
        });
        inventory.window_items(&PacketWindowItems {
            window_id: 5,
            state_id: 3,
            items: vec![item(1, 1, vec![]), empty_slot(), item(2, 5, vec![])],
            carried_item: empty_slot(),
        });
        let view = open_window_view(&inventory).expect("window is open");
        assert_eq!(view.window_id, 5);
        assert_eq!(view.menu_type, 2);
        assert_eq!(view.title_text, "Chest");
        assert_eq!(view.state_id, 3);
        assert_eq!(view.slots.len(), 3);
        assert_eq!(view.slots[0].index, 0);
        assert!(!view.slots[0].empty);
        assert_eq!(view.slots[0].item_id, Some(1));
        assert_eq!(view.slots[0].count, 1);
        assert_eq!(view.slots[1].index, 1);
        assert!(view.slots[1].empty);
        assert_eq!(view.slots[1].item_id, None);
        assert_eq!(view.slots[2].index, 2);
        assert_eq!(view.slots[2].item_id, Some(2));
        assert_eq!(view.slots[2].count, 5);
    }

    #[test]
    fn custom_name_lore_and_enchantments_decode_best_effort() {
        let components = vec![
            SlotComponent {
                r#type: minerider_protocol::generated::v1_21_4::types::SlotComponentType::CustomName,
                data: SlotComponentData::CustomName(text_component_nbt("Excalibur")),
            },
            SlotComponent {
                r#type: minerider_protocol::generated::v1_21_4::types::SlotComponentType::Lore,
                data: SlotComponentData::Lore(vec![
                    Some(text_component_nbt("A legendary blade")),
                    None,
                ]),
            },
            SlotComponent {
                r#type: minerider_protocol::generated::v1_21_4::types::SlotComponentType::Enchantments,
                data: SlotComponentData::Enchantments(
                    minerider_protocol::generated::v1_21_4::types::SlotComponentDataEnchantments {
                        enchantments: vec![
                            minerider_protocol::generated::v1_21_4::types::SlotComponentDataEnchantmentsEnchantmentsItem {
                                id: 9,
                                level: 3,
                            },
                        ],
                        show_tooltip: true,
                    },
                ),
            },
        ];
        let slot = item(276, 1, components.clone());
        let view = GuiSlotView::from_slot(0, &slot);
        assert_eq!(view.custom_name.as_deref(), Some("Excalibur"));
        assert_eq!(view.lore, Some(vec!["A legendary blade".to_string()]));
        assert_eq!(view.enchantments, Some(vec![(9, 3)]));
        assert_eq!(
            view.components, components,
            "raw components preserved verbatim"
        );
    }

    #[test]
    fn unknown_component_is_preserved_but_not_specially_decoded() {
        let components = vec![SlotComponent {
            r#type: minerider_protocol::generated::v1_21_4::types::SlotComponentType::Damage,
            data: SlotComponentData::Damage(7),
        }];
        let slot = item(1, 1, components.clone());
        let view = GuiSlotView::from_slot(0, &slot);
        assert_eq!(view.custom_name, None);
        assert_eq!(view.lore, None);
        assert_eq!(view.enchantments, None);
        assert_eq!(view.components, components);
    }

    #[test]
    fn empty_slot_view_has_no_item_data() {
        let view = GuiSlotView::from_slot(4, &empty_slot());
        assert!(view.empty);
        assert_eq!(view.item_id, None);
        assert_eq!(view.count, 0);
        assert_eq!(view.custom_name, None);
    }

    #[test]
    fn registry_name_is_documented_as_unavailable() {
        let view = GuiSlotView::from_slot(0, &item(1, 1, vec![]));
        assert_eq!(
            view.registry_name, None,
            "no vendored item registry in this phase; see the field doc comment"
        );
    }

    #[test]
    fn dump_text_includes_header_and_every_slot() {
        let mut inventory = InventoryState::new();
        inventory.open_window(&PacketOpenWindow {
            window_id: 5,
            inventory_type: 2,
            window_title: text_component_nbt("Chest"),
        });
        inventory.window_items(&PacketWindowItems {
            window_id: 5,
            state_id: 1,
            items: vec![item(1, 1, vec![])],
            carried_item: empty_slot(),
        });
        let view = open_window_view(&inventory).unwrap();
        let dump = view.dump_text();
        assert!(dump.contains("window_id=5"));
        assert!(dump.contains("slot=0"));
    }

    #[test]
    fn player_inventory_view_uses_the_player_inventory_window_id() {
        let inventory = InventoryState::new();
        let view = player_inventory_view(&inventory);
        assert_eq!(view.window_id, PLAYER_INVENTORY_WINDOW_ID);
    }
}
