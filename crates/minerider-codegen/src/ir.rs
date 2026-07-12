//! Resolved intermediate representation of a minecraft-data protocol.
//!
//! [`Ir::build`] extracts the packet table of every state/direction and
//! validates the whole type graph: every named reference resolves, every
//! `switch` discriminant and array count reference points at an earlier
//! field, every mapper key parses, and only supported kinds appear. All
//! failures are precise: they name the state, packet/type and field.

use std::collections::BTreeMap;

use crate::model::{
    ArrayCount, Complex, ContainerField, DirectionSection, ProtocolFile, TypeDef, TypeRef,
};
use crate::parse::{CodegenError, Result};

/// A protocol state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Handshaking.
    Handshaking,
    /// Status (server list ping).
    Status,
    /// Login.
    Login,
    /// Configuration.
    Configuration,
    /// Play.
    Play,
}

impl State {
    /// All states in wire order.
    pub const ALL: [State; 5] = [
        State::Handshaking,
        State::Status,
        State::Login,
        State::Configuration,
        State::Play,
    ];

    /// minecraft-data section name.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Handshaking => "handshaking",
            State::Status => "status",
            State::Login => "login",
            State::Configuration => "configuration",
            State::Play => "play",
        }
    }
}

/// The resolved protocol.
#[derive(Debug, Clone)]
pub struct Ir {
    /// Top-level shared named types (excluding native markers).
    pub shared_types: BTreeMap<String, TypeDef>,
    /// Per-state resolved tables, in [`State::ALL`] order.
    pub states: Vec<StateIr>,
}

/// One resolved state.
#[derive(Debug, Clone)]
pub struct StateIr {
    /// Which state.
    pub state: State,
    /// Server → client.
    pub clientbound: DirectionIr,
    /// Client → server.
    pub serverbound: DirectionIr,
}

/// One resolved direction of a state.
#[derive(Debug, Clone)]
pub struct DirectionIr {
    /// Local named types (payload `packet_*` types and state-local shared
    /// types), excluding the `packet` dispatch container itself.
    pub local_types: BTreeMap<String, TypeDef>,
    /// Packets sorted by id.
    pub packets: Vec<PacketIr>,
}

/// One resolved packet.
#[derive(Debug, Clone)]
pub struct PacketIr {
    /// Numeric packet id.
    pub id: i32,
    /// minecraft-data packet name, e.g. `keep_alive`.
    pub name: String,
    /// Name of the payload container type (`packet_*`), resolved against
    /// the state's local types or the top-level shared types.
    pub payload: String,
}

impl DirectionIr {
    /// Finds a packet by minecraft-data name.
    pub fn packet(&self, name: &str) -> Option<&PacketIr> {
        self.packets.iter().find(|p| p.name == name)
    }
}

/// How a type behaves when referenced by a `switch` discriminant or an
/// array count.
#[derive(Debug, Clone, PartialEq)]
enum Class {
    /// Any numeric native (ints, varint, varlong).
    Numeric,
    /// Boolean.
    Bool,
    /// Mapper enum with the given variant names.
    Mapper(Vec<String>),
    /// Bitfield with `(name, size)` members.
    Bitfield(Vec<(String, u32)>),
    /// Bitflags with the given flag names.
    Bitflags(Vec<String>),
    /// Anything else (string, struct, buffer, ...).
    Other,
}

/// Fields decoded so far in a container, used to validate references.
type Scope = Vec<(String, Class)>;

impl Ir {
    /// Builds and validates the IR from a parsed `protocol.json`.
    pub fn build(file: &ProtocolFile) -> Result<Ir> {
        let shared_types: BTreeMap<String, TypeDef> = file
            .types
            .iter()
            .filter(|(_, def)| !matches!(def, TypeDef::Native))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut states = Vec::new();
        for state in State::ALL {
            let section = match state {
                State::Handshaking => &file.handshaking,
                State::Status => &file.status,
                State::Login => &file.login,
                State::Configuration => &file.configuration,
                State::Play => &file.play,
            };
            let clientbound = build_direction(file, state, "toClient", &section.to_client)?;
            let serverbound = build_direction(file, state, "toServer", &section.to_server)?;
            states.push(StateIr {
                state,
                clientbound,
                serverbound,
            });
        }

        let ir = Ir {
            shared_types,
            states,
        };
        ir.validate()?;
        Ok(ir)
    }

