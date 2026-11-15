//! State and views: an owner pushes named values (`put`) and sends, once, a view: a tree of
//! the surface's own pieces whose fields are fixed text or bindings to those values. The app
//! builds the page from the two and builds it again when a value it shows has changed. A
//! view is data: it names pieces and keys, it holds no code.
//!
//! The pieces of a view are the items of a surface (heading, text, readout, pick, button)
//! and one shaping part, `each`: one item per record of a list value, with a heading where
//! a field of the records changes. What a view builds is an ordinary surface description
//! (`wire::Spec`), so it is drawn, scrolled and tapped as any page.

use crate::wire::{self, Dot, Item, Spec, Style, WireErr};
use makepad_widgets::makepad_platform::makepad_micro_serde::JsonValue;
use std::collections::{HashMap, HashSet};

/// The most keys one `put` carries, and the most one owner holds.
pub const PUT_KEYS: usize = 16;
pub const OWNER_KEYS: usize = 64;
/// The longest text value, the most records of a list, the most fields of a record.
const TEXT_BYTES: usize = 2048;
const RECORDS: usize = 200;
const FIELDS: usize = 12;
const FIELD_BYTES: usize = 512;
/// The most nodes of a view, and the most items a view builds.
const NODES: usize = 64;
const ITEMS: usize = 400;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Text(String),
    Num(f64),
    Bool(bool),
    /// records: each a few named fields, kept as text
    List(Vec<Vec<(String, String)>>),
}

impl Value {
    /// The value as a field of a piece shows it.
    pub fn text(&self) -> String {
        match self {
            Value::Text(text) => text.clone(),
            Value::Num(n) if n.fract() == 0.0 && n.abs() < 1e15 => format!("{}", *n as i64),
            Value::Num(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::List(records) => format!("{} records", records.len()),
        }
    }
}

/// A key: lower-case letters, digits and `_ . -` in parts joined by `/`; the first part is
/// its owner's name.
pub fn key_ok(key: &str) -> bool {
    (1..=64).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-' | b'/')) && key.split('/').all(|part| !part.is_empty())
}

fn err<T>(code: &'static str, msg: impl Into<String>) -> Result<T, WireErr> {
    Err(WireErr { code, msg: msg.into() })
}

fn scalar(value: &JsonValue) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(b) = value.as_bool() {
        return Some(b.to_string());
    }
    value.as_f64().map(|n| Value::Num(n).text())
}

/// The `state` of a `put`: each key with its new value, `None` to take the key away.
pub fn parse_put(request: &JsonValue) -> Result<Vec<(String, Option<Value>)>, WireErr> {
    let Some(state) = request.key("state").and_then(|s| s.as_object()) else {
        return err("bad_put", "put: state is an object of key: value");
    };
    if state.len() > PUT_KEYS {
        return err("too_big", format!("put: more than {PUT_KEYS} keys"));
    }
    let mut out = Vec::new();
    for (key, value) in state {
        if !key_ok(key) {
            return err("bad_put", format!("put: {key:?} is not a key (a-z 0-9 _ . - in parts joined by /, at most 64)"));
        }
        let value = if let Some(text) = value.as_str() {
            if text.len() > TEXT_BYTES {
                return err("too_big", format!("put {key}: a text of more than {TEXT_BYTES} bytes"));
            }
            Some(Value::Text(text.to_string()))
        } else if let Some(b) = value.as_bool() {
            Some(Value::Bool(b))
        } else if let Some(n) = value.as_f64() {
            Some(Value::Num(if n.is_finite() { n } else { 0.0 }))
        } else if let Some(list) = value.as_array() {
            if list.len() > RECORDS {
                return err("too_big", format!("put {key}: more than {RECORDS} records"));
            }
            let mut records = Vec::with_capacity(list.len());
            for record in list {
                let Some(fields) = record.as_object() else {
                    return err("bad_put", format!("put {key}: a list holds records (objects)"));
                };
                if fields.len() > FIELDS {
                    return err("too_big", format!("put {key}: a record of more than {FIELDS} fields"));
                }
                let mut kept = Vec::with_capacity(fields.len());
                for (name, field) in fields {
                    match scalar(field) {
                        Some(text) if text.len() <= FIELD_BYTES && name.len() <= 32 => kept.push((name.clone(), text)),
                        Some(_) => return err("too_big", format!("put {key}: field {name} is too long")),
                        None => return err("bad_put", format!("put {key}: field {name} is not a text, number or true/false")),
                    }
                }
                records.push(kept);
            }
            Some(Value::List(records))
        } else if value.as_object().is_some() {
            return err("bad_put", format!("put {key}: a value is a text, a number, true/false, a list of records, or null"));
        } else {
            None
        };
        out.push((key.clone(), value));
    }
    Ok(out)
}

