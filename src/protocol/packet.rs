//! Packet representation and id/type registries.

use bytes::BytesMut;

use super::buffer::PacketWriter;

/// A decoded packet: a numeric id plus the payload bytes excluding the id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPacket {
    /// VarInt packet id.
    pub id: i32,
    /// Payload bytes, excluding the leading packet id.
    pub payload: BytesMut,
}

impl RawPacket {
    /// Creates a packet with the given id and payload.
    pub fn new(id: i32, payload: impl Into<BytesMut>) -> Self {
        Self {
            id,
            payload: payload.into(),
        }
    }

    /// Creates a packet with an empty payload.
    pub fn empty(id: i32) -> Self {
        Self::new(id, BytesMut::new())
    }

    /// Builds a packet by serializing fields into a [`PacketWriter`].
    pub fn build(id: i32, f: impl FnOnce(&mut PacketWriter)) -> Self {
        let mut w = PacketWriter::new();
        f(&mut w);
        Self::new(id, w.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::buffer::PacketReader;

    #[test]
    fn constructors() {
        let p = RawPacket::empty(0x00);
        assert_eq!(p.id, 0);
        assert!(p.payload.is_empty());

        let p = RawPacket::build(0x2A, |w| {
            w.put_i32(7);
            w.put_string("hi").unwrap();
        });
        assert_eq!(p.id, 0x2A);
        let mut r = PacketReader::new(&p.payload);
        assert_eq!(r.get_i32().unwrap(), 7);
        assert_eq!(r.read_string().unwrap(), "hi");
    }
}