    /// The resolved table of one state.
    pub fn state_section(&self, state: State) -> &StateIr {
        self.states
            .iter()
            .find(|s| s.state == state)
            .expect("Ir::build emits every state")
    }

    /// Resolves a named type within a state/direction scope: local types
    /// first, then top-level shared types. Returns `None` for natives and
    /// unknown names.
    fn lookup<'a>(&'a self, dir: Option<&'a DirectionIr>, name: &str) -> Option<&'a TypeDef> {
        if let Some(dir) = dir {
            if let Some(def) = dir.local_types.get(name) {
                return Some(def);
            }
        }
        self.shared_types.get(name)
    }

    /// Whether `name` is a native marker type.
    fn is_native(&self, name: &str) -> bool {
        // Natives are declared in the top-level types of the source file
        // as `"name": "native"`; Ir::build filters those out, so check the
        // fixed native set instead.
        NATIVES.contains(&name)
    }

    /// Classifies a type reference for `switch`/count validation.
    fn classify(&self, dir: Option<&DirectionIr>, ty: &TypeRef, ctx: &str) -> Result<Class> {
        match ty {
            TypeRef::Named(name) => self.classify_named(dir, name, ctx, 0),
            TypeRef::Complex(c) => Ok(match c.as_ref() {
                Complex::Mapper(m) => Class::Mapper(m.mappings.values().cloned().collect()),
                Complex::Bitfield(members) => {
                    Class::Bitfield(members.iter().map(|m| (m.name.clone(), m.size)).collect())
                }
                Complex::Bitflags(b) => Class::Bitflags(b.flags.clone()),
                // A switch whose branches all decode to a number (or an
                // option of a number, which protodef unwraps) compares
                // numerically — e.g. scoreboard_objective.styling compares
                // against the varint inside number_format's option.
                Complex::Switch(s) => {
                    if s.fields
                        .values()
                        .chain(s.default.iter())
                        .all(branch_is_numeric_or_void)
                    {
                        Class::Numeric
                    } else {
                        Class::Other
                    }
                }
                // protodef unwraps options: an option of a numeric compares
                // numerically (e.g. scoreboard_score.styling).
                Complex::Option(inner) => {
                    if branch_is_numeric_or_void(inner) {
                        Class::Numeric
                    } else {
                        Class::Other
                    }
                }
                _ => Class::Other,
            }),
        }
    }

    fn classify_named(
        &self,
        dir: Option<&DirectionIr>,
        name: &str,
        ctx: &str,
        depth: u32,
    ) -> Result<Class> {
        if depth > 32 {
            return Err(CodegenError::Invalid(format!(
                "{ctx}: alias cycle involving `{name}`"
            )));
        }
        if self.is_native(name) {
            return Ok(match name {
                "bool" => Class::Bool,
                "i8" | "u8" | "i16" | "u16" | "i32" | "u32" | "i64" | "u64" | "varint"
                | "varlong" => Class::Numeric,
                _ => Class::Other,
            });
        }
        match self.lookup(dir, name) {
            Some(TypeDef::Alias(target)) => self.classify_named(dir, target, ctx, depth + 1),
            Some(TypeDef::Complex(c)) => {
                self.classify(dir, &TypeRef::Complex(Box::new(c.clone())), ctx)
            }
            Some(TypeDef::Native) => unreachable!("natives filtered out of shared_types"),
            None => Err(CodegenError::Invalid(format!(
                "{ctx}: unresolved type reference `{name}`"
            ))),
        }
    }

    /// Runs the semantic validation over every named type.
    fn validate(&self) -> Result<()> {
        for (name, def) in &self.shared_types {
            self.validate_def(None, name, def)?;
        }
        for state in &self.states {
            for dir in [&state.clientbound, &state.serverbound] {
                for (name, def) in &dir.local_types {
                    self.validate_def(Some(dir), name, def)?;
                }
            }
        }
        Ok(())
    }

