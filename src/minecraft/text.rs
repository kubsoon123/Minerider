//! Shared text-component model: the one representation chat, system
//! messages, action bar, titles, boss bars, scoreboards and tab-list
//! header/footer are all built on, instead of five separate ad-hoc parsers.
//!
//! Minecraft 1.21.4 presents text three ways on the wire, all supported here:
//! - **Network NBT** (`minerider_protocol::nbt::Nbt`, via
//!   [`TextComponent::from_nbt`]) — every play-state text field: chat,
//!   system chat, titles, boss bars, tab-list header/footer, and
//!   configuration/play disconnect reasons.
//! - **JSON string** (via [`TextComponent::from_json_str`]) — the
//!   login-state Disconnect packet's `reason` field. Its wire type is a
//!   plain length-prefixed string (`minecraft-data` models it as a bare
//!   `string` native, and the generated struct field is accordingly a
//!   `String`), but the *content* of that string is still JSON-encoded
//!   text-component data, predating 1.20.3's switch to network NBT for
//!   every other text field.
//! - **Plain string** — where the protocol carries no component structure
//!   at all (e.g. a player chat packet's already-rendered `plain_message`),
//!   [`TextComponent::literal`] is the direct, correct conversion.
//!
//! Parsing never fails and never panics: a malformed or hostile component
//! degrades to the best-effort text it can extract (worst case, an empty
//! literal) rather than returning `Err`, since a garbled text component is
//! a display concern, not a wire-protocol violation a caller should have to
//! handle. Recursion depth, total node count and total literal-text bytes
//! are all bounded (see the `MAX_*` constants) so a hostile server cannot
//! blow the stack or force unbounded parse time/memory through `extra`,
//! `with`, or a hover event's nested component.

use minerider_protocol::nbt::Nbt;

/// Recursion depth cap while parsing a component tree.
const MAX_DEPTH: usize = 64;
/// Total node cap (this component plus every sibling/argument reachable
/// from it, at any depth) — bounds parse time/memory independent of depth.
const MAX_NODES: usize = 4_096;
/// Total literal-text byte cap across the whole tree.
const MAX_TEXT_BYTES: usize = 262_144;

/// A parsed Minecraft text component.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextComponent {
    pub content: Content,
    pub style: Style,
    /// Sibling components appended after this one's own text, in order.
    pub extra: Vec<TextComponent>,
}

/// What a component actually says.
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    Literal(String),
    /// A translation key plus its (still-structured, not pre-rendered)
    /// arguments. [`TextComponent::plain_text`] substitutes known keys from
    /// a small built-in table (see [`translation_template`]); an unknown
    /// key falls back to showing the key and its rendered arguments rather
    /// than silently disappearing or claiming a full client-side
    /// localization MineRider does not ship.
    Translate {
        key: String,
        args: Vec<TextComponent>,
    },
    /// A component kind this module doesn't specifically interpret (score,
    /// selector, keybind, nbt-value components, or a shape not
    /// recognized) — the original data is preserved here rather than
    /// silently discarded, so a caller that cares can still inspect it.
    Unknown {
        raw: Nbt,
    },
}

impl Default for Content {
    fn default() -> Self {
        Content::Literal(String::new())
    }
}

