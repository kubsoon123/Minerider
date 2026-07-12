//! Handshake packet (C2S 0x00): protocol version, address, next state.
//!
//! The packet layout comes from the generated protocol
//! (`minerider_protocol::generated::v1_21_4::handshaking`).

use minerider_protocol::buffer::PacketWriter;
use minerider_protocol::generated::v1_21_4::handshaking::{
    PacketSetProtocol, SERVERBOUND_SET_PROTOCOL_ID,
};
use minerider_protocol::traits::Encode;

use crate::core::error::Result;
use crate::network::connection::Connection;

/// Next-state value for the login state.
pub const NEXT_STATE_LOGIN: i32 = 2;

/// Sends the handshake, transitioning the connection to the login state.
///
/// The state change itself happens when the client sends its first packet
/// of the next state (Login Start).
pub async fn send(
    conn: &mut Connection,
    protocol_version: i32,
    server_address: &str,
    server_port: u16,
) -> Result<()> {
    let handshake = PacketSetProtocol {
        protocol_version,
        server_host: server_address.to_string(),
        server_port,
        next_state: NEXT_STATE_LOGIN,
    };
    let mut w = PacketWriter::new();
    handshake.encode(&mut w)?;
    conn.send_packet(SERVERBOUND_SET_PROTOCOL_ID, &w.freeze())
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use minerider_protocol::buffer::PacketReader;
    use minerider_protocol::traits::Decode;

    #[test]
    fn encode_fields() {
        let hs = PacketSetProtocol {
            protocol_version: 769,
            server_host: "localhost".to_string(),
            server_port: 25565,
            next_state: NEXT_STATE_LOGIN,
        };
        let mut w = PacketWriter::new();
        hs.encode(&mut w).unwrap();
        let payload = w.freeze();
        let mut r = PacketReader::new(&payload);
        assert_eq!(r.get_varint().unwrap(), 769);
        assert_eq!(r.read_string().unwrap(), "localhost");
        assert_eq!(r.get_u16().unwrap(), 25565);
        assert_eq!(r.get_varint().unwrap(), 2);
        assert!(r.is_empty());
    }

    #[test]
    fn roundtrip() {
        let hs = PacketSetProtocol {
            protocol_version: 769,
            server_host: "example.org".to_string(),
            server_port: 25565,
            next_state: NEXT_STATE_LOGIN,
        };
        let mut w = PacketWriter::new();
        hs.encode(&mut w).unwrap();
        let payload = w.freeze();
        let mut r = PacketReader::new(&payload);
        let back = PacketSetProtocol::decode(&mut r).unwrap();
        assert_eq!(hs, back);
    }
}
