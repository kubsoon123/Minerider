//! Network NBT codec (the `anonymousNbt` / `anonOptionalNbt` natives).
//!
//! Wire format (Java Edition ≥ 1.20.2): a single tag-id byte followed by the
//! tag payload — no root tag name. `anonOptionalNbt` is the same but a lone
//! `0x00` (TAG_End) byte means "absent".
//!
//! Payloads follow classic NBT: compounds are (tag id, u16-length-prefixed
//! UTF-8 name, payload) entries terminated by `0x00`; lists are an element
//! tag id, an i32 count and count payloads; arrays are i32-length-prefixed.
//!
//! Decoding is hardened against hostile input: recursion depth is capped at
//! [`MAX_NBT_DEPTH`], total nodes at [`MAX_NBT_NODES`], and every length
//! prefix is checked against the remaining bytes before any allocation.

use crate::buffer::{PacketReader, PacketWriter};
use crate::error::{ProtocolError, Result};
use crate::traits::{Decode, Encode};

/// Maximum nesting depth of compounds/lists.
pub const MAX_NBT_DEPTH: usize = 64;
/// Maximum total number of tags in one NBT value.
pub const MAX_NBT_NODES: usize = 1_000_000;

const TAG_END: u8 = 0;
const TAG_BYTE: u8 = 1;
const TAG_SHORT: u8 = 2;
const TAG_INT: u8 = 3;
const TAG_LONG: u8 = 4;
const TAG_FLOAT: u8 = 5;
const TAG_DOUBLE: u8 = 6;
const TAG_BYTE_ARRAY: u8 = 7;
const TAG_STRING: u8 = 8;
const TAG_LIST: u8 = 9;
const TAG_COMPOUND: u8 = 10;
const TAG_INT_ARRAY: u8 = 11;
const TAG_LONG_ARRAY: u8 = 12;

/// An NBT value.
#[derive(Debug, Clone, PartialEq)]
pub enum Nbt {
    /// TAG_Byte.
    Byte(i8),
    /// TAG_Short.
    Short(i16),
    /// TAG_Int.
    Int(i32),
    /// TAG_Long.
    Long(i64),
    /// TAG_Float.
    Float(f32),
    /// TAG_Double.
    Double(f64),
    /// TAG_Byte_Array.
    ByteArray(Vec<u8>),
    /// TAG_String.
    String(String),
    /// TAG_List; homogeneous elements.
    List(NbtList),
    /// TAG_Compound; named entries in wire order.
    Compound(Vec<(String, Nbt)>),
    /// TAG_Int_Array.
    IntArray(Vec<i32>),
    /// TAG_Long_Array.
    LongArray(Vec<i64>),
}

/// The payload of a TAG_List.
#[derive(Debug, Clone, PartialEq)]
pub struct NbtList {
    /// Tag id of the elements (0 when the list is empty).
    pub tag: u8,
    /// Elements; each must have tag id [`NbtList::tag`].
    pub items: Vec<Nbt>,
}

impl NbtList {
    /// Builds a list from homogeneous items, inferring the element tag.
    ///
    /// Fails if the items do not all share the same tag id.
    pub fn new(items: Vec<Nbt>) -> Result<NbtList> {
        let tag = items.first().map(Nbt::tag_id).unwrap_or(TAG_END);
        if items.iter().any(|i| i.tag_id() != tag) {
            return Err(ProtocolError::InvalidNbt(
                "list elements have mixed tag ids".into(),
            ));
        }
        Ok(NbtList { tag, items })
    }
}

impl Nbt {
    /// The tag id of this value.
    pub fn tag_id(&self) -> u8 {
        match self {
            Nbt::Byte(_) => TAG_BYTE,
            Nbt::Short(_) => TAG_SHORT,
            Nbt::Int(_) => TAG_INT,
            Nbt::Long(_) => TAG_LONG,
            Nbt::Float(_) => TAG_FLOAT,
            Nbt::Double(_) => TAG_DOUBLE,
            Nbt::ByteArray(_) => TAG_BYTE_ARRAY,
            Nbt::String(_) => TAG_STRING,
            Nbt::List(_) => TAG_LIST,
            Nbt::Compound(_) => TAG_COMPOUND,
            Nbt::IntArray(_) => TAG_INT_ARRAY,
            Nbt::LongArray(_) => TAG_LONG_ARRAY,
        }
    }

    /// Looks up a named entry of a compound.
    pub fn get(&self, name: &str) -> Option<&Nbt> {
        match self {
            Nbt::Compound(entries) => entries.iter().find(|(n, _)| n == name).map(|(_, v)| v),
            _ => None,
        }
    }

