//! Golden tests: byte-exact encoding/decoding of selected 1.21.4 packets.
//!
//! Every byte sequence here was derived by hand from the protocol spec; the
//! generated codecs must reproduce it exactly in both directions.

use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::generated::v1_21_4::{
    configuration, handshaking, login, play, status, types,
};
use minerider_protocol::nbt::Nbt;
use minerider_protocol::traits::{Decode, Encode};

/// Asserts that `value` encodes to exactly `bytes` and that `bytes` decode
/// back to `value`, consuming the whole buffer.
fn assert_golden<T>(value: &T, bytes: &[u8])
where
    T: Encode + Decode + PartialEq + std::fmt::Debug,
{
    let mut w = PacketWriter::new();
    value.encode(&mut w).expect("encode");
    assert_eq!(&w.freeze()[..], bytes, "encoding mismatch for {value:?}");
    let mut r = PacketReader::new(bytes);
    let decoded = T::decode(&mut r).expect("decode");
    assert_eq!(&decoded, value, "decoding mismatch");
    assert!(r.is_empty(), "trailing bytes after decode");
}

fn text_component(text: &str) -> Nbt {
    Nbt::Compound(vec![("text".to_string(), Nbt::String(text.to_string()))])
}

#[test]
fn handshake_set_protocol() {
    let value = handshaking::PacketSetProtocol {
        protocol_version: 769,
        server_host: "localhost".to_string(),
        server_port: 25565,
        next_state: 2,
    };
    let mut bytes = vec![0x81, 0x06, 0x09];
    bytes.extend_from_slice(b"localhost");
    bytes.extend_from_slice(&[0x63, 0xDD, 0x02]);
    assert_golden(&value, &bytes);
}

#[test]
fn login_start() {
    let value = login::PacketLoginStart {
        username: "Notch".to_string(),
        player_uuid: 0,
    };
    let mut bytes = vec![0x05];
    bytes.extend_from_slice(b"Notch");
    bytes.extend_from_slice(&[0u8; 16]);
    assert_golden(&value, &bytes);
}

#[test]
fn encryption_request_clientbound() {
    let value = login::PacketEncryptionBegin {
        server_id: String::new(),
        public_key: vec![1, 2, 3],
        verify_token: vec![4, 5],
        should_authenticate: true,
    };
    let bytes = [0x00, 0x03, 1, 2, 3, 0x02, 4, 5, 0x01];
    assert_golden(&value, &bytes);
}

#[test]
fn encryption_response_serverbound() {
    // The serverbound layout has NO server_id and NO should_authenticate;
    // this vector guards against cross-direction layout confusion.
    let value = login::PacketEncryptionBeginServerbound {
        shared_secret: vec![9, 9],
        verify_token: vec![8],
    };
    let bytes = [0x02, 9, 9, 0x01, 8];
    assert_golden(&value, &bytes);
}

#[test]
fn set_compression() {
    let value = login::PacketCompress { threshold: 300 };
    assert_golden(&value, &[0xAC, 0x02]);
}

#[test]
fn login_success_no_properties() {
    let value = login::PacketSuccess {
        uuid: 0x00112233445566778899AABBCCDDEEFF,
        username: "Mock".to_string(),
        properties: vec![],
    };
    let mut bytes = vec![
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
        0xFF, 0x04,
    ];
    bytes.extend_from_slice(b"Mock");
    bytes.push(0x00);
    assert_golden(&value, &bytes);
}

#[test]
fn login_success_two_properties() {
    let value = login::PacketSuccess {
        uuid: 0,
        username: "u".to_string(),
        properties: vec![
            login::PacketSuccessPropertiesItem {
                name: "a".to_string(),
                value: "b".to_string(),
                signature: None,
            },
            login::PacketSuccessPropertiesItem {
                name: "c".to_string(),
                value: "d".to_string(),
                signature: Some("sig".to_string()),
            },
        ],
    };
    let mut bytes = vec![0u8; 16];
    bytes.extend_from_slice(&[0x01, b'u', 0x02]);
    bytes.extend_from_slice(&[0x01, b'a', 0x01, b'b', 0x00]);
    bytes.extend_from_slice(&[0x01, b'c', 0x01, b'd', 0x01, 0x03, b's', b'i', b'g']);
    assert_golden(&value, &bytes);
}