    fn validate_def(&self, dir: Option<&DirectionIr>, name: &str, def: &TypeDef) -> Result<()> {
        match def {
            TypeDef::Native => Ok(()),
            TypeDef::Alias(target) => {
                if !self.is_native(target) && self.lookup(dir, target).is_none() {
                    return Err(CodegenError::Invalid(format!(
                        "type `{name}`: alias target `{target}` does not resolve"
                    )));
                }
                Ok(())
            }
            TypeDef::Complex(c) => {
                let mut scope: Scope = Vec::new();
                let ancestors: Vec<Scope> = Vec::new();
                self.validate_complex(dir, c, name, &mut scope, &ancestors)
            }
        }
    }

    /// Validates a complex type expression. `scope` holds the fields decoded
    /// so far in the current container; `ancestors` holds enclosing
    /// container scopes for `../` references.
    #[allow(clippy::too_many_arguments)]
    fn validate_complex(
        &self,
        dir: Option<&DirectionIr>,
        c: &Complex,
        ctx: &str,
        scope: &mut Scope,
        ancestors: &[Scope],
    ) -> Result<()> {
        match c {
            Complex::Container(fields) => {
                // A container's fields form their own scope; the current
                // scope becomes the nearest ancestor for `../` references.
                let mut inner: Scope = Vec::new();
                let mut chain = ancestors.to_vec();
                chain.push(scope.clone());
                self.validate_container(dir, fields, ctx, &mut inner, &chain)
            }
            Complex::Array(a) => {
                if let Some(ct) = &a.count_type {
                    self.require_numeric_native(ct, ctx)?;
                }
                if let Some(ArrayCount::Field(field)) = &a.count {
                    match scope_lookup(scope, field) {
                        Some(Class::Numeric) => {}
                        _ => {
                            return Err(CodegenError::Invalid(format!(
                                "{ctx}: array count field `{field}` is not a previously decoded numeric field"
                            )));
                        }
                    }
                }
                // The element validates against the array's scope: if it is
                // a container, that scope becomes its parent for `../`.
                self.validate_typeref(dir, &a.element, ctx, scope, ancestors)
            }
            Complex::Option(inner) => self.validate_typeref(dir, inner, ctx, scope, ancestors),
            Complex::Switch(s) => {
                let class = self.resolve_compare_to(&s.compare_to, scope, ancestors, ctx)?;
                for (key, branch) in &s.fields {
                    validate_branch_key(&class, key, ctx)?;
                    // Branches live at the switch's field position: they
                    // validate against the switch's own scope, so `../`
                    // inside a branch container reaches the switch's
                    // parent container.
                    self.validate_typeref(dir, branch, ctx, scope, ancestors)?;
                }
                if let Some(default) = &s.default {
                    self.validate_typeref(dir, default, ctx, scope, ancestors)?;
                }
                Ok(())
            }
            Complex::Mapper(m) => {
                self.require_numeric_native(&m.ty, ctx)?;
                for key in m.mappings.keys() {
                    parse_mapper_key(key).ok_or_else(|| {
                        CodegenError::Invalid(format!("{ctx}: unparseable mapper key `{key}`"))
                    })?;
                }
                Ok(())
            }
            Complex::Buffer(b) => {
                if let Some(ct) = &b.count_type {
                    self.require_numeric_native(ct, ctx)?;
                }
                Ok(())
            }
            Complex::Pstring(p) => {
                let ct = p.count_type.as_deref().ok_or_else(|| {
                    CodegenError::Invalid(format!("{ctx}: pstring without countType"))
                })?;
                self.require_numeric_native(ct, ctx)
            }
            Complex::Bitfield(members) => {
                let total: u32 = members.iter().map(|m| m.size).sum();
                if total == 0 || total > 64 {
                    return Err(CodegenError::Invalid(format!(
                        "{ctx}: bitfield total width {total} bits is out of range 1..=64"
                    )));
                }
                for m in members {
                    if m.size == 0 {
                        return Err(CodegenError::Invalid(format!(
                            "{ctx}: bitfield member `{}` has size 0",
                            m.name
                        )));
                    }
                }
                Ok(())
            }
            Complex::Bitflags(b) => {
                if b.ty != "u8" && b.ty != "u32" {
                    return Err(CodegenError::Invalid(format!(
                        "{ctx}: bitflags backing type `{}` is not u8/u32",
                        b.ty
                    )));
                }
                let max = if b.ty == "u8" { 8 } else { 32 };
                if b.flags.len() > max {
                    return Err(CodegenError::Invalid(format!(
                        "{ctx}: bitflags has {} flags but backing type {} holds {max} bits",
                        b.flags.len(),
                        b.ty
                    )));
                }
                Ok(())
            }
            Complex::TopBitSetTerminatedArray { element } => self
                .require_container_with_first_int_field(
                    dir,
                    element,
                    ctx,
                    "topBitSetTerminatedArray",
                ),
            Complex::EntityMetadataLoop(e) => self.require_container_with_first_int_field(
                dir,
                &e.element,
                ctx,
                "entityMetadataLoop",
            ),
            Complex::RegistryEntryHolder(h) => {
                let mut s: Scope = Vec::new();
                self.validate_typeref(dir, &h.otherwise.ty, ctx, &mut s, ancestors)
            }
            Complex::RegistryEntryHolderSet(h) => {
                let mut s: Scope = Vec::new();
                self.validate_typeref(dir, &h.base.ty, ctx, &mut s, ancestors)?;
                let mut s: Scope = Vec::new();
                self.validate_typeref(dir, &h.otherwise.ty, ctx, &mut s, ancestors)
            }
        }
    }