    fn read_payload(
        input: &mut PacketReader<'_>,
        tag: u8,
        depth: usize,
        nodes: &mut usize,
    ) -> Result<Nbt> {
        *nodes += 1;
        if *nodes > MAX_NBT_NODES {
            return Err(ProtocolError::InvalidNbt(format!(
                "more than {MAX_NBT_NODES} nodes"
            )));
        }
        if depth > MAX_NBT_DEPTH {
            return Err(ProtocolError::InvalidNbt(format!(
                "nesting deeper than {MAX_NBT_DEPTH}"
            )));
        }
        match tag {
            TAG_BYTE => Ok(Nbt::Byte(input.get_i8()?)),
            TAG_SHORT => Ok(Nbt::Short(input.get_i16()?)),
            TAG_INT => Ok(Nbt::Int(input.get_i32()?)),
            TAG_LONG => Ok(Nbt::Long(input.get_i64()?)),
            TAG_FLOAT => Ok(Nbt::Float(input.get_f32()?)),
            TAG_DOUBLE => Ok(Nbt::Double(input.get_f64()?)),
            TAG_BYTE_ARRAY => {
                let len = read_length(input, 1)?;
                Ok(Nbt::ByteArray(input.read_bytes(len)?.to_vec()))
            }
            TAG_STRING => Ok(Nbt::String(read_nbt_string(input)?)),
            TAG_LIST => {
                let elem_tag = input.get_u8()?;
                let count = input.get_i32()?;
                if count < 0 {
                    return Err(ProtocolError::InvalidNbt(format!(
                        "negative list length {count}"
                    )));
                }
                if count == 0 {
                    return Ok(Nbt::List(NbtList {
                        tag: elem_tag,
                        items: Vec::new(),
                    }));
                }
                if !(TAG_BYTE..=TAG_LONG_ARRAY).contains(&elem_tag) {
                    return Err(ProtocolError::InvalidNbt(format!(
                        "list of invalid tag id {elem_tag}"
                    )));
                }
                let count = count as usize;
                // Every element costs at least one byte on the wire.
                if count > input.remaining() {
                    return Err(ProtocolError::BufferUnderflow {
                        needed: count,
                        remaining: input.remaining(),
                    });
                }
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(Self::read_payload(input, elem_tag, depth + 1, nodes)?);
                }
                Ok(Nbt::List(NbtList {
                    tag: elem_tag,
                    items,
                }))
            }
            TAG_COMPOUND => {
                let mut entries = Vec::new();
                loop {
                    let entry_tag = input.get_u8()?;
                    if entry_tag == TAG_END {
                        break;
                    }
                    if !(TAG_BYTE..=TAG_LONG_ARRAY).contains(&entry_tag) {
                        return Err(ProtocolError::InvalidNbt(format!(
                            "compound entry with invalid tag id {entry_tag}"
                        )));
                    }
                    let name = read_nbt_string(input)?;
                    let value = Self::read_payload(input, entry_tag, depth + 1, nodes)?;
                    entries.push((name, value));
                }
                Ok(Nbt::Compound(entries))
            }
            TAG_INT_ARRAY => {
                let len = read_length(input, 4)?;
                let mut out = Vec::with_capacity(len);
                for _ in 0..len {
                    out.push(input.get_i32()?);
                }
                Ok(Nbt::IntArray(out))
            }
            TAG_LONG_ARRAY => {
                let len = read_length(input, 8)?;
                let mut out = Vec::with_capacity(len);
                for _ in 0..len {
                    out.push(input.get_i64()?);
                }
                Ok(Nbt::LongArray(out))
            }
            other => Err(ProtocolError::InvalidNbt(format!("invalid tag id {other}"))),
        }
    }

    fn write_payload(&self, out: &mut PacketWriter) -> Result<()> {
        match self {
            Nbt::Byte(v) => out.put_i8(*v),
            Nbt::Short(v) => out.put_i16(*v),
            Nbt::Int(v) => out.put_i32(*v),
            Nbt::Long(v) => out.put_i64(*v),
            Nbt::Float(v) => out.put_f32(*v),
            Nbt::Double(v) => out.put_f64(*v),
            Nbt::ByteArray(v) => {
                out.put_i32(v.len() as i32);
                out.put_bytes(v);
            }
            Nbt::String(v) => write_nbt_string(out, v)?,
            Nbt::List(list) => {
                if list.items.iter().any(|i| i.tag_id() != list.tag) {
                    return Err(ProtocolError::InvalidNbt(
                        "list elements do not match the declared tag id".into(),
                    ));
                }
                out.put_u8(list.tag);
                out.put_i32(list.items.len() as i32);
                for item in &list.items {
                    item.write_payload(out)?;
                }
            }
            Nbt::Compound(entries) => {
                for (name, value) in entries {
                    out.put_u8(value.tag_id());
                    write_nbt_string(out, name)?;
                    value.write_payload(out)?;
                }
                out.put_u8(TAG_END);
            }
            Nbt::IntArray(v) => {
                out.put_i32(v.len() as i32);
                for x in v {
                    out.put_i32(*x);
                }
            }
            Nbt::LongArray(v) => {
                out.put_i32(v.len() as i32);
                for x in v {
                    out.put_i64(*x);
                }
            }
        }
        Ok(())
    }
}

