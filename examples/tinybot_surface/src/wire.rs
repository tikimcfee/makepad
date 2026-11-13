//! The surface protocol's data: what an agent may ask for (one JSON object per line) and the
//! lines the app answers with. A `set` is checked completely here, before anything on screen
//! changes: a description that fails leaves the last good surface alone.

use makepad_widgets::makepad_platform::makepad_micro_serde::{DeJson, JsonValue};
use std::collections::HashSet;

/// Protocol version, in `hello`.
pub const VERSION: u64 = 1;
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
}

#[derive(Clone, Debug)]
pub enum Item {
    Heading(String),
    Text(String),
    /// `value: None` keeps what the user set when the surface id is unchanged
    Toggle { id: String, text: String, value: Option<bool> },
    Row(Vec<Button>),
    Divider,
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
    Clear,
    Status,
    Taps { wait: bool },
    Done { seq: u64 },
}

/// A refused request: `{"type":"err","code":CODE,"msg":MSG}`.
pub struct WireErr {
    pub code: &'static str,
    pub msg: String,
}

fn err<T>(code: &'static str, msg: impl Into<String>) -> Result<T, WireErr> {
    Err(WireErr { code, msg: msg.into() })
}

/// A press of a button, with the toggles as they were at that moment (in item order).
#[derive(Clone, Debug)]
pub struct Tap {
    pub seq: u64,
    pub surface: String,
    pub rev: u64,
    pub id: String,
    pub values: Vec<(String, bool)>,
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
        Some("clear") => Ok(Request::Clear),
        Some("status") => Ok(Request::Status),
        Some("taps") => match value.key("wait") {
            None => Ok(Request::Taps { wait: false }),
            Some(w) => match w.as_bool() {
                Some(wait) => Ok(Request::Taps { wait }),
                None => err("bad_request", "taps: wait is true or false"),
            },
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
    Ok(Button { id, text, style })
}

fn parse_set(value: &JsonValue) -> Result<Spec, WireErr> {
    let surface = match value.key("surface").and_then(|s| s.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return err("bad_set", "surface is missing or not a non-empty string"),
    };
    let rev = match value.key("rev").and_then(|r| r.as_u64()) {
        Some(r) if r >= 1 => r,
        _ => return err("bad_set", "rev is missing or not an integer >= 1"),
    };
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
            "heading" => {
                id_of(item, &what, false, &mut seen)?;
                Item::Heading(text_of(item, n, kind)?)
            }
            "text" => {
                id_of(item, &what, false, &mut seen)?;
                Item::Text(text_of(item, n, kind)?)
            }
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
            "button" => Item::Row(vec![parse_button(item, &what, &mut seen)?]),
            "row" => {
                id_of(item, &what, false, &mut seen)?;
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
                Item::Row(row)
            }
            "input" => return err("unsupported", format!("item {n}: kind input is not built yet")),
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
        let values: Vec<String> = self.values.iter().map(|(id, v)| format!("{}:{v}", json_str(id))).collect();
        format!(
            "{{\"type\":\"tap\",\"seq\":{},{},\"id\":{},\"values\":{{{}}}}}\n",
            self.seq,
            surface_fields(Some(&self.surface), self.rev),
            json_str(&self.id),
            values.join(",")
        )
    }
}
