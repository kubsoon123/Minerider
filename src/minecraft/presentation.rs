//! Headless presentation state for protocol-769 chat, titles, action bars,
//! tab-list header/footer, boss bars, and disconnect reasons.
//!
//! Generated packet types are decoded at this module boundary and projected
//! into stable public models. Consumers therefore do not need to depend on
//! generated switch enums, while signed-chat material needed by a future
//! verifier is retained without claiming verification.

use std::collections::{BTreeMap, VecDeque};

use minerider_protocol::buffer::PacketReader;
use minerider_protocol::generated::v1_21_4::play::{
    ChatType, ChatTypeParameterType, ChatTypes, ChatTypesHolder, PacketActionBar, PacketBossBar,
    PacketBossBarColor, PacketBossBarDividers, PacketBossBarFlags, PacketBossBarHealth,
    PacketBossBarTitle, PacketClearTitles, PacketKickDisconnect, PacketPlayerChat,
    PacketPlayerChatFilterTypeMask, PacketPlayerlistHeader, PacketProfilelessChat,
    PacketSetTitleSubtitle, PacketSetTitleText, PacketSetTitleTime, PacketSystemChat,
    CLIENTBOUND_ACTION_BAR_ID, CLIENTBOUND_BOSS_BAR_ID, CLIENTBOUND_CLEAR_TITLES_ID,
    CLIENTBOUND_KICK_DISCONNECT_ID, CLIENTBOUND_PLAYERLIST_HEADER_ID, CLIENTBOUND_PLAYER_CHAT_ID,
    CLIENTBOUND_PROFILELESS_CHAT_ID, CLIENTBOUND_SET_TITLE_SUBTITLE_ID,
    CLIENTBOUND_SET_TITLE_TEXT_ID, CLIENTBOUND_SET_TITLE_TIME_ID, CLIENTBOUND_SYSTEM_CHAT_ID,
};
use minerider_protocol::generated::v1_21_4::types::PreviousMessagesItemSignature;
use minerider_protocol::holder::Holder;
use minerider_protocol::traits::Decode;

use crate::core::error::Result;
use crate::minecraft::text::{Style, TextComponent};

/// Retained chat entries. Oldest entries are discarded first.
pub const MAX_CHAT_HISTORY: usize = 512;
/// Maximum concurrently tracked boss bars from an untrusted server.
pub const MAX_BOSS_BARS: usize = 256;
/// Defensive cap for the previous-message chain copied into public state.
pub const MAX_PREVIOUS_MESSAGES: usize = 64;
/// Defensive cap for a partial-filter bit mask copied into public state.
pub const MAX_FILTER_MASK_WORDS: usize = 512;
/// Forward-compatible bound for inline chat-decoration parameters.
pub const MAX_CHAT_TYPE_PARAMETERS: usize = 16;

/// All headless-readable presentation state. A clone is included in every
/// play-state snapshot.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PresentationState {
    pub chat: VecDeque<ChatMessage>,
    pub action_bar: Option<TextComponent>,
    pub titles: TitleState,
    pub tab_list: TabListPresentation,
    /// Deterministic ordering makes snapshots and tests stable.
    pub boss_bars: BTreeMap<u128, BossBar>,
    pub disconnect_reason: Option<TextComponent>,
    next_chat_sequence: u64,
}

impl PresentationState {
    /// Applies one decoded update and returns its ordered public event.
    pub(crate) fn apply(&mut self, update: PresentationUpdate) -> PresentationEvent {
        match update {
            PresentationUpdate::Chat(message) => {
                let mut message = *message;
                message.sequence = self.next_chat_sequence;
                self.next_chat_sequence = self.next_chat_sequence.saturating_add(1);
                if self.chat.len() == MAX_CHAT_HISTORY {
                    self.chat.pop_front();
                }
                self.chat.push_back(message.clone());
                PresentationEvent::Chat(Box::new(message))
            }
            PresentationUpdate::ActionBar(text) => {
                self.action_bar = Some(text.clone());
                PresentationEvent::ActionBarChanged { text }
            }
            PresentationUpdate::Title(text) => {
                self.titles.title = Some(text.clone());
                PresentationEvent::TitleChanged { text }
            }
            PresentationUpdate::Subtitle(text) => {
                self.titles.subtitle = Some(text.clone());
                PresentationEvent::SubtitleChanged { text }
            }
            PresentationUpdate::TitleTiming(timing) => {
                self.titles.timing = timing;
                PresentationEvent::TitleTimingChanged { timing }
            }
            PresentationUpdate::ClearTitles { reset } => {
                self.titles.title = None;
                self.titles.subtitle = None;
                if reset {
                    self.titles.timing = TitleTiming::default();
                }
                PresentationEvent::TitlesCleared { reset }
            }
            PresentationUpdate::TabList { header, footer } => {
                self.tab_list.header = Some(header.clone());
                self.tab_list.footer = Some(footer.clone());
                PresentationEvent::TabListChanged { header, footer }
            }
            PresentationUpdate::BossBar(update) => self.apply_boss_bar(update),
            PresentationUpdate::Disconnect(reason) => {
                self.disconnect_reason = Some(reason.clone());
                PresentationEvent::Disconnected { reason }
            }
        }
    }

