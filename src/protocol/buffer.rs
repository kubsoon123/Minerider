//! Cursor-based read/write buffer over packet bytes.
//!
//! All readers are bounds-checked: short reads produce a
//! [`MineRiderError::Protocol`] instead of panicking.

use bytes::{BufMut, BytesMut};

use super::{varint, varlong};
use crate::core::error::{MineRiderError, Result};

/// Maximum number of UTF-8 characters allowed in a protocol string.
pub const MAX_STRING_CHARS: usize = 32767;

/// A growable buffer for serializing packet payloads.
#[derive(Debug, Default)]
pub struct PacketWriter {
    buf: BytesMut,
}

impl PacketWriter {
    /// Creates an empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a single unsigned byte.
    pub fn put_u8(&mut self, v: u8) {
        self.buf.put_u8(v);
    }

    /// Appends a boolean (`0x01` for true, `0x00` for false).
    pub fn put_bool(&mut self, v: bool) {
        self.buf.put_u8(v as u8);
    }

    /// Appends a big-endian signed 16-bit integer.
    pub fn put_i16(&mut self, v: i16) {
        self.buf.put_i16(v);
    }

    /// Appends a big-endian unsigned 16-bit integer.
    pub fn put_u16(&mut self, v: u16) {
        self.buf.put_u16(v);
    }

    /// Appends a big-endian signed 32-bit integer.
    pub fn put_i32(&mut self, v: i32) {
        self.buf.put_i32(v);
    }

    /// Appends a big-endian signed 64-bit integer.
    pub fn put_i64(&mut self, v: i64) {
        self.buf.put_i64(v);
    }

    /// Appends a big-endian unsigned 64-bit integer.
    pub fn put_u64(&mut self, v: u64) {
        self.buf.put_u64(v);
    }

    /// Appends a big-endian 32-bit float.
    pub fn put_f32(&mut self, v: f32) {
        self.buf.put_f32(v);
    }

    /// Appends a big-endian 64-bit float.
    pub fn put_f64(&mut self, v: f64) {
        self.buf.put_f64(v);
    }

    /// Appends a Minecraft VarInt.
    pub fn put_varint(&mut self, v: i32) {
        varint::write_varint(&mut self.buf, v);
    }

    /// Appends a Minecraft VarLong.
    pub fn put_varlong(&mut self, v: i64) {
        varlong::write_varlong(&mut self.buf, v);
    }