/// Formatting/interaction fields. Every field is optional: `None` means
/// "not specified here" (a real client would inherit from a parent
/// component), not "explicitly off".
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Style {
    pub color: Option<Color>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underlined: Option<bool>,
    pub strikethrough: Option<bool>,
    pub obfuscated: Option<bool>,
    pub insertion: Option<String>,
    pub font: Option<String>,
    pub click_event: Option<ClickEvent>,
    pub hover_event: Option<HoverEvent>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Color {
    /// A named color ("red", "gold", "reset", ...) exactly as the server
    /// sent it — not mapped to an RGB value, since MineRider has no
    /// renderer that would need one.
    Named(String),
    /// A `#RRGGBB` hex color (without the leading `#`).
    Hex(String),
}

/// Preserved click-event data: `{action, value}` in both JSON and network
/// NBT text components as of 1.21.4.
#[derive(Debug, Clone, PartialEq)]
pub struct ClickEvent {
    pub action: String,
    pub value: String,
}

/// Preserved hover-event data.
#[derive(Debug, Clone, PartialEq)]
pub struct HoverEvent {
    pub action: String,
    /// Populated when `action` is `"show_text"` — the nested component is
    /// parsed like any other (and shares this parse's node/text budget, so
    /// a hover event can't be used to bypass the limits above).
    /// `show_item`/`show_entity` carry structured item/entity data this
    /// module does not model; use `raw` for those.
    pub text: Option<Box<TextComponent>>,
    pub raw: RawEvent,
}

/// The hover event's un-interpreted payload, in whichever encoding it was
/// parsed from — kept so a caller needing `show_item`/`show_entity` detail
/// this module doesn't model can still get at it.
#[derive(Debug, Clone, PartialEq)]
pub enum RawEvent {
    Nbt(Nbt),
    Json(serde_json::Value),
}

impl TextComponent {
    /// A plain literal component with default style and no siblings.
    pub fn literal(text: impl Into<String>) -> Self {
        TextComponent {
            content: Content::Literal(text.into()),
            style: Style::default(),
            extra: Vec::new(),
        }
    }

    /// Parses a network NBT text component — the encoding every 1.21.4
    /// play-state text field uses.
    pub fn from_nbt(nbt: &Nbt) -> TextComponent {
        let mut budget = Budget::default();
        nbt::parse(nbt, &mut budget, 0)
    }

    /// Parses a JSON-encoded text component (the login-state Disconnect
    /// `reason` field). Malformed JSON degrades to a literal component
    /// containing the raw string, rather than an error.
    pub fn from_json_str(json: &str) -> TextComponent {
        match serde_json::from_str::<serde_json::Value>(json) {
            Ok(value) => {
                let mut budget = Budget::default();
                json::parse(&value, &mut budget, 0)
            }
            Err(_) => TextComponent::literal(json),
        }
    }

    /// Flattened, unstyled text: this component's literal/translated text
    /// followed by every sibling's, in order.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        self.render_plain(&mut out);
        out
    }

    fn render_plain(&self, out: &mut String) {
        match &self.content {
            Content::Literal(text) => out.push_str(text),
            Content::Translate { key, args } => render_translation(key, args, out),
            Content::Unknown { .. } => {}
        }
        for child in &self.extra {
            child.render_plain(out);
        }
    }
}

impl std::fmt::Display for TextComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.plain_text())
    }
}

/// Tracks how much of the parse budget has been spent, so a hostile/huge
/// component tree degrades gracefully instead of consuming unbounded
/// time/memory. Depth is checked separately (a simple counter passed down
/// the recursion), since a long linear chain of single-child components
/// can blow the stack without ever using many "nodes".
#[derive(Default)]
struct Budget {
    nodes: usize,
    text_bytes: usize,
}

impl Budget {
    /// Whether the node budget is used up — checked *before* even
    /// attempting to parse another sibling/argument, so an oversized list
    /// is truncated in length, not just left with empty-content entries
    /// past the limit.
    fn is_exhausted(&self) -> bool {
        self.nodes >= MAX_NODES
    }

    /// Reserves one node; `false` once the node budget is exhausted (the
    /// caller must treat this as "stop, there is nothing more to parse").
    fn take_node(&mut self) -> bool {
        if self.is_exhausted() {
            return false;
        }
        self.nodes += 1;
        true
    }

    /// Reserves `len` bytes of literal text; `false` (the caller must not
    /// append anything) once the text budget is exhausted.
    fn take_text(&mut self, len: usize) -> bool {
        if self.text_bytes.saturating_add(len) > MAX_TEXT_BYTES {
            return false;
        }
        self.text_bytes += len;
        true
    }
}

fn literal_bounded(text: &str, budget: &mut Budget) -> TextComponent {
    if budget.take_text(text.len()) {
        TextComponent::literal(text)
    } else {
        TextComponent::literal("")
    }
}

/// Network-NBT parsing.
mod nbt {
    use super::*;

