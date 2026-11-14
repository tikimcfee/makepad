//! The panel file: a surface description plus what the runner does for its buttons and
//! readouts. Everything is checked when the file is read, strictly: a field the runner does
//! not know is refused, because a misspelt `confirm` or `timeout_s` must not pass as absent.

use crate::json::Json;
use std::collections::HashSet;
use std::io::Read;
use std::os::unix::fs::MetadataExt;

/// The longest `set` line the app takes, less room for what the runner adds to a page.
const PAGE_BYTES: usize = 60 * 1024;

pub struct Panel {
    pub name: String,
    pub title: String,
    pub summary: String,
    /// its place on Home: lower first, then by title
    pub order: f64,
    /// the all-off: behind the Stop button and behind every lock
    pub stop: Option<Stop>,
    pub items: Vec<Item>,
}

pub struct Stop {
    pub argv: Vec<String>,
    pub timeout_s: f64,
}

pub enum Item {
    /// heading, text, divider: as the app takes it
    Plain { id: String, json: Json },
    /// toggle, stepper, number: what a tap's `values` hold
    Input { id: String, kind: Input, json: Json },
    Readout { id: String, json: Json, poll: Option<Poll>, every_s: f64 },
    Row { id: String, buttons: Vec<Button> },
}

pub enum Input {
    Toggle,
    Stepper(Vec<String>),
    Number,
}

#[derive(Clone)]
pub enum Poll {
    /// read in this process: open, up to 256 bytes, close
    File(String),
    /// run directly as the app's user, killed at 2 s
    Argv(Vec<String>),
}

pub struct Button {
    pub id: String,
    /// the button as the app takes it
    pub json: Json,
    pub does: Does,
}

pub enum Does {
    /// a tap is recorded, nothing runs
    Nothing,
    /// shows another panel
    Open(String),
    Action(Action),
}

pub struct Action {
    /// each element a sequence of literal text and `{id}` values
    pub argv: Vec<Vec<Part>>,
    pub as_root: bool,
    pub timeout_s: f64,
    pub stop_post: Vec<String>,
    /// the readout, text or heading that shows `rc N · line`
    pub result: Option<String>,
    /// the words asked for after the try
    pub ask: Vec<String>,
    /// the answers that run Stop and lock the panel
    pub stop_on: Vec<String>,
    pub checks: Vec<Check>,
}

pub struct Check {
    pub name: String,
    pub argv: Vec<String>,
}

pub enum Part {
    Text(String),
    /// the tap's value of the item with this id
    Value(String),
}

/// A panel's name is part of file names and unit names. Names that start with `_` are the
/// runner's own pages.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 32 && !name.starts_with('_') && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Root's and nobody else's to write: the rule for a panel file and for a panel directory.
pub fn roots_own(meta: &std::fs::Metadata) -> Result<(), String> {
    if meta.uid() != 0 {
        return Err(format!("owned by uid {}, not by root", meta.uid()));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(format!("writable by group or others (mode {:04o})", meta.mode() & 0o7777));
    }
    Ok(())
}

