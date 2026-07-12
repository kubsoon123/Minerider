//! Encode/decode traits implemented by every generated packet and type.
//!
//! Primitives with a single wire representation (fixed-width ints, floats,
//! bool, string, UUID-as-u128) get impls here. `i32`/`i64` deliberately do
//! NOT implement the traits: they are ambiguous between fixed-width and
//! VarInt/VarLong, so generated code calls the explicit
//! [`PacketWriter::put_varint`] / [`PacketReader::get_varint`] style methods
//! for those.

use crate::buffer::{PacketReader, PacketWriter};
use crate::error::{ProtocolError, Result};

/// Serializes a value into a packet payload.
pub trait Encode {
    /// Appends the wire representation of `self` to `out`.
    fn encode(&self, out: &mut PacketWriter) -> Result<()>;
}

/// Parses a value from a packet payload.
pub trait Decode: Sized {
    /// Reads the wire representation of `Self` from `input`.
    fn decode(input: &mut PacketReader<'_>) -> Result<Self>;
}

macro_rules! impl_fixed {
    ($ty:ty, $get:ident, $put:ident) => {
        impl Encode for $ty {
            fn encode(&self, out: &mut PacketWriter) -> Result<()> {
                out.$put(*self);
                Ok(())
            }
        }
        impl Decode for $ty {
            fn decode(input: &mut PacketReader<'_>) -> Result<Self> {
                input.$get()
            }
        }
    };
}

impl_fixed!(i8, get_i8, put_i8);
impl_fixed!(u8, get_u8, put_u8);
impl_fixed!(i16, get_i16, put_i16);
impl_fixed!(u16, get_u16, put_u16);
impl_fixed!(u32, get_u32, put_u32);
impl_fixed!(u64, get_u64, put_u64);
impl_fixed!(f32, get_f32, put_f32);
impl_fixed!(f64, get_f64, put_f64);

impl Encode for bool {
    fn encode(&self, out: &mut PacketWriter) -> Result<()> {
        out.put_bool(*self);
        Ok(())
    }
}

impl Decode for bool {
    fn decode(input: &mut PacketReader<'_>) -> Result<Self> {
        input.get_bool()
    }
}

/// The unit type encodes to nothing (the `void` type of the protocol data).
impl Encode for () {
    fn encode(&self, _out: &mut PacketWriter) -> Result<()> {
        Ok(())
    }
}

impl Decode for () {
    fn decode(_input: &mut PacketReader<'_>) -> Result<Self> {
        Ok(())
    }
}

/// Strings use the VarInt-length-prefixed bounded protocol string.
impl Encode for String {
    fn encode(&self, out: &mut PacketWriter) -> Result<()> {
        out.put_string(self)
    }
}

impl Decode for String {
    fn decode(input: &mut PacketReader<'_>) -> Result<Self> {
        Ok(input.read_string()?.to_string())
    }
}

/// `u128` carries a UUID: 16 big-endian bytes.
impl Encode for u128 {
    fn encode(&self, out: &mut PacketWriter) -> Result<()> {
        out.put_uuid(*self);
        Ok(())
    }
}

impl Decode for u128 {
    fn decode(input: &mut PacketReader<'_>) -> Result<Self> {
        input.read_uuid()
    }
}

/// VarInt-length-prefixed arrays of encodable elements.
impl<T: Encode> Encode for Vec<T> {
    fn encode(&self, out: &mut PacketWriter) -> Result<()> {
        out.put_varint(self.len() as i32);
        for item in self {
            item.encode(out)?;
        }
        Ok(())
    }
}

impl<T: Decode> Decode for Vec<T> {
    fn decode(input: &mut PacketReader<'_>) -> Result<Self> {
        input.read_array(T::decode)
    }
}

/// Boolean-prefixed optional values.
impl<T: Encode> Encode for Option<T> {
    fn encode(&self, out: &mut PacketWriter) -> Result<()> {
        out.put_bool(self.is_some());
        if let Some(v) = self {
            v.encode(out)?;
        }
        Ok(())
    }
}

impl<T: Decode> Decode for Option<T> {
    fn decode(input: &mut PacketReader<'_>) -> Result<Self> {
        input.read_option(T::decode)
    }
}

/// Boxes provide the indirection recursive protocol types need (e.g.
/// `ItemEffectDetail.hidden_effect: Option<Box<ItemEffectDetail>>`).
impl<T: Encode> Encode for Box<T> {
    fn encode(&self, out: &mut PacketWriter) -> Result<()> {
        (**self).encode(out)
    }
}

impl<T: Decode> Decode for Box<T> {
    fn decode(input: &mut PacketReader<'_>) -> Result<Self> {
        Ok(Box::new(T::decode(input)?))
    }
}

/// Rejects trailing bytes after a top-level packet decode.
pub(crate) fn ensure_consumed(input: &PacketReader<'_>, context: &'static str) -> Result<()> {
    if input.is_empty() {
        Ok(())
    } else {
        Err(ProtocolError::TrailingBytes {
            context,
            remaining: input.remaining(),
        })
    }
}
