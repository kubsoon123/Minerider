//! Round-trip tests: value → encode → enum-dispatch decode → equal, for
//! every packet of the four small states plus a representative play-state
//! selection (arrays, options, switches, nested switches, NBT, metadata
//! loops).

use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::error::Result;
use minerider_protocol::generated::v1_21_4::{
    configuration, handshaking, login, play, status, types,
};
use minerider_protocol::nbt::Nbt;
use minerider_protocol::traits::Encode;

/// Encodes `packet` (id varint + payload), reads the id back and decodes
/// through the generated enum dispatch, asserting equality and full
/// consumption.
fn rt<E>(packet: E, decode: fn(i32, &mut PacketReader<'_>) -> Result<E>)
where
    E: Encode + PartialEq + std::fmt::Debug,
{
    let mut w = PacketWriter::new();
    packet.encode(&mut w).expect("encode");
    let bytes = w.freeze();
    let mut r = PacketReader::new(&bytes);
    let id = r.get_varint().expect("packet id varint");
    let back = decode(id, &mut r).expect("enum decode");
    assert_eq!(packet, back, "round-trip mismatch");
    assert!(r.is_empty(), "trailing bytes after decode");
}

fn nbt_text(text: &str) -> Nbt {
    Nbt::Compound(vec![("text".to_string(), Nbt::String(text.to_string()))])
}

fn settings() -> types::PacketCommonSettings {
    types::PacketCommonSettings {
        locale: "en_us".to_string(),
        view_distance: 10,
        chat_flags: 0,
        chat_colors: true,
        skin_parts: 0x7F,
        main_hand: 1,
        enable_text_filtering: false,
        enable_server_listing: true,
        particle_status: types::PacketCommonSettingsParticleStatus::Minimal,
    }
}

fn known_packs() -> types::PacketCommonSelectKnownPacks {
    types::PacketCommonSelectKnownPacks {
        packs: vec![types::PacketCommonSelectKnownPacksPacksItem {
            namespace: "minecraft".to_string(),
            id: "core".to_string(),
            version: "1.21.4".to_string(),
        }],
    }
}

fn report_details() -> types::PacketCommonCustomReportDetails {
    types::PacketCommonCustomReportDetails {
        details: vec![types::PacketCommonCustomReportDetailsDetailsItem {
            key: "k".to_string(),
            value: "v".to_string(),
        }],
    }
}

#[test]
fn handshaking_roundtrips() {
    use handshaking::*;
    rt(
        ServerboundHandshakingPacket::SetProtocol(PacketSetProtocol {
            protocol_version: 769,
            server_host: "localhost".to_string(),
            server_port: 25565,
            next_state: 2,
        }),
        ServerboundHandshakingPacket::decode,
    );
    rt(
        ServerboundHandshakingPacket::LegacyServerListPing(PacketLegacyServerListPing {
            payload: 1,
        }),
        ServerboundHandshakingPacket::decode,
    );
}

#[test]
fn status_roundtrips() {
    use status::*;
    rt(
        ClientboundStatusPacket::ServerInfo(PacketServerInfo {
            response: "{\"version\":{}}".to_string(),
        }),
        ClientboundStatusPacket::decode,
    );
    rt(
        ClientboundStatusPacket::Ping(PacketPing { time: -7 }),
        ClientboundStatusPacket::decode,
    );
    rt(
        ServerboundStatusPacket::PingStart,
        ServerboundStatusPacket::decode,
    );
    rt(
        ServerboundStatusPacket::Ping(PacketPing { time: 7 }),
        ServerboundStatusPacket::decode,
    );
}

#[test]
fn login_roundtrips() {
    use login::*;
    rt(
        ClientboundLoginPacket::Disconnect(PacketDisconnect {
            reason: "{\"text\":\"no\"}".to_string(),
        }),
        ClientboundLoginPacket::decode,
    );
    rt(
        ClientboundLoginPacket::EncryptionBegin(PacketEncryptionBegin {
            server_id: String::new(),
            public_key: vec![1, 2, 3],
            verify_token: vec![4],
            should_authenticate: false,
        }),
        ClientboundLoginPacket::decode,
    );
    rt(
        ClientboundLoginPacket::Success(PacketSuccess {
            uuid: 42,
            username: "u".to_string(),
            properties: vec![PacketSuccessPropertiesItem {
                name: "n".to_string(),
                value: "v".to_string(),
                signature: Some("s".to_string()),
            }],
        }),
        ClientboundLoginPacket::decode,
    );
    rt(
        ClientboundLoginPacket::Compress(PacketCompress { threshold: 256 }),
        ClientboundLoginPacket::decode,
    );
    rt(
        ClientboundLoginPacket::LoginPluginRequest(PacketLoginPluginRequest {
            message_id: 3,
            channel: "fml:handshake".to_string(),
            data: vec![9, 8, 7],
        }),
        ClientboundLoginPacket::decode,
    );
    rt(
        ClientboundLoginPacket::CookieRequest(types::PacketCommonCookieRequest {
            cookie: "session".to_string(),
        }),
        ClientboundLoginPacket::decode,
    );
    rt(
        ServerboundLoginPacket::LoginStart(PacketLoginStart {
            username: "Notch".to_string(),
            player_uuid: 0,
        }),
        ServerboundLoginPacket::decode,
    );
    rt(
        ServerboundLoginPacket::EncryptionBegin(PacketEncryptionBeginServerbound {
            shared_secret: vec![5, 5],
            verify_token: vec![6],
        }),
        ServerboundLoginPacket::decode,
    );
    rt(
        ServerboundLoginPacket::LoginPluginResponse(PacketLoginPluginResponse {
            message_id: 3,
            data: None,
        }),
        ServerboundLoginPacket::decode,
    );
    rt(
        ServerboundLoginPacket::LoginAcknowledged,
        ServerboundLoginPacket::decode,
    );
    rt(
        ServerboundLoginPacket::CookieResponse(types::PacketCommonCookieResponse {
            key: "session".to_string(),
            value: Some(vec![1, 2]),
        }),
        ServerboundLoginPacket::decode,
    );
}

#[test]
fn configuration_roundtrips() {
    use configuration::*;
    let cb: Vec<ClientboundConfigurationPacket> = vec![
        ClientboundConfigurationPacket::CookieRequest(types::PacketCommonCookieRequest {
            cookie: "c".to_string(),
        }),
        ClientboundConfigurationPacket::CustomPayload(PacketCustomPayload {
            channel: "minecraft:brand".to_string(),
            data: b"\x07vanilla".to_vec(),
        }),
        ClientboundConfigurationPacket::Disconnect(PacketDisconnect {
            reason: nbt_text("no"),
        }),
        ClientboundConfigurationPacket::FinishConfiguration,
        ClientboundConfigurationPacket::KeepAlive(PacketKeepAlive { keep_alive_id: 1 }),
        ClientboundConfigurationPacket::Ping(PacketPing { id: 2 }),
        ClientboundConfigurationPacket::ResetChat,
        ClientboundConfigurationPacket::RegistryData(PacketRegistryData {
            id: "minecraft:dimension_type".to_string(),
            entries: vec![PacketRegistryDataEntriesItem {
                key: "minecraft:overworld".to_string(),
                value: None,
            }],
        }),
        ClientboundConfigurationPacket::RemoveResourcePack(types::PacketCommonRemoveResourcePack {
            uuid: Some(7),
        }),
        ClientboundConfigurationPacket::AddResourcePack(types::PacketCommonAddResourcePack {
            uuid: 7,
            url: "https://example.org/pack.zip".to_string(),
            hash: "0123456789abcdef0123456789abcdef01234567".to_string(),
            forced: true,
            prompt_message: Some(nbt_text("get the pack")),
        }),
        ClientboundConfigurationPacket::StoreCookie(types::PacketCommonStoreCookie {
            key: "k".to_string(),
            value: vec![1, 2, 3],
        }),
        ClientboundConfigurationPacket::Transfer(types::PacketCommonTransfer {
            host: "example.org".to_string(),
            port: 25565,
        }),
        ClientboundConfigurationPacket::FeatureFlags(PacketFeatureFlags {
            features: vec!["minecraft:vanilla".to_string()],
        }),
        ClientboundConfigurationPacket::Tags(PacketTags { tags: vec![] }),
        ClientboundConfigurationPacket::SelectKnownPacks(known_packs()),
        ClientboundConfigurationPacket::CustomReportDetails(report_details()),
        ClientboundConfigurationPacket::ServerLinks(types::PacketCommonServerLinks {
            links: vec![],
        }),
    ];
    for packet in cb {
        rt(packet, ClientboundConfigurationPacket::decode);
    }

    let sb: Vec<ServerboundConfigurationPacket> = vec![
        ServerboundConfigurationPacket::Settings(settings()),
        ServerboundConfigurationPacket::CookieResponse(types::PacketCommonCookieResponse {
            key: "k".to_string(),
            value: None,
        }),
        ServerboundConfigurationPacket::CustomPayload(PacketCustomPayload {
            channel: "minecraft:brand".to_string(),
            data: b"\x09minerider".to_vec(),
        }),
        ServerboundConfigurationPacket::FinishConfiguration,
        ServerboundConfigurationPacket::KeepAlive(PacketKeepAlive { keep_alive_id: 1 }),
        ServerboundConfigurationPacket::Pong(PacketPong { id: 2 }),
        ServerboundConfigurationPacket::ResourcePackReceive(PacketResourcePackReceive {
            uuid: 7,
            result: 0,
        }),
        ServerboundConfigurationPacket::SelectKnownPacks(known_packs()),
        ServerboundConfigurationPacket::CustomReportDetails(report_details()),
        ServerboundConfigurationPacket::ServerLinks(types::PacketCommonServerLinks {
            links: vec![],
        }),
    ];
    for packet in sb {
        rt(packet, ServerboundConfigurationPacket::decode);
    }
}

#[test]
fn play_roundtrips() {
    use play::*;
    rt(
        ClientboundPlayPacket::KeepAlive(PacketKeepAlive {
            keep_alive_id: 0x0102030405060708,
        }),
        ClientboundPlayPacket::decode,
    );
    rt(
        ClientboundPlayPacket::KickDisconnect(PacketKickDisconnect {
            reason: nbt_text("bye"),
        }),
        ClientboundPlayPacket::decode,
    );
    // entityMetadataLoop (255-terminated).
    rt(
        ClientboundPlayPacket::EntityMetadata(PacketEntityMetadata {
            entity_id: 5,
            metadata: types::EntityMetadata(vec![]),
        }),
        ClientboundPlayPacket::decode,
    );
    // Nested switch: styling is a switch on `action` whose branches are
    // switches on `number_format` (an option<varint>).
    rt(
        ClientboundPlayPacket::ScoreboardObjective(PacketScoreboardObjective {
            name: "objective".to_string(),
            action: 0,
            display_text: PacketScoreboardObjectiveDisplayText::V0(nbt_text("Score")),
            r#type: PacketScoreboardObjectiveType::V0(1),
            number_format: PacketScoreboardObjectiveNumberFormat::V0(Some(1)),
            styling: PacketScoreboardObjectiveStyling::V0(PacketScoreboardObjectiveStylingV0::V1(
                nbt_text("bold"),
            )),
        }),
        ClientboundPlayPacket::decode,
    );
    rt(
        ClientboundPlayPacket::ScoreboardObjective(PacketScoreboardObjective {
            name: "objective".to_string(),
            action: 2,
            display_text: PacketScoreboardObjectiveDisplayText::V2(nbt_text("Score")),
            r#type: PacketScoreboardObjectiveType::V2(0),
            number_format: PacketScoreboardObjectiveNumberFormat::V2(Some(2)),
            styling: PacketScoreboardObjectiveStyling::V2(PacketScoreboardObjectiveStylingV2::V2(
                nbt_text("red"),
            )),
        }),
        ClientboundPlayPacket::decode,
    );
    // Switch default branch (action 1 carries nothing).
    rt(
        ClientboundPlayPacket::ScoreboardObjective(PacketScoreboardObjective {
            name: "objective".to_string(),
            action: 1,
            display_text: PacketScoreboardObjectiveDisplayText::Default,
            r#type: PacketScoreboardObjectiveType::Default,
            number_format: PacketScoreboardObjectiveNumberFormat::Default,
            styling: PacketScoreboardObjectiveStyling::Default,
        }),
        ClientboundPlayPacket::decode,
    );
    // Option<varint> discriminant switch (scoreboard score styling).
    rt(
        ClientboundPlayPacket::ScoreboardScore(PacketScoreboardScore {
            item_name: "player".to_string(),
            score_name: "objective".to_string(),
            value: 3,
            display_name: Some(nbt_text("Player")),
            number_format: Some(1),
            styling: PacketScoreboardScoreStyling::V1(nbt_text("styled")),
        }),
        ClientboundPlayPacket::decode,
    );
    rt(
        ClientboundPlayPacket::ScoreboardScore(PacketScoreboardScore {
            item_name: "player".to_string(),
            score_name: "objective".to_string(),
            value: -3,
            display_name: None,
            number_format: None,
            styling: PacketScoreboardScoreStyling::Default,
        }),
        ClientboundPlayPacket::decode,
    );
    // Arrays of structs + varint arrays.
    rt(
        ClientboundPlayPacket::DeclareRecipes(PacketDeclareRecipes {
            recipes: vec![PacketDeclareRecipesRecipesItem {
                name: "minecraft:shaped".to_string(),
                items: vec![1, 2, 3],
            }],
            stone_cutter_recipes: vec![PacketDeclareRecipesStoneCutterRecipesItem {
                input_: types::IdSet::Ids(vec![10, 20]),
                slot_display: SlotDisplay {
                    r#type: SlotDisplayType::Item,
                    data: SlotDisplayData::Item(55),
                },
            }],
        }),
        ClientboundPlayPacket::decode,
    );
}