impl Panel {
    /// Reads and checks a panel file. The ownership is that of the file that was opened.
    pub fn load(path: &std::path::Path) -> Result<Panel, String> {
        let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err("not a regular file".into());
        }
        roots_own(&meta)?;
        let mut text = String::new();
        file.read_to_string(&mut text).map_err(|e| e.to_string())?;
        let panel = Panel::parse(&Json::parse(&text)?)?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        if panel.name != stem {
            return Err(format!("panel is {:?} but the file is {stem}.json: they must be the same", panel.name));
        }
        Ok(panel)
    }

    pub fn parse(file: &Json) -> Result<Panel, String> {
        let members = file.as_obj().ok_or("a panel is one JSON object")?;
        known(members, &["panel", "title", "summary", "order", "stop", "items"], "the panel")?;
        let name = string(file, "panel", "the panel")?.ok_or("panel (the name) is missing")?;
        if !valid_name(&name) {
            return Err(format!("panel {name:?}: a name is 1 to 32 of A-Z a-z 0-9 _ - and does not start with _"));
        }
        let title = string(file, "title", "the panel")?.unwrap_or_else(|| name.clone());
        let summary = string(file, "summary", "the panel")?.unwrap_or_default();
        if summary.contains('\n') {
            return Err("summary is one line".into());
        }
        let order = match file.get("order") {
            None => f64::MAX,
            Some(order) => order.as_f64().ok_or("order is not a number")?,
        };
        let stop = match file.get("stop") {
            None => None,
            Some(stop) => {
                let what = "stop";
                known(stop.as_obj().ok_or("stop is not an object")?, &["argv", "timeout_s"], what)?;
                Some(Stop { argv: fixed_argv(stop.get("argv"), what)?, timeout_s: seconds(stop, "timeout_s", what)?.ok_or("stop: timeout_s is missing")? })
            }
        };
        let list = file.get("items").and_then(|i| i.as_arr()).ok_or("items is missing or not an array")?;
        let mut ids = HashSet::new();
        let mut items = Vec::new();
        for (n, item) in list.iter().enumerate() {
            items.push(parse_item(item, n, &mut ids)?);
        }
        let panel = Panel { name, title, summary, order, stop, items };
        panel.check_references()?;
        let page = panel.items.iter().map(|item| item.json(false).to_string().len() + 1).sum::<usize>() + panel.title.len();
        if page > PAGE_BYTES {
            return Err(format!("the page is {page} bytes, more than the {PAGE_BYTES} the app takes in one line"));
        }
        Ok(panel)
    }

    /// What can only be checked with every item known: placeholders, result items, the
    /// all-off an answer asks for.
    fn check_references(&self) -> Result<(), String> {
        for button in self.buttons() {
            let Does::Action(action) = &button.does else {
                continue;
            };
            let what = format!("button {}", button.id);
            for part in action.argv.iter().flatten() {
                if let Part::Value(id) = part {
                    if self.input(id).is_none() {
                        return Err(format!("{what}: unknown placeholder {{{id}}}: no toggle, stepper or number has this id"));
                    }
                }
            }
            if let Some(result) = &action.result {
                let shows = self.items.iter().any(|item| match item {
                    Item::Plain { id, json } => id == result && json.get("kind").and_then(|k| k.as_str()) != Some("divider"),
                    Item::Readout { id, .. } => id == result,
                    _ => false,
                });
                if !shows {
                    return Err(format!("{what}: result {result:?} is not the id of a readout, text or heading"));
                }
            }
            if !action.stop_on.is_empty() && self.stop.is_none() {
                return Err(format!("{what}: on_answer asks for stop, but the panel has no stop"));
            }
        }
        Ok(())
    }

    pub fn buttons(&self) -> impl Iterator<Item = &Button> {
        self.items.iter().flat_map(|item| match item {
            Item::Row { buttons, .. } => buttons.as_slice(),
            _ => &[],
        })
    }

    pub fn button(&self, id: &str) -> Option<&Button> {
        self.buttons().find(|b| b.id == id)
    }

    pub fn input(&self, id: &str) -> Option<&Input> {
        self.items.iter().find_map(|item| match item {
            Item::Input { id: this, kind, .. } if this == id => Some(kind),
            _ => None,
        })
    }

    /// Whether `id` is a readout (its `value` is changed) or a text or heading (its `text`).
    pub fn is_readout(&self, id: &str) -> bool {
        self.items.iter().any(|item| matches!(item, Item::Readout { id: this, .. } if this == id))
    }
}

impl Item {
    /// The item as the app takes it. `keep_user`: for a page the user is already on, the
    /// inputs go without `value`, so the app keeps what the user set.
    pub fn json(&self, keep_user: bool) -> Json {
        match self {
            Item::Plain { json, .. } | Item::Readout { json, .. } => json.clone(),
            Item::Input { json, .. } => {
                let mut json = json.clone();
                if keep_user {
                    json.remove("value");
                }
                json
            }
            Item::Row { id, buttons } => row_json(id, buttons.iter()),
        }
    }
}

/// A row of the given buttons as the app takes it.
pub fn row_json<'a>(id: &str, buttons: impl Iterator<Item = &'a Button>) -> Json {
    let mut row = Json::obj([("kind", "row".into())]);
    if !id.is_empty() {
        row.set("id", id.into());
    }
    row.set("buttons", Json::Arr(buttons.map(|b| b.json.clone()).collect()));
    row
}

/// Refuses a member the runner does not know.
fn known(members: &[(String, Json)], names: &[&str], what: &str) -> Result<(), String> {
    match members.iter().find(|(key, _)| !names.contains(&key.as_str())) {
        Some((key, _)) => Err(format!("{what}: unknown field {key:?} (known: {})", names.join(", "))),
        None => Ok(()),
    }
}

fn string(of: &Json, name: &str, what: &str) -> Result<Option<String>, String> {
    match of.get(name) {
        None => Ok(None),
        Some(value) => match value.as_str() {
            Some(text) => Ok(Some(text.to_string())),
            None => Err(format!("{what}: {name} is not a string")),
        },
    }
}

