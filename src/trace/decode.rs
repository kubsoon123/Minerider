//! Packet name lookup and best-effort field decoding for trace events,
//! routed through the generated protocol definitions.

use minerider_protocol::buffer::PacketReader;
use minerider_protocol::generated::v1_21_4::{configuration, handshaking, login, play, status};

use crate::core::state::ConnectionState;
use crate::trace::format::Direction;

/// Resolves a packet id to its generated name for the given
/// state/direction.
pub fn packet_name(state: ConnectionState, dir: Direction, id: i32) -> Option<&'static str> {
    use ConnectionState as S;
    use Direction as D;
    match (state, dir) {
        (S::Handshaking, D::Clientbound) => handshaking::clientbound_packet_name(id),
        (S::Handshaking, D::Serverbound) => handshaking::serverbound_packet_name(id),
        (S::Status, D::Clientbound) => status::clientbound_packet_name(id),
        (S::Status, D::Serverbound) => status::serverbound_packet_name(id),
        (S::Login, D::Clientbound) => login::clientbound_packet_name(id),
        (S::Login, D::Serverbound) => login::serverbound_packet_name(id),
        (S::Configuration, D::Clientbound) => configuration::clientbound_packet_name(id),
        (S::Configuration, D::Serverbound) => configuration::serverbound_packet_name(id),
        (S::Play, D::Clientbound) => play::clientbound_packet_name(id),
        (S::Play, D::Serverbound) => play::serverbound_packet_name(id),
    }
}

/// Decodes a packet payload through the generated enum of its
/// state/direction and serializes the result to JSON. Returns `None` when
/// the id is unknown or decoding fails.
pub fn decode_fields(
    state: ConnectionState,
    dir: Direction,
    id: i32,
    payload: &[u8],
) -> Option<serde_json::Value> {
    use ConnectionState as S;
    use Direction as D;
    let mut r = PacketReader::new(payload);
    let value = match (state, dir) {
        (S::Handshaking, D::Clientbound) => None,
        (S::Handshaking, D::Serverbound) => {
            handshaking::ServerboundHandshakingPacket::decode(id, &mut r)
                .ok()
                .and_then(|p| serde_json::to_value(p).ok())
        }
        (S::Status, D::Clientbound) => status::ClientboundStatusPacket::decode(id, &mut r)
            .ok()
            .and_then(|p| serde_json::to_value(p).ok()),
        (S::Status, D::Serverbound) => status::ServerboundStatusPacket::decode(id, &mut r)
            .ok()
            .and_then(|p| serde_json::to_value(p).ok()),
        (S::Login, D::Clientbound) => login::ClientboundLoginPacket::decode(id, &mut r)
            .ok()
            .and_then(|p| serde_json::to_value(p).ok()),
        (S::Login, D::Serverbound) => login::ServerboundLoginPacket::decode(id, &mut r)
            .ok()
            .and_then(|p| serde_json::to_value(p).ok()),
        (S::Configuration, D::Clientbound) => {
            configuration::ClientboundConfigurationPacket::decode(id, &mut r)
                .ok()
                .and_then(|p| serde_json::to_value(p).ok())
        }
        (S::Configuration, D::Serverbound) => {
            configuration::ServerboundConfigurationPacket::decode(id, &mut r)
                .ok()
                .and_then(|p| serde_json::to_value(p).ok())
        }
        (S::Play, D::Clientbound) => play::ClientboundPlayPacket::decode(id, &mut r)
            .ok()
            .and_then(|p| serde_json::to_value(p).ok()),
        (S::Play, D::Serverbound) => play::ServerboundPlayPacket::decode(id, &mut r)
            .ok()
            .and_then(|p| serde_json::to_value(p).ok()),
    }?;
    Some(value)
}

/// A state name as recorded in traces.
pub fn state_name(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Handshaking => "handshaking",
        ConnectionState::Status => "status",
        ConnectionState::Login => "login",
        ConnectionState::Configuration => "configuration",
        ConnectionState::Play => "play",
    }
}