    pub(super) fn parse(value: &Nbt, budget: &mut Budget, depth: usize) -> TextComponent {
        if depth > MAX_DEPTH || !budget.take_node() {
            return TextComponent::literal("");
        }
        match value {
            Nbt::String(text) => literal_bounded(text, budget),
            // A bare list is a component array: the first element is the
            // base, the rest behave like `extra` — some servers send chat
            // this way.
            Nbt::List(list) => {
                let mut items = list.items.iter();
                let Some(first) = items.next() else {
                    return TextComponent::literal("");
                };
                let mut component = parse(first, budget, depth + 1);
                for item in items {
                    component.extra.push(parse(item, budget, depth + 1));
                }
                component
            }
            Nbt::Compound(_) => parse_compound(value, budget, depth),
            // A bare number/byte is not a valid component root in
            // practice; "nothing to show" rather than a guess.
            _ => TextComponent::literal(""),
        }
    }

    fn parse_compound(value: &Nbt, budget: &mut Budget, depth: usize) -> TextComponent {
        let mut component = if let Some(Nbt::String(text)) = value.get("text") {
            literal_bounded(text, budget)
        } else if let Some(Nbt::String(key)) = value.get("translate") {
            let args = list_field(value, "with")
                .map(|items| parse_list(items, budget, depth))
                .unwrap_or_default();
            TextComponent {
                content: Content::Translate {
                    key: key.clone(),
                    args,
                },
                style: Style::default(),
                extra: Vec::new(),
            }
        } else {
            TextComponent {
                content: Content::Unknown { raw: value.clone() },
                style: Style::default(),
                extra: Vec::new(),
            }
        };

        component.style = parse_style(value, budget);
        if let Some(items) = list_field(value, "extra") {
            component.extra = parse_list(items, budget, depth);
        }
        component
    }

    fn list_field<'a>(value: &'a Nbt, name: &str) -> Option<&'a [Nbt]> {
        match value.get(name) {
            Some(Nbt::List(list)) => Some(&list.items),
            _ => None,
        }
    }

    fn parse_list(items: &[Nbt], budget: &mut Budget, depth: usize) -> Vec<TextComponent> {
        let mut out = Vec::new();
        for item in items {
            if budget.is_exhausted() {
                break;
            }
            out.push(parse(item, budget, depth + 1));
        }
        out
    }

    fn string_field(value: &Nbt, key: &str) -> Option<String> {
        match value.get(key) {
            Some(Nbt::String(s)) => Some(s.clone()),
            _ => None,
        }
    }

    fn bool_field(value: &Nbt, key: &str) -> Option<bool> {
        match value.get(key) {
            Some(Nbt::Byte(b)) => Some(*b != 0),
            _ => None,
        }
    }

    /// Click/hover event keys are checked under both the historical
    /// camelCase spelling (`clickEvent`/`hoverEvent`) and snake_case: the
    /// exact NBT key casing for 1.21.4 has not been independently verified
    /// against a live capture (see `docs/progress.md`), so this parses
    /// defensively rather than assuming — an absent/wrongly-spelled event
    /// is simply not populated, never a parse failure.
    fn event_field<'a>(value: &'a Nbt, camel: &str, snake: &str) -> Option<&'a Nbt> {
        value.get(camel).or_else(|| value.get(snake))
    }

    fn parse_style(value: &Nbt, budget: &mut Budget) -> Style {
        Style {
            color: string_field(value, "color").map(|c| parse_color(&c)),
            bold: bool_field(value, "bold"),
            italic: bool_field(value, "italic"),
            underlined: bool_field(value, "underlined"),
            strikethrough: bool_field(value, "strikethrough"),
            obfuscated: bool_field(value, "obfuscated"),
            insertion: string_field(value, "insertion"),
            font: string_field(value, "font"),
            click_event: parse_click_event(value),
            hover_event: parse_hover_event(value, budget),
        }
    }

    fn parse_click_event(value: &Nbt) -> Option<ClickEvent> {
        let event = event_field(value, "clickEvent", "click_event")?;
        let action = string_field(event, "action")?;
        let click_value = string_field(event, "value").unwrap_or_default();
        Some(ClickEvent {
            action,
            value: click_value,
        })
    }

    fn parse_hover_event(value: &Nbt, budget: &mut Budget) -> Option<HoverEvent> {
        let event = event_field(value, "hoverEvent", "hover_event")?;
        let action = string_field(event, "action")?;
        let contents = event_field(event, "contents", "value");
        let text = if action == "show_text" {
            contents.map(|c| Box::new(parse(c, budget, 0)))
        } else {
            None
        };
        Some(HoverEvent {
            action,
            text,
            raw: RawEvent::Nbt(event.clone()),
        })
    }
}