fn flag(of: &Json, name: &str, what: &str) -> Result<Option<bool>, String> {
    match of.get(name) {
        None => Ok(None),
        Some(value) => value.as_bool().map(Some).ok_or(format!("{what}: {name} is not true or false")),
    }
}

/// A time in seconds: a number above zero.
fn seconds(of: &Json, name: &str, what: &str) -> Result<Option<f64>, String> {
    match of.get(name) {
        None => Ok(None),
        Some(value) => match value.as_f64() {
            Some(s) if s > 0.0 && s.is_finite() => Ok(Some(s)),
            _ => Err(format!("{what}: {name} is not a number of seconds above 0")),
        },
    }
}

fn strings(value: Option<&Json>, what: &str) -> Result<Vec<String>, String> {
    let list = value.and_then(|v| v.as_arr()).ok_or(format!("{what} is missing or not an array of strings"))?;
    list.iter().map(|v| v.as_str().map(str::to_string).ok_or(format!("{what} holds something that is not a string"))).collect()
}

/// An argv element as literal text and `{id}` values. `{{` is a literal brace.
fn template(element: &str, what: &str) -> Result<Vec<Part>, String> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut rest = element;
    while let Some(open) = rest.find('{') {
        text.push_str(&rest[..open]);
        rest = &rest[open + 1..];
        if let Some(after) = rest.strip_prefix('{') {
            text.push('{');
            rest = after;
            continue;
        }
        let Some(close) = rest.find('}') else {
            return Err(format!("{what}: a {{ without its }} in {element:?} (write {{{{ for a literal brace)"));
        };
        let id = &rest[..close];
        if id.is_empty() || id.contains('{') {
            return Err(format!("{what}: a bad placeholder in {element:?}"));
        }
        if !text.is_empty() {
            parts.push(Part::Text(std::mem::take(&mut text)));
        }
        parts.push(Part::Value(id.to_string()));
        rest = &rest[close + 1..];
    }
    text.push_str(rest);
    if !text.is_empty() || parts.is_empty() {
        parts.push(Part::Text(text));
    }
    Ok(parts)
}

/// The program of an argv: an absolute path. Nothing is looked up in a PATH.
fn absolute(program: &str, what: &str) -> Result<(), String> {
    if program.starts_with('/') && !program.contains('\0') {
        Ok(())
    } else {
        Err(format!("{what}: the program {program:?} is not an absolute path"))
    }
}

/// An argv with `{id}` values in its arguments (never in the program).
fn argv_template(value: Option<&Json>, what: &str) -> Result<Vec<Vec<Part>>, String> {
    let elements = strings(value, &format!("{what}: argv"))?;
    let Some(program) = elements.first() else {
        return Err(format!("{what}: argv is empty"));
    };
    let mut argv = Vec::new();
    for element in &elements {
        if element.contains('\0') {
            return Err(format!("{what}: argv holds a NUL"));
        }
        argv.push(template(element, what)?);
    }
    match argv[0].as_slice() {
        [Part::Text(program)] => absolute(program, what)?,
        _ => return Err(format!("{what}: the program {program:?} holds a placeholder")),
    }
    Ok(argv)
}

/// An argv without placeholders: what it runs never depends on what was typed.
fn fixed_argv(value: Option<&Json>, what: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for parts in argv_template(value, what)? {
        match parts.as_slice() {
            [Part::Text(text)] => out.push(text.clone()),
            _ => return Err(format!("{what}: a placeholder in a fixed argv (only an action's argv takes values)")),
        }
    }
    Ok(out)
}

/// An id, unique in the panel. Ids that start with `_` are the runner's.
fn id_of(item: &Json, what: &str, required: bool, ids: &mut HashSet<String>) -> Result<String, String> {
    match string(item, "id", what)? {
        None if required => Err(format!("{what}: id is missing")),
        None => Ok(String::new()),
        Some(id) if id.is_empty() => Err(format!("{what}: id is empty")),
        Some(id) if id.starts_with('_') => Err(format!("{what}: id {id:?} starts with _, which is kept for the runner's own items")),
        Some(id) => {
            if !ids.insert(id.clone()) {
                return Err(format!("duplicate id: {id}"));
            }
            Ok(id)
        }
    }
}

fn text_of(item: &Json, what: &str) -> Result<String, String> {
    string(item, "text", what)?.ok_or(format!("{what}: text is missing"))
}

