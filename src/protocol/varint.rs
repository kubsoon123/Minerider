//! Minecraft VarInt (i32, max 5 bytes) encoding.

use bytes::{BufMut, BytesMut};

use super::buffer::PacketReader;
use crate::core::error::{MineRiderError, Result};

/// Maximum number of bytes an encoded VarInt may occupy.
pub const MAX_VARINT_BYTES: usize = 5;

/// Encodes `v` as a Minecraft VarInt and appends it to `buf`.
///
/// Seven bits are stored per byte with the high bit (0x80) marking
/// continuation. Negative values are encoded in two's complement.
pub fn write_varint(buf: &mut BytesMut, v: i32) {
    let mut value = v as u32;
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.put_u8(byte);
        if value == 0 {
            break;
        }
    }
}

/// Decodes a VarInt from the reader, advancing it.
///
/// Returns a [`MineRiderError::Protocol`] if the encoding uses more than
/// five bytes or the underlying data is truncated.
pub fn read_varint(r: &mut PacketReader<'_>) -> Result<i32> {
    let mut result: u32 = 0;
    for i in 0..MAX_VARINT_BYTES {
        let byte = r.get_u8()?;
        result |= ((byte & 0x7F) as u32) << (i * 7);
        if byte & 0x80 == 0 {
            return Ok(result as i32);
        }
    }
    Err(MineRiderError::Protocol(
        "varint is too long (more than 5 bytes)".to_string(),
    ))
}

/// Returns the number of bytes `v` occupies when encoded as a VarInt.
pub fn varint_size(v: i32) -> usize {
    let mut value = v as u32;
    let mut size = 1;
    while value & !0x7F != 0 {
        value >>= 7;
        size += 1;
    }
    size
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: i32) {
        let mut buf = BytesMut::new();
        write_varint(&mut buf, v);
        assert_eq!(buf.len(), varint_size(v));
        let mut r = PacketReader::new(&buf);
        assert_eq!(read_varint(&mut r).unwrap(), v);
        assert!(r.is_empty());
    }

    #[test]
    fn roundtrips() {
        for v in [0, 1, -1, i32::MAX, i32::MIN, 2_147_483_647, 255, 100_000, -100_000] {
            roundtrip(v);
        }
        // Deterministic pseudo-random sweep.
        let mut state: u32 = 0x1234_5678;
        for _ in 0..256 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            roundtrip(state as i32);
        }
    }

    #[test]
    fn exact_encodings() {
        let cases: &[(i32, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (2, &[0x02]),
            (127, &[0x7F]),
            (128, &[0x80, 0x01]),
            (255, &[0xFF, 0x01]),
            (-1, &[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]),
            (2_147_483_647, &[0xFF, 0xFF, 0xFF, 0xFF, 0x07]),
            (-2_147_483_648, &[0x80, 0x80, 0x80, 0x80, 0x08]),
        ];
        for (v, bytes) in cases {
            let mut buf = BytesMut::new();
            write_varint(&mut buf, *v);
            assert_eq!(&buf[..], *bytes, "encoding of {v}");
        }
    }

    #[test]
    fn rejects_six_byte_varint() {
        let data = [0x80, 0x80, 0x80, 0x80, 0x80, 0x01];
        let mut r = PacketReader::new(&data);
        assert!(matches!(
            read_varint(&mut r),
            Err(MineRiderError::Protocol(_))
        ));
    }

    #[test]
    fn truncated_varint_errors() {
        let data = [0x80, 0x80];
        let mut r = PacketReader::new(&data);
        assert!(matches!(
            read_varint(&mut r),
            Err(MineRiderError::Protocol(_))
        ));
    }

    #[test]
    fn size_boundaries() {
        assert_eq!(varint_size(0), 1);
        assert_eq!(varint_size(127), 1);
        assert_eq!(varint_size(128), 2);
        assert_eq!(varint_size(-1), 5);
        assert_eq!(varint_size(i32::MIN), 5);
        assert_eq!(varint_size(i32::MAX), 5);
    }
}