    fn validate_container(
        &self,
        dir: Option<&DirectionIr>,
        fields: &[ContainerField],
        ctx: &str,
        scope: &mut Scope,
        ancestors: &[Scope],
    ) -> Result<()> {
        for field in fields {
            let field_ctx = match &field.name {
                Some(n) => format!("{ctx}.{n}"),
                None => format!("{ctx}.<anon>"),
            };
            if field.anon {
                // Anonymous containers merge their fields into the parent
                // scope; anonymous switches compare against it directly.
                match &field.ty {
                    TypeRef::Complex(c) if matches!(c.as_ref(), Complex::Container(_)) => {
                        if let Complex::Container(fields) = c.as_ref() {
                            self.validate_container(dir, fields, &field_ctx, scope, ancestors)?;
                        }
                    }
                    other => {
                        self.validate_typeref(dir, other, &field_ctx, scope, ancestors)?;
                    }
                }
            } else {
                let name = field.name.clone().expect("non-anon field has a name");
                // The field's type validates against the current scope:
                // a switch here compares against its siblings directly.
                self.validate_typeref(dir, &field.ty, &field_ctx, scope, ancestors)?;
                let class = self.classify(dir, &field.ty, &field_ctx)?;
                scope.push((name, class));
            }
        }
        Ok(())
    }

    fn validate_typeref(
        &self,
        dir: Option<&DirectionIr>,
        ty: &TypeRef,
        ctx: &str,
        scope: &mut Scope,
        ancestors: &[Scope],
    ) -> Result<()> {
        match ty {
            TypeRef::Named(name) => {
                if !self.is_native(name) && self.lookup(dir, name).is_none() {
                    return Err(CodegenError::Invalid(format!(
                        "{ctx}: unresolved type reference `{name}`"
                    )));
                }
                Ok(())
            }
            TypeRef::Complex(c) => self.validate_complex(dir, c, ctx, scope, ancestors),
        }
    }

