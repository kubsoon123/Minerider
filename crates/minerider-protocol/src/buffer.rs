//! Cursor-based read/write buffer over packet bytes.
//!
//! All readers are bounds-checked: short reads produce a
//! [`ProtocolError::BufferUnderflow`] instead of panicking.

use bytes::{BufMut, BytesMut};

use super::{varint, varlong};
use crate::error::{ProtocolError, Result};

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
    /// Fails with [`ProtocolError::InvalidString`] if the string exceeds
    /// [`MAX_STRING_CHARS`] characters.
    pub fn put_string(&mut self, s: &str) -> Result<()> {
        if s.chars().count() > MAX_STRING_CHARS {
            return Err(ProtocolError::InvalidString(format!(
                "string too long: {} chars (max {MAX_STRING_CHARS})",
                s.chars().count()
            )));
        }
        let bytes = s.as_bytes();
        self.put_varint(bytes.len() as i32);
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// Appends a vanilla identifier (`namespace:path`), validating the
    /// charset of both parts.
    ///
    /// Namespaces accept `[a-z0-9._-]`, paths additionally `/`. Fails with
    /// [`ProtocolError::InvalidIdentifier`] otherwise.
    pub fn put_identifier(&mut self, namespace: &str, path: &str) -> Result<()> {
        if !is_valid_namespace(namespace) {
            return Err(ProtocolError::InvalidIdentifier(format!(
                "invalid namespace {namespace:?} (allowed: [a-z0-9._-])"
            )));
        }
        if !is_valid_path(path) {
            return Err(ProtocolError::InvalidIdentifier(format!(
                "invalid path {path:?} (allowed: [a-z0-9/._-])"
            )));
        }
        self.put_string(&format!("{namespace}:{path}"))
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

    /// Appends a VarInt-length-prefixed array, encoding each element with
    /// `write`.
    pub fn put_array<T>(&mut self, items: &[T], mut write: impl FnMut(&mut Self, &T)) {
        self.put_varint(items.len() as i32);
        for item in items {
            write(self, item);
        }
    }

    /// Appends an optional value: a boolean prefix, then the value encoded
    /// with `write` when present.
    pub fn put_option<T>(&mut self, value: Option<&T>, write: impl FnOnce(&mut Self, &T)) {
        self.put_bool(value.is_some());
        if let Some(v) = value {
            write(self, v);
        }
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
            return Err(ProtocolError::BufferUnderflow {
                needed: n,
                remaining: self.remaining(),
            });
        }
        Ok(())
    }

    /// Reads `N` bytes as a fixed-size array (no `unwrap`/`expect`; the
    /// bounds check above guarantees the slice is exactly `N` bytes).
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.require(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(&self.data[self.pos..self.pos + N]);
        self.pos += N;
        Ok(out)
    }

    /// Reads a single unsigned byte.
    pub fn get_u8(&mut self) -> Result<u8> {
        Ok(self.take::<1>()?[0])
    }

    /// Reads a boolean (`0x00` false, anything else true).
    pub fn get_bool(&mut self) -> Result<bool> {
        Ok(self.get_u8()? != 0)
    }

    /// Reads a big-endian signed 16-bit integer.
    pub fn get_i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.take()?))
    }

    /// Reads a big-endian unsigned 16-bit integer.
    pub fn get_u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take()?))
    }

    /// Reads a big-endian signed 32-bit integer.
    pub fn get_i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.take()?))
    }

    /// Reads a big-endian signed 64-bit integer.
    pub fn get_i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.take()?))
    }

    /// Reads a big-endian unsigned 64-bit integer.
    pub fn get_u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take()?))
    }

    /// Reads a big-endian 32-bit float.
    pub fn get_f32(&mut self) -> Result<f32> {
        Ok(f32::from_be_bytes(self.take()?))
    }

    /// Reads a big-endian 64-bit float.
    pub fn get_f64(&mut self) -> Result<f64> {
        Ok(f64::from_be_bytes(self.take()?))
    }

    /// Reads a Minecraft VarInt.
    pub fn get_varint(&mut self) -> Result<i32> {
        varint::read_varint(self)
    }

    /// Reads a Minecraft VarLong.
    pub fn get_varlong(&mut self) -> Result<i64> {
        varlong::read_varlong(self)
    }

    /// Reads exactly `n` raw bytes, lending a slice of the underlying data
    /// (zero copy).
    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.require(n)?;
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// Reads a VarInt-length-prefixed UTF-8 string, lending a `&str` of the
    /// underlying data (zero copy).
    ///
    /// Fails with [`ProtocolError::InvalidUtf8`] on invalid UTF-8 or
    /// [`ProtocolError::InvalidString`] if the string exceeds
    /// [`MAX_STRING_CHARS`] characters.
    pub fn read_string(&mut self) -> Result<&'a str> {
        let len = self.get_varint()?;
        if !(0..=(MAX_STRING_CHARS as i32 * 4)).contains(&len) {
            return Err(ProtocolError::InvalidString(format!(
                "string byte length {len} out of bounds"
            )));
        }
        let bytes = self.read_bytes(len as usize)?;
        let s = std::str::from_utf8(bytes).map_err(|_| ProtocolError::InvalidUtf8)?;
        if s.chars().count() > MAX_STRING_CHARS {
            return Err(ProtocolError::InvalidString(format!(
                "string too long: {} chars (max {MAX_STRING_CHARS})",
                s.chars().count()
            )));
        }
        Ok(s)
    }

    /// Reads and validates a vanilla identifier. Returns the normalized
    /// `namespace:path` string; a missing namespace defaults to `minecraft`
    /// (vanilla behavior).
    pub fn read_identifier(&mut self) -> Result<String> {
        let raw = self.read_string()?;
        let (namespace, path) = match raw.split_once(':') {
            Some((ns, path)) => (ns.to_string(), path),
            None => ("minecraft".to_string(), raw),
        };
        if !is_valid_namespace(&namespace) {
            return Err(ProtocolError::InvalidIdentifier(format!(
                "invalid namespace {namespace:?} (allowed: [a-z0-9._-])"
            )));
        }
        if !is_valid_path(path) {
            return Err(ProtocolError::InvalidIdentifier(format!(
                "invalid path {path:?} (allowed: [a-z0-9/._-])"
            )));
        }
        Ok(format!("{namespace}:{path}"))
    }

    /// Reads a VarInt-length-prefixed byte array, lending a slice of the
    /// underlying data (zero copy).
    pub fn read_byte_array(&mut self) -> Result<&'a [u8]> {
        let len = self.get_varint()?;
        if len < 0 {
            return Err(ProtocolError::NegativeLength(len));
        }
        self.read_bytes(len as usize)
    }

    /// Reads a VarInt-length-prefixed array, decoding each element with
    /// `read`.
    pub fn read_array<T>(
        &mut self,
        mut read: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        let len = self.get_varint()?;
        if len < 0 {
            return Err(ProtocolError::NegativeLength(len));
        }
        let mut out = Vec::with_capacity(len as usize);
        for _ in 0..len {
            out.push(read(self)?);
        }
        Ok(out)
    }

    /// Reads an optional value: a boolean prefix, then the value decoded
    /// with `read` when the prefix is true.
    pub fn read_option<T>(
        &mut self,
        read: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<Option<T>> {
        if self.get_bool()? {
            Ok(Some(read(self)?))
        } else {
            Ok(None)
        }
    }

    /// Reads a UUID from 16 big-endian bytes.
    pub fn read_uuid(&mut self) -> Result<u128> {
        Ok(u128::from_be_bytes(self.take()?))
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

/// True if `s` is a valid vanilla identifier namespace (`[a-z0-9._-]`).
pub fn is_valid_namespace(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// True if `s` is a valid vanilla identifier path (`[a-z0-9/._-]`).
pub fn is_valid_path(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-' | b'/')
        })
}

/// Packs a block position into the Minecraft `i64` representation:
/// 26 bits for x, 26 bits for z, 12 bits for y.
pub(crate) fn pack_position(x: i32, y: i32, z: i32) -> i64 {
    ((x as i64 & 0x3FF_FFFF) << 38) | ((z as i64 & 0x3FF_FFFF) << 12) | (y as i64 & 0xFFF)
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
    fn string_max_length_boundary() {
        // Exactly 32767 characters is legal in both directions.
        let s = "a".repeat(MAX_STRING_CHARS);
        let mut w = PacketWriter::new();
        w.put_string(&s).unwrap();
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert_eq!(r.read_string().unwrap(), s);

        // 32768 characters is rejected on write...
        let too_long = "a".repeat(MAX_STRING_CHARS + 1);
        let mut w = PacketWriter::new();
        assert!(matches!(
            w.put_string(&too_long),
            Err(ProtocolError::InvalidString(_))
        ));

        // ...and on read (valid UTF-8 but too many chars).
        let mut w = PacketWriter::new();
        w.put_varint((MAX_STRING_CHARS + 1) as i32);
        w.put_bytes(&too_long.into_bytes());
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert!(matches!(
            r.read_string(),
            Err(ProtocolError::InvalidString(_))
        ));
    }

    #[test]
    fn multibyte_char_count_boundary() {
        // The limit counts characters, not bytes: 32767 '€' (3 bytes each)
        // is fine (byte length 98301 <= 4*32767), one more is not.
        let s = "€".repeat(MAX_STRING_CHARS);
        let mut w = PacketWriter::new();
        w.put_string(&s).unwrap();
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert_eq!(r.read_string().unwrap(), s);

        let mut w = PacketWriter::new();
        let too_long = "€".repeat(MAX_STRING_CHARS + 1);
        assert!(matches!(
            w.put_string(&too_long),
            Err(ProtocolError::InvalidString(_))
        ));
    }

    #[test]
    fn invalid_utf8_string_errors() {
        let mut w = PacketWriter::new();
        w.put_varint(2);
        w.put_bytes(&[0xFF, 0xFE]);
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert!(matches!(r.read_string(), Err(ProtocolError::InvalidUtf8)));
    }

    #[test]
    fn identifier_roundtrip() {
        let mut w = PacketWriter::new();
        w.put_identifier("minecraft", "block/stone").unwrap();
        w.put_identifier("my_mod", "item-1.v2").unwrap();
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert_eq!(r.read_identifier().unwrap(), "minecraft:block/stone");
        assert_eq!(r.read_identifier().unwrap(), "my_mod:item-1.v2");
        assert!(r.is_empty());
    }

    #[test]
    fn identifier_default_namespace() {
        let mut w = PacketWriter::new();
        w.put_string("stone").unwrap();
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert_eq!(r.read_identifier().unwrap(), "minecraft:stone");
    }

    #[test]
    fn identifier_charset_rejected() {
        // Uppercase, spaces and non-ASCII are illegal on write...
        for (ns, path) in [
            ("Minecraft", "stone"),
            ("minecraft", "Stone"),
            ("my mod", "stone"),
            ("minecraft", "blöck"),
            ("", "stone"),
            ("minecraft", ""),
        ] {
            let mut w = PacketWriter::new();
            assert!(
                matches!(
                    w.put_identifier(ns, path),
                    Err(ProtocolError::InvalidIdentifier(_))
                ),
                "accepted {ns}:{path}"
            );
        }
        // ...and on read.
        let mut w = PacketWriter::new();
        w.put_string("Bad:stone").unwrap();
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert!(matches!(
            r.read_identifier(),
            Err(ProtocolError::InvalidIdentifier(_))
        ));
    }

    #[test]
    fn option_roundtrip() {
        let mut w = PacketWriter::new();
        w.put_option(Some(&42i32), |w, v| w.put_i32(*v));
        w.put_option(None::<&i32>, |w, v| w.put_i32(*v));
        w.put_option(Some(&"hi".to_string()), |w, v| w.put_string(v).unwrap());
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert_eq!(r.read_option(|r| r.get_i32()).unwrap(), Some(42));
        assert_eq!(r.read_option(|r| r.get_i32()).unwrap(), None);
        assert_eq!(
            r.read_option(|r| Ok(r.read_string()?.to_string())).unwrap(),
            Some("hi".to_string())
        );
        assert!(r.is_empty());
    }

    #[test]
    fn array_roundtrip_including_empty() {
        let mut w = PacketWriter::new();
        w.put_array(&[1i32, -2, 3], |w, v| w.put_i32(*v));
        w.put_array(&[] as &[i32], |w, v| w.put_i32(*v));
        w.put_array(&["a".to_string(), "bc".to_string()], |w, v| {
            w.put_string(v).unwrap();
        });
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert_eq!(r.read_array(|r| r.get_i32()).unwrap(), vec![1, -2, 3]);
        assert_eq!(r.read_array(|r| r.get_i32()).unwrap(), Vec::<i32>::new());
        assert_eq!(
            r.read_array(|r| Ok(r.read_string()?.to_string())).unwrap(),
            vec!["a".to_string(), "bc".to_string()]
        );
        assert!(r.is_empty());

        // Byte arrays, including empty, go through the same length prefix.
        let mut w = PacketWriter::new();
        w.put_byte_array(&[]);
        w.put_byte_array(&[9, 8, 7]);
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert_eq!(r.read_byte_array().unwrap(), &[]);
        assert_eq!(r.read_byte_array().unwrap(), &[9, 8, 7]);
        assert!(r.is_empty());
    }

    #[test]
    fn negative_array_length_errors() {
        let mut w = PacketWriter::new();
        w.put_varint(-1);
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert!(matches!(
            r.read_byte_array(),
            Err(ProtocolError::NegativeLength(-1))
        ));
        let mut r = PacketReader::new(&buf);
        assert!(matches!(
            r.read_array(|r| r.get_i32()),
            Err(ProtocolError::NegativeLength(-1))
        ));
    }

    #[test]
    fn uuid_byte_order() {
        // Vanilla serializes UUIDs as two big-endian i64 halves, i.e. the
        // 16 big-endian bytes of the u128 value, most significant first.
        let uuid = 0x00112233445566778899AABBCCDDEEFFu128;
        let mut w = PacketWriter::new();
        w.put_uuid(uuid);
        let buf = w.into_inner();
        assert_eq!(
            &buf[..],
            &[
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
                0xEE, 0xFF
            ]
        );
        let mut r = PacketReader::new(&buf);
        assert_eq!(r.read_uuid().unwrap(), uuid);
    }

    #[test]
    fn position_vanilla_vector() {
        // Vector from the vanilla protocol documentation:
        // (1835762, 831, -20882616) packs to 0x0700BCAC15B4833F.
        let packed = pack_position(1835762, 831, -20882616);
        assert_eq!(packed, 0x0700_BCAC_15B4_833F);
        assert_eq!(unpack_position(packed), (1835762, 831, -20882616));
    }

    #[test]
    fn short_reads_error_not_panic() {
        let data = [0x01, 0x02];
        let mut r = PacketReader::new(&data);
        assert!(matches!(
            r.get_i64(),
            Err(ProtocolError::BufferUnderflow { .. })
        ));
        assert!(matches!(
            r.read_bytes(5),
            Err(ProtocolError::BufferUnderflow { .. })
        ));
        assert!(matches!(
            r.read_uuid(),
            Err(ProtocolError::BufferUnderflow { .. })
        ));
        assert!(matches!(
            r.read_position(),
            Err(ProtocolError::BufferUnderflow { .. })
        ));
        // Cursor must not have moved out of bounds.
        assert_eq!(r.remaining(), 2);

        // Length prefixes that promise more bytes than remain.
        let mut w = PacketWriter::new();
        w.put_varint(10);
        w.put_bytes(&[0x01, 0x02]);
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert!(matches!(
            r.read_string(),
            Err(ProtocolError::BufferUnderflow { .. })
        ));
        let mut r = PacketReader::new(&buf);
        assert!(matches!(
            r.read_byte_array(),
            Err(ProtocolError::BufferUnderflow { .. })
        ));
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