/// JSON parsing. Field names are the same as network NBT (network NBT text
/// components are a direct re-encoding of the JSON schema), so the shape
/// mirrors the `nbt` module exactly — just walking `serde_json::Value`
/// instead of `Nbt`.
mod json {
    use super::*;
    use serde_json::Value;

    pub(super) fn parse(value: &Value, budget: &mut Budget, depth: usize) -> TextComponent {
        if depth > MAX_DEPTH || !budget.take_node() {
            return TextComponent::literal("");
        }
        match value {
            Value::String(text) => literal_bounded(text, budget),
            Value::Array(items) => {
                let mut items = items.iter();
                let Some(first) = items.next() else {
                    return TextComponent::literal("");
                };
                let mut component = parse(first, budget, depth + 1);
                for item in items {
                    component.extra.push(parse(item, budget, depth + 1));
                }
                component
            }
            Value::Object(_) => parse_object(value, budget, depth),
            _ => TextComponent::literal(""),
        }
    }

    fn parse_object(value: &Value, budget: &mut Budget, depth: usize) -> TextComponent {
        let mut component = if let Some(Value::String(text)) = value.get("text") {
            literal_bounded(text, budget)
        } else if let Some(Value::String(key)) = value.get("translate") {
            let args = list_field(value, "with")
                .map(|items| parse_list(items, budget, depth))
                .unwrap_or_default();
            TextComponent {
                content: Content::Translate {
                    key: key.clone(),
                    args,
                },
                style: Style::default(),
                extra: Vec::new(),
            }
        } else {
            TextComponent {
                content: Content::Unknown {
                    raw: json_to_nbt_lossy(value),
                },
                style: Style::default(),
                extra: Vec::new(),
            }
        };

        component.style = parse_style(value, budget);
        if let Some(items) = list_field(value, "extra") {
            component.extra = parse_list(items, budget, depth);
        }
        component
    }

    fn list_field<'a>(value: &'a Value, name: &str) -> Option<&'a Vec<Value>> {
        value.get(name).and_then(Value::as_array)
    }

    fn parse_list(items: &[Value], budget: &mut Budget, depth: usize) -> Vec<TextComponent> {
        let mut out = Vec::new();
        for item in items {
            if budget.is_exhausted() {
                break;
            }
            out.push(parse(item, budget, depth + 1));
        }
        out
    }

    fn string_field(value: &Value, key: &str) -> Option<String> {
        value.get(key).and_then(Value::as_str).map(str::to_string)
    }

    fn bool_field(value: &Value, key: &str) -> Option<bool> {
        value.get(key).and_then(Value::as_bool)
    }

    fn event_field<'a>(value: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
        value.get(camel).or_else(|| value.get(snake))
    }

    fn parse_style(value: &Value, budget: &mut Budget) -> Style {
        Style {
            color: string_field(value, "color").map(|c| parse_color(&c)),
            bold: bool_field(value, "bold"),
            italic: bool_field(value, "italic"),
            underlined: bool_field(value, "underlined"),
            strikethrough: bool_field(value, "strikethrough"),
            obfuscated: bool_field(value, "obfuscated"),
            insertion: string_field(value, "insertion"),
            font: string_field(value, "font"),
            click_event: parse_click_event(value),
            hover_event: parse_hover_event(value, budget),
        }
    }

    fn parse_click_event(value: &Value) -> Option<ClickEvent> {
        let event = event_field(value, "clickEvent", "click_event")?;
        let action = string_field(event, "action")?;
        let click_value = string_field(event, "value").unwrap_or_default();
        Some(ClickEvent {
            action,
            value: click_value,
        })
    }

    fn parse_hover_event(value: &Value, budget: &mut Budget) -> Option<HoverEvent> {
        let event = event_field(value, "hoverEvent", "hover_event")?;
        let action = string_field(event, "action")?;
        let contents = event_field(event, "contents", "value");
        let text = if action == "show_text" {
            contents.map(|c| Box::new(parse(c, budget, 0)))
        } else {
            None
        };
        Some(HoverEvent {
            action,
            text,
            raw: RawEvent::Json(event.clone()),
        })
    }

    /// A lossy, best-effort `Value` -> `Nbt` conversion so an unrecognized
    /// JSON component's raw data can be preserved in the same [`Content::Unknown`]
    /// shape the NBT parser uses, without adding a second "unknown content"
    /// representation. Numbers that don't fit the target type are clamped
    /// rather than causing a parse failure — fine for diagnostics, which is
    /// the only thing this path is for.
    fn json_to_nbt_lossy(value: &Value) -> Nbt {
        match value {
            Value::Null => Nbt::Compound(Vec::new()),
            Value::Bool(b) => Nbt::Byte(*b as i8),
            Value::Number(n) => n
                .as_i64()
                .map(Nbt::Long)
                .or_else(|| n.as_f64().map(Nbt::Double))
                .unwrap_or(Nbt::Long(0)),
            Value::String(s) => Nbt::String(s.clone()),
            Value::Array(items) => {
                let items: Vec<Nbt> = items.iter().map(json_to_nbt_lossy).collect();
                let tag = items.first().map(Nbt::tag_id).unwrap_or(8);
                Nbt::List(minerider_protocol::nbt::NbtList { tag, items })
            }
            Value::Object(map) => Nbt::Compound(
                map.iter()
                    .map(|(k, v)| (k.clone(), json_to_nbt_lossy(v)))
                    .collect(),
            ),
        }
    }
}