fn parse_item(item: &Json, n: usize, ids: &mut HashSet<String>) -> Result<Item, String> {
    let members = item.as_obj().ok_or(format!("item {n}: not an object"))?;
    let kind = string(item, "kind", &format!("item {n}"))?.ok_or(format!("item {n}: kind is missing"))?;
    let what = format!("item {n} ({kind})");
    let what = what.as_str();
    Ok(match kind.as_str() {
        "heading" | "text" => {
            known(members, &["kind", "id", "text"], what)?;
            text_of(item, what)?;
            Item::Plain { id: id_of(item, what, false, ids)?, json: item.clone() }
        }
        "divider" => {
            known(members, &["kind", "id"], what)?;
            Item::Plain { id: id_of(item, what, false, ids)?, json: item.clone() }
        }
        "toggle" => {
            known(members, &["kind", "id", "text", "value"], what)?;
            text_of(item, what)?;
            flag(item, "value", what)?;
            Item::Input { id: id_of(item, what, true, ids)?, kind: Input::Toggle, json: item.clone() }
        }
        "stepper" => {
            known(members, &["kind", "id", "text", "values", "value"], what)?;
            text_of(item, what)?;
            let values = strings(item.get("values"), &format!("{what}: values"))?;
            if values.is_empty() {
                return Err(format!("{what}: values is empty"));
            }
            if let Some(value) = string(item, "value", what)? {
                if !values.contains(&value) {
                    return Err(format!("{what}: value {value:?} is not one of values"));
                }
            }
            Item::Input { id: id_of(item, what, true, ids)?, kind: Input::Stepper(values), json: item.clone() }
        }
        "number" => {
            known(members, &["kind", "id", "text", "value"], what)?;
            text_of(item, what)?;
            string(item, "value", what)?;
            Item::Input { id: id_of(item, what, true, ids)?, kind: Input::Number, json: item.clone() }
        }
        "readout" => {
            known(members, &["kind", "id", "text", "value", "poll", "every_s"], what)?;
            text_of(item, what)?;
            string(item, "value", what)?;
            let poll = match item.get("poll") {
                None => None,
                Some(poll) => {
                    let what = format!("{what}: poll");
                    known(poll.as_obj().ok_or(format!("{what} is not an object"))?, &["file", "argv"], &what)?;
                    Some(match (string(poll, "file", &what)?, poll.get("argv")) {
                        (Some(file), None) => {
                            absolute(&file, &what)?;
                            Poll::File(file)
                        }
                        (None, Some(argv)) => Poll::Argv(fixed_argv(Some(argv), &what)?),
                        _ => return Err(format!("{what} takes either file or argv")),
                    })
                }
            };
            let every_s = match seconds(item, "every_s", what)? {
                Some(s) if s < 1.0 => return Err(format!("{what}: every_s is at least 1")),
                Some(_) if poll.is_none() => return Err(format!("{what}: every_s without poll")),
                Some(s) => s,
                None => 2.0,
            };
            // the app is sent the readout without the runner's fields
            let mut json = item.clone();
            json.remove("poll");
            json.remove("every_s");
            Item::Readout { id: id_of(item, what, true, ids)?, json, poll, every_s }
        }
        "button" => Item::Row { id: String::new(), buttons: vec![parse_button(item, what, true, ids)?] },
        "row" => {
            known(members, &["kind", "id", "buttons"], what)?;
            let id = id_of(item, what, false, ids)?;
            let list = item.get("buttons").and_then(|b| b.as_arr()).ok_or(format!("{what}: buttons is missing or not an array"))?;
            if list.is_empty() {
                return Err(format!("{what}: buttons is empty"));
            }
            let mut buttons = Vec::new();
            for (k, button) in list.iter().enumerate() {
                buttons.push(parse_button(button, &format!("item {n} (row) button {k}"), false, ids)?);
            }
            Item::Row { id, buttons }
        }
        other => return Err(format!("item {n}: unknown kind: {other}")),
    })
}

