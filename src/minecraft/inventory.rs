//! Inventory/container ("GUI") state: the player's own inventory, at most one
//! currently open non-player container, and the item on the cursor.
//!
//! Clientbound packets remain authoritative: outbound clicks are recorded as
//! bounded transactions but never mutate slots optimistically. A later
//! `set_slot`/`window_items` update confirms or corrects them.

use std::collections::{BTreeMap, VecDeque};

use minerider_protocol::generated::v1_21_4::play::{
    PacketCloseWindow, PacketCraftProgressBar, PacketHeldItemSlot, PacketOpenWindow,
    PacketSetCursorItem, PacketSetPlayerInventory, PacketSetSlot, PacketWindowClick,
    PacketWindowItems,
};
use minerider_protocol::generated::v1_21_4::types::{Slot, SlotValue};
use minerider_protocol::nbt::Nbt;

/// The player's own inventory window id: always open, never closed by the
/// server, and absent from `open_window`/`close_window` traffic.
pub const PLAYER_INVENTORY_WINDOW_ID: i32 = 0;
pub const MAX_WINDOW_SLOTS: usize = 1_024;
pub const MAX_WINDOW_PROPERTIES: usize = 1_024;
pub const MAX_PENDING_TRANSACTIONS: usize = 64;
pub const MAX_COMPLETED_TRANSACTIONS: usize = 128;
pub const TRANSACTION_TIMEOUT_TICKS: u64 = 100;

