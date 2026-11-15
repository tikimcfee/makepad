//! A feed file: items another tool publishes for a panel's `feed` item (agents, networks,
//! files, readings). The runner reads it while the feed's page is on screen and shows its
//! items where the feed stands; a tap on one of them runs the feed's action with its id.
//!
//! The file is data from another process, read often: nothing in it can stop the runner. It
//! is read up to a size, its items up to a count, and whatever is wrong with it is one line
//! on the page where the items would be.
//!
//! ```json
//! {"items":[
//!   {"kind":"heading","text":"monolith1"},
//!   {"kind":"pick","id":"optimizer","text":"tiny-monolith-optimizer","note":"idle","dot":"green","selected":true},
//!   {"kind":"readout","id":"battery","text":"Battery","value":"81 %"},
//!   {"kind":"button","id":"refresh","text":"Look again"}
//! ]}
//! ```
//!
//! The kinds are the surface's own, those that need no way back for a value: heading, text,
//! readout, pick and button. None of them arms or asks: what needs a second tap or a check
//! stays in a panel file.

use crate::json::Json;
use crate::panel;
use std::io::Read;
use std::os::unix::fs::MetadataExt;

/// The most of a feed file that is read. A larger file is not a feed.
pub const FILE_BYTES: usize = 64 * 1024;
/// The most items that are shown. Of a longer feed the first ones, and a line that says how
/// many are left out.
pub const ITEMS: usize = 200;
/// The longest id.
const ID_BYTES: usize = 64;

#[derive(Clone, PartialEq, Debug)]
pub enum Item {
    Heading(String),
    /// with an id its text is changed in place
    Text { id: Option<String>, text: String },
    Readout { id: String, text: String, value: String },
    Pick {
        id: String,
        text: String,
        note: String,
        /// "green", "amber", "red" or "off"
        dot: Option<String>,
        selected: bool,
        /// its id can be an argument: only such an item can be tapped
        tappable: bool,
    },
    Button {
        id: String,
        text: String,
        /// "primary", "secondary" or "danger"
        style: Option<String>,
        tappable: bool,
    },
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Feed {
    pub items: Vec<Item>,
    /// items of the file beyond `ITEMS`
    pub more: usize,
    /// why there are no items, when the file could not be read as a feed
    pub error: Option<String>,
}

/// What tells one state of a file from another without reading it: its inode, size and the
/// time its inode last changed (its content, its owner or its mode). A file replaced by a
/// rename is another inode.
pub type Stamp = (u64, u64, i64, i64);

pub fn stamp(path: &str) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.ino(), meta.size(), meta.ctime(), meta.ctime_nsec()))
}

/// The kernel's word that a directory changed (inotify), so a feed file that is replaced
/// shows at once and a page with a feed costs no wakeup while nothing happens. One inotify
/// instance for the runner; a thread sleeps in its read and hands each event on.
pub struct Watcher {
    fd: i32,
}

/// What a watch told: the watch, and the name in its directory that changed. `None`: the
/// watch itself is gone or events were lost, so every file under it may have changed.
pub struct Watched {
    pub watch: i32,
    pub name: Option<String>,
}

extern "C" {
    fn inotify_init1(flags: i32) -> i32;
    fn inotify_add_watch(fd: i32, path: *const std::ffi::c_char, mask: u32) -> i32;
    fn inotify_rm_watch(fd: i32, watch: i32) -> i32;
    #[link_name = "read"]
    fn read_fd(fd: i32, bytes: *mut std::ffi::c_void, count: usize) -> isize;
}

/// A file written and closed, renamed into or out of the directory, made, removed, or
/// given another owner or mode.
const IN_FILE: u32 = 0x8 | 0x40 | 0x80 | 0x100 | 0x200 | 0x4;
/// The directory itself removed or moved, the watch dropped, events lost.
const IN_LOST: u32 = 0x400 | 0x800 | 0x4000 | 0x8000;