/// A field of a piece: fixed, the value of a state key, or (inside `each`) a field of the
/// record.
#[derive(Clone, Debug)]
pub enum Bound {
    Lit(String),
    Bind(String),
    Field(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Heading,
    Text,
    Readout,
    Pick,
    Button,
}

/// Whether a pick is the selected one: said outright, or "its id is the value of this key".
#[derive(Clone, Debug)]
enum Selected {
    No,
    Yes,
    Is(String),
}

#[derive(Clone, Debug)]
pub struct Piece {
    kind: Kind,
    id: Option<Bound>,
    text: Bound,
    note: Option<Bound>,
    value: Option<Bound>,
    dot: Option<Bound>,
    style: Style,
    selected: Selected,
    /// a tap sets this key to the piece's id, at once, in the app
    set: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Node {
    One(Piece),
    /// one piece per record of the list `of`; a heading where the field `group` changes
    Each { of: String, group: Option<String>, item: Piece },
}

#[derive(Clone, Debug)]
pub struct View {
    pub name: String,
    pub title: String,
    pub nodes: Vec<Node>,
}

/// A view's name: part of nothing but messages, kept short and plain.
pub fn name_ok(name: &str) -> bool {
    (1..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

fn bound(value: &JsonValue, what: &str, in_each: bool) -> Result<Bound, WireErr> {
    if let Some(text) = value.as_str() {
        return Ok(Bound::Lit(wire_text(text)));
    }
    let key = |name: &str| value.key(name).and_then(|k| k.as_str());
    match (key("bind"), key("field")) {
        (Some(bind), None) if key_ok(bind) => Ok(Bound::Bind(bind.to_string())),
        (None, Some(field)) if in_each && !field.is_empty() => Ok(Bound::Field(field.to_string())),
        (None, Some(_)) => err("bad_view", format!("{what}: a field is a record's, inside each")),
        _ => err("bad_view", format!("{what}: a text, {{\"bind\":KEY}} or {{\"field\":NAME}}")),
    }
}

fn wire_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_control() || *c == '\n').collect()
}

fn piece(node: &JsonValue, n: usize, in_each: bool) -> Result<Piece, WireErr> {
    let Some(members) = node.as_object() else {
        return err("bad_view", format!("node {n}: not an object"));
    };
    let kind_name = node.key("kind").and_then(|k| k.as_str()).unwrap_or_default();
    let (kind, known): (Kind, &[&str]) = match kind_name {
        "heading" => (Kind::Heading, &["kind", "text"]),
        "text" => (Kind::Text, &["kind", "id", "text"]),
        "readout" => (Kind::Readout, &["kind", "id", "text", "value"]),
        "pick" => (Kind::Pick, &["kind", "id", "text", "note", "dot", "selected", "set"]),
        "button" => (Kind::Button, &["kind", "id", "text", "style"]),
        other => return err("bad_view", format!("node {n}: unknown kind {other:?} (a view takes heading, text, readout, pick, button, each)")),
    };
    let what = format!("node {n} ({kind_name})");
    if let Some((name, _)) = members.iter().find(|(name, _)| !known.contains(&name.as_str())) {
        return err("bad_view", format!("{what}: unknown field {name:?}"));
    }
    let field = |name: &str| node.key(name).map(|value| bound(value, &format!("{what}: {name}"), in_each)).transpose();
    let id = field("id")?;
    if id.is_none() && matches!(kind, Kind::Readout | Kind::Pick | Kind::Button) {
        return err("bad_view", format!("{what}: id is missing"));
    }
    let Some(text) = field("text")? else {
        return err("bad_view", format!("{what}: text is missing"));
    };
    let selected = match node.key("selected") {
        None => Selected::No,
        Some(value) => match (value.as_bool(), value.key("is").and_then(|k| k.as_str())) {
            (Some(true), _) => Selected::Yes,
            (Some(false), _) => Selected::No,
            (None, Some(key)) if key_ok(key) => Selected::Is(key.to_string()),
            _ => return err("bad_view", format!("{what}: selected is true, false or {{\"is\":KEY}}")),
        },
    };
    let set = match node.key("set") {
        None => None,
        Some(key) => match key.as_str() {
            Some(key) if key_ok(key) => Some(key.to_string()),
            _ => return err("bad_view", format!("{what}: set is a key")),
        },
    };
    let style = match node.key("style").and_then(|s| s.as_str()) {
        Some("primary") => Style::Primary,
        Some("danger") => Style::Danger,
        _ => Style::Secondary,
    };
    Ok(Piece { kind, id, text, note: field("note")?, value: field("value")?, dot: field("dot")?, style, selected, set })
}

/// A `view` request: `Ok((name, None))` takes the view of that name away.
pub fn parse_view(request: &JsonValue) -> Result<(String, Option<View>), WireErr> {
    let name = match request.key("name").and_then(|n| n.as_str()) {
        Some(name) if name_ok(name) => name.to_string(),
        _ => return err("bad_view", "view: name is 1 to 32 of a-z 0-9 _ -"),
    };
    let Some(tree) = request.key("tree") else {
        return Ok((name, None));
    };
    let Some(tree) = tree.as_array() else {
        return err("bad_view", "view: tree is an array of nodes");
    };
    if tree.len() > NODES {
        return err("too_big", format!("view: more than {NODES} nodes"));
    }
    let title = wire_text(request.key("title").and_then(|t| t.as_str()).unwrap_or(&name));
    let mut nodes = Vec::new();
    for (n, node) in tree.iter().enumerate() {
        if node.key("kind").and_then(|k| k.as_str()) != Some("each") {
            nodes.push(Node::One(piece(node, n, false)?));
            continue;
        }
        if let Some((name, _)) = node.as_object().and_then(|members| members.iter().find(|(name, _)| !["kind", "of", "group", "item"].contains(&name.as_str()))) {
            return err("bad_view", format!("node {n} (each): unknown field {name:?}"));
        }
        let of = match node.key("of").and_then(|k| k.as_str()) {
            Some(of) if key_ok(of) => of.to_string(),
            _ => return err("bad_view", format!("node {n} (each): of is the key of a list")),
        };
        let group = match node.key("group") {
            None => None,
            Some(group) => match group.as_str() {
                Some(group) if !group.is_empty() => Some(group.to_string()),
                _ => return err("bad_view", format!("node {n} (each): group is a field's name")),
            },
        };
        let Some(item) = node.key("item") else {
            return err("bad_view", format!("node {n} (each): item is missing"));
        };
        let item = piece(item, n, true)?;
        if item.kind == Kind::Heading || item.id.is_none() {
            return err("bad_view", format!("node {n} (each): the item is a text, readout, pick or button with an id"));
        }
        nodes.push(Node::Each { of, group, item });
    }
    Ok((name.clone(), Some(View { name, title, nodes })))
}

/// Every key a view reads or sets: all of them must be its owner's.
pub fn keys_of(view: &View) -> Vec<&str> {
    let mut keys = Vec::new();
    for node in &view.nodes {
        let piece = match node {
            Node::One(piece) => piece,
            Node::Each { of, item, .. } => {
                keys.push(of.as_str());
                item
            }
        };
        for bound in [piece.id.as_ref(), Some(&piece.text), piece.note.as_ref(), piece.value.as_ref(), piece.dot.as_ref()].into_iter().flatten() {
            if let Bound::Bind(key) = bound {
                keys.push(key);
            }
        }
        if let Selected::Is(key) = &piece.selected {
            keys.push(key);
        }
        keys.extend(piece.set.as_deref());
    }
    keys
}

#[derive(Default)]
pub struct Store {
    values: HashMap<String, Value>,
}

/// What a view is on screen, and what a tap on one of its items does besides being told.
pub struct Built {
    pub spec: Spec,
    /// item id to the state key a tap on it sets (to that id)
    pub sets: HashMap<String, String>,
}

impl Store {
    /// Takes the values of a `put`. `true` when something is another than before.
    pub fn put(&mut self, values: Vec<(String, Option<Value>)>) -> bool {
        let mut changed = false;
        for (key, value) in values {
            match value {
                Some(value) if self.values.get(&key) == Some(&value) => continue,
                Some(value) => {
                    self.values.insert(key, value);
                }
                None if self.values.remove(&key).is_none() => continue,
                None => {}
            }
            changed = true;
        }
        changed
    }

