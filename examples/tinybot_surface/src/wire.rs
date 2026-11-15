//! The surface protocol's data: what an agent may ask for (one JSON object per line) and the
//! lines the app answers with. A `set` is checked completely here, before anything on screen
//! changes: a description that fails leaves the last good surface alone. An `update` is
//! checked here for its shape and by the socket thread against the surface on screen.

use makepad_widgets::makepad_platform::makepad_micro_serde::{DeJson, JsonValue};
use std::collections::HashSet;

/// Protocol version, in `hello`.
pub const VERSION: u64 = 2;
/// The longest request line. A longer one is answered with `too_long` and the connection is
/// closed, since the framing is lost.
pub const MAX_LINE: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style {
    Primary,
    Secondary,
    Danger,
}

#[derive(Clone, Debug)]
pub struct Button {
    pub id: String,
    pub text: String,
    pub style: Style,
    /// the first tap only arms the button; a second one within `CONFIRM_SECONDS` is the tap
    pub confirm: bool,
    /// `false`: a tap does not lock the row, and the button stays live in a locked row
    pub lock: bool,
    /// drawn faded but live: there is a better button for this just now
    pub dim: bool,
}

/// How long a `confirm` button stays armed.
pub const CONFIRM_SECONDS: f64 = 3.0;

/// The items. An `id` that is optional and was left out is empty. For the three kinds the
/// user sets (toggle, stepper, number), `value: None` keeps what the user set when the
/// surface id is unchanged.
#[derive(Clone, Debug)]
pub enum Item {
    Heading { id: String, text: String },
    Text { id: String, text: String },
    Toggle { id: String, text: String, value: Option<bool> },
    /// one of `values`, the device's own steps as strings; `value` is a position in them
    Stepper { id: String, text: String, values: Vec<String>, value: Option<usize> },
    /// a text field; what was typed is sent as it is
    Number { id: String, text: String, value: Option<String> },
    /// a label and a value the agent changes with `update`
    Readout { id: String, text: String, value: String },
    Row { id: String, buttons: Vec<Button> },
    /// a full-width row to pick: a status dot, a name and a line under it; a tap on it is a
    /// tap with its id. It does not lock: the agent says with an `update` what the pick did.
    /// `selected`: this is the row that is chosen, drawn as the primary button is
    Pick { id: String, text: String, note: String, dot: Option<Dot>, selected: bool },
    Divider,
}

/// The status dot of a `pick`, drawn by the app as a shape. Without one the row has no dot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dot {
    Green,
    Amber,
    Red,
    /// grey: known and not there
    Off,
}

/// What a tap says of an item the user sets.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Text(String),
}

/// What an `update` may name: an item it can change, or a row to arm again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// heading or text: `text`
    Label,
    /// readout: `text` and `value`
    Readout,
    /// a row's own id, or the id of one of its buttons
    Row,
    /// toggle, stepper, number: the user's, not for `update`
    Input,
    /// pick: `text`, `note`, `dot` and `selected`
    Pick,
}

impl Spec {
    /// Every id of the surface and what an `update` may do with it.
    pub fn targets(&self) -> Vec<(String, Target)> {
        let mut out = Vec::new();
        for item in &self.items {
            match item {
                Item::Heading { id, .. } | Item::Text { id, .. } => out.push((id.clone(), Target::Label)),
                Item::Readout { id, .. } => out.push((id.clone(), Target::Readout)),
                Item::Pick { id, .. } => out.push((id.clone(), Target::Pick)),
                Item::Toggle { id, .. } | Item::Stepper { id, .. } | Item::Number { id, .. } => out.push((id.clone(), Target::Input)),
                Item::Row { id, buttons } => {
                    out.push((id.clone(), Target::Row));
                    out.extend(buttons.iter().map(|b| (b.id.clone(), Target::Row)));
                }
                Item::Divider => {}
            }
        }
        out.retain(|(id, _)| !id.is_empty());
        out
    }
}