impl Watcher {
    /// `None` where the kernel gives no inotify instance: the runner then looks at its
    /// feeds on a timer, as slowly as it has to.
    pub fn new(mut hand_on: impl FnMut(Watched) + Send + 'static) -> Option<Watcher> {
        // IN_CLOEXEC: the tools the runner starts do not get it
        let fd = unsafe { inotify_init1(0o2000000) };
        if fd < 0 {
            return None;
        }
        let spawned = std::thread::Builder::new().name("tb-panel-watch".into()).spawn(move || {
            // struct inotify_event: watch, mask, cookie, the name's length, the name
            let mut bytes = [0u8; 4096];
            loop {
                let n = unsafe { read_fd(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
                if n <= 0 {
                    if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return;
                }
                let mut at = 0;
                while at + 16 <= n as usize {
                    let word = |i: usize| u32::from_ne_bytes([bytes[at + i], bytes[at + i + 1], bytes[at + i + 2], bytes[at + i + 3]]);
                    let (watch, mask, len) = (word(0) as i32, word(4), word(12) as usize);
                    let name = &bytes[(at + 16).min(n as usize)..(at + 16 + len).min(n as usize)];
                    let name = String::from_utf8_lossy(name.split(|b| *b == 0).next().unwrap_or_default()).into_owned();
                    at += 16 + len;
                    if mask & IN_LOST != 0 {
                        hand_on(Watched { watch, name: None });
                    } else if mask & IN_FILE != 0 {
                        hand_on(Watched { watch, name: Some(name) });
                    }
                }
            }
        });
        spawned.ok().map(|_| Watcher { fd })
    }

    /// Watches the directory of `file`. `None`: it cannot be watched (it is not there).
    pub fn watch_dir_of(&self, file: &str) -> Option<i32> {
        let dir = std::path::Path::new(file).parent()?;
        let dir = std::ffi::CString::new(dir.to_str()?).ok()?;
        // IN_ONLYDIR 0x01000000: a directory, nothing else
        let watch = unsafe { inotify_add_watch(self.fd, dir.as_ptr(), IN_FILE | 0x400 | 0x800 | 0x0100_0000) };
        (watch >= 0).then_some(watch)
    }

    pub fn unwatch(&self, watch: i32) {
        unsafe { inotify_rm_watch(self.fd, watch) };
    }
}

/// An id goes to the action as one argument. It is kept to characters that mean nothing to
/// any tool that takes it: letters, digits and `. _ @ : -`, and it does not start with `-`
/// (an argument that starts with one is an option to most tools).
pub fn id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= ID_BYTES && !id.starts_with('-') && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'@' | b':' | b'-'))
}

/// Reads a feed file. It never fails: a file that is not a feed is a `Feed` with its `error`.
pub fn read(path: &str) -> Feed {
    match parse_file(path) {
        Ok(feed) => feed,
        Err(why) => Feed { error: Some(why), ..Feed::default() },
    }
}

fn parse_file(path: &str) -> Result<Feed, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    // root's alone to write, as a panel file: its ids become arguments of root's action
    panel::roots_own(&meta)?;
    let mut bytes = Vec::new();
    (&mut file).take(FILE_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() > FILE_BYTES {
        return Err(format!("larger than {} KiB", FILE_BYTES / 1024));
    }
    let text = String::from_utf8(bytes).map_err(|_| "not UTF-8".to_string())?;
    parse(&Json::parse(&text)?)
}