/// Reads an i32 length prefix for an array whose elements are `elem_size`
/// bytes, rejecting negative and impossible lengths.
fn read_length(input: &mut PacketReader<'_>, elem_size: usize) -> Result<usize> {
    let len = input.get_i32()?;
    if len < 0 {
        return Err(ProtocolError::InvalidNbt(format!(
            "negative array length {len}"
        )));
    }
    let len = len as usize;
    let bytes = len.saturating_mul(elem_size);
    if bytes > input.remaining() {
        return Err(ProtocolError::BufferUnderflow {
            needed: bytes,
            remaining: input.remaining(),
        });
    }
    Ok(len)
}

/// Reads a u16-length-prefixed UTF-8 NBT string.
fn read_nbt_string(input: &mut PacketReader<'_>) -> Result<String> {
    let len = input.get_u16()? as usize;
    let bytes = input.read_bytes(len)?;
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|_| ProtocolError::InvalidUtf8)
}

fn write_nbt_string(out: &mut PacketWriter, s: &str) -> Result<()> {
    if s.len() > u16::MAX as usize {
        return Err(ProtocolError::InvalidNbt(format!(
            "string of {} bytes exceeds the NBT u16 length limit",
            s.len()
        )));
    }
    out.put_u16(s.len() as u16);
    out.put_bytes(s.as_bytes());
    Ok(())
}

impl Encode for Nbt {
    fn encode(&self, out: &mut PacketWriter) -> Result<()> {
        out.put_u8(self.tag_id());
        self.write_payload(out)
    }
}

impl Decode for Nbt {
    fn decode(input: &mut PacketReader<'_>) -> Result<Self> {
        let tag = input.get_u8()?;
        if tag == TAG_END {
            return Err(ProtocolError::InvalidNbt(
                "standalone TAG_End is not a valid root".into(),
            ));
        }
        let mut nodes = 0;
        Self::read_payload(input, tag, 0, &mut nodes)
    }
}

/// Reads an `anonOptionalNbt`: a lone `0x00` byte means absent.
pub fn read_optional(input: &mut PacketReader<'_>) -> Result<Option<Nbt>> {
    if input.rest().first() == Some(&TAG_END) {
        let _ = input.get_u8()?;
        return Ok(None);
    }
    Ok(Some(Nbt::decode(input)?))
}

