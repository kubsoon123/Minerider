//! Minecraft VarLong (i64, max 10 bytes) encoding.

use bytes::{BufMut, BytesMut};

use super::buffer::PacketReader;
use crate::error::{ProtocolError, Result};

/// Maximum number of bytes an encoded VarLong may occupy.
pub const MAX_VARLONG_BYTES: usize = 10;

/// Encodes `v` as a Minecraft VarLong and appends it to `buf`.
///
/// Seven bits are stored per byte with the high bit (0x80) marking
/// continuation. Negative values are encoded in two's complement.
pub fn write_varlong(buf: &mut BytesMut, v: i64) {
    let mut value = v as u64;
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

/// Decodes a VarLong from the reader, advancing it.
///
/// Returns [`ProtocolError::VarLongTooLong`] if the encoding uses more than
/// ten bytes or the underlying data is truncated.
pub fn read_varlong(r: &mut PacketReader<'_>) -> Result<i64> {
    let mut result: u64 = 0;
    for i in 0..MAX_VARLONG_BYTES {
        let byte = r.get_u8()?;
        result |= ((byte & 0x7F) as u64) << (i * 7);
        if byte & 0x80 == 0 {
            return Ok(result as i64);
        }
    }
    Err(ProtocolError::VarLongTooLong)
}

/// Returns the number of bytes `v` occupies when encoded as a VarLong.
pub fn varlong_size(v: i64) -> usize {
    let mut value = v as u64;
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

    fn roundtrip(v: i64) {
        let mut buf = BytesMut::new();
        write_varlong(&mut buf, v);
        assert_eq!(buf.len(), varlong_size(v));
        let mut r = PacketReader::new(&buf);
        assert_eq!(read_varlong(&mut r).unwrap(), v);
        assert!(r.is_empty());
    }

    #[test]
    fn roundtrips() {
        for v in [
            0,
            1,
            -1,
            i64::MAX,
            i64::MIN,
            i32::MAX as i64,
            2_147_483_647,
            9_223_372_036_854_775_807,
            -9_223_372_036_854_775_808,
        ] {
            roundtrip(v);
        }
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..256 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            roundtrip(state as i64);
        }
    }

    #[test]
    fn exact_encodings() {
        // Byte-exact vectors from the vanilla protocol documentation.
        let cases: &[(i64, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (2, &[0x02]),
            (127, &[0x7F]),
            (128, &[0x80, 0x01]),
            (255, &[0xFF, 0x01]),
            (25565, &[0xDD, 0xC7, 0x01]),
            (2_147_483_647, &[0xFF, 0xFF, 0xFF, 0xFF, 0x07]),
            (
                9_223_372_036_854_775_807,
                &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F],
            ),
            (
                -1,
                &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01],
            ),
            (
                -9_223_372_036_854_775_808,
                &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01],
            ),
        ];
        for (v, bytes) in cases {
            let mut buf = BytesMut::new();
            write_varlong(&mut buf, *v);
            assert_eq!(&buf[..], *bytes, "encoding of {v}");
            let mut r = PacketReader::new(bytes);
            assert_eq!(
                read_varlong(&mut r).unwrap(),
                *v,
                "decoding of {bytes:02x?}"
            );
            assert!(r.is_empty());
        }
    }

    #[test]
    fn rejects_eleven_byte_varlong() {
        let data = [0x80; 11];
        let mut r = PacketReader::new(&data);
        assert!(matches!(
            read_varlong(&mut r),
            Err(ProtocolError::VarLongTooLong)
        ));
    }

    #[test]
    fn truncated_varlong_errors() {
        let data = [0x80, 0x80, 0x80];
        let mut r = PacketReader::new(&data);
        assert!(matches!(
            read_varlong(&mut r),
            Err(ProtocolError::BufferUnderflow { .. })
        ));
    }

    #[test]
    fn size_boundaries() {
        assert_eq!(varlong_size(0), 1);
        assert_eq!(varlong_size(127), 1);
        assert_eq!(varlong_size(128), 2);
        assert_eq!(varlong_size(-1), 10);
        assert_eq!(varlong_size(i64::MIN), 10);
        assert_eq!(varlong_size(i64::MAX), 9);
    }
}
