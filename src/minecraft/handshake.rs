//! Handshake packet (C2S 0x00): protocol version, address, next state.

use minerider_protocol::buffer::PacketWriter;

use crate::core::error::Result;
use crate::network::connection::Connection;

/// Serverbound handshake packet id (all protocol states).
pub const HANDSHAKE_PACKET_ID: i32 = 0x00;

/// The Handshake packet, sent immediately after connecting.
#[derive(Debug, Clone)]
pub struct Handshake<'a> {
    /// Client protocol version (769 for Minecraft 1.21.4).
    pub protocol_version: i32,
    /// Server address the client used to connect.
    pub server_address: &'a str,
    /// Server port.
    pub server_port: u16,
    /// Next state: 1 = status (server list ping), 2 = login.
    pub next_state: i32,
}

impl Handshake<'_> {
    /// Serializes the handshake fields into a packet payload.
    pub fn encode(&self) -> Result<bytes::Bytes> {
        let mut w = PacketWriter::new();
        w.put_varint(self.protocol_version);
        w.put_string(self.server_address)?;
        w.put_u16(self.server_port);
        w.put_varint(self.next_state);
        Ok(w.freeze())
    }
}

/// Sends the handshake and transitions nothing — the state change happens
/// when the client sends its first packet of the next state.
pub async fn send(conn: &mut Connection, handshake: &Handshake<'_>) -> Result<()> {
    conn.send_packet(HANDSHAKE_PACKET_ID, &handshake.encode()?)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use minerider_protocol::buffer::PacketReader;

    #[test]
    fn encode_fields() {
        let hs = Handshake {
            protocol_version: 769,
            server_address: "localhost",
            server_port: 25565,
            next_state: 2,
        };
        let payload = hs.encode().unwrap();
        let mut r = PacketReader::new(&payload);
        assert_eq!(r.get_varint().unwrap(), 769);
        assert_eq!(r.read_string().unwrap(), "localhost");
        assert_eq!(r.get_u16().unwrap(), 25565);
        assert_eq!(r.get_varint().unwrap(), 2);
        assert!(r.is_empty());
    }
}