    pub fn set_text(&mut self, key: &str, text: &str) {
        self.values.insert(key.to_string(), Value::Text(text.to_string()));
    }

    pub fn clear(&mut self) {
        self.values.clear();
    }

    fn resolve(&self, bound: &Bound, record: Option<&[(String, String)]>) -> String {
        match bound {
            Bound::Lit(text) => text.clone(),
            Bound::Bind(key) => self.values.get(key).map(Value::text).unwrap_or_default(),
            Bound::Field(name) => record.and_then(|record| record.iter().find(|(field, _)| field == name)).map(|(_, value)| value.clone()).unwrap_or_default(),
        }
    }

    fn item(&self, piece: &Piece, record: Option<&[(String, String)]>, seen: &mut HashSet<String>, sets: &mut HashMap<String, String>) -> Option<Item> {
        let field = |bound: &Option<Bound>| bound.as_ref().map(|bound| self.resolve(bound, record)).unwrap_or_default();
        let text = self.resolve(&piece.text, record);
        let id = field(&piece.id);
        // an id is unique on a page: a second record of the same id is left out
        if !id.is_empty() && !seen.insert(id.clone()) {
            return None;
        }
        if id.is_empty() && !matches!(piece.kind, Kind::Heading | Kind::Text) {
            return None;
        }
        Some(match piece.kind {
            Kind::Heading => Item::Heading { id, text },
            Kind::Text => Item::Text { id, text },
            Kind::Readout => Item::Readout { id, text, value: field(&piece.value) },
            Kind::Button => Item::Row { id: String::new(), buttons: vec![wire::Button { id, text, style: piece.style, confirm: false, lock: false, dim: false }] },
            Kind::Pick => {
                let dot = match field(&piece.dot).as_str() {
                    "green" => Some(Dot::Green),
                    "amber" => Some(Dot::Amber),
                    "red" => Some(Dot::Red),
                    "off" => Some(Dot::Off),
                    _ => None,
                };
                let selected = match &piece.selected {
                    Selected::No => false,
                    Selected::Yes => true,
                    Selected::Is(key) => self.values.get(key).is_some_and(|value| value.text() == id),
                };
                if let Some(key) = &piece.set {
                    sets.insert(id.clone(), key.clone());
                }
                Item::Pick { id, text, note: field(&piece.note), dot, selected }
            }
        })
    }