    /// Resolves a `switch` compareTo path to the class it compares against.
    fn resolve_compare_to(
        &self,
        path: &str,
        scope: &Scope,
        ancestors: &[Scope],
        ctx: &str,
    ) -> Result<Class> {
        let mut segments: Vec<&str> = path.split('/').collect();
        let mut scopes: Vec<&Scope> = Vec::new();
        // Leading `..` segments walk up the ancestor chain.
        let mut up = 0;
        while segments.first() == Some(&"..") {
            up += 1;
            segments.remove(0);
        }
        if up > ancestors.len() {
            return Err(CodegenError::Invalid(format!(
                "{ctx}: compareTo `{path}` walks above the root container"
            )));
        }
        scopes.push(scope);
        scopes.extend(ancestors.iter().rev());
        let target_scope = scopes[up];
        let field = segments.first().copied().unwrap_or("");
        let mut class = scope_lookup(target_scope, field).cloned().ok_or_else(|| {
            CodegenError::Invalid(format!(
                "{ctx}: compareTo `{path}` does not name a previously decoded field"
            ))
        })?;
        for member in &segments[1..] {
            class = match class {
                Class::Bitfield(members) => {
                    let (_, size) = members.iter().find(|(n, _)| n == member).ok_or_else(|| {
                        CodegenError::Invalid(format!(
                            "{ctx}: compareTo `{path}`: `{member}` is not a bitfield member"
                        ))
                    })?;
                    if *size == 1 {
                        Class::Bool
                    } else {
                        Class::Numeric
                    }
                }
                Class::Bitflags(flags) => {
                    if !flags.iter().any(|f| f == member) {
                        return Err(CodegenError::Invalid(format!(
                            "{ctx}: compareTo `{path}`: `{member}` is not a bitflags flag"
                        )));
                    }
                    Class::Bool
                }
                Class::Mapper(variants) => {
                    if !variants.iter().any(|v| v == member) {
                        return Err(CodegenError::Invalid(format!(
                            "{ctx}: compareTo `{path}`: `{member}` is not a mapper variant"
                        )));
                    }
                    Class::Bool
                }
                _ => {
                    return Err(CodegenError::Invalid(format!(
                        "{ctx}: compareTo `{path}`: `{field}` has no member `{member}`"
                    )));
                }
            };
        }
        Ok(class)
    }

    fn require_numeric_native(&self, name: &str, ctx: &str) -> Result<()> {
        if matches!(
            name,
            "i8" | "u8" | "i16" | "u16" | "i32" | "u32" | "i64" | "u64" | "varint" | "varlong"
        ) {
            Ok(())
        } else {
            Err(CodegenError::Invalid(format!(
                "{ctx}: `{name}` is not a numeric native type"
            )))
        }
    }

    /// topBitSetTerminatedArray / entityMetadataLoop elements must be
    /// containers whose first field is a single-byte integer.
    fn require_container_with_first_int_field(
        &self,
        dir: Option<&DirectionIr>,
        ty: &TypeRef,
        ctx: &str,
        kind: &str,
    ) -> Result<()> {
        let resolved = match ty {
            TypeRef::Complex(c) => Some(c.as_ref()),
            TypeRef::Named(n) => match self.lookup(dir, n) {
                Some(TypeDef::Complex(c)) => Some(c),
                Some(TypeDef::Alias(target)) => {
                    return self.require_container_with_first_int_field(
                        dir,
                        &TypeRef::Named(target.clone()),
                        ctx,
                        kind,
                    );
                }
                _ => None,
            },
        };
        let Some(Complex::Container(fields)) = resolved else {
            return Err(CodegenError::Invalid(format!(
                "{ctx}: {kind} element is not a container"
            )));
        };
        let first = fields.first().ok_or_else(|| {
            CodegenError::Invalid(format!("{ctx}: {kind} element container is empty"))
        })?;
        match &first.ty {
            TypeRef::Named(n) if n == "i8" || n == "u8" => Ok(()),
            _ => Err(CodegenError::Invalid(format!(
                "{ctx}: {kind} element's first field is not i8/u8"
            ))),
        }
    }
}

fn scope_lookup<'a>(scope: &'a Scope, name: &str) -> Option<&'a Class> {
    scope.iter().rev().find(|(n, _)| n == name).map(|(_, c)| c)
}

/// True for `void`, numeric natives, and options of numeric natives.
fn branch_is_numeric_or_void(ty: &TypeRef) -> bool {
    fn is_numeric(name: &str) -> bool {
        matches!(
            name,
            "i8" | "u8" | "i16" | "u16" | "i32" | "u32" | "i64" | "u64" | "varint" | "varlong"
        )
    }
    match ty {
        TypeRef::Named(n) => n == "void" || is_numeric(n),
        TypeRef::Complex(c) => match c.as_ref() {
            Complex::Option(inner) => matches!(inner, TypeRef::Named(n) if is_numeric(n)),
            _ => false,
        },
    }
}