    fn apply_boss_bar(&mut self, update: BossBarUpdate) -> PresentationEvent {
        let id = update.id();
        let action = update.action();
        let applied = match update {
            BossBarUpdate::Add(bar) => {
                if self.boss_bars.contains_key(&id) || self.boss_bars.len() < MAX_BOSS_BARS {
                    self.boss_bars.insert(id, bar);
                    true
                } else {
                    false
                }
            }
            BossBarUpdate::Remove { .. } => self.boss_bars.remove(&id).is_some(),
            BossBarUpdate::Progress { progress, .. } => self
                .boss_bars
                .get_mut(&id)
                .map(|bar| bar.progress = progress)
                .is_some(),
            BossBarUpdate::Title { title, .. } => self
                .boss_bars
                .get_mut(&id)
                .map(|bar| bar.title = title)
                .is_some(),
            BossBarUpdate::Style { color, overlay, .. } => self
                .boss_bars
                .get_mut(&id)
                .map(|bar| {
                    bar.color = color;
                    bar.overlay = overlay;
                })
                .is_some(),
            BossBarUpdate::Flags { flags, .. } => self
                .boss_bars
                .get_mut(&id)
                .map(|bar| bar.flags = flags)
                .is_some(),
            BossBarUpdate::Unknown { .. } => false,
        };
        PresentationEvent::BossBarChanged {
            id,
            action,
            current: self.boss_bars.get(&id).cloned(),
            applied,
        }
    }
}

/// Current title/subtitle and vanilla timing values, measured in ticks.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TitleState {
    pub title: Option<TextComponent>,
    pub subtitle: Option<TextComponent>,
    pub timing: TitleTiming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitleTiming {
    pub fade_in: i32,
    pub stay: i32,
    pub fade_out: i32,
}

