//! Tab-list player tracking: the online players the server reports via
//! `player_info` / `player_remove`, the equivalent of Mineflayer's
//! `bot.players`.
//!
//! A pure projection of those two packets. Player-info entries are additive
//! per action flag (a packet may set only latency, or only game mode, for an
//! already-known player), so fields are applied individually and a
//! previously-unseen uuid is inserted with just the fields present.

use std::collections::BTreeMap;

use minerider_protocol::generated::v1_21_4::play::{
    PacketPlayerInfo, PacketPlayerInfoAction, PacketPlayerInfoDataItem,
    PacketPlayerInfoDataItemChatSession, PacketPlayerInfoDataItemDisplayName,
    PacketPlayerInfoDataItemGamemode, PacketPlayerInfoDataItemLatency,
    PacketPlayerInfoDataItemListPriority, PacketPlayerInfoDataItemListed,
    PacketPlayerInfoDataItemPlayer, PacketPlayerInfoDataItemShowHat, PacketPlayerRemove,
};

use crate::minecraft::text::TextComponent;

/// A defensive ceiling for tab-list state. Real servers stay far below this;
/// the bound prevents a hostile stream of unique UUIDs from growing forever.
pub const MAX_PLAYERS: usize = 4_096;

/// One entry in the tab list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlayerEntry {
    pub uuid: u128,
    /// Account name from the `add_player` action; empty until seen.
    pub name: String,
    /// Game mode (0 survival, 1 creative, 2 adventure, 3 spectator), or -1
    /// if the server has not reported it.
    pub gamemode: i32,
    /// Ping in milliseconds, or -1 if unknown.
    pub latency: i32,
    /// Whether the player is shown in the tab list.
    pub listed: bool,
    /// Server-provided tab-list display name, distinct from the account name.
    pub display_name: Option<TextComponent>,
    /// Sorting priority within the tab list.
    pub list_priority: i32,
    /// Whether the player's hat skin layer is shown in the tab list.
    pub show_hat: bool,
    /// Bounded metadata for the currently initialized signed-chat session.
    pub chat_session: Option<PlayerChatSession>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerChatSession {
    pub session_id: u128,
    pub expires_at_millis: i64,
    pub public_key_bytes: usize,
    pub signature_bytes: usize,
}

/// The set of players currently known from the tab list, keyed by uuid.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlayerList {
    players: BTreeMap<u128, PlayerEntry>,
}

/// The outcome of applying a `player_info` packet, so the caller can emit
/// join events only for players that were genuinely new.
#[derive(Debug, Default)]
pub struct PlayerInfoChanges {
    /// `(uuid, name)` for players inserted by this packet (first seen).
    pub joined: Vec<(u128, String)>,
    /// Every UUID whose accepted entry was touched, in packet order.
    pub updated: Vec<u128>,
    /// Entries refused because the defensive player bound was full.
    pub rejected: usize,
}

impl PlayerList {
    /// Number of tracked players.
    pub fn len(&self) -> usize {
        self.players.len()
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.players.is_empty()
    }

    /// Looks up a player by uuid.
    pub fn get(&self, uuid: u128) -> Option<&PlayerEntry> {
        self.players.get(&uuid)
    }

    /// Iterates over all tracked players.
    pub fn iter(&self) -> impl Iterator<Item = &PlayerEntry> {
        self.players.values()
    }