/// One item's part of an `update`.
#[derive(Clone, Debug)]
pub struct Change {
    pub id: String,
    pub text: Option<String>,
    pub value: Option<String>,
    /// a pick's second line
    pub note: Option<String>,
    /// a pick's dot
    pub dot: Option<Dot>,
    /// whether a pick is the chosen row
    pub selected: Option<bool>,
}

/// A checked `update`: changes to items in place and rows to arm again. Whether the ids
/// exist is checked against the surface on screen, by the socket thread.
#[derive(Clone, Debug)]
pub struct Update {
    pub surface: String,
    pub rev: u64,
    /// a new header line
    pub title: Option<String>,
    pub changes: Vec<Change>,
    pub arm: Vec<String>,
}

/// A checked surface description.
#[derive(Clone, Debug)]
pub struct Spec {
    pub surface: String,
    pub rev: u64,
    pub title: String,
    pub items: Vec<Item>,
}

pub enum Request {
    Set(Spec),
    Update(Update),
    Clear,
    Status,
    /// `layers`: a waiting request is also answered when the layers change
    Taps { wait: bool, layers: bool },
    Done { seq: u64 },
    /// root only: which layer is on screen
    Show(Layer),
    /// a root client that is the panel runner says so, and gets root's layer
    Runner,
    /// a root client that is the voice service says so, and gets the voice strip
    Voice,
    /// the voice client's text for the strip; `None` takes the strip away
    Say(Option<String>),
}

/// The most of a voice text that is kept, in bytes: its end, which is what the strip shows.
pub const VOICE_BYTES: usize = 600;

/// A voice text as the strip takes it: line ends and tabs are spaces, other control
/// characters are left out, and of a long text the last `VOICE_BYTES` bytes are kept (from a
/// character's start). `None` when nothing is left to show.
pub fn voice_text(raw: &str) -> Option<String> {
    let clean: String = raw
        .chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    let clean = clean.trim();
    let mut from = clean.len().saturating_sub(VOICE_BYTES);
    while !clean.is_char_boundary(from) {
        from += 1;
    }
    let kept = clean[from..].trim_start();
    (!kept.is_empty()).then(|| kept.to_string())
}

/// The app holds two surfaces, and one of them is on screen: root's (set by a uid-0 peer,
/// the panel runner) and the agent's (set by a peer of the app's own user).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layer {
    Root,
    Agent,
}

impl Layer {
    pub fn name(self) -> &'static str {
        match self {
            Layer::Root => "root",
            Layer::Agent => "agent",
        }
    }
}

/// A refused request: `{"type":"err","code":CODE,"msg":MSG}`.
pub struct WireErr {
    pub code: &'static str,
    pub msg: String,
}

fn err<T>(code: &'static str, msg: impl Into<String>) -> Result<T, WireErr> {
    Err(WireErr { code, msg: msg.into() })
}

/// A press of a button, with the toggles, steppers and numbers as they were at that moment
/// (in item order).
#[derive(Clone, Debug)]
pub struct Tap {
    pub seq: u64,
    pub surface: String,
    pub rev: u64,
    pub id: String,
    pub values: Vec<(String, Value)>,
    /// the tap was made on root's layer: only root's peers are given it
    pub privileged: bool,
}