    /// Appends a VarInt-length-prefixed UTF-8 string.
    ///
    /// Fails with [`MineRiderError::Protocol`] if the string exceeds
    /// [`MAX_STRING_CHARS`] characters.
    pub fn put_string(&mut self, s: &str) -> Result<()> {
        if s.chars().count() > MAX_STRING_CHARS {
            return Err(MineRiderError::Protocol(format!(
                "string too long: {} chars (max {MAX_STRING_CHARS})",
                s.chars().count()
            )));
        }
        let bytes = s.as_bytes();
        self.put_varint(bytes.len() as i32);
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// Appends raw bytes without any length prefix.
    pub fn put_bytes(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Appends a VarInt-length-prefixed byte array.
    pub fn put_byte_array(&mut self, data: &[u8]) {
        self.put_varint(data.len() as i32);
        self.buf.extend_from_slice(data);
    }

    /// Appends a UUID as 16 big-endian bytes.
    pub fn put_uuid(&mut self, uuid: u128) {
        self.buf.extend_from_slice(&uuid.to_be_bytes());
    }

    /// Appends a packed block position (26/26/12 bits for x/z/y).
    pub fn put_position(&mut self, x: i32, y: i32, z: i32) {
        self.put_i64(pack_position(x, y, z));
    }

    /// Consumes the writer and returns the underlying buffer.
    pub fn into_inner(self) -> BytesMut {
        self.buf
    }

    /// Consumes the writer and returns the serialized bytes.
    pub fn freeze(self) -> bytes::Bytes {
        self.buf.freeze()
    }
}

/// A bounds-checked cursor for reading packet payloads.
#[derive(Debug, Clone)]
pub struct PacketReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PacketReader<'a> {
    /// Creates a reader over `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Number of unread bytes remaining.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// True if there are no unread bytes left.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Ensures `n` bytes can be read, else returns a protocol error.
    fn require(&self, n: usize) -> Result<()> {
        if self.remaining() < n {
            return Err(MineRiderError::Protocol(format!(
                "unexpected end of packet: need {n} bytes, {} remain",
                self.remaining()
            )));
        }
        Ok(())
    }

    /// Reads a single unsigned byte.
    pub fn get_u8(&mut self) -> Result<u8> {
        self.require(1)?;
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    /// Reads a boolean (`0x00` false, anything else true).
    pub fn get_bool(&mut self) -> Result<bool> {
        Ok(self.get_u8()? != 0)
    }

    /// Reads a big-endian signed 16-bit integer.
    pub fn get_i16(&mut self) -> Result<i16> {
        self.require(2)?;
        let v = i16::from_be_bytes(self.data[self.pos..self.pos + 2].try_into().expect("len 2"));
        self.pos += 2;
        Ok(v)
    }

    /// Reads a big-endian unsigned 16-bit integer.
    pub fn get_u16(&mut self) -> Result<u16> {
        self.require(2)?;
        let v = u16::from_be_bytes(self.data[self.pos..self.pos + 2].try_into().expect("len 2"));
        self.pos += 2;
        Ok(v)
    }

    /// Reads a big-endian signed 32-bit integer.
    pub fn get_i32(&mut self) -> Result<i32> {
        self.require(4)?;
        let v = i32::from_be_bytes(self.data[self.pos..self.pos + 4].try_into().expect("len 4"));
        self.pos += 4;
        Ok(v)
    }

    /// Reads a big-endian signed 64-bit integer.
    pub fn get_i64(&mut self) -> Result<i64> {
        self.require(8)?;
        let v = i64::from_be_bytes(self.data[self.pos..self.pos + 8].try_into().expect("len 8"));
        self.pos += 8;
        Ok(v)
    }

    /// Reads a big-endian unsigned 64-bit integer.
    pub fn get_u64(&mut self) -> Result<u64> {
        self.require(8)?;
        let v = u64::from_be_bytes(self.data[self.pos..self.pos + 8].try_into().expect("len 8"));
        self.pos += 8;
        Ok(v)
    }

    /// Reads a big-endian 32-bit float.
    pub fn get_f32(&mut self) -> Result<f32> {
        self.require(4)?;
        let v = f32::from_be_bytes(self.data[self.pos..self.pos + 4].try_into().expect("len 4"));
        self.pos += 4;
        Ok(v)
    }

    /// Reads a big-endian 64-bit float.
    pub fn get_f64(&mut self) -> Result<f64> {
        self.require(8)?;
        let v = f64::from_be_bytes(self.data[self.pos..self.pos + 8].try_into().expect("len 8"));
        self.pos += 8;
        Ok(v)
    }

    /// Reads a Minecraft VarInt.
    pub fn get_varint(&mut self) -> Result<i32> {
        varint::read_varint(self)
    }

    /// Reads a Minecraft VarLong.
    pub fn get_varlong(&mut self) -> Result<i64> {
        varlong::read_varlong(self)
    }

    /// Reads exactly `n` raw bytes.
    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.require(n)?;
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// Reads a VarInt-length-prefixed UTF-8 string.
    ///
    /// Fails with [`MineRiderError::Protocol`] on invalid UTF-8 or if the
    /// string exceeds [`MAX_STRING_CHARS`] characters.
    pub fn read_string(&mut self) -> Result<String> {
        let len = self.get_varint()?;
        if !(0..=(MAX_STRING_CHARS as i32 * 4)).contains(&len) {
            return Err(MineRiderError::Protocol(format!(
                "string byte length {len} out of bounds"
            )));
        }
        let bytes = self.read_bytes(len as usize)?;
        let s = String::from_utf8(bytes.to_vec()).map_err(|_| {
            MineRiderError::Protocol("invalid UTF-8 in string".to_string())
        })?;
        if s.chars().count() > MAX_STRING_CHARS {
            return Err(MineRiderError::Protocol(format!(
                "string too long: {} chars (max {MAX_STRING_CHARS})",
                s.chars().count()
            )));
        }
        Ok(s)
    }

    /// Reads a VarInt-length-prefixed byte array.
    pub fn read_byte_array(&mut self) -> Result<&'a [u8]> {
        let len = self.get_varint()?;
        if len < 0 {
            return Err(MineRiderError::Protocol(format!(
                "negative byte array length {len}"
            )));
        }
        self.read_bytes(len as usize)
    }

    /// Reads a UUID from 16 big-endian bytes.
    pub fn read_uuid(&mut self) -> Result<u128> {
        let bytes = self.read_bytes(16)?;
        Ok(u128::from_be_bytes(bytes.try_into().expect("len 16")))
    }

    /// Reads a packed block position into `(x, y, z)`.
    pub fn read_position(&mut self) -> Result<(i32, i32, i32)> {
        let packed = self.get_i64()?;
        Ok(unpack_position(packed))
    }

    /// Returns the remaining unread bytes without advancing.
    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    /// Advances past all remaining bytes.
    pub fn skip_all(&mut self) {
        self.pos = self.data.len();
    }
}

/// Packs a block position into the Minecraft `i64` representation:
/// 26 bits for x, 26 bits for z, 12 bits for y.
pub(crate) fn pack_position(x: i32, y: i32, z: i32) -> i64 {
    ((x as i64 & 0x3FF_FFFF) << 38)
        | ((z as i64 & 0x3FF_FFFF) << 12)
        | (y as i64 & 0xFFF)
}

/// Unpacks a Minecraft packed position into `(x, y, z)`.
pub(crate) fn unpack_position(packed: i64) -> (i32, i32, i32) {
    let x = (packed >> 38) as i32;
    let y = ((packed << 52) >> 52) as i32;
    let z = ((packed << 26) >> 38) as i32;
    (x, y, z)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_roundtrips() {
        let mut w = PacketWriter::new();
        w.put_u8(0xAB);
        w.put_bool(true);
        w.put_bool(false);
        w.put_i16(-12345);
        w.put_u16(54321);
        w.put_i32(-2_000_000_000);
        w.put_i64(-9_000_000_000_000);
        w.put_u64(18_000_000_000_000_000_000);
        w.put_f32(-1.5);
        w.put_f64(2.75);
        w.put_varint(-1);
        w.put_varlong(i64::MIN);
        w.put_string("héllo §").unwrap();
        w.put_bytes(&[1, 2, 3]);
        w.put_byte_array(&[9, 8, 7, 6]);
        w.put_uuid(0x00112233445566778899AABBCCDDEEFF);
        w.put_position(-14, 64, 20_000_000);

        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert_eq!(r.get_u8().unwrap(), 0xAB);
        assert!(r.get_bool().unwrap());
        assert!(!r.get_bool().unwrap());
        assert_eq!(r.get_i16().unwrap(), -12345);
        assert_eq!(r.get_u16().unwrap(), 54321);
        assert_eq!(r.get_i32().unwrap(), -2_000_000_000);
        assert_eq!(r.get_i64().unwrap(), -9_000_000_000_000);
        assert_eq!(r.get_u64().unwrap(), 18_000_000_000_000_000_000);
        assert_eq!(r.get_f32().unwrap(), -1.5);
        assert_eq!(r.get_f64().unwrap(), 2.75);
        assert_eq!(r.get_varint().unwrap(), -1);
        assert_eq!(r.get_varlong().unwrap(), i64::MIN);
        assert_eq!(r.read_string().unwrap(), "héllo §");
        assert_eq!(r.read_bytes(3).unwrap(), &[1, 2, 3]);
        assert_eq!(r.read_byte_array().unwrap(), &[9, 8, 7, 6]);
        assert_eq!(r.read_uuid().unwrap(), 0x00112233445566778899AABBCCDDEEFF);
        assert_eq!(r.read_position().unwrap(), (-14, 64, 20_000_000));
        assert!(r.is_empty());
    }

    #[test]
    fn string_too_long_errors() {
        let s = "a".repeat(MAX_STRING_CHARS + 1);
        let mut w = PacketWriter::new();
        assert!(matches!(
            w.put_string(&s),
            Err(MineRiderError::Protocol(_))
        ));

        // Reading: valid UTF-8 but too many chars.
        let mut w = PacketWriter::new();
        w.put_varint((MAX_STRING_CHARS + 1) as i32);
        w.put_bytes(&s.into_bytes());
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert!(matches!(
            r.read_string(),
            Err(MineRiderError::Protocol(_))
        ));
    }

    #[test]
    fn invalid_utf8_string_errors() {
        let mut w = PacketWriter::new();
        w.put_varint(2);
        w.put_bytes(&[0xFF, 0xFE]);
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert!(matches!(
            r.read_string(),
            Err(MineRiderError::Protocol(_))
        ));
    }

    #[test]
    fn short_reads_error_not_panic() {
        let data = [0x01, 0x02];
        let mut r = PacketReader::new(&data);
        assert!(matches!(r.get_i64(), Err(MineRiderError::Protocol(_))));
        assert!(matches!(r.read_bytes(5), Err(MineRiderError::Protocol(_))));
        assert!(matches!(r.read_uuid(), Err(MineRiderError::Protocol(_))));
        assert!(matches!(r.read_position(), Err(MineRiderError::Protocol(_))));
        // Cursor must not have moved out of bounds.
        assert_eq!(r.remaining(), 2);

        // Length prefixes that promise more bytes than remain.
        let mut w = PacketWriter::new();
        w.put_varint(10);
        w.put_bytes(&[0x01, 0x02]);
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert!(matches!(r.read_string(), Err(MineRiderError::Protocol(_))));
        let mut r = PacketReader::new(&buf);
        assert!(matches!(r.read_byte_array(), Err(MineRiderError::Protocol(_))));
    }

    #[test]
    fn position_pack_unpack() {
        for (x, y, z) in [
            (0, 0, 0),
            (-14, 64, 20_000_000),
            (33_554_431, 2047, 33_554_431),
            (-33_554_432, -2048, -33_554_432),
            (1, -1, 2),
        ] {
            let packed = pack_position(x, y, z);
            assert_eq!(unpack_position(packed), (x, y, z));
        }
    }

    #[test]
    fn negative_array_lengths_error() {
        let mut w = PacketWriter::new();
        w.put_varint(-1);
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert!(matches!(
            r.read_byte_array(),
            Err(MineRiderError::Protocol(_))
        ));
    }

    #[test]
    fn remaining_tracking() {
        let mut w = PacketWriter::new();
        w.put_u8(1);
        w.put_u8(2);
        w.put_u8(3);
        let buf = w.freeze();
        let mut r = PacketReader::new(&buf);
        assert!(!r.is_empty());
        assert_eq!(r.remaining(), 3);
        let _ = r.get_u8().unwrap();
        assert_eq!(r.remaining(), 2);
        r.skip_all();
        assert!(r.is_empty());
    }
}
