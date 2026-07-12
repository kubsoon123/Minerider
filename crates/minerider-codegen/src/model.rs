//! Serde model of a minecraft-data `protocol.json` file.
//!
//! The JSON shape (protodef): a top-level `types` section plus one section
//! per protocol state (`handshaking`, `status`, `login`, `configuration`,
//! `play`), each with `toClient` / `toServer` direction sub-sections that
//! carry their own `types` maps.
//!
//! Type definitions come in three shapes:
//! - the string `"native"` — marker meaning the type *name* is a native,
//! - any other string — an alias to another named type,
//! - a `[kind, args]` pair — a complex type expression.

use std::collections::BTreeMap;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};

/// A parsed `protocol.json`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProtocolFile {
    /// Top-level named types (natives, aliases and shared definitions).
    pub types: BTreeMap<String, TypeDef>,
    /// Handshaking state.
    pub handshaking: StateSection,
    /// Status state.
    pub status: StateSection,
    /// Login state.
    pub login: StateSection,
    /// Configuration state.
    pub configuration: StateSection,
    /// Play state.
    pub play: StateSection,
}

/// One protocol state, both directions.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StateSection {
    /// Server → client packets and local types.
    #[serde(rename = "toClient")]
    pub to_client: DirectionSection,
    /// Client → server packets and local types.
    #[serde(rename = "toServer")]
    pub to_server: DirectionSection,
}

/// One direction of one state.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DirectionSection {
    /// Local named types: the `packet` container plus packet payload types
    /// (`packet_*`) and, for play, a few shared types.
    pub types: BTreeMap<String, TypeDef>,
}

/// A named type definition.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeDef {
    /// The `"native"` marker: the type name itself is a native.
    Native,
    /// A plain alias to another named type (e.g. `"ContainerID": "varint"`).
    Alias(String),
    /// A complex type expression.
    Complex(Complex),
}

/// An inline type expression: either a reference to a named type/native or
/// a complex `[kind, args]` expression.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeRef {
    /// Reference by name (native or named type).
    Named(String),
    /// Complex inline expression.
    Complex(Box<Complex>),
}

/// A `[kind, args]` complex type expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Complex {
    /// Ordered list of named (or anonymous) fields → Rust struct.
    Container(Vec<ContainerField>),
    /// Length-prefixed, fixed-size or field-counted array.
    Array(ArrayArgs),
    /// Boolean-prefixed optional value.
    Option(TypeRef),
    /// Discriminated union over a previously decoded field.
    Switch(SwitchArgs),
    /// Numeric value → named mapping → Rust enum.
    Mapper(MapperArgs),
    /// Length-prefixed or fixed-size raw byte buffer.
    Buffer(BufferArgs),
    /// Length-prefixed string (vanilla strings go through this).
    Pstring(PstringArgs),
    /// Bit-packed integer split into named fields.
    Bitfield(Vec<BitfieldMember>),
    /// Bit mask with named flags → newtype wrapper.
    Bitflags(BitflagsArgs),
    /// Array terminated by the top bit of each element's first byte.
    TopBitSetTerminatedArray {
        /// Element type (a container whose first field is i8/u8).
        element: TypeRef,
    },
    /// Entity metadata entries terminated by a sentinel byte.
    EntityMetadataLoop(EntityMetadataLoopArgs),
    /// Registry reference id or inline value → `Holder<T>`.
    RegistryEntryHolder(HolderArgs),
    /// Registry tag name or id list → `HolderSet`.
    RegistryEntryHolderSet(HolderSetArgs),
}

/// A field of a container.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ContainerField {
    /// Field name; absent when `anon` is true.
    #[serde(default)]
    pub name: Option<String>,
    /// Anonymous field: inlined into the parent without a name.
    #[serde(default)]
    pub anon: bool,
    /// Field type.
    #[serde(rename = "type")]
    pub ty: TypeRef,
}

/// `array` arguments.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ArrayArgs {
    /// Element type.
    #[serde(rename = "type")]
    pub element: TypeRef,
    /// Native type of the length prefix (always `varint` in practice).
    #[serde(rename = "countType", default)]
    pub count_type: Option<String>,
    /// Fixed element count, or the name of an earlier sibling field that
    /// holds the count.
    #[serde(default)]
    pub count: Option<ArrayCount>,
}

/// The `count` of an array: a literal or a sibling field reference.
#[derive(Debug, Clone, PartialEq)]
pub enum ArrayCount {
    /// Fixed element count.
    Fixed(u32),
    /// Name of an earlier sibling field holding the count.
    Field(String),
}

/// `switch` arguments.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SwitchArgs {
    /// Path to the discriminant: a previously decoded sibling field
    /// (`field`), a bitfield/bitflags member (`field/member`), or the same
    /// in the parent container (`../field`, `../field/member`).
    #[serde(rename = "compareTo")]
    pub compare_to: String,
    /// Branch per discriminant value. Keys are mapper names, decimal
    /// integers, or `"0"`/`"1"` for booleans.
    pub fields: BTreeMap<String, TypeRef>,
    /// Fallback branch for values without an explicit branch.
    #[serde(default)]
    pub default: Option<TypeRef>,
}