    /// Applies a `player_info` packet, folding each action's fields into the
    /// per-uuid entry. Returns which uuids were newly inserted.
    pub fn apply_info(&mut self, packet: &PacketPlayerInfo) -> PlayerInfoChanges {
        let action = &packet.action;
        let mut changes = PlayerInfoChanges::default();
        for item in &packet.data {
            let is_new = !self.players.contains_key(&item.uuid);
            if is_new && self.players.len() >= MAX_PLAYERS {
                changes.rejected += 1;
                continue;
            }
            let entry = self
                .players
                .entry(item.uuid)
                .or_insert_with(|| PlayerEntry {
                    uuid: item.uuid,
                    gamemode: -1,
                    latency: -1,
                    ..PlayerEntry::default()
                });
            apply_item(entry, action, item);
            changes.updated.push(item.uuid);
            // A newly-inserted entry only counts as a "join" once it actually
            // carries a name (the add_player action); otherwise it's a
            // latency/listed update for a player we simply hadn't seen named.
            if is_new && !entry.name.is_empty() {
                changes.joined.push((entry.uuid, entry.name.clone()));
            }
        }
        changes
    }

    /// Applies a `player_remove` packet, returning the uuids removed.
    pub fn apply_remove(&mut self, packet: &PacketPlayerRemove) -> Vec<u128> {
        let mut removed = Vec::new();
        for uuid in &packet.players {
            if self.players.remove(uuid).is_some() {
                removed.push(*uuid);
            }
        }
        removed
    }
}