fn validate_branch_key(class: &Class, key: &str, ctx: &str) -> Result<()> {
    let ok = match class {
        Class::Bool => key == "0" || key == "1" || key == "true" || key == "false",
        Class::Numeric => key.parse::<i64>().is_ok(),
        Class::Mapper(variants) => variants.iter().any(|v| v == key),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(CodegenError::Invalid(format!(
            "{ctx}: switch branch key `{key}` does not match its discriminant type"
        )))
    }
}

/// Parses a mapper key: decimal or `0x` hex, optionally negative.
pub fn parse_mapper_key(key: &str) -> Option<i64> {
    let (neg, digits) = match key.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, key),
    };
    let value = match digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        Some(hex) => i64::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<i64>().ok()?,
    };
    Some(if neg { -value } else { value })
}

/// Native marker types known to the generator (from the top-level `types`
/// section of protocol.json).
const NATIVES: &[&str] = &[
    "varint",
    "varlong",
    "pstring",
    "buffer",
    "u8",
    "u16",
    "u32",
    "u64",
    "i8",
    "i16",
    "i32",
    "i64",
    "bool",
    "f32",
    "f64",
    "UUID",
    "option",
    "entityMetadataLoop",
    "topBitSetTerminatedArray",
    "bitfield",
    "bitflags",
    "container",
    "switch",
    "void",
    "array",
    "restBuffer",
    "anonymousNbt",
    "anonOptionalNbt",
    "registryEntryHolder",
    "registryEntryHolderSet",
];