pub fn parse_request(line: &str) -> Result<Request, WireErr> {
    let value = match JsonValue::deserialize_json(line) {
        Ok(v) => v,
        Err(e) => return err("bad_json", format!("{e:?}")),
    };
    if value.as_object().is_none() {
        return err("bad_json", "a request is one JSON object");
    }
    match value.key("type").and_then(|t| t.as_str()) {
        Some("set") => parse_set(&value).map(Request::Set),
        Some("update") => parse_update(&value).map(Request::Update),
        Some("clear") => Ok(Request::Clear),
        Some("status") => Ok(Request::Status),
        Some("taps") => {
            let flag = |name: &str| match value.key(name) {
                None => Ok(false),
                Some(flag) => flag.as_bool().ok_or(WireErr { code: "bad_request", msg: format!("taps: {name} is true or false") }),
            };
            Ok(Request::Taps { wait: flag("wait")?, layers: flag("layers")? })
        }
        Some("hello") => match value.key("role").and_then(|r| r.as_str()) {
            Some("runner") => Ok(Request::Runner),
            Some("voice") => Ok(Request::Voice),
            _ => err("bad_request", "hello: the roles a client can ask for are \"runner\" and \"voice\""),
        },
        Some("voice") => match value.key("text") {
            None => Ok(Request::Say(None)),
            Some(text) => match text.as_str() {
                Some(text) => Ok(Request::Say(voice_text(text))),
                None => err("bad_request", "voice: text is a string; without it the strip goes away"),
            },
        },
        Some("show") => match value.key("layer").and_then(|l| l.as_str()) {
            Some("root") => Ok(Request::Show(Layer::Root)),
            Some("agent") => Ok(Request::Show(Layer::Agent)),
            _ => err("bad_request", "show: layer is \"root\" or \"agent\""),
        },
        Some("done") => match value.key("seq").and_then(|s| s.as_u64()) {
            Some(seq) => Ok(Request::Done { seq }),
            None => err("bad_request", "done: seq is the seq of the last tap received"),
        },
        Some(other) => err("bad_type", format!("unknown type: {other}")),
        None => err("bad_type", "missing type"),
    }
}

fn text_of(item: &JsonValue, n: usize, kind: &str) -> Result<String, WireErr> {
    match item.key("text").and_then(|t| t.as_str()) {
        Some(t) => Ok(t.to_string()),
        None => err("bad_item", format!("item {n} ({kind}): text is missing or not a string")),
    }
}

/// An item's or button's id: unique over the whole surface when given.
fn id_of(v: &JsonValue, what: &str, required: bool, seen: &mut HashSet<String>) -> Result<String, WireErr> {
    match v.key("id") {
        None if !required => Ok(String::new()),
        None => err("bad_item", format!("{what}: id is missing")),
        Some(id) => match id.as_str() {
            Some(id) if !id.is_empty() => {
                if !seen.insert(id.to_string()) {
                    return err("bad_item", format!("duplicate id: {id}"));
                }
                Ok(id.to_string())
            }
            _ => err("bad_item", format!("{what}: id is not a non-empty string")),
        },
    }
}

fn parse_button(v: &JsonValue, what: &str, seen: &mut HashSet<String>) -> Result<Button, WireErr> {
    if v.as_object().is_none() {
        return err("bad_item", format!("{what}: not an object"));
    }
    let id = id_of(v, what, true, seen)?;
    let text = match v.key("text").and_then(|t| t.as_str()) {
        Some(t) => t.to_string(),
        None => return err("bad_item", format!("{what}: text is missing or not a string")),
    };
    // an unknown style is drawn as the plain button rather than refused
    let style = match v.key("style").and_then(|s| s.as_str()) {
        Some("primary") => Style::Primary,
        Some("danger") => Style::Danger,
        _ => Style::Secondary,
    };
    Ok(Button {
        id,
        text,
        style,
        confirm: flag_of(v, "confirm", false, what)?,
        lock: flag_of(v, "lock", true, what)?,
        dim: flag_of(v, "dim", false, what)?,
    })
}

/// An optional true/false field.
fn flag_of(v: &JsonValue, name: &str, default: bool, what: &str) -> Result<bool, WireErr> {
    match v.key(name) {
        None => Ok(default),
        Some(flag) => match flag.as_bool() {
            Some(flag) => Ok(flag),
            None => err("bad_item", format!("{what}: {name} is not true or false")),
        },
    }
}

/// The optional `dot` of a pick: one of four words.
fn dot_of(v: &JsonValue, what: &str) -> Result<Option<Dot>, WireErr> {
    match string_of(v, "dot", what)?.as_deref() {
        None => Ok(None),
        Some("green") => Ok(Some(Dot::Green)),
        Some("amber") => Ok(Some(Dot::Amber)),
        Some("red") => Ok(Some(Dot::Red)),
        Some("off") => Ok(Some(Dot::Off)),
        Some(other) => err("bad_item", format!("{what}: dot is \"green\", \"amber\", \"red\" or \"off\", not {other:?}")),
    }
}