#[test]
fn login_acknowledged_is_empty() {
    let mut w = PacketWriter::new();
    login::ServerboundLoginPacket::LoginAcknowledged
        .encode(&mut w)
        .expect("encode");
    assert_eq!(&w.freeze()[..], &[0x03]);
}

#[test]
fn login_disconnect_string_reason() {
    let value = login::PacketDisconnect {
        reason: "{\"text\":\"bye\"}".to_string(),
    };
    let mut bytes = vec![0x0E];
    bytes.extend_from_slice(b"{\"text\":\"bye\"}");
    assert_golden(&value, &bytes);
}

#[test]
fn configuration_keep_alive() {
    let value = configuration::PacketKeepAlive { keep_alive_id: 42 };
    assert_golden(&value, &[0, 0, 0, 0, 0, 0, 0, 42]);
}

#[test]
fn configuration_ping_pong() {
    assert_golden(&configuration::PacketPing { id: 7 }, &[0, 0, 0, 7]);
    assert_golden(
        &configuration::PacketPong { id: -7 },
        &[0xFF, 0xFF, 0xFF, 0xF9],
    );
}

#[test]
fn configuration_disconnect_nbt_reason() {
    let value = configuration::PacketDisconnect {
        reason: text_component("kicked"),
    };
    let bytes = [
        0x0A, 0x08, 0x00, 0x04, b't', b'e', b'x', b't', 0x00, 0x06, b'k', b'i', b'c', b'k', b'e',
        b'd', 0x00,
    ];
    assert_golden(&value, &bytes);
}

#[test]
fn finish_configuration_is_empty() {
    let mut w = PacketWriter::new();
    configuration::ServerboundConfigurationPacket::FinishConfiguration
        .encode(&mut w)
        .expect("encode");
    assert_eq!(&w.freeze()[..], &[0x03]);
}

#[test]
fn client_information() {
    let value = types::PacketCommonSettings {
        locale: "en_us".to_string(),
        view_distance: 10,
        chat_flags: 0,
        chat_colors: true,
        skin_parts: 0x7F,
        main_hand: 1,
        enable_text_filtering: false,
        enable_server_listing: true,
        particle_status: types::PacketCommonSettingsParticleStatus::All,
    };
    let mut bytes = vec![0x05];
    bytes.extend_from_slice(b"en_us");
    bytes.extend_from_slice(&[0x0A, 0x00, 0x01, 0x7F, 0x01, 0x00, 0x01, 0x00]);
    assert_golden(&value, &bytes);
}

#[test]
fn status_ping_pong() {
    assert_golden(&status::PacketPing { time: 42 }, &[0, 0, 0, 0, 0, 0, 0, 42]);
    let info = status::PacketServerInfo {
        response: "{}".to_string(),
    };
    assert_golden(&info, &[0x02, b'{', b'}']);
}

#[test]
fn play_keep_alive() {
    let value = play::PacketKeepAlive { keep_alive_id: 42 };
    assert_golden(&value, &[0, 0, 0, 0, 0, 0, 0, 42]);
}

#[test]
fn play_kick_disconnect_nbt_reason() {
    let value = play::PacketKickDisconnect {
        reason: text_component("bye"),
    };
    let bytes = [
        0x0A, 0x08, 0x00, 0x04, b't', b'e', b'x', b't', 0x00, 0x03, b'b', b'y', b'e', 0x00,
    ];
    assert_golden(&value, &bytes);
}

#[test]
fn enum_encode_prefixes_packet_id() {
    let mut w = PacketWriter::new();
    login::ClientboundLoginPacket::Compress(login::PacketCompress { threshold: 300 })
        .encode(&mut w)
        .expect("encode");
    assert_eq!(&w.freeze()[..], &[0x03, 0xAC, 0x02]);

    let mut r = PacketReader::new(&[0xAC, 0x02]);
    let decoded = login::ClientboundLoginPacket::decode(login::CLIENTBOUND_COMPRESS_ID, &mut r)
        .expect("enum decode");
    assert_eq!(
        decoded,
        login::ClientboundLoginPacket::Compress(login::PacketCompress { threshold: 300 })
    );
}