/// Extracts the packet table of one direction.
fn build_direction(
    file: &ProtocolFile,
    state: State,
    dir_name: &str,
    section: &DirectionSection,
) -> Result<DirectionIr> {
    let ctx = format!("{}.{}", state.as_str(), dir_name);
    let packet_def = section
        .types
        .get("packet")
        .ok_or_else(|| CodegenError::Invalid(format!("{ctx}: missing `packet` type")))?;
    let TypeDef::Complex(Complex::Container(fields)) = packet_def else {
        return Err(CodegenError::Invalid(format!(
            "{ctx}: `packet` is not a container"
        )));
    };
    if fields.len() != 2 {
        return Err(CodegenError::Invalid(format!(
            "{ctx}: `packet` container must have exactly 2 fields (name, params)"
        )));
    }
    let TypeRef::Complex(mapper) = &fields[0].ty else {
        return Err(CodegenError::Invalid(format!(
            "{ctx}: packet id field is not a mapper"
        )));
    };
    let Complex::Mapper(mapper) = mapper.as_ref() else {
        return Err(CodegenError::Invalid(format!(
            "{ctx}: packet id field is not a mapper"
        )));
    };
    let TypeRef::Complex(switch) = &fields[1].ty else {
        return Err(CodegenError::Invalid(format!(
            "{ctx}: packet params field is not a switch"
        )));
    };
    let Complex::Switch(switch) = switch.as_ref() else {
        return Err(CodegenError::Invalid(format!(
            "{ctx}: packet params field is not a switch"
        )));
    };

    let mut packets: Vec<PacketIr> = Vec::new();
    for (id_key, name) in &mapper.mappings {
        let id = parse_mapper_key(id_key).ok_or_else(|| {
            CodegenError::Invalid(format!("{ctx}: unparseable packet id `{id_key}`"))
        })?;
        let id = i32::try_from(id).map_err(|_| {
            CodegenError::Invalid(format!("{ctx}: packet id `{id_key}` out of i32 range"))
        })?;
        let payload_ref = switch.fields.get(name).ok_or_else(|| {
            CodegenError::Invalid(format!(
                "{ctx}: packet `{name}` has an id mapping but no params branch"
            ))
        })?;
        let TypeRef::Named(payload) = payload_ref else {
            return Err(CodegenError::Invalid(format!(
                "{ctx}: packet `{name}` params branch is not a named payload type"
            )));
        };
        let in_section = section.types.contains_key(payload.as_str());
        let in_shared = file.types.contains_key(payload.as_str());
        if payload != "void" && !in_section && !in_shared {
            return Err(CodegenError::Invalid(format!(
                "{ctx}: packet `{name}` payload type `{payload}` does not resolve"
            )));
        }
        packets.push(PacketIr {
            id,
            name: name.clone(),
            payload: payload.clone(),
        });
    }
    for branch in switch.fields.keys() {
        if !mapper.mappings.values().any(|n| n == branch) {
            return Err(CodegenError::Invalid(format!(
                "{ctx}: params branch `{branch}` has no id mapping"
            )));
        }
    }
    packets.sort_by_key(|p| p.id);
    for w in packets.windows(2) {
        if w[0].id == w[1].id {
            return Err(CodegenError::Invalid(format!(
                "{ctx}: duplicate packet id {:#x} (`{}` and `{}`)",
                w[0].id, w[0].name, w[1].name
            )));
        }
    }

    let local_types: BTreeMap<String, TypeDef> = section
        .types
        .iter()
        .filter(|(k, _)| k.as_str() != "packet")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Ok(DirectionIr {
        local_types,
        packets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn vendored_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/minecraft-data/pc/1.21.4")
    }

    fn build_vendored() -> Ir {
        let (_, file) = crate::parse::load(&vendored_dir()).expect("vendored data loads");
        Ir::build(&file).expect("vendored 1.21.4 data builds")
    }

    #[test]
    fn parses_vendored_1_21_4() {
        build_vendored();
    }

    #[test]
    fn packet_counts_match_spec() {
        let ir = build_vendored();
        let expect = [
            (State::Handshaking, 0, 2),
            (State::Status, 2, 2),
            (State::Login, 6, 5),
            (State::Configuration, 17, 10),
            (State::Play, 131, 62),
        ];
        for (state, clientbound, serverbound) in expect {
            let s = ir.state_section(state);
            assert_eq!(
                s.clientbound.packets.len(),
                clientbound,
                "{:?} clientbound",
                state
            );
            assert_eq!(
                s.serverbound.packets.len(),
                serverbound,
                "{:?} serverbound",
                state
            );
        }
        let total: usize = ir
            .states
            .iter()
            .map(|s| s.clientbound.packets.len() + s.serverbound.packets.len())
            .sum();
        assert_eq!(total, 237);
    }

    #[test]
    fn ids_are_unique_and_sorted() {
        let ir = build_vendored();
        for state in &ir.states {
            for dir in [&state.clientbound, &state.serverbound] {
                let ids: Vec<i32> = dir.packets.iter().map(|p| p.id).collect();
                let mut sorted = ids.clone();
                sorted.sort();
                sorted.dedup();
                assert_eq!(ids, sorted, "{:?} ids not unique/sorted", state.state);
            }
        }
    }

    #[test]
    fn known_packet_ids() {
        let ir = build_vendored();
        let login = ir.state_section(State::Login);
        assert_eq!(login.clientbound.packet("success").unwrap().id, 0x02);
        assert_eq!(
            login.clientbound.packet("encryption_begin").unwrap().id,
            0x01
        );
        let play = ir.state_section(State::Play);
        assert_eq!(play.clientbound.packet("keep_alive").unwrap().id, 0x27);
        assert_eq!(play.serverbound.packet("keep_alive").unwrap().id, 0x1a);
        let handshaking = ir.state_section(State::Handshaking);
        assert_eq!(
            handshaking.serverbound.packet("set_protocol").unwrap().id,
            0x00
        );
    }

    #[test]
    fn payloads_resolve() {
        let ir = build_vendored();
        let login = ir.state_section(State::Login);
        let success = login.clientbound.packet("success").unwrap();
        assert_eq!(success.payload, "packet_success");
        assert!(login.clientbound.local_types.contains_key("packet_success"));
        // cookie_request's payload lives in the shared top-level types.
        let cookie = login.clientbound.packet("cookie_request").unwrap();
        assert!(ir.shared_types.contains_key(&cookie.payload));
    }

    fn minimal_doc(extra_type: &str) -> serde_json::Value {
        let direction = serde_json::json!({
            "types": {
                "packet": ["container", [
                    {"name": "name", "type": ["mapper", {"type": "varint", "mappings": {}}]},
                    {"name": "params", "type": ["switch", {"compareTo": "name", "fields": {}}]}
                ]]
            }
        });
        let section = serde_json::json!({
            "toClient": direction,
            "toServer": direction
        });
        serde_json::json!({
            "types": {
                "varint": "native",
                "bool": "native",
                "Bogus": extra_type_placeholder(extra_type)
            },
            "handshaking": section,
            "status": section,
            "login": section,
            "configuration": section,
            "play": section
        })
    }

    // serde_json::json! cannot interpolate arbitrary JSON fragments; build
    // the extra type from a raw string instead.
    fn extra_type_placeholder(raw: &str) -> serde_json::Value {
        serde_json::from_str(raw).expect("test json is valid")
    }

    #[test]
    fn unsupported_kind_error_is_precise() {
        let doc = minimal_doc(r#"["frobnicate", {"x": 1}]"#);
        let file: std::result::Result<ProtocolFile, _> = serde_json::from_value(doc);
        let err = file.expect_err("bogus kind must fail to parse");
        let msg = err.to_string();
        assert!(
            msg.contains("frobnicate"),
            "error must name the kind, got: {msg}"
        );
    }

    #[test]
    fn unresolved_reference_error_is_precise() {
        let doc = minimal_doc(r#""DoesNotExist""#);
        // Alias to an unknown name parses but fails IR validation.
        let mut doc = doc;
        doc["types"]["Bogus"] = serde_json::json!(["container", [
            {"name": "f", "type": "DoesNotExist"}
        ]]);
        let file: ProtocolFile = serde_json::from_value(doc).unwrap();
        let err = Ir::build(&file).expect_err("unresolved reference must fail");
        let msg = err.to_string();
        assert!(msg.contains("DoesNotExist"), "got: {msg}");
        assert!(msg.contains("Bogus"), "got: {msg}");
    }

    #[test]
    fn bad_switch_compare_to_is_rejected() {
        let mut doc = minimal_doc("\"varint\"");
        doc["types"]["Bogus"] = serde_json::json!(["container", [
            {"name": "a", "type": "varint"},
            {"name": "b", "type": ["switch", {"compareTo": "nope", "fields": {"1": "varint"}}]}
        ]]);
        let file: ProtocolFile = serde_json::from_value(doc).unwrap();
        let err = Ir::build(&file).expect_err("bad compareTo must fail");
        let msg = err.to_string();
        assert!(msg.contains("nope"), "got: {msg}");
        assert!(msg.contains("Bogus.b"), "got: {msg}");
    }

    #[test]
    fn duplicate_packet_ids_are_rejected() {
        let mut doc = minimal_doc("\"varint\"");
        let dir = serde_json::json!({
            "types": {
                "packet": ["container", [
                    {"name": "name", "type": ["mapper", {"type": "varint", "mappings": {
                        "0x00": "a", "0x00": "b"
                    }}]},
                    {"name": "params", "type": ["switch", {"compareTo": "name", "fields": {}}]}
                ]]
            }
        });
        // serde_json dedups object keys, so craft two ids mapping to the
        // same numeric value via different spellings instead.
        let mut dir = dir;
        dir["types"]["packet"][1][0]["type"][1]["mappings"] = serde_json::json!({
            "0": "a", "0x00": "b"
        });
        dir["types"]["packet"][1][1]["type"][1]["fields"] = serde_json::json!({
            "a": "packet_a", "b": "packet_b"
        });
        dir["types"]["packet_a"] = serde_json::json!(["container", []]);
        dir["types"]["packet_b"] = serde_json::json!(["container", []]);
        doc["login"]["toServer"] = dir;
        let file: ProtocolFile = serde_json::from_value(doc).unwrap();
        let err = Ir::build(&file).expect_err("duplicate ids must fail");
        assert!(
            err.to_string().contains("duplicate packet id"),
            "got: {err}"
        );
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let dir = tempfile_dir();
        std::fs::write(
            dir.join("version.json"),
            r#"{"minecraftVersion": "1.20.1", "version": 763}"#,
        )
        .unwrap();
        std::fs::write(dir.join("protocol.json"), "{}").unwrap();
        let err = crate::parse::load(&dir).expect_err("wrong version must fail");
        let msg = err.to_string();
        assert!(msg.contains("763"), "got: {msg}");
        assert!(msg.contains("769"), "got: {msg}");
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "minerider-codegen-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