/// An optional string field.
fn string_of(v: &JsonValue, name: &str, what: &str) -> Result<Option<String>, WireErr> {
    match v.key(name) {
        None => Ok(None),
        Some(s) => match s.as_str() {
            Some(s) => Ok(Some(s.to_string())),
            None => err("bad_item", format!("{what}: {name} is not a string")),
        },
    }
}

fn surface_and_rev(value: &JsonValue) -> Result<(String, u64), WireErr> {
    let surface = match value.key("surface").and_then(|s| s.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return err("bad_set", "surface is missing or not a non-empty string"),
    };
    let rev = match value.key("rev").and_then(|r| r.as_u64()) {
        Some(r) if r >= 1 => r,
        _ => return err("bad_set", "rev is missing or not an integer >= 1"),
    };
    Ok((surface, rev))
}

fn parse_update(value: &JsonValue) -> Result<Update, WireErr> {
    let (surface, rev) = surface_and_rev(value)?;
    let title = match value.key("title") {
        None => None,
        Some(title) => match title.as_str() {
            Some(title) => Some(title.to_string()),
            None => return err("bad_request", "update: title is not a string"),
        },
    };
    let mut changes = Vec::new();
    if let Some(items) = value.key("items") {
        let Some(items) = items.as_object() else {
            return err("bad_request", "update: items is an object of id: {text, value}");
        };
        for (id, change) in items {
            if change.as_object().is_none() {
                return err("bad_request", format!("update: {id} is not an object"));
            }
            let what = format!("update {id}");
            changes.push(Change {
                id: id.clone(),
                text: string_of(change, "text", &what)?,
                value: string_of(change, "value", &what)?,
                note: string_of(change, "note", &what)?,
                dot: dot_of(change, &what)?,
                selected: match change.key("selected") {
                    None => None,
                    Some(_) => Some(flag_of(change, "selected", false, &what)?),
                },
            });
        }
    }
    let mut arm = Vec::new();
    if let Some(list) = value.key("arm") {
        let Some(list) = list.as_array() else {
            return err("bad_request", "update: arm is an array of row or button ids");
        };
        for id in list {
            match id.as_str() {
                Some(id) => arm.push(id.to_string()),
                None => return err("bad_request", "update: arm is an array of row or button ids"),
            }
        }
    }
    Ok(Update { surface, rev, title, changes, arm })
}

