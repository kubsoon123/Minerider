//! Malformed-input tests for the generated codecs: truncated fields,
//! invalid VarInts, oversized lengths, invalid mapper values, trailing
//! bytes and unknown ids. Nothing may panic, hang or allocate
//! uncontrollably.

use minerider_protocol::buffer::PacketReader;
use minerider_protocol::error::ProtocolError;
use minerider_protocol::generated::v1_21_4::{configuration, login, play, types};
use minerider_protocol::traits::Decode;

fn decode_err<T: Decode + std::fmt::Debug>(bytes: &[u8]) -> ProtocolError {
    let mut r = PacketReader::new(bytes);
    T::decode(&mut r).expect_err("malformed input must be rejected")
}

#[test]
fn truncated_fields_error() {
    // Login Start: username length 5 but only 2 bytes follow.
    let err = decode_err::<login::PacketLoginStart>(&[0x05, b'N', b'o']);
    assert!(matches!(
        err,
        ProtocolError::BufferUnderflow { .. } | ProtocolError::InvalidString(_)
    ));
}

#[test]
fn invalid_varint_errors() {
    // Six continuation bytes: VarInt may use at most five.
    let err = decode_err::<login::PacketCompress>(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]);
    assert!(matches!(err, ProtocolError::VarIntTooLong));
}

#[test]
fn oversized_string_does_not_allocate() {
    // String length 127 with 3 bytes available: must error before copying.
    let err = decode_err::<login::PacketLoginStart>(&[0x7F, b'a', b'b', b'c']);
    assert!(matches!(err, ProtocolError::BufferUnderflow { .. }));
}

#[test]
fn oversized_array_does_not_allocate() {
    // Feature flags: count 2^30-1 with 1 byte available.
    let err =
        decode_err::<configuration::PacketFeatureFlags>(&[0xFF, 0xFF, 0xFF, 0xFF, 0x07, 0x00]);
    assert!(matches!(err, ProtocolError::BufferUnderflow { .. }));
}

#[test]
fn negative_array_length_errors() {
    let err = decode_err::<configuration::PacketFeatureFlags>(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
    assert!(matches!(err, ProtocolError::NegativeLength(_)));
}

#[test]
fn invalid_mapper_value_errors() {
    // Client information with particle_status varint 9 (valid: 0..=2).
    let mut bytes = vec![0x05];
    bytes.extend_from_slice(b"en_us");
    bytes.extend_from_slice(&[0x0A, 0x00, 0x01, 0x7F, 0x01, 0x00, 0x01, 0x09]);
    let err = decode_err::<types::PacketCommonSettings>(&bytes);
    assert!(matches!(
        err,
        ProtocolError::UnknownEnumValue { value: 9, .. }
    ));
}

#[test]
fn trailing_bytes_rejected_by_enum_dispatch() {
    // Compress threshold 5 plus a stray byte.
    let mut r = PacketReader::new(&[0x05, 0x00]);
    let err = login::ClientboundLoginPacket::decode(login::CLIENTBOUND_COMPRESS_ID, &mut r)
        .expect_err("trailing bytes must be rejected");
    assert!(matches!(err, ProtocolError::TrailingBytes { .. }));
}

#[test]
fn unknown_packet_id_rejected() {
    let mut r = PacketReader::new(&[]);
    let err = login::ClientboundLoginPacket::decode(0x7F, &mut r)
        .expect_err("unknown id must be rejected");
    assert!(matches!(
        err,
        ProtocolError::UnknownPacketId { id: 0x7F, .. }
    ));
}

#[test]
fn nbt_depth_bomb_rejected_without_stack_overflow() {
    // Configuration Disconnect reason: 200 nested compounds, then TAG_Ends.
    let mut bytes = vec![0x0A];
    for _ in 0..200 {
        bytes.extend_from_slice(&[0x0A, 0x00, 0x01, b'a']);
    }
    bytes.extend(std::iter::repeat_n(0x00, 201));
    let err = decode_err::<configuration::PacketDisconnect>(&bytes);
    assert!(matches!(err, ProtocolError::InvalidNbt(_)));
}

#[test]
fn switch_default_branch_decodes() {
    // Scoreboard score with number_format absent (option varint = false):
    // the styling switch falls to its default (void) branch.
    let mut bytes = vec![0x06];
    bytes.extend_from_slice(b"player");
    bytes.push(0x09);
    bytes.extend_from_slice(b"objective");
    bytes.push(0x03); // value (varint)
    bytes.push(0x00); // display_name: None
    bytes.push(0x00); // number_format: None
    let mut r = PacketReader::new(&bytes);
    let packet = play::PacketScoreboardScore::decode(&mut r).expect("decode");
    assert_eq!(packet.number_format, None);
    assert_eq!(packet.styling, play::PacketScoreboardScoreStyling::Default);
    assert!(r.is_empty());
}

#[test]
fn every_truncated_prefix_is_rejected_cleanly() {
    // A valid Login Success packet; no proper prefix may panic or succeed.
    let valid: &[u8] = &[
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
        0xFF, 0x04, b'M', b'o', b'c', b'k', 0x00,
    ];
    for prefix_len in 0..valid.len() {
        let mut r = PacketReader::new(&valid[..prefix_len]);
        let _ =
            login::PacketSuccess::decode(&mut r).expect_err("truncated packet must be rejected");
    }
}