/// Writes an `anonOptionalNbt`: absent is a lone `0x00` byte.
pub fn write_optional(out: &mut PacketWriter, value: Option<&Nbt>) -> Result<()> {
    match value {
        None => {
            out.put_u8(TAG_END);
            Ok(())
        }
        Some(nbt) => nbt.encode(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Nbt {
        Nbt::Compound(vec![
            ("name".into(), Nbt::String("Steve".into())),
            ("health".into(), Nbt::Float(20.0)),
            (
                "pos".into(),
                Nbt::List(NbtList {
                    tag: TAG_DOUBLE,
                    items: vec![Nbt::Double(1.5), Nbt::Double(-64.0), Nbt::Double(2.5)],
                }),
            ),
            (
                "nested".into(),
                Nbt::Compound(vec![("x".into(), Nbt::Int(7))]),
            ),
            ("bytes".into(), Nbt::ByteArray(vec![1, 2, 3])),
            ("ints".into(), Nbt::IntArray(vec![10, -20])),
            ("longs".into(), Nbt::LongArray(vec![1 << 40])),
        ])
    }

    #[test]
    fn roundtrip_all_tags() {
        let value = sample();
        let mut w = PacketWriter::new();
        value.encode(&mut w).unwrap();
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert_eq!(Nbt::decode(&mut r).unwrap(), value);
        assert!(r.is_empty());
    }

    #[test]
    fn wire_format_is_tag_then_payload_no_root_name() {
        // 0x0A (compound), entry 0x08 (string) name len 4 "text",
        // value len 6 "kicked", 0x00 end — this is the exact byte layout a
        // vanilla server sends for a chat-component disconnect reason.
        let bytes = [
            0x0A, 0x08, 0x00, 0x04, b't', b'e', b'x', b't', 0x00, 0x06, b'k', b'i', b'c', b'k',
            b'e', b'd', 0x00,
        ];
        let mut r = PacketReader::new(&bytes);
        let nbt = Nbt::decode(&mut r).unwrap();
        assert_eq!(
            nbt,
            Nbt::Compound(vec![("text".into(), Nbt::String("kicked".into()))])
        );
        assert!(r.is_empty());
        let mut w = PacketWriter::new();
        nbt.encode(&mut w).unwrap();
        assert_eq!(&w.into_inner()[..], &bytes);
    }

    #[test]
    fn optional_roundtrip() {
        let mut w = PacketWriter::new();
        write_optional(&mut w, None).unwrap();
        write_optional(&mut w, Some(&sample())).unwrap();
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert_eq!(read_optional(&mut r).unwrap(), None);
        assert_eq!(read_optional(&mut r).unwrap(), Some(sample()));
        assert!(r.is_empty());
        // Absent is exactly one zero byte.
        let mut w = PacketWriter::new();
        write_optional(&mut w, None).unwrap();
        assert_eq!(&w.into_inner()[..], &[0x00]);
    }

    #[test]
    fn depth_cap_errors_without_stack_overflow() {
        // Root compound tag, then 200 nested compounds, each:
        // entry tag 0x0A, name length 1, name 'a'. Then enough TAG_ENDs to
        // close every level (never reached — the depth cap fires first).
        let mut bytes = vec![TAG_COMPOUND];
        for _ in 0..200 {
            bytes.extend_from_slice(&[TAG_COMPOUND, 0x00, 0x01, b'a']);
        }
        bytes.extend(std::iter::repeat_n(TAG_END, 201));
        let mut r = PacketReader::new(&bytes);
        match Nbt::decode(&mut r) {
            Err(ProtocolError::InvalidNbt(_)) => {}
            other => panic!("expected depth error, got {other:?}"),
        }
    }

    #[test]
    fn node_cap_errors() {
        // A list claiming 2M ints with enough bytes would otherwise decode
        // 2M nodes; the node cap must trip first.
        let mut w = PacketWriter::new();
        w.put_u8(TAG_LIST);
        w.put_u8(TAG_INT);
        w.put_i32(2_000_000);
        for _ in 0..1_100_000 {
            w.put_i32(0);
        }
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        match Nbt::decode(&mut r) {
            Err(ProtocolError::InvalidNbt(_)) => {}
            other => panic!("expected node-cap error, got {other:?}"),
        }
    }

    #[test]
    fn malformed_inputs_error_without_panic() {
        // Unknown root tag.
        let mut r = PacketReader::new(&[0x7F]);
        assert!(matches!(
            Nbt::decode(&mut r),
            Err(ProtocolError::InvalidNbt(_))
        ));
        // Lone TAG_End root.
        let mut r = PacketReader::new(&[TAG_END]);
        assert!(matches!(
            Nbt::decode(&mut r),
            Err(ProtocolError::InvalidNbt(_))
        ));
        // Negative list length.
        let mut w = PacketWriter::new();
        w.put_u8(TAG_LIST);
        w.put_u8(TAG_INT);
        w.put_i32(-1);
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert!(matches!(
            Nbt::decode(&mut r),
            Err(ProtocolError::InvalidNbt(_))
        ));
        // List count larger than remaining bytes.
        let mut w = PacketWriter::new();
        w.put_u8(TAG_LIST);
        w.put_u8(TAG_BYTE);
        w.put_i32(100);
        w.put_bytes(&[1, 2, 3]);
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert!(matches!(
            Nbt::decode(&mut r),
            Err(ProtocolError::BufferUnderflow { .. })
        ));
        // Truncated compound (no terminator).
        let mut r = PacketReader::new(&[TAG_COMPOUND, TAG_INT, 0x00, 0x01, b'x']);
        assert!(matches!(
            Nbt::decode(&mut r),
            Err(ProtocolError::BufferUnderflow { .. })
        ));
        // Invalid UTF-8 in a string.
        let mut r = PacketReader::new(&[TAG_STRING, 0x00, 0x02, 0xFF, 0xFE]);
        assert!(matches!(
            Nbt::decode(&mut r),
            Err(ProtocolError::InvalidUtf8)
        ));
        // Negative byte-array length.
        let mut w = PacketWriter::new();
        w.put_u8(TAG_BYTE_ARRAY);
        w.put_i32(-5);
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert!(matches!(
            Nbt::decode(&mut r),
            Err(ProtocolError::InvalidNbt(_))
        ));
    }

    #[test]
    fn mixed_list_rejected_on_encode() {
        let bad = Nbt::List(NbtList {
            tag: TAG_INT,
            items: vec![Nbt::Int(1), Nbt::String("no".into())],
        });
        assert!(matches!(
            bad.encode(&mut PacketWriter::new()),
            Err(ProtocolError::InvalidNbt(_))
        ));
        assert!(NbtList::new(vec![Nbt::Int(1), Nbt::Byte(2)]).is_err());
    }
}