fn parse_set(value: &JsonValue) -> Result<Spec, WireErr> {
    let (surface, rev) = surface_and_rev(value)?;
    let title = match value.key("title") {
        None => String::new(),
        Some(t) => match t.as_str() {
            Some(t) => t.to_string(),
            None => return err("bad_set", "title is not a string"),
        },
    };
    let Some(list) = value.key("items").and_then(|i| i.as_array()) else {
        return err("bad_set", "items is missing or not an array");
    };
    let mut seen = HashSet::new();
    let mut items = Vec::with_capacity(list.len());
    for (n, item) in list.iter().enumerate() {
        if item.as_object().is_none() {
            return err("bad_item", format!("item {n}: not an object"));
        }
        let Some(kind) = item.key("kind").and_then(|k| k.as_str()) else {
            return err("bad_item", format!("item {n}: kind is missing"));
        };
        let what = format!("item {n} ({kind})");
        items.push(match kind {
            "heading" => Item::Heading { id: id_of(item, &what, false, &mut seen)?, text: text_of(item, n, kind)? },
            "text" => Item::Text { id: id_of(item, &what, false, &mut seen)?, text: text_of(item, n, kind)? },
            "divider" => {
                id_of(item, &what, false, &mut seen)?;
                Item::Divider
            }
            "toggle" => {
                let id = id_of(item, &what, true, &mut seen)?;
                let value = match item.key("value") {
                    None => None,
                    Some(v) => match v.as_bool() {
                        Some(b) => Some(b),
                        None => return err("bad_item", format!("{what}: value is not true or false")),
                    },
                };
                Item::Toggle { id, text: text_of(item, n, kind)?, value }
            }
            "stepper" => {
                let id = id_of(item, &what, true, &mut seen)?;
                let mut values = Vec::new();
                for v in item.key("values").and_then(|v| v.as_array()).map(|v| v.as_slice()).unwrap_or_default() {
                    match v.as_str() {
                        Some(v) => values.push(v.to_string()),
                        None => return err("bad_item", format!("{what}: values holds something that is not a string")),
                    }
                }
                if values.is_empty() {
                    return err("bad_item", format!("{what}: values is missing or not a non-empty array of strings"));
                }
                let value = match string_of(item, "value", &what)? {
                    None => None,
                    Some(value) => match values.iter().position(|v| *v == value) {
                        Some(at) => Some(at),
                        None => return err("bad_item", format!("{what}: value is not one of values")),
                    },
                };
                Item::Stepper { id, text: text_of(item, n, kind)?, values, value }
            }
            "number" => {
                let id = id_of(item, &what, true, &mut seen)?;
                Item::Number { id, text: text_of(item, n, kind)?, value: string_of(item, "value", &what)? }
            }
            "readout" => {
                let id = id_of(item, &what, true, &mut seen)?;
                Item::Readout { id, text: text_of(item, n, kind)?, value: string_of(item, "value", &what)?.unwrap_or_default() }
            }
            "pick" => {
                let id = id_of(item, &what, true, &mut seen)?;
                Item::Pick {
                    id,
                    text: text_of(item, n, kind)?,
                    note: string_of(item, "note", &what)?.unwrap_or_default(),
                    dot: dot_of(item, &what)?,
                    selected: flag_of(item, "selected", false, &what)?,
                }
            }
            "button" => Item::Row { id: String::new(), buttons: vec![parse_button(item, &what, &mut seen)?] },
            "row" => {
                let id = id_of(item, &what, false, &mut seen)?;
                let Some(buttons) = item.key("buttons").and_then(|b| b.as_array()) else {
                    return err("bad_item", format!("{what}: buttons is missing or not an array"));
                };
                if buttons.is_empty() {
                    return err("bad_item", format!("{what}: buttons is empty"));
                }
                let mut row = Vec::with_capacity(buttons.len());
                for (k, b) in buttons.iter().enumerate() {
                    row.push(parse_button(b, &format!("item {n} (row) button {k}"), &mut seen)?);
                }
                Item::Row { id, buttons: row }
            }
            other => return err("bad_item", format!("item {n}: unknown kind: {other}")),
        });
    }
    Ok(Spec { surface, rev, title, items })
}

/// A JSON string literal.
pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `"surface":"id","rev":N`, or `"surface":null,"rev":0` when nothing is shown.
pub fn surface_fields(surface: Option<&str>, rev: u64) -> String {
    match surface {
        Some(s) => format!("\"surface\":{},\"rev\":{rev}", json_str(s)),
        None => "\"surface\":null,\"rev\":0".to_string(),
    }
}

impl WireErr {
    pub fn line(&self) -> String {
        format!("{{\"type\":\"err\",\"code\":{},\"msg\":{}}}\n", json_str(self.code), json_str(&self.msg))
    }
}

impl Tap {
    pub fn line(&self) -> String {
        let values: Vec<String> = self
            .values
            .iter()
            .map(|(id, v)| match v {
                Value::Bool(v) => format!("{}:{v}", json_str(id)),
                Value::Text(v) => format!("{}:{}", json_str(id), json_str(v)),
            })
            .collect();
        format!(
            "{{\"type\":\"tap\",\"seq\":{},{},\"id\":{},\"values\":{{{}}}}}\n",
            self.seq,
            surface_fields(Some(&self.surface), self.rev),
            json_str(&self.id),
            values.join(",")
        )
    }
}