pub fn parse(file: &Json) -> Result<Feed, String> {
    let members = file.as_obj().ok_or("a feed file is one JSON object")?;
    if let Some((key, _)) = members.iter().find(|(key, _)| key != "items") {
        return Err(format!("unknown field {key:?} (known: items)"));
    }
    let all = file.get("items").and_then(|r| r.as_arr()).ok_or("items is missing or not an array")?;
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (n, item) in all.iter().enumerate().take(ITEMS) {
        let members = item.as_obj().ok_or(format!("item {n}: not an object"))?;
        let kind = item.get("kind").and_then(|k| k.as_str()).ok_or(format!("item {n}: kind is missing"))?;
        let what = format!("item {n} ({kind})");
        let known = |names: &[&str]| match members.iter().find(|(key, _)| key != "kind" && !names.contains(&key.as_str())) {
            Some((key, _)) => Err(format!("{what}: unknown field {key:?} (known: {})", names.join(", "))),
            None => Ok(()),
        };
        let string = |name: &str| match item.get(name) {
            None => Ok(None),
            Some(value) => value.as_str().map(|s| Some(s.to_string())).ok_or(format!("{what}: {name} is not a string")),
        };
        let text = || string("text")?.ok_or(format!("{what}: text is missing"));
        // an id is unique in the feed; `plain`: it must be one that can be an argument
        let mut id = |required: bool, plain: bool| -> Result<Option<String>, String> {
            match string("id")? {
                None if required => Err(format!("{what}: id is missing")),
                None => Ok(None),
                Some(id) => {
                    if plain && !id_ok(&id) {
                        return Err(format!("{what}: id {id:?} is not 1 to {ID_BYTES} of letters, digits and . _ @ : -"));
                    }
                    if !seen.insert(id.clone()) {
                        return Err(format!("{what}: id {id:?} twice"));
                    }
                    Ok(Some(id))
                }
            }
        };
        items.push(match kind {
            "heading" => {
                known(&["text"])?;
                Item::Heading(text()?)
            }
            "text" => {
                known(&["id", "text"])?;
                Item::Text { id: id(false, true)?, text: text()? }
            }
            "readout" => {
                known(&["id", "text", "value"])?;
                Item::Readout { id: id(true, true)?.unwrap_or_default(), text: text()?, value: string("value")?.unwrap_or_default() }
            }
            "pick" => {
                known(&["id", "text", "note", "dot", "selected"])?;
                let id = id(true, false)?.unwrap_or_default();
                let dot = string("dot")?;
                if dot.as_deref().is_some_and(|dot| !["green", "amber", "red", "off"].contains(&dot)) {
                    return Err(format!("{what}: dot is green, amber, red or off"));
                }
                let selected = match item.get("selected") {
                    None => false,
                    Some(value) => value.as_bool().ok_or(format!("{what}: selected is not true or false"))?,
                };
                Item::Pick { tappable: id_ok(&id), id, text: text()?, note: string("note")?.unwrap_or_default(), dot, selected }
            }
            "button" => {
                // no confirm, no lock: a feed's button is one tap and nothing arms
                known(&["id", "text", "style"])?;
                let id = id(true, false)?.unwrap_or_default();
                let style = string("style")?;
                if style.as_deref().is_some_and(|style| !["primary", "secondary", "danger"].contains(&style)) {
                    return Err(format!("{what}: style is primary, secondary or danger"));
                }
                Item::Button { tappable: id_ok(&id), id, text: text()?, style }
            }
            other => return Err(format!("item {n}: unknown kind {other:?} (a feed takes heading, text, readout, pick, button)")),
        });
    }
    Ok(Feed { items, more: all.len().saturating_sub(ITEMS), error: None })
}

/// The id an item of a feed has on the page. Ids that start with `_` are the runner's own.
pub fn page_id(id: &str) -> String {
    format!("_feed:{id}")
}

/// A line of text in place of an item that cannot be tapped.
fn untappable(text: &str, note: &str) -> Json {
    let line = if note.is_empty() { format!("{text} (cannot be tapped: its id is not a plain name)") } else { format!("{text} · {note} (cannot be tapped: its id is not a plain name)") };
    Json::obj([("kind", "text".into()), ("text", line.into())])
}

impl Item {
    /// The item as the app takes it.
    pub fn json(&self) -> Json {
        match self {
            Item::Heading(text) => Json::obj([("kind", "heading".into()), ("text", text.as_str().into())]),
            Item::Text { id, text } => {
                let mut item = Json::obj([("kind", "text".into()), ("text", text.as_str().into())]);
                if let Some(id) = id {
                    item.set("id", page_id(id).into());
                }
                item
            }
            Item::Readout { id, text, value } => Json::obj([("kind", "readout".into()), ("id", page_id(id).into()), ("text", text.as_str().into()), ("value", value.as_str().into())]),
            Item::Pick { id, text, note, dot, selected, tappable: true } => {
                let mut item = Json::obj([("kind", "pick".into()), ("id", page_id(id).into()), ("text", text.as_str().into()), ("note", note.as_str().into()), ("selected", (*selected).into())]);
                if let Some(dot) = dot {
                    item.set("dot", dot.as_str().into());
                }
                item
            }
            Item::Button { id, text, style, tappable: true } => {
                // it does not lock its row: nothing would arm it again
                let mut item = Json::obj([("kind", "button".into()), ("id", page_id(id).into()), ("text", text.as_str().into()), ("lock", false.into())]);
                if let Some(style) = style {
                    item.set("style", style.as_str().into());
                }
                item
            }
            Item::Pick { text, note, .. } => untappable(text, note),
            Item::Button { text, .. } => untappable(text, ""),
        }
    }