/// `mapper` arguments.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MapperArgs {
    /// Underlying numeric native type.
    #[serde(rename = "type")]
    pub ty: String,
    /// Wire value (`"0x1a"`, `"26"`, `"-1"`) → name.
    pub mappings: BTreeMap<String, String>,
}

/// `buffer` arguments.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BufferArgs {
    /// Native type of the length prefix (always `varint` in practice).
    #[serde(rename = "countType", default)]
    pub count_type: Option<String>,
    /// Fixed byte count.
    #[serde(default)]
    pub count: Option<u32>,
}

/// `pstring` arguments.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PstringArgs {
    /// Native type of the length prefix (always `varint` in practice).
    #[serde(rename = "countType", default)]
    pub count_type: Option<String>,
}

/// One member of a bitfield.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BitfieldMember {
    /// Member name.
    pub name: String,
    /// Width in bits.
    pub size: u32,
    /// Whether the member is sign-extended.
    pub signed: bool,
}

/// `bitflags` arguments.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BitflagsArgs {
    /// Backing native type (`u8` or `u32`).
    #[serde(rename = "type")]
    pub ty: String,
    /// Flag names; bit position is the index in this list.
    pub flags: Vec<String>,
}

/// `entityMetadataLoop` arguments.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EntityMetadataLoopArgs {
    /// Sentinel byte that terminates the loop (255).
    #[serde(rename = "endVal")]
    pub end_val: u8,
    /// Entry type (a container whose first field is a u8 index).
    #[serde(rename = "type")]
    pub element: TypeRef,
}

/// A `{name, type}` pair used by holder types.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NamedField {
    /// Field name.
    pub name: String,
    /// Field type.
    #[serde(rename = "type")]
    pub ty: TypeRef,
}

/// `registryEntryHolder` arguments.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct HolderArgs {
    /// Name of the registry-reference id field (documentation only).
    #[serde(rename = "baseName")]
    pub base_name: String,
    /// Inline value decoded when the id is 0.
    pub otherwise: NamedField,
}

/// `registryEntryHolderSet` arguments.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct HolderSetArgs {
    /// Tag form decoded when the count is 0.
    pub base: NamedField,
    /// Id-list form decoded otherwise.
    pub otherwise: NamedField,
}

impl<'de> Deserialize<'de> for TypeDef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::String(s) if s == "native" => Ok(TypeDef::Native),
            serde_json::Value::String(s) => Ok(TypeDef::Alias(s)),
            other => Ok(TypeDef::Complex(
                serde_json::from_value(other).map_err(de::Error::custom)?,
            )),
        }
    }
}

impl<'de> Deserialize<'de> for TypeRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::String(s) => Ok(TypeRef::Named(s)),
            other => Ok(TypeRef::Complex(Box::new(
                serde_json::from_value(other).map_err(de::Error::custom)?,
            ))),
        }
    }
}

impl<'de> Deserialize<'de> for ArrayCount {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::Number(n) => n
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .map(ArrayCount::Fixed)
                .ok_or_else(|| de::Error::custom("array count must be a non-negative integer")),
            serde_json::Value::String(s) => Ok(ArrayCount::Field(s)),
            _ => Err(de::Error::custom(
                "array count must be an integer or a field name",
            )),
        }
    }
}

impl<'de> Deserialize<'de> for Complex {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_seq(ComplexVisitor)
    }
}

struct ComplexVisitor;

impl<'de> Visitor<'de> for ComplexVisitor {
    type Value = Complex;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a [kind, args] type expression")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Complex, A::Error> {
        let kind: String = seq
            .next_element()?
            .ok_or_else(|| de::Error::custom("type expression missing kind"))?;
        let args: serde_json::Value = seq
            .next_element()?
            .ok_or_else(|| de::Error::custom(format!("type expression `{kind}` missing args")))?;
        if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
            return Err(de::Error::custom(format!(
                "type expression `{kind}` has more than two elements"
            )));
        }
        let parsed = match kind.as_str() {
            "container" => Complex::Container(from_value(args)?),
            "array" => Complex::Array(from_value(args)?),
            "option" => Complex::Option(from_value(args)?),
            "switch" => Complex::Switch(from_value(args)?),
            "mapper" => Complex::Mapper(from_value(args)?),
            "buffer" => Complex::Buffer(from_value(args)?),
            "pstring" => Complex::Pstring(from_value(args)?),
            "bitfield" => Complex::Bitfield(from_value(args)?),
            "bitflags" => Complex::Bitflags(from_value(args)?),
            "topBitSetTerminatedArray" => {
                #[derive(Deserialize)]
                struct Args {
                    #[serde(rename = "type")]
                    element: TypeRef,
                }
                let a: Args = from_value(args)?;
                Complex::TopBitSetTerminatedArray { element: a.element }
            }
            "entityMetadataLoop" => Complex::EntityMetadataLoop(from_value(args)?),
            "registryEntryHolder" => Complex::RegistryEntryHolder(from_value(args)?),
            "registryEntryHolderSet" => Complex::RegistryEntryHolderSet(from_value(args)?),
            other => {
                return Err(de::Error::custom(format!(
                    "unsupported type kind `{other}`"
                )));
            }
        };
        Ok(parsed)
    }
}

fn from_value<T: serde::de::DeserializeOwned, E: de::Error>(v: serde_json::Value) -> Result<T, E> {
    serde_json::from_value(v).map_err(de::Error::custom)
}