fn parse_color(raw: &str) -> Color {
    match raw.strip_prefix('#') {
        Some(hex) => Color::Hex(hex.to_string()),
        None => Color::Named(raw.to_string()),
    }
}

/// Renders a `translate` component's plain text: substitutes `%s`/`%N$s`
/// placeholders in a small built-in table of the keys a bot most often
/// sees, or falls back to showing the key and its rendered arguments for
/// anything not in that table — this is not a bundled vanilla lang file.
fn render_translation(key: &str, args: &[TextComponent], out: &mut String) {
    let rendered_args: Vec<String> = args.iter().map(TextComponent::plain_text).collect();
    if let Some(template) = translation_template(key) {
        substitute_placeholders(template, &rendered_args, out);
    } else if rendered_args.is_empty() {
        out.push_str(key);
    } else {
        out.push_str(key);
        out.push('{');
        out.push_str(&rendered_args.join(", "));
        out.push('}');
    }
}

/// A small table of the translation keys a bot most often sees in chat/
/// system messages. Not the full vanilla lang file — unknown keys fall
/// back to showing the key and its args (see [`render_translation`]).
fn translation_template(key: &str) -> Option<&'static str> {
    Some(match key {
        "chat.type.text" => "<%s> %s",
        "chat.type.announcement" => "[%s] %s",
        "chat.type.emote" => "* %s %s",
        "chat.type.team.text" => "%s <%s> %s",
        "multiplayer.player.joined" => "%s joined the game",
        "multiplayer.player.joined.renamed" => "%s (formerly known as %s) joined the game",
        "multiplayer.player.left" => "%s left the game",
        "commands.message.display.incoming" => "%s whispers to you: %s",
        "commands.message.display.outgoing" => "You whisper to %s: %s",
        _ => return None,
    })
}