const OUTSIDE_SLOT: i16 = -999;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotbarDestination {
    Slot(u8),
    Offhand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragPhase {
    Start,
    AddSlot,
    End,
}

/// Typed protocol-769 container click modes. Slot-bearing variants use the
/// current window's zero-based slot index; only `Pickup::outside` addresses
/// the special outside slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventoryClick {
    Pickup {
        slot: Option<i16>,
        button: MouseButton,
    },
    QuickMove {
        slot: i16,
    },
    HotbarSwap {
        slot: i16,
        destination: HotbarDestination,
    },
    Clone {
        slot: i16,
    },
    Throw {
        slot: i16,
        whole_stack: bool,
    },
    QuickCraft {
        phase: DragPhase,
        button: DragButton,
        slot: Option<i16>,
    },
    PickupAll {
        slot: i16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InventoryClickRequest {
    pub transaction_id: u64,
    pub generation: u64,
    pub window_id: i32,
    pub state_id: i32,
    pub click: InventoryClick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InventoryError {
    #[error("window {window_id} is not open")]
    UnknownWindow { window_id: i32 },
    #[error("window state is stale: requested {requested}, current {current}")]
    StaleState { requested: i32, current: i32 },
    #[error("slot {slot} is outside the current window's {slot_count} slots")]
    InvalidSlot { slot: i16, slot_count: usize },
    #[error("hotbar destination {slot} is outside 0..=8")]
    InvalidHotbarSlot { slot: u8 },
    #[error("middle-click clone and drag require creative mode")]
    CreativeOnly,
    #[error("drag phase does not match the current drag lifecycle")]
    InvalidDragPhase,
    #[error("another drag lifecycle is already active")]
    DragInProgress,
    #[error("inventory transaction id {transaction_id} already exists")]
    DuplicateTransaction { transaction_id: u64 },
    #[error("the bounded inventory transaction queue is full")]
    QueueFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventoryTransactionStatus {
    Queued,
    Sent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InventoryTransaction {
    pub request: InventoryClickRequest,
    pub status: InventoryTransactionStatus,
    pub queued_tick: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventoryOutcome {
    /// A phase that has no server confirmation of its own was sent.
    Sent,
    /// A newer incremental authoritative state update followed the click.
    Confirmed {
        state_id: i32,
    },
    /// A full authoritative window resynchronization followed the click.
    Corrected {
        state_id: i32,
    },
    TimedOut,
    WindowClosed,
    Rejected(InventoryError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventoryEvent {
    WindowOpened {
        window_id: i32,
    },
    WindowClosed {
        window_id: i32,
    },
    WindowSynchronized {
        window_id: i32,
        state_id: i32,
    },
    SlotUpdated {
        window_id: i32,
        state_id: i32,
        slot: i16,
    },
    CursorUpdated,
    PropertyUpdated {
        window_id: i32,
        property: i16,
    },
    SelectedHotbarChanged {
        slot: i32,
        applied: bool,
    },
    TransactionQueued {
        transaction_id: u64,
    },
    TransactionSent {
        transaction_id: u64,
    },
    TransactionFinished {
        transaction_id: u64,
        outcome: InventoryOutcome,
    },
    TransactionRejected {
        transaction_id: u64,
        error: InventoryError,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DragState {
    pub button: DragButton,
}

#[derive(Debug, PartialEq)]
pub(crate) struct PreparedClick {
    pub packet: PacketWindowClick,
    pub event: InventoryEvent,
}

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
    pub slots_truncated: bool,
    /// Container properties (furnace burn/cook progress, enchanting-table
    /// levels and costs, ...), keyed by the vanilla property index.
    pub properties: BTreeMap<i16, i16>,
}

impl Window {
    fn player_inventory() -> Self {
        Self {
            id: PLAYER_INVENTORY_WINDOW_ID,
            kind: 0,
            title: Nbt::Compound(vec![]),
            state_id: 0,
            slots: Vec::new(),
            slots_truncated: false,
            properties: BTreeMap::new(),
        }
    }

    fn from_open(p: &PacketOpenWindow) -> Self {
        Self {
            id: p.window_id,
            kind: p.inventory_type,
            title: p.window_title.clone(),
            state_id: 0,
            slots: Vec::new(),
            slots_truncated: false,
            properties: BTreeMap::new(),
        }
    }

    /// Applies one `set_slot` index update, growing the slot list with empty
    /// slots if the index arrives before a full `window_items` snapshot
    /// (server-driven data, so we grow rather than treat it as fatal).
    fn set_slot(&mut self, index: i16, item: Slot) -> bool {
        let Ok(index) = usize::try_from(index) else {
            return false;
        };
        if index >= MAX_WINDOW_SLOTS {
            return false;
        }
        if index >= self.slots.len() {
            self.slots.resize(index + 1, empty_slot());
        }
        self.slots[index] = item;
        true
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
    /// Bounded in-flight click transactions, in submission order.
    pub pending_transactions: VecDeque<InventoryTransaction>,
    /// Bounded terminal outcomes retained so lagged event consumers can
    /// recover from a fresh snapshot.
    pub completed_transactions: BTreeMap<u64, InventoryOutcome>,
    pub drag: Option<DragState>,
}

impl InventoryState {
    pub fn new() -> Self {
        Self {
            player_inventory: Window::player_inventory(),
            open_window: None,
            cursor: empty_slot(),
            selected_hotbar_slot: 0,
            pending_transactions: VecDeque::new(),
            completed_transactions: BTreeMap::new(),
            drag: None,
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

    pub fn window(&self, window_id: i32) -> Option<&Window> {
        if window_id == PLAYER_INVENTORY_WINDOW_ID {
            Some(&self.player_inventory)
        } else {
            self.open_window
                .as_ref()
                .filter(|window| window.id == window_id)
        }
    }

    /// Applies `open_window`.
    pub fn open_window(&mut self, p: &PacketOpenWindow) -> Vec<InventoryEvent> {
        let mut events = Vec::new();
        if let Some(previous) = self.open_window.take() {
            events.push(InventoryEvent::WindowClosed {
                window_id: previous.id,
            });
            events.extend(
                self.finish_window_transactions(previous.id, InventoryOutcome::WindowClosed),
            );
        }
        self.open_window = Some(Window::from_open(p));
        self.drag = None;
        events.push(InventoryEvent::WindowOpened {
            window_id: p.window_id,
        });
        events
    }

    /// Applies `close_window`. A close for a window id that is not (or is no
    /// longer) the open one is a stale/no-op, not an error.
    pub fn close_window(&mut self, p: &PacketCloseWindow) -> Vec<InventoryEvent> {
        if self
            .open_window
            .as_ref()
            .is_some_and(|w| w.id == p.window_id)
        {
            self.open_window = None;
            self.drag = None;
            let mut events = vec![InventoryEvent::WindowClosed {
                window_id: p.window_id,
            }];
            events.extend(
                self.finish_window_transactions(p.window_id, InventoryOutcome::WindowClosed),
            );
            return events;
        }
        Vec::new()
    }

    /// Applies `window_items`: a full slot refresh for one window plus the
    /// cursor item it carries. A refresh for a window id we don't currently
    /// recognize (already closed/replaced) is ignored rather than fatal.
    pub fn window_items(&mut self, p: &PacketWindowItems) -> Vec<InventoryEvent> {
        let mut applied = false;
        if let Some(window) = self.window_mut(p.window_id) {
            window.slots_truncated = p.items.len() > MAX_WINDOW_SLOTS;
            window.slots = p.items.iter().take(MAX_WINDOW_SLOTS).cloned().collect();
            window.state_id = p.state_id;
            applied = true;
        }
        self.cursor = p.carried_item.clone();
        let mut events = vec![InventoryEvent::CursorUpdated];
        if applied {
            events.push(InventoryEvent::WindowSynchronized {
                window_id: p.window_id,
                state_id: p.state_id,
            });
            events.extend(self.finish_window_transactions(
                p.window_id,
                InventoryOutcome::Corrected {
                    state_id: p.state_id,
                },
            ));
        }
        events
    }

    /// Applies `set_slot`. Window id `-1` is the legacy cursor-slot address,
    /// superseded by `set_cursor_item` but still valid wire.
    pub fn set_slot(&mut self, p: &PacketSetSlot) -> Vec<InventoryEvent> {
        if p.window_id == -1 {
            self.cursor = p.item.clone();
            return vec![InventoryEvent::CursorUpdated];
        }
        let mut applied = false;
        if let Some(window) = self.window_mut(p.window_id) {
            window.state_id = p.state_id;
            applied = window.set_slot(p.slot, p.item.clone());
        }
        if !applied {
            return Vec::new();
        }
        let mut events = vec![InventoryEvent::SlotUpdated {
            window_id: p.window_id,
            state_id: p.state_id,
            slot: p.slot,
        }];
        events.extend(self.finish_confirmed_transactions(p.window_id, p.state_id));
        events
    }

    /// Applies `set_cursor_item`: the authoritative cursor-item update.
    pub fn set_cursor_item(&mut self, p: &PacketSetCursorItem) -> InventoryEvent {
        self.cursor = p.contents.clone();
        InventoryEvent::CursorUpdated
    }

    /// Applies the protocol-769 direct player-inventory slot update.
    pub fn set_player_inventory(&mut self, p: &PacketSetPlayerInventory) -> Vec<InventoryEvent> {
        let Ok(slot) = i16::try_from(p.slot_id) else {
            return Vec::new();
        };
        if !self.player_inventory.set_slot(slot, p.contents.clone()) {
            return Vec::new();
        }
        vec![InventoryEvent::SlotUpdated {
            window_id: PLAYER_INVENTORY_WINDOW_ID,
            state_id: self.player_inventory.state_id,
            slot,
        }]
    }

    /// Applies `craft_progress_bar`: a container property (furnace progress,
    /// enchanting-table levels/costs, ...) on the addressed window.
    pub fn craft_progress_bar(&mut self, p: &PacketCraftProgressBar) -> Option<InventoryEvent> {
        if let Some(window) = self.window_mut(p.window_id) {
            if window.properties.contains_key(&p.property)
                || window.properties.len() < MAX_WINDOW_PROPERTIES
            {
                window.properties.insert(p.property, p.value);
                return Some(InventoryEvent::PropertyUpdated {
                    window_id: p.window_id,
                    property: p.property,
                });
            }
        }
        None
    }

    /// Applies `held_item_slot`: the server-selected hotbar slot.
    pub fn held_item_slot(&mut self, p: &PacketHeldItemSlot) -> InventoryEvent {
        let applied = (0..=8).contains(&p.slot);
        if applied {
            self.selected_hotbar_slot = p.slot;
        }
        InventoryEvent::SelectedHotbarChanged {
            slot: self.selected_hotbar_slot,
            applied,
        }
    }

    pub(crate) fn prepare_click(
        &mut self,
        request: InventoryClickRequest,
        tick: u64,
        creative_mode: bool,
    ) -> Result<PreparedClick, InventoryError> {
        if self.pending_transactions.len() >= MAX_PENDING_TRANSACTIONS {
            return Err(InventoryError::QueueFull);
        }
        if self
            .pending_transactions
            .iter()
            .any(|transaction| transaction.request.transaction_id == request.transaction_id)
            || self
                .completed_transactions
                .contains_key(&request.transaction_id)
        {
            return Err(InventoryError::DuplicateTransaction {
                transaction_id: request.transaction_id,
            });
        }
        let window = self
            .window(request.window_id)
            .ok_or(InventoryError::UnknownWindow {
                window_id: request.window_id,
            })?;
        if request.state_id != window.state_id {
            return Err(InventoryError::StaleState {
                requested: request.state_id,
                current: window.state_id,
            });
        }
        let slot_count = window.slots.len();
        let (slot, mouse_button, mode) =
            self.validate_click(request.click, slot_count, creative_mode)?;
        let packet = PacketWindowClick {
            window_id: request.window_id,
            state_id: request.state_id,
            slot,
            mouse_button,
            mode,
            // MineRider never applies guessed local mutations. The server's
            // next authoritative update is retained as confirmation or a
            // correction instead.
            changed_slots: Vec::new(),
            cursor_item: self.cursor.clone(),
        };
        self.pending_transactions.push_back(InventoryTransaction {
            request,
            status: InventoryTransactionStatus::Queued,
            queued_tick: tick,
        });
        Ok(PreparedClick {
            packet,
            event: InventoryEvent::TransactionQueued {
                transaction_id: request.transaction_id,
            },
        })
    }

    fn validate_click(
        &mut self,
        click: InventoryClick,
        slot_count: usize,
        creative_mode: bool,
    ) -> Result<(i16, i8, i32), InventoryError> {
        if !matches!(click, InventoryClick::QuickCraft { .. }) && self.drag.is_some() {
            return Err(InventoryError::DragInProgress);
        }
        match click {
            InventoryClick::Pickup { slot, button } => Ok((
                validate_optional_slot(slot, slot_count)?,
                mouse_button(button),
                0,
            )),
            InventoryClick::QuickMove { slot } => {
                validate_slot(slot, slot_count)?;
                Ok((slot, 0, 1))
            }
            InventoryClick::HotbarSwap { slot, destination } => {
                validate_slot(slot, slot_count)?;
                let button = match destination {
                    HotbarDestination::Slot(slot @ 0..=8) => slot as i8,
                    HotbarDestination::Slot(slot) => {
                        return Err(InventoryError::InvalidHotbarSlot { slot });
                    }
                    HotbarDestination::Offhand => 40,
                };
                Ok((slot, button, 2))
            }
            InventoryClick::Clone { slot } => {
                validate_slot(slot, slot_count)?;
                if !creative_mode {
                    return Err(InventoryError::CreativeOnly);
                }
                Ok((slot, 2, 3))
            }
            InventoryClick::Throw { slot, whole_stack } => {
                validate_slot(slot, slot_count)?;
                Ok((slot, if whole_stack { 1 } else { 0 }, 4))
            }
            InventoryClick::QuickCraft {
                phase,
                button,
                slot,
            } => {
                if button == DragButton::Middle && !creative_mode {
                    return Err(InventoryError::CreativeOnly);
                }
                let wire_slot = match phase {
                    DragPhase::Start | DragPhase::End if slot.is_none() => OUTSIDE_SLOT,
                    DragPhase::AddSlot => {
                        let slot = slot.ok_or(InventoryError::InvalidDragPhase)?;
                        validate_slot(slot, slot_count)?;
                        slot
                    }
                    _ => return Err(InventoryError::InvalidDragPhase),
                };
                match phase {
                    DragPhase::Start if self.drag.is_none() => {
                        self.drag = Some(DragState { button });
                    }
                    DragPhase::AddSlot if self.drag.is_some_and(|drag| drag.button == button) => {}
                    DragPhase::End if self.drag.is_some_and(|drag| drag.button == button) => {
                        self.drag = None;
                    }
                    _ => return Err(InventoryError::InvalidDragPhase),
                }
                Ok((wire_slot, drag_button(button, phase), 5))
            }
            InventoryClick::PickupAll { slot } => {
                validate_slot(slot, slot_count)?;
                Ok((slot, 0, 6))
            }
        }
    }

    pub(crate) fn mark_sent(&mut self, transaction_id: u64) -> Vec<InventoryEvent> {
        let mut finish_without_confirmation = false;
        if let Some(transaction) = self
            .pending_transactions
            .iter_mut()
            .find(|transaction| transaction.request.transaction_id == transaction_id)
        {
            transaction.status = InventoryTransactionStatus::Sent;
            finish_without_confirmation = matches!(
                transaction.request.click,
                InventoryClick::QuickCraft {
                    phase: DragPhase::Start | DragPhase::AddSlot,
                    ..
                }
            );
        } else {
            return Vec::new();
        }
        let mut events = vec![InventoryEvent::TransactionSent { transaction_id }];
        if finish_without_confirmation {
            if let Some(event) = self.finish_transaction(transaction_id, InventoryOutcome::Sent) {
                events.push(event);
            }
        }
        events
    }

    pub(crate) fn expire_transactions(&mut self, tick: u64) -> Vec<InventoryEvent> {
        let expired: Vec<_> = self
            .pending_transactions
            .iter()
            .filter(|transaction| {
                tick.saturating_sub(transaction.queued_tick) >= TRANSACTION_TIMEOUT_TICKS
            })
            .map(|transaction| transaction.request.transaction_id)
            .collect();
        expired
            .into_iter()
            .filter_map(|id| self.finish_transaction(id, InventoryOutcome::TimedOut))
            .collect()
    }

    fn finish_confirmed_transactions(
        &mut self,
        window_id: i32,
        state_id: i32,
    ) -> Vec<InventoryEvent> {
        let confirmed: Vec<_> = self
            .pending_transactions
            .iter()
            .filter(|transaction| {
                transaction.status == InventoryTransactionStatus::Sent
                    && transaction.request.window_id == window_id
                    && state_id > transaction.request.state_id
            })
            .map(|transaction| transaction.request.transaction_id)
            .collect();
        confirmed
            .into_iter()
            .filter_map(|id| self.finish_transaction(id, InventoryOutcome::Confirmed { state_id }))
            .collect()
    }

    fn finish_window_transactions(
        &mut self,
        window_id: i32,
        outcome: InventoryOutcome,
    ) -> Vec<InventoryEvent> {
        let transactions: Vec<_> = self
            .pending_transactions
            .iter()
            .filter(|transaction| transaction.request.window_id == window_id)
            .map(|transaction| transaction.request.transaction_id)
            .collect();
        transactions
            .into_iter()
            .filter_map(|id| self.finish_transaction(id, outcome))
            .collect()
    }

    fn finish_transaction(
        &mut self,
        transaction_id: u64,
        outcome: InventoryOutcome,
    ) -> Option<InventoryEvent> {
        let index = self
            .pending_transactions
            .iter()
            .position(|transaction| transaction.request.transaction_id == transaction_id)?;
        self.pending_transactions.remove(index);
        if !self.completed_transactions.contains_key(&transaction_id)
            && self.completed_transactions.len() >= MAX_COMPLETED_TRANSACTIONS
        {
            if let Some(oldest) = self.completed_transactions.keys().next().copied() {
                self.completed_transactions.remove(&oldest);
            }
        }
        self.completed_transactions.insert(transaction_id, outcome);
        Some(InventoryEvent::TransactionFinished {
            transaction_id,
            outcome,
        })
    }

    pub(crate) fn reject_transaction(
        &mut self,
        transaction_id: u64,
        error: InventoryError,
    ) -> InventoryEvent {
        if !self.completed_transactions.contains_key(&transaction_id)
            && self.completed_transactions.len() >= MAX_COMPLETED_TRANSACTIONS
        {
            if let Some(oldest) = self.completed_transactions.keys().next().copied() {
                self.completed_transactions.remove(&oldest);
            }
        }
        self.completed_transactions
            .insert(transaction_id, InventoryOutcome::Rejected(error));
        InventoryEvent::TransactionRejected {
            transaction_id,
            error,
        }
    }
}

fn validate_slot(slot: i16, slot_count: usize) -> Result<(), InventoryError> {
    let Ok(index) = usize::try_from(slot) else {
        return Err(InventoryError::InvalidSlot { slot, slot_count });
    };
    if index >= slot_count {
        return Err(InventoryError::InvalidSlot { slot, slot_count });
    }
    Ok(())
}

fn validate_optional_slot(slot: Option<i16>, slot_count: usize) -> Result<i16, InventoryError> {
    match slot {
        Some(slot) => {
            validate_slot(slot, slot_count)?;
            Ok(slot)
        }
        None => Ok(OUTSIDE_SLOT),
    }
}

fn mouse_button(button: MouseButton) -> i8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
    }
}

fn drag_button(button: DragButton, phase: DragPhase) -> i8 {
    let base = match button {
        DragButton::Left => 0,
        DragButton::Right => 4,
        DragButton::Middle => 8,
    };
    base + match phase {
        DragPhase::Start => 0,
        DragPhase::AddSlot => 1,
        DragPhase::End => 2,
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

    fn transaction(id: u64, click: InventoryClick) -> InventoryClickRequest {
        InventoryClickRequest {
            transaction_id: id,
            generation: 3,
            window_id: 0,
            state_id: 7,
            click,
        }
    }

    fn synchronized_inventory() -> InventoryState {
        let mut inventory = InventoryState::new();
        inventory.window_items(&PacketWindowItems {
            window_id: 0,
            state_id: 7,
            items: vec![item(1, 5), empty_slot(), item(2, 1)],
            carried_item: empty_slot(),
        });
        inventory
    }

    #[test]
    fn typed_click_modes_encode_protocol_769_fields() {
        let cases = [
            (
                InventoryClick::Pickup {
                    slot: Some(0),
                    button: MouseButton::Right,
                },
                (0, 1, 0),
                false,
            ),
            (InventoryClick::QuickMove { slot: 1 }, (1, 0, 1), false),
            (
                InventoryClick::HotbarSwap {
                    slot: 2,
                    destination: HotbarDestination::Slot(8),
                },
                (2, 8, 2),
                false,
            ),
            (InventoryClick::Clone { slot: 0 }, (0, 2, 3), true),
            (
                InventoryClick::Throw {
                    slot: 0,
                    whole_stack: true,
                },
                (0, 1, 4),
                false,
            ),
            (InventoryClick::PickupAll { slot: 0 }, (0, 0, 6), false),
        ];
        let mut inventory = synchronized_inventory();
        for (index, (click, expected, creative)) in cases.into_iter().enumerate() {
            let prepared = inventory
                .prepare_click(transaction(index as u64 + 1, click), 10, creative)
                .unwrap();
            assert_eq!(
                (
                    prepared.packet.slot,
                    prepared.packet.mouse_button,
                    prepared.packet.mode,
                ),
                expected
            );
            assert_eq!(prepared.packet.window_id, 0);
            assert_eq!(prepared.packet.state_id, 7);
            assert!(prepared.packet.changed_slots.is_empty());
            assert_eq!(prepared.packet.cursor_item, empty_slot());
        }
    }

    #[test]
    fn malformed_slot_state_hotbar_and_creative_clicks_are_rejected() {
        let mut inventory = synchronized_inventory();
        assert!(matches!(
            inventory.prepare_click(
                transaction(1, InventoryClick::QuickMove { slot: -1 }),
                0,
                false
            ),
            Err(InventoryError::InvalidSlot { .. })
        ));
        let mut stale = transaction(2, InventoryClick::QuickMove { slot: 0 });
        stale.state_id = 6;
        assert!(matches!(
            inventory.prepare_click(stale, 0, false),
            Err(InventoryError::StaleState { .. })
        ));
        assert_eq!(
            inventory.prepare_click(
                transaction(
                    3,
                    InventoryClick::HotbarSwap {
                        slot: 0,
                        destination: HotbarDestination::Slot(9),
                    },
                ),
                0,
                false,
            ),
            Err(InventoryError::InvalidHotbarSlot { slot: 9 })
        );
        assert_eq!(
            inventory.prepare_click(transaction(4, InventoryClick::Clone { slot: 0 }), 0, false),
            Err(InventoryError::CreativeOnly)
        );
    }

    #[test]
    fn drag_lifecycle_validates_phases_and_buttons() {
        let mut inventory = synchronized_inventory();
        let start = InventoryClick::QuickCraft {
            phase: DragPhase::Start,
            button: DragButton::Left,
            slot: None,
        };
        let prepared = inventory
            .prepare_click(transaction(1, start), 0, false)
            .unwrap();
        assert_eq!(
            (
                prepared.packet.slot,
                prepared.packet.mouse_button,
                prepared.packet.mode
            ),
            (-999, 0, 5)
        );
        let events = inventory.mark_sent(1);
        assert!(events.iter().any(|event| matches!(
            event,
            InventoryEvent::TransactionFinished {
                outcome: InventoryOutcome::Sent,
                ..
            }
        )));

        let add = InventoryClick::QuickCraft {
            phase: DragPhase::AddSlot,
            button: DragButton::Left,
            slot: Some(1),
        };
        let prepared = inventory
            .prepare_click(transaction(2, add), 1, false)
            .unwrap();
        assert_eq!((prepared.packet.slot, prepared.packet.mouse_button), (1, 1));
        inventory.mark_sent(2);
        let wrong_end = InventoryClick::QuickCraft {
            phase: DragPhase::End,
            button: DragButton::Right,
            slot: None,
        };
        assert_eq!(
            inventory.prepare_click(transaction(3, wrong_end), 2, false),
            Err(InventoryError::InvalidDragPhase)
        );
        let end = InventoryClick::QuickCraft {
            phase: DragPhase::End,
            button: DragButton::Left,
            slot: None,
        };
        let prepared = inventory
            .prepare_click(transaction(4, end), 2, false)
            .unwrap();
        assert_eq!(
            (prepared.packet.slot, prepared.packet.mouse_button),
            (-999, 2)
        );
        assert!(inventory.drag.is_none());
    }

    #[test]
    fn incremental_update_confirms_and_full_sync_corrects_transactions() {
        let mut inventory = synchronized_inventory();
        inventory
            .prepare_click(
                transaction(1, InventoryClick::QuickMove { slot: 0 }),
                0,
                false,
            )
            .unwrap();
        inventory.mark_sent(1);
        let events = inventory.set_slot(&PacketSetSlot {
            window_id: 0,
            state_id: 8,
            slot: 0,
            item: empty_slot(),
        });
        assert!(events.iter().any(|event| matches!(
            event,
            InventoryEvent::TransactionFinished {
                transaction_id: 1,
                outcome: InventoryOutcome::Confirmed { state_id: 8 },
            }
        )));

        let mut request = transaction(2, InventoryClick::PickupAll { slot: 1 });
        request.state_id = 8;
        inventory.prepare_click(request, 1, false).unwrap();
        inventory.mark_sent(2);
        let events = inventory.window_items(&PacketWindowItems {
            window_id: 0,
            state_id: 9,
            items: vec![empty_slot(); 3],
            carried_item: item(1, 5),
        });
        assert!(events.iter().any(|event| matches!(
            event,
            InventoryEvent::TransactionFinished {
                transaction_id: 2,
                outcome: InventoryOutcome::Corrected { state_id: 9 },
            }
        )));
        assert_eq!(inventory.cursor, item(1, 5));
    }

    #[test]
    fn transaction_queue_and_completed_history_are_bounded() {
        let mut inventory = synchronized_inventory();
        for id in 0..MAX_PENDING_TRANSACTIONS as u64 {
            inventory
                .prepare_click(
                    transaction(
                        id,
                        InventoryClick::Pickup {
                            slot: None,
                            button: MouseButton::Left,
                        },
                    ),
                    0,
                    false,
                )
                .unwrap();
        }
        assert_eq!(
            inventory.prepare_click(
                transaction(
                    MAX_PENDING_TRANSACTIONS as u64,
                    InventoryClick::PickupAll { slot: 0 }
                ),
                0,
                false,
            ),
            Err(InventoryError::QueueFull)
        );
        let events = inventory.expire_transactions(TRANSACTION_TIMEOUT_TICKS);
        assert_eq!(events.len(), MAX_PENDING_TRANSACTIONS);
        assert!(inventory.pending_transactions.is_empty());
        assert_eq!(
            inventory.completed_transactions.len(),
            MAX_PENDING_TRANSACTIONS
        );
    }

    #[test]
    fn closing_window_cancels_pending_transaction() {
        let mut inventory = InventoryState::new();
        inventory.open_window(&PacketOpenWindow {
            window_id: 5,
            inventory_type: 2,
            window_title: Nbt::Compound(vec![]),
        });
        inventory.window_items(&PacketWindowItems {
            window_id: 5,
            state_id: 1,
            items: vec![empty_slot()],
            carried_item: empty_slot(),
        });
        let request = InventoryClickRequest {
            transaction_id: 9,
            generation: 1,
            window_id: 5,
            state_id: 1,
            click: InventoryClick::QuickMove { slot: 0 },
        };
        inventory.prepare_click(request, 0, false).unwrap();
        inventory.mark_sent(9);
        let events = inventory.close_window(&PacketCloseWindow { window_id: 5 });
        assert!(events.iter().any(|event| matches!(
            event,
            InventoryEvent::TransactionFinished {
                transaction_id: 9,
                outcome: InventoryOutcome::WindowClosed,
            }
        )));
    }
}