    /// The page a view is with the values as they are now.
    pub fn build(&self, view: &View, rev: u64) -> Built {
        let mut items = Vec::new();
        let mut seen = HashSet::new();
        let mut sets = HashMap::new();
        for node in &view.nodes {
            match node {
                Node::One(piece) => items.extend(self.item(piece, None, &mut seen, &mut sets)),
                Node::Each { of, group, item } => {
                    let Some(Value::List(records)) = self.values.get(of) else {
                        continue;
                    };
                    let mut heading: Option<String> = None;
                    for record in records {
                        if let Some(group) = group {
                            let name = record.iter().find(|(field, _)| field == group).map(|(_, value)| value.clone()).unwrap_or_default();
                            if heading.as_ref() != Some(&name) {
                                items.push(Item::Heading { id: String::new(), text: name.clone() });
                                heading = Some(name);
                            }
                        }
                        items.extend(self.item(item, Some(record), &mut seen, &mut sets));
                    }
                }
            }
            if items.len() >= ITEMS {
                items.truncate(ITEMS);
                break;
            }
        }
        Built { spec: Spec { surface: view.name.clone(), rev, title: view.title.clone(), items }, sets }
    }
}

/// What turns the page `old` into `new` in place, when the two have the same items: the
/// texts, values, notes, dots and selection that differ. `None`: items came or went, or
/// something an update cannot change is another (the page is set again then).
pub fn diff(old: &Spec, new: &Spec) -> Option<wire::Update> {
    if old.items.len() != new.items.len() {
        return None;
    }
    let mut changes = Vec::new();
    for (was, now) in old.items.iter().zip(&new.items) {
        if was == now {
            continue;
        }
        let mut change = wire::Change { id: String::new(), text: None, value: None, note: None, dot: None, selected: None };
        match (was, now) {
            (Item::Text { id: a, text: _ }, Item::Text { id, text }) if a == id && !id.is_empty() => {
                change.id.clone_from(id);
                change.text = Some(text.clone());
            }
            (Item::Readout { id: a, text: t0, value: v0 }, Item::Readout { id, text, value }) if a == id => {
                change.id.clone_from(id);
                change.text = (t0 != text).then(|| text.clone());
                change.value = (v0 != value).then(|| value.clone());
            }
            (Item::Pick { id: a, text: t0, note: n0, dot: d0, selected: s0 }, Item::Pick { id, text, note, dot, selected }) if a == id && d0.is_some() == dot.is_some() => {
                change.id.clone_from(id);
                change.text = (t0 != text).then(|| text.clone());
                change.note = (n0 != note).then(|| note.clone());
                change.dot = if d0 != dot { *dot } else { None };
                change.selected = (s0 != selected).then_some(*selected);
            }
            _ => return None,
        }
        changes.push(change);
    }
    Some(wire::Update { surface: new.surface.clone(), rev: old.rev, title: (old.title != new.title).then(|| new.title.clone()), changes, arm: Vec::new() })
}
