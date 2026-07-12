//! Generic registry holder types used by generated packet definitions.
//!
//! Several protocol fields reference a registry entry either by numeric id
//! or inline by value (`registryEntryHolder`), and sets of entries either
//! by tag name or by id list (`registryEntryHolderSet`).

use crate::buffer::{PacketReader, PacketWriter};
use crate::error::{ProtocolError, Result};
use crate::traits::{Decode, Encode};

/// A registry entry: either a reference by id or an inline value.
///
/// On the wire a VarInt `n` of 0 means an inline value follows; otherwise
/// the entry is the registry id `n - 1`.
#[derive(Debug, Clone, PartialEq)]
pub enum Holder<T> {
    /// Registry id reference (wire value minus one).
    Reference(i32),
    /// Inline entry value.
    Inline(T),
}

impl<T: Encode> Encode for Holder<T> {
    fn encode(&self, out: &mut PacketWriter) -> Result<()> {
        match self {
            Holder::Reference(id) => {
                if *id < 0 {
                    return Err(ProtocolError::InvalidData(format!(
                        "holder reference id {id} is negative"
                    )));
                }
                out.put_varint(*id + 1);
                Ok(())
            }
            Holder::Inline(value) => {
                out.put_varint(0);
                value.encode(out)
            }
        }
    }
}

impl<T: Decode> Decode for Holder<T> {
    fn decode(input: &mut PacketReader<'_>) -> Result<Self> {
        let n = input.get_varint()?;
        if n < 0 {
            return Err(ProtocolError::NegativeLength(n));
        }
        if n == 0 {
            Ok(Holder::Inline(T::decode(input)?))
        } else {
            Ok(Holder::Reference(n - 1))
        }
    }
}

/// A set of registry entries: either a tag name or a list of ids.
///
/// On the wire a VarInt `n` of 0 means a tag name string follows; otherwise
/// `n - 1` VarInt ids follow.
#[derive(Debug, Clone, PartialEq)]
pub enum HolderSet {
    /// A registry tag, e.g. `minecraft:wooden_slabs`.
    Tag(String),
    /// Explicit registry ids.
    Ids(Vec<i32>),
}

impl Encode for HolderSet {
    fn encode(&self, out: &mut PacketWriter) -> Result<()> {
        match self {
            HolderSet::Tag(name) => {
                out.put_varint(0);
                out.put_string(name)
            }
            HolderSet::Ids(ids) => {
                out.put_varint(ids.len() as i32 + 1);
                for id in ids {
                    out.put_varint(*id);
                }
                Ok(())
            }
        }
    }
}

impl Decode for HolderSet {
    fn decode(input: &mut PacketReader<'_>) -> Result<Self> {
        let n = input.get_varint()?;
        if n < 0 {
            return Err(ProtocolError::NegativeLength(n));
        }
        if n == 0 {
            return Ok(HolderSet::Tag(input.read_string()?.to_string()));
        }
        let count = (n - 1) as usize;
        // Every id costs at least one byte on the wire; reject impossible
        // counts before allocating.
        if count > input.remaining() {
            return Err(ProtocolError::BufferUnderflow {
                needed: count,
                remaining: input.remaining(),
            });
        }
        let mut ids = Vec::with_capacity(count);
        for _ in 0..count {
            ids.push(input.get_varint()?);
        }
        Ok(HolderSet::Ids(ids))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holder_roundtrip() {
        let mut w = PacketWriter::new();
        Holder::<u16>::Reference(41i32).encode(&mut w).unwrap();
        Holder::Inline(7u16).encode(&mut w).unwrap();
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert_eq!(
            Holder::<u16>::decode(&mut r).unwrap(),
            Holder::Reference(41)
        );
        assert_eq!(Holder::<u16>::decode(&mut r).unwrap(), Holder::Inline(7));
        assert!(r.is_empty());
    }

    #[test]
    fn holder_negative_errors() {
        let mut w = PacketWriter::new();
        w.put_varint(-1);
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert!(matches!(
            Holder::<u16>::decode(&mut r),
            Err(ProtocolError::NegativeLength(-1))
        ));
        assert!(matches!(
            Holder::<u16>::Reference(-1i32).encode(&mut PacketWriter::new()),
            Err(ProtocolError::InvalidData(_))
        ));
    }

    #[test]
    fn holder_set_roundtrip() {
        let mut w = PacketWriter::new();
        HolderSet::Tag("minecraft:wooden_slabs".into())
            .encode(&mut w)
            .unwrap();
        HolderSet::Ids(vec![1, 300, -5]).encode(&mut w).unwrap();
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert_eq!(
            HolderSet::decode(&mut r).unwrap(),
            HolderSet::Tag("minecraft:wooden_slabs".into())
        );
        assert_eq!(
            HolderSet::decode(&mut r).unwrap(),
            HolderSet::Ids(vec![1, 300, -5])
        );
        assert!(r.is_empty());
    }

    #[test]
    fn holder_set_oversized_count_errors() {
        let mut w = PacketWriter::new();
        w.put_varint(1000);
        w.put_bytes(&[0x01, 0x02]);
        let buf = w.into_inner();
        let mut r = PacketReader::new(&buf);
        assert!(matches!(
            HolderSet::decode(&mut r),
            Err(ProtocolError::BufferUnderflow { .. })
        ));
    }
}