    /// What decides which items the page has, and what of them an `update` cannot change:
    /// when this is the same for every item of two reads, the newer one is an `update` of
    /// the older one's items.
    fn shape(&self) -> (u8, String) {
        match self {
            Item::Heading(text) => (0, text.clone()),
            Item::Text { id: None, text } => (1, text.clone()),
            Item::Text { id: Some(id), .. } => (2, id.clone()),
            Item::Readout { id, .. } => (3, id.clone()),
            Item::Pick { id, tappable: true, dot, .. } => (if dot.is_some() { 4 } else { 5 }, id.clone()),
            // a button's text and style are not an update's to change
            Item::Button { id, text, style, tappable: true } => (6, format!("{id}\0{text}\0{}", style.as_deref().unwrap_or_default())),
            // drawn as text without an id: any change of it is another item
            Item::Pick { id, text, note, .. } => (7, format!("{id}\0{text}\0{note}")),
            Item::Button { id, text, .. } => (8, format!("{id}\0{text}")),
        }
    }
}

impl Feed {
    /// The items the feed is on the page, at most `cap` of them. `id`: the feed item's own
    /// id, for the lines the runner adds.
    pub fn page_items(&self, id: &str, cap: usize) -> Vec<Json> {
        let line = |tag: &str, text: String| Json::obj([("kind", "text".into()), ("id", format!("_feed_{tag}:{id}").into()), ("text", text.into())]);
        if let Some(why) = &self.error {
            return vec![line("error", format!("feed unavailable: {why}"))];
        }
        let mut items: Vec<Json> = self.items.iter().take(cap).map(Item::json).collect();
        let left_out = self.more + self.items.len().saturating_sub(cap);
        if left_out > 0 {
            items.push(line("more", format!("… {left_out} more not shown")));
        }
        if self.items.is_empty() {
            items.push(line("empty", "nothing here".into()));
        }
        items
    }

    /// The changes that turn the page of `old` into the page of `self`, as the members of an
    /// `update`'s items; `None` when the two do not have the same items (items came or went,
    /// a pick got or lost its dot, a button reads otherwise, an error came or went): the
    /// page is set again then.
    pub fn changes_from(&self, old: &Feed) -> Option<Vec<(String, Json)>> {
        if self.error != old.error || self.more != old.more || self.items.len() != old.items.len() {
            return None;
        }
        if self.items.iter().zip(&old.items).any(|(new, old)| new.shape() != old.shape()) {
            return None;
        }
        let mut changes = Vec::new();
        for (new, old) in self.items.iter().zip(&old.items) {
            let mut change = Json::Obj(Vec::new());
            let mut differs = |name: &str, new: &str, old: &str| {
                if new != old {
                    change.set(name, new.into());
                }
            };
            let id = match (new, old) {
                (Item::Text { id: Some(id), text }, Item::Text { text: was, .. }) => {
                    differs("text", text, was);
                    id
                }
                (Item::Readout { id, text, value }, Item::Readout { text: was, value: had, .. }) => {
                    differs("text", text, was);
                    differs("value", value, had);
                    id
                }
                (Item::Pick { id, text, note, dot, selected, tappable: true }, Item::Pick { text: was, note: had, dot: old_dot, selected: old_selected, .. }) => {
                    differs("text", text, was);
                    differs("note", note, had);
                    if let (Some(dot), Some(old_dot)) = (dot, old_dot) {
                        differs("dot", dot, old_dot);
                    }
                    if selected != old_selected {
                        change.set("selected", (*selected).into());
                    }
                    id
                }
                // the same shape says everything of the other kinds
                _ => continue,
            };
            if change.as_obj().is_some_and(|members| !members.is_empty()) {
                changes.push((page_id(id), change));
            }
        }
        Some(changes)
    }

    /// Makes the pick `id` the selected one, as its tap most likely will: the changes for the
    /// page, so the tap is seen at once. The next read of the file says what is true.
    pub fn select(&mut self, id: &str) -> Vec<(String, Json)> {
        if !self.items.iter().any(|item| matches!(item, Item::Pick { id: this, tappable: true, .. } if this == id)) {
            return Vec::new();
        }
        let mut changes = Vec::new();
        for item in &mut self.items {
            if let Item::Pick { id: this, selected, tappable: true, .. } = item {
                if *selected != (this == id) {
                    *selected = this == id;
                    changes.push((page_id(this), Json::obj([("selected", (*selected).into())])));
                }
            }
        }
        changes
    }

    /// Whether `id` is an item of this feed that can be tapped.
    pub fn taps(&self, id: &str) -> bool {
        self.items.iter().any(|item| matches!(item, Item::Pick { id: this, tappable: true, .. } | Item::Button { id: this, tappable: true, .. } if this == id))
    }
}