impl Default for TitleTiming {
    fn default() -> Self {
        Self {
            fade_in: 10,
            stay: 70,
            fade_out: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TabListPresentation {
    pub header: Option<TextComponent>,
    pub footer: Option<TextComponent>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BossBar {
    pub id: u128,
    pub title: TextComponent,
    pub progress: f32,
    pub color: BossBarColor,
    pub overlay: BossBarOverlay,
    pub flags: BossBarFlags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BossBarColor {
    Pink,
    Blue,
    Red,
    Green,
    Yellow,
    Purple,
    White,
    Unknown(i32),
}

impl From<i32> for BossBarColor {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::Pink,
            1 => Self::Blue,
            2 => Self::Red,
            3 => Self::Green,
            4 => Self::Yellow,
            5 => Self::Purple,
            6 => Self::White,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BossBarOverlay {
    Progress,
    Notched6,
    Notched10,
    Notched12,
    Notched20,
    Unknown(i32),
}

impl From<i32> for BossBarOverlay {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::Progress,
            1 => Self::Notched6,
            2 => Self::Notched10,
            3 => Self::Notched12,
            4 => Self::Notched20,
            other => Self::Unknown(other),
        }
    }
}

/// Boss-bar flags are a bit set. Unknown future bits remain in raw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BossBarFlags {
    pub raw: u8,
}

impl BossBarFlags {
    pub fn darken_sky(self) -> bool {
        self.raw & 0x01 != 0
    }

    pub fn play_end_music(self) -> bool {
        self.raw & 0x02 != 0
    }

    pub fn create_world_fog(self) -> bool {
        self.raw & 0x04 != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BossBarAction {
    Add,
    Remove,
    UpdateProgress,
    UpdateTitle,
    UpdateStyle,
    UpdateFlags,
    Unknown(i32),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatKind {
    Player,
    Disguised,
    System,
}

/// One safe display message plus preserved protocol metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    /// Monotonic within one play session; assigned when applied to state.
    pub sequence: u64,
    pub kind: ChatKind,
    pub content: TextComponent,
    pub sender: Option<TextComponent>,
    pub target: Option<TextComponent>,
    pub chat_type: Option<ChatTypeReference>,
    pub signed: Option<SignedChatData>,
}

impl ChatMessage {
    /// Deterministic, non-executing display rendering.
    pub fn display_text(&self) -> String {
        let content = self.content.plain_text();
        match self.sender.as_ref().map(TextComponent::plain_text) {
            Some(sender) if !sender.is_empty() => format!("<{sender}> {content}"),
            _ => content,
        }
    }
}

/// Data required by a future signed-chat verifier. Retaining it does not
/// imply verification.
#[derive(Debug, Clone, PartialEq)]
pub struct SignedChatData {
    pub sender_uuid: u128,
    pub index: i32,
    pub signature: Option<Vec<u8>>,
    pub plain_message: String,
    pub timestamp: i64,
    pub salt: i64,
    pub previous_messages: Vec<PreviousMessage>,
    pub previous_messages_truncated: bool,
    pub filter: ChatFilter,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreviousMessage {
    pub id: i32,
    pub signature: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatFilter {
    PassThrough,
    FullyFiltered,
    PartiallyFiltered { mask: Vec<i64>, truncated: bool },
    Unknown(i32),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatTypeReference {
    Registry(i32),
    Inline(Box<ChatDecorations>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatDecorations {
    pub chat: ChatDecoration,
    pub narration: ChatDecoration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatDecoration {
    pub translation_key: String,
    pub parameters: Vec<ChatTypeParameter>,
    pub parameters_truncated: bool,
    pub style: Style,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatTypeParameter {
    Content,
    Sender,
    Target,
}

/// Ordered presentation event carried by the bot event stream.
#[derive(Debug, Clone, PartialEq)]
pub enum PresentationEvent {
    Chat(Box<ChatMessage>),
    ActionBarChanged {
        text: TextComponent,
    },
    TitleChanged {
        text: TextComponent,
    },
    SubtitleChanged {
        text: TextComponent,
    },
    TitleTimingChanged {
        timing: TitleTiming,
    },
    TitlesCleared {
        reset: bool,
    },
    TabListChanged {
        header: TextComponent,
        footer: TextComponent,
    },
    BossBarChanged {
        id: u128,
        action: BossBarAction,
        current: Option<BossBar>,
        /// False for out-of-order updates/removals, capacity rejects, and
        /// unknown actions. State remains unchanged.
        applied: bool,
    },
    Disconnected {
        reason: TextComponent,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum PresentationUpdate {
    Chat(Box<ChatMessage>),
    ActionBar(TextComponent),
    Title(TextComponent),
    Subtitle(TextComponent),
    TitleTiming(TitleTiming),
    ClearTitles {
        reset: bool,
    },
    TabList {
        header: TextComponent,
        footer: TextComponent,
    },
    BossBar(BossBarUpdate),
    Disconnect(TextComponent),
}

#[derive(Debug, Clone)]
pub(crate) enum BossBarUpdate {
    Add(BossBar),
    Remove {
        id: u128,
    },
    Progress {
        id: u128,
        progress: f32,
    },
    Title {
        id: u128,
        title: TextComponent,
    },
    Style {
        id: u128,
        color: BossBarColor,
        overlay: BossBarOverlay,
    },
    Flags {
        id: u128,
        flags: BossBarFlags,
    },
    Unknown {
        id: u128,
        action: i32,
    },
}

impl BossBarUpdate {
    fn id(&self) -> u128 {
        match self {
            Self::Add(bar) => bar.id,
            Self::Remove { id }
            | Self::Progress { id, .. }
            | Self::Title { id, .. }
            | Self::Style { id, .. }
            | Self::Flags { id, .. }
            | Self::Unknown { id, .. } => *id,
        }
    }

    fn action(&self) -> BossBarAction {
        match self {
            Self::Add(_) => BossBarAction::Add,
            Self::Remove { .. } => BossBarAction::Remove,
            Self::Progress { .. } => BossBarAction::UpdateProgress,
            Self::Title { .. } => BossBarAction::UpdateTitle,
            Self::Style { .. } => BossBarAction::UpdateStyle,
            Self::Flags { .. } => BossBarAction::UpdateFlags,
            Self::Unknown { action, .. } => BossBarAction::Unknown(*action),
        }
    }
}

/// Decodes every protocol-769 presentation packet supported by the generated
/// schema. None means the id belongs to another subsystem.
pub(crate) fn decode_update(id: i32, payload: &[u8]) -> Result<Option<PresentationUpdate>> {
    let mut input = PacketReader::new(payload);
    let update = match id {
        CLIENTBOUND_SYSTEM_CHAT_ID => {
            let packet = PacketSystemChat::decode(&mut input)?;
            let content = TextComponent::from_nbt(&packet.content);
            if packet.is_action_bar {
                PresentationUpdate::ActionBar(content)
            } else {
                PresentationUpdate::Chat(Box::new(ChatMessage {
                    sequence: 0,
                    kind: ChatKind::System,
                    content,
                    sender: None,
                    target: None,
                    chat_type: None,
                    signed: None,
                }))
            }
        }
        CLIENTBOUND_PLAYER_CHAT_ID => {
            let packet = PacketPlayerChat::decode(&mut input)?;
            PresentationUpdate::Chat(Box::new(player_chat(packet)))
        }
        CLIENTBOUND_PROFILELESS_CHAT_ID => {
            let packet = PacketProfilelessChat::decode(&mut input)?;
            PresentationUpdate::Chat(Box::new(ChatMessage {
                sequence: 0,
                kind: ChatKind::Disguised,
                content: TextComponent::from_nbt(&packet.message),
                sender: Some(TextComponent::from_nbt(&packet.name)),
                target: packet.target.as_ref().map(TextComponent::from_nbt),
                chat_type: Some(chat_type_reference(packet.r#type)),
                signed: None,
            }))
        }
        CLIENTBOUND_ACTION_BAR_ID => {
            let packet = PacketActionBar::decode(&mut input)?;
            PresentationUpdate::ActionBar(TextComponent::from_nbt(&packet.text))
        }
        CLIENTBOUND_SET_TITLE_TEXT_ID => {
            let packet = PacketSetTitleText::decode(&mut input)?;
            PresentationUpdate::Title(TextComponent::from_nbt(&packet.text))
        }
        CLIENTBOUND_SET_TITLE_SUBTITLE_ID => {
            let packet = PacketSetTitleSubtitle::decode(&mut input)?;
            PresentationUpdate::Subtitle(TextComponent::from_nbt(&packet.text))
        }
        CLIENTBOUND_SET_TITLE_TIME_ID => {
            let packet = PacketSetTitleTime::decode(&mut input)?;
            PresentationUpdate::TitleTiming(TitleTiming {
                fade_in: packet.fade_in,
                stay: packet.stay,
                fade_out: packet.fade_out,
            })
        }
        CLIENTBOUND_CLEAR_TITLES_ID => {
            let packet = PacketClearTitles::decode(&mut input)?;
            PresentationUpdate::ClearTitles {
                reset: packet.reset,
            }
        }
        CLIENTBOUND_PLAYERLIST_HEADER_ID => {
            let packet = PacketPlayerlistHeader::decode(&mut input)?;
            PresentationUpdate::TabList {
                header: TextComponent::from_nbt(&packet.header),
                footer: TextComponent::from_nbt(&packet.footer),
            }
        }
        CLIENTBOUND_BOSS_BAR_ID => {
            let packet = PacketBossBar::decode(&mut input)?;
            PresentationUpdate::BossBar(boss_bar_update(packet))
        }
        CLIENTBOUND_KICK_DISCONNECT_ID => {
            let packet = PacketKickDisconnect::decode(&mut input)?;
            PresentationUpdate::Disconnect(TextComponent::from_nbt(&packet.reason))
        }
        _ => return Ok(None),
    };
    Ok(Some(update))
}

fn player_chat(packet: PacketPlayerChat) -> ChatMessage {
    let content = packet
        .unsigned_chat_content
        .as_ref()
        .map(TextComponent::from_nbt)
        .unwrap_or_else(|| TextComponent::literal(packet.plain_message.clone()));
    let previous_messages_truncated = packet.previous_messages.len() > MAX_PREVIOUS_MESSAGES;
    let previous_messages = packet
        .previous_messages
        .into_iter()
        .take(MAX_PREVIOUS_MESSAGES)
        .map(|previous| PreviousMessage {
            id: previous.id,
            signature: match previous.signature {
                PreviousMessagesItemSignature::V0(signature) => Some(signature),
                PreviousMessagesItemSignature::Default => None,
            },
        })
        .collect();
    let filter = match (packet.filter_type, packet.filter_type_mask) {
        (0, _) => ChatFilter::PassThrough,
        (1, _) => ChatFilter::FullyFiltered,
        (2, PacketPlayerChatFilterTypeMask::V2(mask)) => {
            let truncated = mask.len() > MAX_FILTER_MASK_WORDS;
            ChatFilter::PartiallyFiltered {
                mask: mask.into_iter().take(MAX_FILTER_MASK_WORDS).collect(),
                truncated,
            }
        }
        (other, _) => ChatFilter::Unknown(other),
    };
    ChatMessage {
        sequence: 0,
        kind: ChatKind::Player,
        content,
        sender: Some(TextComponent::from_nbt(&packet.network_name)),
        target: packet
            .network_target_name
            .as_ref()
            .map(TextComponent::from_nbt),
        chat_type: Some(chat_type_reference(packet.r#type)),
        signed: Some(SignedChatData {
            sender_uuid: packet.sender_uuid,
            index: packet.index,
            signature: packet.signature,
            plain_message: packet.plain_message,
            timestamp: packet.timestamp,
            salt: packet.salt,
            previous_messages,
            previous_messages_truncated,
            filter,
        }),
    }
}

fn chat_type_reference(holder: ChatTypesHolder) -> ChatTypeReference {
    match holder {
        Holder::Reference(id) => ChatTypeReference::Registry(id),
        Holder::Inline(types) => ChatTypeReference::Inline(Box::new(chat_decorations(types))),
    }
}

fn chat_decorations(types: ChatTypes) -> ChatDecorations {
    ChatDecorations {
        chat: chat_decoration(types.chat),
        narration: chat_decoration(types.narration),
    }
}

fn chat_decoration(decoration: ChatType) -> ChatDecoration {
    let parameters_truncated = decoration.parameters.len() > MAX_CHAT_TYPE_PARAMETERS;
    let parameters = decoration
        .parameters
        .into_iter()
        .take(MAX_CHAT_TYPE_PARAMETERS)
        .map(|parameter| match parameter {
            ChatTypeParameterType::Content => ChatTypeParameter::Content,
            ChatTypeParameterType::Sender => ChatTypeParameter::Sender,
            ChatTypeParameterType::Target => ChatTypeParameter::Target,
        })
        .collect();
    ChatDecoration {
        translation_key: decoration.translation_key,
        parameters,
        parameters_truncated,
        style: TextComponent::from_nbt(&decoration.style).style,
    }
}

fn boss_bar_update(packet: PacketBossBar) -> BossBarUpdate {
    let id = packet.entity_uuid;
    match packet.action {
        0 => BossBarUpdate::Add(BossBar {
            id,
            title: boss_title(packet.title).unwrap_or_default(),
            progress: boss_progress(packet.health).unwrap_or_default(),
            color: boss_color(packet.color).unwrap_or(BossBarColor::Unknown(-1)),
            overlay: boss_overlay(packet.dividers).unwrap_or(BossBarOverlay::Unknown(-1)),
            flags: boss_flags(packet.flags).unwrap_or(BossBarFlags { raw: 0 }),
        }),
        1 => BossBarUpdate::Remove { id },
        2 => match boss_progress(packet.health) {
            Some(progress) => BossBarUpdate::Progress { id, progress },
            None => BossBarUpdate::Unknown {
                id,
                action: packet.action,
            },
        },
        3 => match boss_title(packet.title) {
            Some(title) => BossBarUpdate::Title { id, title },
            None => BossBarUpdate::Unknown {
                id,
                action: packet.action,
            },
        },
        4 => match (boss_color(packet.color), boss_overlay(packet.dividers)) {
            (Some(color), Some(overlay)) => BossBarUpdate::Style { id, color, overlay },
            _ => BossBarUpdate::Unknown {
                id,
                action: packet.action,
            },
        },
        5 => match boss_flags(packet.flags) {
            Some(flags) => BossBarUpdate::Flags { id, flags },
            None => BossBarUpdate::Unknown {
                id,
                action: packet.action,
            },
        },
        action => BossBarUpdate::Unknown { id, action },
    }
}

fn boss_title(value: PacketBossBarTitle) -> Option<TextComponent> {
    match value {
        PacketBossBarTitle::V0(value) | PacketBossBarTitle::V3(value) => {
            Some(TextComponent::from_nbt(&value))
        }
        PacketBossBarTitle::Default => None,
    }
}

fn boss_progress(value: PacketBossBarHealth) -> Option<f32> {
    match value {
        PacketBossBarHealth::V0(value) | PacketBossBarHealth::V2(value) => Some(value),
        PacketBossBarHealth::Default => None,
    }
}

fn boss_color(value: PacketBossBarColor) -> Option<BossBarColor> {
    match value {
        PacketBossBarColor::V0(value) | PacketBossBarColor::V4(value) => Some(value.into()),
        PacketBossBarColor::Default => None,
    }
}

fn boss_overlay(value: PacketBossBarDividers) -> Option<BossBarOverlay> {
    match value {
        PacketBossBarDividers::V0(value) | PacketBossBarDividers::V4(value) => Some(value.into()),
        PacketBossBarDividers::Default => None,
    }
}

fn boss_flags(value: PacketBossBarFlags) -> Option<BossBarFlags> {
    match value {
        PacketBossBarFlags::V0(raw) | PacketBossBarFlags::V5(raw) => Some(BossBarFlags { raw }),
        PacketBossBarFlags::Default => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minerider_protocol::buffer::PacketWriter;
    use minerider_protocol::nbt::Nbt;
    use minerider_protocol::traits::Encode;

    fn text(value: &str) -> Nbt {
        Nbt::Compound(vec![("text".into(), Nbt::String(value.into()))])
    }

    fn apply_packet<T: Encode>(
        state: &mut PresentationState,
        id: i32,
        packet: &T,
    ) -> PresentationEvent {
        let mut output = PacketWriter::new();
        packet.encode(&mut output).unwrap();
        let payload = output.into_inner();
        let update = decode_update(id, &payload)
            .unwrap()
            .expect("presentation packet");
        state.apply(update)
    }

    #[test]
    fn system_and_explicit_action_bar_packets_share_state() {
        let mut state = PresentationState::default();
        let event = apply_packet(
            &mut state,
            CLIENTBOUND_SYSTEM_CHAT_ID,
            &PacketSystemChat {
                content: text("maintenance soon"),
                is_action_bar: false,
            },
        );
        assert!(matches!(event, PresentationEvent::Chat(_)));
        assert_eq!(
            state.chat.back().unwrap().content.plain_text(),
            "maintenance soon"
        );

        apply_packet(
            &mut state,
            CLIENTBOUND_SYSTEM_CHAT_ID,
            &PacketSystemChat {
                content: text("system bar"),
                is_action_bar: true,
            },
        );
        assert_eq!(
            state.action_bar.as_ref().unwrap().plain_text(),
            "system bar"
        );
        assert_eq!(state.chat.len(), 1);

        apply_packet(
            &mut state,
            CLIENTBOUND_ACTION_BAR_ID,
            &PacketActionBar { text: text("bar") },
        );
        assert_eq!(state.action_bar.as_ref().unwrap().plain_text(), "bar");
    }

    #[test]
    fn player_chat_preserves_raw_signature_and_unsigned_display() {
        let mut state = PresentationState::default();
        let packet = PacketPlayerChat {
            sender_uuid: 42,
            index: 7,
            signature: Some(vec![9; 256]),
            plain_message: "signed body".into(),
            timestamp: 123,
            salt: 456,
            previous_messages: Vec::new(),
            unsigned_chat_content: Some(text("decorated body")),
            filter_type: 2,
            filter_type_mask: PacketPlayerChatFilterTypeMask::V2(vec![3, 4]),
            r#type: Holder::Reference(5),
            network_name: text("Alice"),
            network_target_name: Some(text("Bob")),
        };
        let event = apply_packet(&mut state, CLIENTBOUND_PLAYER_CHAT_ID, &packet);
        let PresentationEvent::Chat(message) = event else {
            panic!("expected chat event");
        };
        assert_eq!(message.content.plain_text(), "decorated body");
        assert_eq!(message.display_text(), "<Alice> decorated body");
        assert_eq!(message.target.as_ref().unwrap().plain_text(), "Bob");
        assert_eq!(message.chat_type, Some(ChatTypeReference::Registry(5)));
        let raw = message.signed.expect("signed metadata");
        assert_eq!(raw.signature.unwrap(), vec![9; 256]);
        assert_eq!(raw.plain_message, "signed body");
        assert!(matches!(
            raw.filter,
            ChatFilter::PartiallyFiltered {
                mask,
                truncated: false
            } if mask == vec![3, 4]
        ));
    }

    #[test]
    fn disguised_chat_is_structured_but_never_marked_signed() {
        let mut state = PresentationState::default();
        let event = apply_packet(
            &mut state,
            CLIENTBOUND_PROFILELESS_CHAT_ID,
            &PacketProfilelessChat {
                message: text("hello"),
                r#type: Holder::Reference(2),
                name: text("Server"),
                target: None,
            },
        );
        let PresentationEvent::Chat(message) = event else {
            panic!("expected chat event");
        };
        assert_eq!(message.kind, ChatKind::Disguised);
        assert!(message.signed.is_none());
        assert_eq!(message.display_text(), "<Server> hello");
    }

    #[test]
    fn title_clear_and_reset_have_distinct_timing_semantics() {
        let mut state = PresentationState::default();
        apply_packet(
            &mut state,
            CLIENTBOUND_SET_TITLE_TEXT_ID,
            &PacketSetTitleText {
                text: text("Title"),
            },
        );
        apply_packet(
            &mut state,
            CLIENTBOUND_SET_TITLE_SUBTITLE_ID,
            &PacketSetTitleSubtitle {
                text: text("Subtitle"),
            },
        );
        apply_packet(
            &mut state,
            CLIENTBOUND_SET_TITLE_TIME_ID,
            &PacketSetTitleTime {
                fade_in: 1,
                stay: 2,
                fade_out: 3,
            },
        );
        apply_packet(
            &mut state,
            CLIENTBOUND_CLEAR_TITLES_ID,
            &PacketClearTitles { reset: false },
        );
        assert!(state.titles.title.is_none());
        assert!(state.titles.subtitle.is_none());
        assert_eq!(state.titles.timing.fade_in, 1);

        apply_packet(
            &mut state,
            CLIENTBOUND_CLEAR_TITLES_ID,
            &PacketClearTitles { reset: true },
        );
        assert_eq!(state.titles.timing, TitleTiming::default());
    }

    #[test]
    fn tab_list_header_and_footer_update_atomically() {
        let mut state = PresentationState::default();
        let event = apply_packet(
            &mut state,
            CLIENTBOUND_PLAYERLIST_HEADER_ID,
            &PacketPlayerlistHeader {
                header: text("Header"),
                footer: text("Footer"),
            },
        );
        assert!(matches!(event, PresentationEvent::TabListChanged { .. }));
        assert_eq!(
            state.tab_list.header.as_ref().unwrap().plain_text(),
            "Header"
        );
        assert_eq!(
            state.tab_list.footer.as_ref().unwrap().plain_text(),
            "Footer"
        );
    }

    fn boss_packet(id: u128, action: i32) -> PacketBossBar {
        PacketBossBar {
            entity_uuid: id,
            action,
            title: PacketBossBarTitle::Default,
            health: PacketBossBarHealth::Default,
            color: PacketBossBarColor::Default,
            dividers: PacketBossBarDividers::Default,
            flags: PacketBossBarFlags::Default,
        }
    }

    #[test]
    fn boss_bar_complete_lifecycle_and_unknown_values() {
        let mut state = PresentationState::default();
        let id = 99;
        apply_packet(
            &mut state,
            CLIENTBOUND_BOSS_BAR_ID,
            &PacketBossBar {
                entity_uuid: id,
                action: 0,
                title: PacketBossBarTitle::V0(text("Raid")),
                health: PacketBossBarHealth::V0(0.75),
                color: PacketBossBarColor::V0(99),
                dividers: PacketBossBarDividers::V0(4),
                flags: PacketBossBarFlags::V0(0x07),
            },
        );
        let bar = state.boss_bars.get(&id).unwrap();
        assert_eq!(bar.color, BossBarColor::Unknown(99));
        assert_eq!(bar.overlay, BossBarOverlay::Notched20);
        assert!(bar.flags.darken_sky());
        assert!(bar.flags.play_end_music());
        assert!(bar.flags.create_world_fog());

        let mut progress = boss_packet(id, 2);
        progress.health = PacketBossBarHealth::V2(0.25);
        apply_packet(&mut state, CLIENTBOUND_BOSS_BAR_ID, &progress);

        let mut title = boss_packet(id, 3);
        title.title = PacketBossBarTitle::V3(text("Updated"));
        apply_packet(&mut state, CLIENTBOUND_BOSS_BAR_ID, &title);

        let mut style = boss_packet(id, 4);
        style.color = PacketBossBarColor::V4(2);
        style.dividers = PacketBossBarDividers::V4(1);
        apply_packet(&mut state, CLIENTBOUND_BOSS_BAR_ID, &style);

        let mut flags = boss_packet(id, 5);
        flags.flags = PacketBossBarFlags::V5(0);
        apply_packet(&mut state, CLIENTBOUND_BOSS_BAR_ID, &flags);

        let bar = state.boss_bars.get(&id).unwrap();
        assert_eq!(bar.progress, 0.25);
        assert_eq!(bar.title.plain_text(), "Updated");
        assert_eq!(bar.color, BossBarColor::Red);
        assert_eq!(bar.overlay, BossBarOverlay::Notched6);
        assert_eq!(bar.flags.raw, 0);

        let removed = apply_packet(&mut state, CLIENTBOUND_BOSS_BAR_ID, &boss_packet(id, 1));
        assert!(matches!(
            removed,
            PresentationEvent::BossBarChanged {
                action: BossBarAction::Remove,
                applied: true,
                current: None,
                ..
            }
        ));
    }

    #[test]
    fn out_of_order_boss_bar_updates_are_safe_noops() {
        let mut state = PresentationState::default();
        let mut packet = boss_packet(123, 2);
        packet.health = PacketBossBarHealth::V2(0.5);
        let event = apply_packet(&mut state, CLIENTBOUND_BOSS_BAR_ID, &packet);
        assert!(matches!(
            event,
            PresentationEvent::BossBarChanged {
                applied: false,
                current: None,
                ..
            }
        ));
        assert!(state.boss_bars.is_empty());

        let missing_remove =
            apply_packet(&mut state, CLIENTBOUND_BOSS_BAR_ID, &boss_packet(123, 1));
        assert!(matches!(
            missing_remove,
            PresentationEvent::BossBarChanged { applied: false, .. }
        ));
    }

    #[test]
    fn collections_are_bounded() {
        let mut state = PresentationState::default();
        for index in 0..(MAX_CHAT_HISTORY + 5) {
            state.apply(PresentationUpdate::Chat(Box::new(ChatMessage {
                sequence: 0,
                kind: ChatKind::System,
                content: TextComponent::literal(index.to_string()),
                sender: None,
                target: None,
                chat_type: None,
                signed: None,
            })));
        }
        assert_eq!(state.chat.len(), MAX_CHAT_HISTORY);
        assert_eq!(state.chat.front().unwrap().sequence, 5);

        for id in 0..(MAX_BOSS_BARS as u128 + 1) {
            state.apply(PresentationUpdate::BossBar(BossBarUpdate::Add(BossBar {
                id,
                title: TextComponent::literal("bar"),
                progress: 1.0,
                color: BossBarColor::White,
                overlay: BossBarOverlay::Progress,
                flags: BossBarFlags { raw: 0 },
            })));
        }
        assert_eq!(state.boss_bars.len(), MAX_BOSS_BARS);
        assert!(!state.boss_bars.contains_key(&(MAX_BOSS_BARS as u128)));
    }

    #[test]
    fn disconnect_reason_is_structured_state_and_event() {
        let mut state = PresentationState::default();
        let event = apply_packet(
            &mut state,
            CLIENTBOUND_KICK_DISCONNECT_ID,
            &PacketKickDisconnect {
                reason: text("Kicked"),
            },
        );
        assert!(matches!(event, PresentationEvent::Disconnected { .. }));
        assert_eq!(
            state.disconnect_reason.as_ref().unwrap().plain_text(),
            "Kicked"
        );
    }
}