/// Substitutes `%s` (sequential) and `%N$s` (indexed) placeholders in a
/// vanilla translation template with the supplied, already-rendered
/// arguments.
fn substitute_placeholders(template: &str, args: &[String], out: &mut String) {
    let mut chars = template.chars().peekable();
    let mut next_seq = 0usize;
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('%') => {
                chars.next();
                out.push('%');
            }
            Some('s') => {
                chars.next();
                if let Some(arg) = args.get(next_seq) {
                    out.push_str(arg);
                }
                next_seq += 1;
            }
            Some(d) if d.is_ascii_digit() => {
                let mut index = 0usize;
                while let Some(d) = chars.peek().filter(|c| c.is_ascii_digit()) {
                    index = index * 10 + (*d as usize - '0' as usize);
                    chars.next();
                }
                if chars.peek() == Some(&'$') {
                    chars.next();
                    if chars.peek() == Some(&'s') {
                        chars.next();
                    }
                }
                if let Some(arg) = index.checked_sub(1).and_then(|i| args.get(i)) {
                    out.push_str(arg);
                }
            }
            _ => out.push('%'),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minerider_protocol::nbt::NbtList;

    fn nbt_text(s: &str) -> Nbt {
        Nbt::Compound(vec![("text".into(), Nbt::String(s.into()))])
    }

    fn nbt_list(items: Vec<Nbt>) -> Nbt {
        Nbt::List(NbtList { tag: 10, items })
    }

    #[test]
    fn literal_component() {
        let c = TextComponent::from_nbt(&nbt_text("hello"));
        assert_eq!(c.content, Content::Literal("hello".to_string()));
        assert_eq!(c.plain_text(), "hello");

        // A bare string is also a valid literal component root.
        let bare = TextComponent::from_nbt(&Nbt::String("bare".into()));
        assert_eq!(bare.plain_text(), "bare");
    }

    #[test]
    fn styled_component_preserves_every_field() {
        let component = Nbt::Compound(vec![
            ("text".into(), Nbt::String("warn".into())),
            ("color".into(), Nbt::String("red".into())),
            ("bold".into(), Nbt::Byte(1)),
            ("italic".into(), Nbt::Byte(0)),
            ("obfuscated".into(), Nbt::Byte(1)),
            ("insertion".into(), Nbt::String("click-fill".into())),
            (
                "clickEvent".into(),
                Nbt::Compound(vec![
                    ("action".into(), Nbt::String("run_command".into())),
                    ("value".into(), Nbt::String("/help".into())),
                ]),
            ),
            (
                "hoverEvent".into(),
                Nbt::Compound(vec![
                    ("action".into(), Nbt::String("show_text".into())),
                    ("contents".into(), nbt_text("more info")),
                ]),
            ),
        ]);
        let c = TextComponent::from_nbt(&component);
        assert_eq!(c.style.color, Some(Color::Named("red".to_string())));
        assert_eq!(c.style.bold, Some(true));
        assert_eq!(c.style.italic, Some(false));
        assert_eq!(c.style.obfuscated, Some(true));
        assert_eq!(c.style.insertion.as_deref(), Some("click-fill"));
        let click = c.style.click_event.expect("click event");
        assert_eq!(click.action, "run_command");
        assert_eq!(click.value, "/help");
        let hover = c.style.hover_event.expect("hover event");
        assert_eq!(hover.action, "show_text");
        assert_eq!(hover.text.expect("hover text").plain_text(), "more info");
    }

    #[test]
    fn hex_color_is_distinguished_from_named() {
        let component = Nbt::Compound(vec![
            ("text".into(), Nbt::String("x".into())),
            ("color".into(), Nbt::String("#FF00FF".into())),
        ]);
        let c = TextComponent::from_nbt(&component);
        assert_eq!(c.style.color, Some(Color::Hex("FF00FF".to_string())));
    }

    #[test]
    fn nested_extra_children_render_in_order() {
        let component = Nbt::Compound(vec![
            ("text".into(), Nbt::String("a".into())),
            (
                "extra".into(),
                nbt_list(vec![nbt_text("b"), Nbt::String("c".into())]),
            ),
        ]);
        let c = TextComponent::from_nbt(&component);
        assert_eq!(c.plain_text(), "abc");
        assert_eq!(c.extra.len(), 2);
    }

    #[test]
    fn bare_list_root_is_a_component_array() {
        let root = nbt_list(vec![nbt_text("a"), nbt_text("b")]);
        assert_eq!(TextComponent::from_nbt(&root).plain_text(), "ab");
    }

    #[test]
    fn translated_component_with_structured_args() {
        let component = Nbt::Compound(vec![
            ("translate".into(), Nbt::String("chat.type.text".into())),
            (
                "with".into(),
                nbt_list(vec![nbt_text("Notch"), nbt_text("hi")]),
            ),
        ]);
        let c = TextComponent::from_nbt(&component);
        assert!(matches!(&c.content, Content::Translate { key, .. } if key == "chat.type.text"));
        assert_eq!(c.plain_text(), "<Notch> hi");
    }

    #[test]
    fn unknown_translation_key_falls_back_to_key_and_args() {
        let component = Nbt::Compound(vec![
            ("translate".into(), Nbt::String("some.unknown.key".into())),
            ("with".into(), nbt_list(vec![nbt_text("x")])),
        ]);
        assert_eq!(
            TextComponent::from_nbt(&component).plain_text(),
            "some.unknown.key{x}"
        );
    }

    #[test]
    fn unknown_content_kind_is_preserved_not_dropped() {
        // A score component: neither `text` nor `translate`.
        let component = Nbt::Compound(vec![(
            "score".into(),
            Nbt::Compound(vec![
                ("name".into(), Nbt::String("Notch".into())),
                ("objective".into(), Nbt::String("kills".into())),
            ]),
        )]);
        let c = TextComponent::from_nbt(&component);
        assert_eq!(c.plain_text(), "", "nothing sensible to show as plain text");
        match c.content {
            Content::Unknown { raw } => assert!(raw.get("score").is_some()),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn malformed_extra_type_is_ignored_not_a_panic() {
        // `extra` is a Compound instead of a List — must be ignored, not panic.
        let component = Nbt::Compound(vec![
            ("text".into(), Nbt::String("a".into())),
            ("extra".into(), Nbt::Compound(vec![])),
        ]);
        let c = TextComponent::from_nbt(&component);
        assert_eq!(c.plain_text(), "a");
        assert!(c.extra.is_empty());
    }

    #[test]
    fn deeply_nested_extra_does_not_overflow_the_stack() {
        let mut root = nbt_text("bottom");
        for i in 0..(MAX_DEPTH * 4) {
            root = Nbt::Compound(vec![
                ("text".into(), Nbt::String(format!("{i}."))),
                ("extra".into(), nbt_list(vec![root])),
            ]);
        }
        // Must return without panicking; depth past the limit degrades to
        // an empty component rather than being fully rendered.
        let c = TextComponent::from_nbt(&root);
        let _ = c.plain_text();
    }

    #[test]
    fn oversized_sibling_list_is_truncated_by_the_node_budget() {
        let items: Vec<Nbt> = (0..(MAX_NODES * 2))
            .map(|i| nbt_text(&i.to_string()))
            .collect();
        let root = Nbt::Compound(vec![
            ("text".into(), Nbt::String("root".into())),
            ("extra".into(), nbt_list(items)),
        ]);
        let c = TextComponent::from_nbt(&root);
        // Bounded: nowhere near 2*MAX_NODES siblings actually got parsed.
        assert!(c.extra.len() < MAX_NODES);
    }

    #[test]
    fn oversized_literal_text_is_truncated_by_the_text_budget() {
        let huge = "x".repeat(MAX_TEXT_BYTES * 2);
        let c = TextComponent::from_nbt(&nbt_text(&huge));
        assert!(c.plain_text().len() <= MAX_TEXT_BYTES);
    }

    #[test]
    fn json_literal_and_styled_component() {
        let c = TextComponent::from_json_str(r#"{"text":"hi","bold":true,"color":"gold"}"#);
        assert_eq!(c.content, Content::Literal("hi".to_string()));
        assert_eq!(c.style.bold, Some(true));
        assert_eq!(c.style.color, Some(Color::Named("gold".to_string())));
    }

    #[test]
    fn json_nested_and_translated() {
        let c = TextComponent::from_json_str(
            r#"{"translate":"multiplayer.player.joined","with":[{"text":"Steve"}]}"#,
        );
        assert_eq!(c.plain_text(), "Steve joined the game");
    }

    #[test]
    fn malformed_json_degrades_to_literal_raw_string() {
        let c = TextComponent::from_json_str("{not valid json");
        assert_eq!(c.plain_text(), "{not valid json");
    }

    #[test]
    fn json_array_root_is_a_component_array() {
        let c = TextComponent::from_json_str(r#"[{"text":"a"},"b"]"#);
        assert_eq!(c.plain_text(), "ab");
    }

    #[test]
    fn display_matches_plain_text() {
        let c = TextComponent::literal("hi there");
        assert_eq!(c.to_string(), c.plain_text());
    }

    #[test]
    fn substitutes_indexed_placeholders_directly() {
        // Vanilla death messages use indexed args like
        // "%1$s was slain by %2$s"; not in the built-in template table, but
        // the substitution primitive itself is tested directly here.
        let mut out = String::new();
        substitute_placeholders(
            "%1$s was slain by %2$s",
            &["Steve".to_string(), "Zombie".to_string()],
            &mut out,
        );
        assert_eq!(out, "Steve was slain by Zombie");
    }
}