fn parse_button(button: &Json, what: &str, as_item: bool, ids: &mut HashSet<String>) -> Result<Button, String> {
    let members = button.as_obj().ok_or(format!("{what}: not an object"))?;
    let mut names = vec!["id", "text", "style", "confirm", "lock", "action", "open"];
    if as_item {
        names.push("kind");
    }
    known(members, &names, what)?;
    let id = id_of(button, what, true, ids)?;
    let what = format!("button {id}");
    let what = what.as_str();
    text_of(button, what)?;
    if let Some(style) = string(button, "style", what)? {
        if !["primary", "secondary", "danger"].contains(&style.as_str()) {
            return Err(format!("{what}: style {style:?} is not primary, secondary or danger"));
        }
    }
    flag(button, "confirm", what)?;
    flag(button, "lock", what)?;
    let does = match (button.get("action"), string(button, "open", what)?) {
        (Some(_), Some(_)) => return Err(format!("{what}: both action and open")),
        (Some(action), None) => Does::Action(parse_action(action, what)?),
        (None, Some(open)) if valid_name(&open) => Does::Open(open),
        (None, Some(open)) => return Err(format!("{what}: open {open:?} is not a panel name")),
        (None, None) => Does::Nothing,
    };
    // the app is sent the button without the runner's fields
    let mut json = button.clone();
    for name in ["kind", "action", "open"] {
        json.remove(name);
    }
    Ok(Button { id, json, does })
}

fn parse_action(action: &Json, what: &str) -> Result<Action, String> {
    let what = format!("{what}: action");
    let what = what.as_str();
    let members = action.as_obj().ok_or(format!("{what} is not an object"))?;
    known(members, &["argv", "as", "timeout_s", "stop_post", "result", "ask", "on_answer", "checks"], what)?;
    let as_root = match string(action, "as", what)?.as_deref() {
        None | Some("user") => false,
        Some("root") => true,
        Some(other) => return Err(format!("{what}: as is {other:?}, not root or user")),
    };
    let stop_post = match action.get("stop_post") {
        None => Vec::new(),
        Some(argv) => fixed_argv(Some(argv), &format!("{what}: stop_post"))?,
    };
    let ask = match action.get("ask") {
        None => Vec::new(),
        Some(ask) => strings(Some(ask), &format!("{what}: ask"))?,
    };
    if ask.iter().any(|word| word.is_empty()) || ask.iter().collect::<HashSet<_>>().len() != ask.len() {
        return Err(format!("{what}: ask holds an empty word or one word twice"));
    }
    let mut stop_on = Vec::new();
    if let Some(on_answer) = action.get("on_answer") {
        for (word, then) in on_answer.as_obj().ok_or(format!("{what}: on_answer is not an object"))? {
            if !ask.contains(word) {
                return Err(format!("{what}: on_answer names {word:?}, which is not one of ask"));
            }
            if then.as_str() != Some("stop") {
                return Err(format!("{what}: on_answer {word:?} is not \"stop\" (the one thing an answer can do)"));
            }
            stop_on.push(word.clone());
        }
    }
    let mut checks = Vec::new();
    if let Some(list) = action.get("checks") {
        for check in list.as_arr().ok_or(format!("{what}: checks is not an array"))? {
            let what = format!("{what}: check");
            known(check.as_obj().ok_or(format!("{what} is not an object"))?, &["name", "argv"], &what)?;
            let name = string(check, "name", &what)?.filter(|n| !n.is_empty()).ok_or(format!("{what}: name is missing"))?;
            checks.push(Check { argv: fixed_argv(check.get("argv"), &format!("{what} {name}"))?, name });
        }
    }
    Ok(Action {
        argv: argv_template(action.get("argv"), what)?,
        as_root,
        timeout_s: seconds(action, "timeout_s", what)?.ok_or(format!("{what}: timeout_s is missing"))?,
        stop_post,
        result: string(action, "result", what)?,
        ask,
        stop_on,
        checks,
    })
}

/// A tap's value of an input, checked again by the runner: the page the tap came from can be
/// older than the file. `None` is "refused: stale value".
pub fn tap_value(kind: &Input, value: Option<&Json>) -> Option<String> {
    match (kind, value?) {
        (Input::Toggle, Json::Bool(on)) => Some(on.to_string()),
        (Input::Stepper(values), Json::Str(value)) if values.contains(value) => Some(value.clone()),
        // a number is passed on as typed; it only has to be an argument at all
        (Input::Number, Json::Str(value)) if (1..=64).contains(&value.len()) && !value.contains(['\0', '\n']) => Some(value.clone()),
        _ => None,
    }
}

impl Action {
    /// The argv of a try, each element whole whatever its values hold. `None`: a value is
    /// missing or does not pass `tap_value`.
    pub fn fill(&self, panel: &Panel, values: &Json) -> Option<Vec<String>> {
        let mut argv = Vec::new();
        for parts in &self.argv {
            let mut element = String::new();
            for part in parts {
                match part {
                    Part::Text(text) => element.push_str(text),
                    Part::Value(id) => element.push_str(&tap_value(panel.input(id)?, values.get(id))?),
                }
            }
            argv.push(element);
        }
        Some(argv)
    }
}