fn apply_item(
    entry: &mut PlayerEntry,
    action: &PacketPlayerInfoAction,
    item: &PacketPlayerInfoDataItem,
) {
    if action.contains(PacketPlayerInfoAction::ADD_PLAYER) {
        if let PacketPlayerInfoDataItemPlayer::True(profile) = &item.player {
            entry.name = profile.name.clone();
        }
    }
    if action.contains(PacketPlayerInfoAction::INITIALIZE_CHAT) {
        if let PacketPlayerInfoDataItemChatSession::True(session) = &item.chat_session {
            entry.chat_session = session.as_ref().map(|session| PlayerChatSession {
                session_id: session.uuid,
                expires_at_millis: session.public_key.expire_time,
                public_key_bytes: session.public_key.key_bytes.len(),
                signature_bytes: session.public_key.key_signature.len(),
            });
        }
    }
    if action.contains(PacketPlayerInfoAction::UPDATE_GAME_MODE) {
        if let PacketPlayerInfoDataItemGamemode::True(mode) = item.gamemode {
            entry.gamemode = mode;
        }
    }
    if action.contains(PacketPlayerInfoAction::UPDATE_LATENCY) {
        if let PacketPlayerInfoDataItemLatency::True(ms) = item.latency {
            entry.latency = ms;
        }
    }
    if action.contains(PacketPlayerInfoAction::UPDATE_LISTED) {
        if let PacketPlayerInfoDataItemListed::True(listed) = item.listed {
            // Wire type is a varint per minecraft-data (0/1), not a native bool.
            entry.listed = listed != 0;
        }
    }
    if action.contains(PacketPlayerInfoAction::UPDATE_DISPLAY_NAME) {
        if let PacketPlayerInfoDataItemDisplayName::True(display_name) = &item.display_name {
            entry.display_name = display_name.as_ref().map(TextComponent::from_nbt);
        }
    }
    if action.contains(PacketPlayerInfoAction::UPDATE_LIST_ORDER) {
        if let PacketPlayerInfoDataItemListPriority::True(priority) = item.list_priority {
            entry.list_priority = priority;
        }
    }
    if action.contains(PacketPlayerInfoAction::UPDATE_HAT) {
        if let PacketPlayerInfoDataItemShowHat::True(show_hat) = item.show_hat {
            entry.show_hat = show_hat;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minerider_protocol::generated::v1_21_4::play::{
        PacketPlayerInfoDataItemChatSession, PacketPlayerInfoDataItemDisplayName,
        PacketPlayerInfoDataItemListPriority, PacketPlayerInfoDataItemShowHat,
    };
    use minerider_protocol::generated::v1_21_4::types::{
        ChatSessionValue, ChatSessionValuePublicKey, GameProfile,
    };
    use minerider_protocol::nbt::Nbt;

    fn add_item(uuid: u128, name: &str, gamemode: i32, latency: i32) -> PacketPlayerInfoDataItem {
        PacketPlayerInfoDataItem {
            uuid,
            player: PacketPlayerInfoDataItemPlayer::True(GameProfile {
                name: name.to_string(),
                properties: vec![],
            }),
            chat_session: PacketPlayerInfoDataItemChatSession::Default,
            gamemode: PacketPlayerInfoDataItemGamemode::True(gamemode),
            listed: PacketPlayerInfoDataItemListed::True(1),
            latency: PacketPlayerInfoDataItemLatency::True(latency),
            display_name: PacketPlayerInfoDataItemDisplayName::Default,
            list_priority: PacketPlayerInfoDataItemListPriority::Default,
            show_hat: PacketPlayerInfoDataItemShowHat::Default,
        }
    }

    fn add_action() -> PacketPlayerInfoAction {
        PacketPlayerInfoAction(
            PacketPlayerInfoAction::ADD_PLAYER
                | PacketPlayerInfoAction::UPDATE_GAME_MODE
                | PacketPlayerInfoAction::UPDATE_LISTED
                | PacketPlayerInfoAction::UPDATE_LATENCY,
        )
    }

    #[test]
    fn add_player_records_name_and_fields() {
        let mut list = PlayerList::default();
        let changes = list.apply_info(&PacketPlayerInfo {
            action: add_action(),
            data: vec![add_item(7, "Notch", 1, 42)],
        });
        assert_eq!(changes.joined, vec![(7, "Notch".to_string())]);
        let entry = list.get(7).unwrap();
        assert_eq!(entry.name, "Notch");
        assert_eq!(entry.gamemode, 1);
        assert_eq!(entry.latency, 42);
        assert!(entry.listed);
        assert_eq!(changes.updated, vec![7]);
    }

    #[test]
    fn latency_only_update_does_not_reset_name_or_count_as_join() {
        let mut list = PlayerList::default();
        list.apply_info(&PacketPlayerInfo {
            action: add_action(),
            data: vec![add_item(7, "Notch", 0, 10)],
        });
        // A latency-only update for the same player.
        let latency_item = PacketPlayerInfoDataItem {
            uuid: 7,
            player: PacketPlayerInfoDataItemPlayer::Default,
            chat_session: PacketPlayerInfoDataItemChatSession::Default,
            gamemode: PacketPlayerInfoDataItemGamemode::Default,
            listed: PacketPlayerInfoDataItemListed::Default,
            latency: PacketPlayerInfoDataItemLatency::True(99),
            display_name: PacketPlayerInfoDataItemDisplayName::Default,
            list_priority: PacketPlayerInfoDataItemListPriority::Default,
            show_hat: PacketPlayerInfoDataItemShowHat::Default,
        };
        let changes = list.apply_info(&PacketPlayerInfo {
            action: PacketPlayerInfoAction(PacketPlayerInfoAction::UPDATE_LATENCY),
            data: vec![latency_item],
        });
        assert!(changes.joined.is_empty(), "latency update is not a join");
        let entry = list.get(7).unwrap();
        assert_eq!(entry.name, "Notch", "name preserved");
        assert_eq!(entry.latency, 99, "latency updated");
    }

    #[test]
    fn remove_reports_removed_uuids() {
        let mut list = PlayerList::default();
        list.apply_info(&PacketPlayerInfo {
            action: add_action(),
            data: vec![add_item(7, "Notch", 0, 10), add_item(8, "Jeb", 0, 20)],
        });
        let removed = list.apply_remove(&PacketPlayerRemove {
            players: vec![7, 99],
        });
        assert_eq!(removed, vec![7]);
        assert!(list.get(7).is_none());
        assert!(list.get(8).is_some());
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn modern_player_info_fields_are_typed_and_clearable() {
        let mut list = PlayerList::default();
        list.apply_info(&PacketPlayerInfo {
            action: add_action(),
            data: vec![add_item(7, "Notch", 0, 10)],
        });
        let modern = PacketPlayerInfoDataItem {
            uuid: 7,
            player: PacketPlayerInfoDataItemPlayer::Default,
            chat_session: PacketPlayerInfoDataItemChatSession::True(Some(ChatSessionValue {
                uuid: 99,
                public_key: ChatSessionValuePublicKey {
                    expire_time: 1234,
                    key_bytes: vec![1, 2],
                    key_signature: vec![3, 4, 5],
                },
            })),
            gamemode: PacketPlayerInfoDataItemGamemode::Default,
            listed: PacketPlayerInfoDataItemListed::Default,
            latency: PacketPlayerInfoDataItemLatency::Default,
            display_name: PacketPlayerInfoDataItemDisplayName::True(Some(Nbt::String(
                "Boss".into(),
            ))),
            list_priority: PacketPlayerInfoDataItemListPriority::True(12),
            show_hat: PacketPlayerInfoDataItemShowHat::True(true),
        };
        list.apply_info(&PacketPlayerInfo {
            action: PacketPlayerInfoAction(
                PacketPlayerInfoAction::INITIALIZE_CHAT
                    | PacketPlayerInfoAction::UPDATE_DISPLAY_NAME
                    | PacketPlayerInfoAction::UPDATE_LIST_ORDER
                    | PacketPlayerInfoAction::UPDATE_HAT,
            ),
            data: vec![modern],
        });
        let entry = list.get(7).unwrap();
        assert_eq!(entry.display_name.as_ref().unwrap().plain_text(), "Boss");
        assert_eq!(entry.list_priority, 12);
        assert!(entry.show_hat);
        assert_eq!(entry.chat_session.as_ref().unwrap().session_id, 99);
        assert_eq!(entry.chat_session.as_ref().unwrap().signature_bytes, 3);

        let mut clear = add_item(7, "", 0, 0);
        clear.chat_session = PacketPlayerInfoDataItemChatSession::True(None);
        clear.display_name = PacketPlayerInfoDataItemDisplayName::True(None);
        list.apply_info(&PacketPlayerInfo {
            action: PacketPlayerInfoAction(
                PacketPlayerInfoAction::INITIALIZE_CHAT
                    | PacketPlayerInfoAction::UPDATE_DISPLAY_NAME,
            ),
            data: vec![clear],
        });
        let entry = list.get(7).unwrap();
        assert!(entry.chat_session.is_none());
        assert!(entry.display_name.is_none());
    }

    #[test]
    fn player_bound_rejects_only_new_entries() {
        let mut list = PlayerList::default();
        for uuid in 0..MAX_PLAYERS as u128 {
            list.players.insert(
                uuid,
                PlayerEntry {
                    uuid,
                    ..PlayerEntry::default()
                },
            );
        }
        let changes = list.apply_info(&PacketPlayerInfo {
            action: add_action(),
            data: vec![add_item(MAX_PLAYERS as u128 + 1, "overflow", 0, 0)],
        });
        assert_eq!(changes.rejected, 1);
        assert!(changes.updated.is_empty());

        let changes = list.apply_info(&PacketPlayerInfo {
            action: PacketPlayerInfoAction(PacketPlayerInfoAction::UPDATE_LATENCY),
            data: vec![PacketPlayerInfoDataItem {
                uuid: 0,
                latency: PacketPlayerInfoDataItemLatency::True(5),
                ..add_item(0, "ignored", 0, 0)
            }],
        });
        assert_eq!(changes.updated, vec![0]);
        assert_eq!(list.get(0).unwrap().latency, 5);
    }

    #[test]
    fn iteration_is_deterministic_by_uuid() {
        let mut list = PlayerList::default();
        list.apply_info(&PacketPlayerInfo {
            action: add_action(),
            data: vec![add_item(9, "nine", 0, 0), add_item(2, "two", 0, 0)],
        });
        assert_eq!(
            list.iter().map(|entry| entry.uuid).collect::<Vec<_>>(),
            vec![2, 9]
        );
    }
}
