//! Virtualized `PortalList`-backed feed widget (`ChatFeed` / `VirtualFeed`) for `tb-shell`.
//!
//! Solves the scroll bottlenecks on mobile TBDR GPUs (PowerVR GE8320):
//! 1. **Viewport Virtualization (`O(viewport)`):** Wraps Makepad's `PortalList` (`auto_tail`,
//!    `keep_end_on_resize`, `keep_nearby_items`, `reuse_items`) so only the rows intersecting
//!    the viewport run layout/draw on a scroll frame, whether the feed holds 18 or 1,000+
//!    messages; rows leaving the screen are recycled by template.
//! 2. **One row per Markdown block:** each message is split once (cached by content hash) at
//!    its top-level blocks (paragraphs, lists, tables, quotes, fenced code; small neighbours
//!    merged) into seamless `Single` / `Top` / `Mid` / `Bot` `RoundedYView` slices (all
//!    `new_batch: false`), so a row's layout cost is one block's, however long the message.
//! 3. **Rows keep their place:** every row has a key (its message's event id and its block
//!    index). An update is diffed by key into `PortalList::splice_items` edits, so when older
//!    messages leave the front of the window or new ones arrive at the end, the rows on screen
//!    keep their ids, widgets and measured heights: nothing under the reader jumps.
//! 4. **Plain-Text Fast Path + Hash-Gated Binding:** Rows with no Markdown syntax route to
//!    single-walk `Label` templates (0 `pulldown-cmark` parses, 1 `TurtleWalk`), Markdown
//!    rows route to `Markdown{ use_math_widget: false }`, and `existed` rows skip `set_text`
//!    completely whenever their 64-bit row hash already matches `applied_hashes`.
//!
//! Data sources:
//! - `store_key: "chat"` (default): reads `tb-shell`'s Rust store directly with zero Splash
//!   JSON serialization (`{ msgs: [...], hidden_before, agent }` or any `[...]` array).
//! - `items: [...]` in `.splash`: generic Splash array fallback (`VirtualFeed{ items: rows }`).

use makepad_micro_serde::JsonValue;
use makepad_widgets::makepad_script::ScriptFnRef;
use makepad_widgets::portal_list::{PortalList, PortalListWidgetRefExt};
use makepad_widgets::widget_async::CxWidgetToScriptCallExt;
use makepad_widgets::widget_tree::CxWidgetExt;
use makepad_widgets::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

thread_local! {
    /// UI-thread mirror of top-level store entries (`key -> (version, Rc<JsonValue>)`).
    static FEED_STORE: RefCell<HashMap<String, (u64, Rc<JsonValue>)>> = RefCell::new(HashMap::new());
    static NEXT_VER: RefCell<u64> = const { RefCell::new(1) };
}

pub fn publish_store(key: &str, value: &JsonValue) {
    let ver = NEXT_VER.with(|n| {
        let mut n = n.borrow_mut();
        let v = *n;
        *n = n.wrapping_add(1);
        v
    });
    FEED_STORE.with(|m| {
        m.borrow_mut().insert(key.to_string(), (ver, Rc::new(value.clone())));
    });
}

pub fn remove_store(key: &str) {
    let ver = NEXT_VER.with(|n| {
        let mut n = n.borrow_mut();
        let v = *n;
        *n = n.wrapping_add(1);
        v
    });
    FEED_STORE.with(|m| {
        m.borrow_mut().insert(key.to_string(), (ver, Rc::new(JsonValue::Null)));
    });
}

fn read_store(key: &str, seen: u64) -> Option<(u64, Rc<JsonValue>)> {
    FEED_STORE.with(|m| {
        m.borrow()
            .get(key)
            .filter(|(v, _)| *v != seen)
            .map(|(v, val)| (*v, Rc::clone(val)))
    })
}

fn fnv1a(bytes: &[u8], mut h: u64) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

const FNV_INIT: u64 = 0xcbf29ce484222325;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RoleKind {
    You,
    Tool,
    Say,
}

impl RoleKind {
    fn from_str(s: &str) -> Self {
        match s {
            "you" | "user" | "prompt" => Self::You,
            "tool" | "meta" | "dim" => Self::Tool,
            _ => Self::Say,
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::You => 1,
            Self::Tool => 2,
            Self::Say => 3,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlicePos {
    Single,
    Top,
    Mid,
    Bot,
}

impl SlicePos {
    fn tag(self) -> u8 {
        match self {
            Self::Single => 1,
            Self::Top => 2,
            Self::Mid => 3,
            Self::Bot => 4,
        }
    }
}

#[derive(Clone)]
struct FeedMsg {
    /// The message's identity across updates: its event id, else a hash of what it says.
    id: String,
    role: RoleKind,
    hdr: String,
    text: String,
    msg_index: usize,
}

#[derive(Clone)]
struct FlatRow {
    /// The row's identity across updates (its message's id and its place in the message).
    key: u64,
    role: RoleKind,
    pos: SlicePos,
    is_md: bool,
    hdr: String,
    body: String,
    msg_index: usize,
    hash: u64,
}

impl FlatRow {
    fn template_id(&self) -> LiveId {
        match (self.role, self.pos, self.is_md) {
            (RoleKind::You, SlicePos::Single, false) => id!(YouSinglePlain),
            (RoleKind::You, SlicePos::Single, true) => id!(YouSingleMd),
            (RoleKind::You, SlicePos::Top, false) => id!(YouTopPlain),
            (RoleKind::You, SlicePos::Top, true) => id!(YouTopMd),
            (RoleKind::You, SlicePos::Mid, false) => id!(YouMidPlain),
            (RoleKind::You, SlicePos::Mid, true) => id!(YouMidMd),
            (RoleKind::You, SlicePos::Bot, false) => id!(YouBotPlain),
            (RoleKind::You, SlicePos::Bot, true) => id!(YouBotMd),

            (RoleKind::Tool, SlicePos::Single, false) => id!(ToolSinglePlain),
            (RoleKind::Tool, SlicePos::Single, true) => id!(ToolSingleMd),
            (RoleKind::Tool, SlicePos::Top, false) => id!(ToolTopPlain),
            (RoleKind::Tool, SlicePos::Top, true) => id!(ToolTopMd),
            (RoleKind::Tool, SlicePos::Mid, false) => id!(ToolMidPlain),
            (RoleKind::Tool, SlicePos::Mid, true) => id!(ToolMidMd),
            (RoleKind::Tool, SlicePos::Bot, false) => id!(ToolBotPlain),
            (RoleKind::Tool, SlicePos::Bot, true) => id!(ToolBotMd),

            (RoleKind::Say, SlicePos::Single, false) => id!(SaySinglePlain),
            (RoleKind::Say, SlicePos::Single, true) => id!(SaySingleMd),
            (RoleKind::Say, SlicePos::Top, false) => id!(SayTopPlain),
            (RoleKind::Say, SlicePos::Top, true) => id!(SayTopMd),
            (RoleKind::Say, SlicePos::Mid, false) => id!(SayMidPlain),
            (RoleKind::Say, SlicePos::Mid, true) => id!(SayMidMd),
            (RoleKind::Say, SlicePos::Bot, false) => id!(SayBotPlain),
            (RoleKind::Say, SlicePos::Bot, true) => id!(SayBotMd),
        }
    }

    fn has_header(&self) -> bool {
        matches!(self.pos, SlicePos::Single | SlicePos::Top)
    }
}

/// Fast syntax check: returns `false` when `s` is guaranteed plain prose with zero Markdown
/// constructs, allowing `ChatFeed` to route it to a single-walk `Label` instead of `Markdown`.
fn needs_markdown(s: &str) -> bool {
    let bytes = s.as_bytes();
    for &b in bytes {
        if matches!(b, b'`' | b'*' | b'_' | b'~' | b'[' | b'|' | b'<' | b'\\') {
            return true;
        }
    }
    for line in s.lines() {
        let t = line.trim_start();
        if t.is_empty() {
            continue;
        }
        let b0 = t.as_bytes()[0];
        if b0 == b'#' || b0 == b'>' {
            return true;
        }
        if t.starts_with("- ") || t.starts_with("+ ") || t == "---" {
            return true;
        }
        if b0.is_ascii_digit() {
            let digits = t.bytes().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 && digits <= 6 {
                let rest = &t[digits..];
                if rest.starts_with(". ") || rest.starts_with(") ") {
                    return true;
                }
            }
        }
    }
    false
}

/// The run of 3+ backticks or tildes that opens a fence, as written (a fence opened with
/// four closes with four).
fn fence_marker(trimmed: &str) -> Option<&str> {
    let c = *trimmed.as_bytes().first()?;
    if c != b'`' && c != b'~' {
        return None;
    }
    let n = trimmed.bytes().take_while(|b| *b == c).count();
    (n >= 3).then(|| &trimmed[..n])
}

/// Whether `trimmed` closes the fence `marker` opened: the same character, at least as many,
/// nothing after them.
fn closes_fence(trimmed: &str, marker: &str) -> bool {
    let c = marker.as_bytes()[0];
    let n = trimmed.bytes().take_while(|b| *b == c).count();
    n >= marker.len() && trimmed[n..].trim().is_empty()
}

/// An ATX heading: 1 to 6 `#` and then a space or the line's end (`#123` is not one).
fn is_heading(trimmed: &str) -> bool {
    let n = trimmed.bytes().take_while(|b| *b == b'#').count();
    (1..=6).contains(&n) && trimmed[n..].chars().next().map_or(true, char::is_whitespace)
}

/// Rows smaller than this are merged with the next block of the same message, so a reply of
/// many one-line paragraphs isn't one row per line (each row costs a card and a widget).
const MERGE_BELOW_BYTES: usize = 360;
/// A fenced code block longer than this many lines continues in the next row.
const CODE_ROW_LINES: usize = 40;

/// Splits a message into its rows: one per top-level Markdown block (a paragraph, a list, a
/// table, a quote, a fenced code block; a heading stays with the block after it), small
/// neighbours merged up to `MERGE_BELOW_BYTES`. The list then recycles and lays out what is
/// on screen block by block, whatever the message's length: a row's cost is bounded by its
/// block, not by the message. Only a block with no boundary of its own is cut further: code
/// every `CODE_ROW_LINES` lines (the fence reopened with its language), and a wall of text
/// at a line end once it passes `wall_bytes`.
fn split_message_rows(text: &str, wall_bytes: usize) -> Vec<(String, bool)> {
    let wall = wall_bytes.max(600);
    let mut blocks: Vec<String> = Vec::new();
    let mut cur = String::new();
    // (marker, opening line) of the fence `cur` is inside, and the fence's indent: a fence
    // under a list item stands alone in its row, so its lines lose that indent.
    let mut fence: Option<(String, String)> = None;
    let mut fence_indent = 0usize;
    let dedent = |line: &'_ str, n: usize| -> String {
        let spaces = line.bytes().take(n).take_while(|b| *b == b' ').count();
        line[spaces..].to_string()
    };
    let mut fence_lines = 0usize;
    let mut heading_pending = false;
    // A wall of text passed `wall`: the row ends before the next line, unless that line
    // continues something by indent (it would render as code at the start of a row).
    let mut cut_pending = false;

    let flush = |cur: &mut String, blocks: &mut Vec<String>| {
        let t = cur.trim_matches(['\n', '\r']);
        if !t.trim().is_empty() {
            blocks.push(t.to_string());
        }
        cur.clear();
    };

    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if let Some((marker, open)) = fence.as_ref() {
            cur.push_str(&dedent(line, fence_indent));
            if closes_fence(trimmed, marker) {
                fence = None;
                flush(&mut cur, &mut blocks);
                continue;
            }
            fence_lines += 1;
            if fence_lines >= CODE_ROW_LINES {
                let (marker, open) = (marker.clone(), open.clone());
                if !cur.ends_with('\n') {
                    cur.push('\n');
                }
                cur.push_str(&marker);
                flush(&mut cur, &mut blocks);
                cur.push_str(&open);
                cur.push('\n');
                fence_lines = 0;
            }
            continue;
        }
        if let Some(marker) = fence_marker(trimmed) {
            if !heading_pending {
                flush(&mut cur, &mut blocks);
            }
            heading_pending = false;
            cut_pending = false;
            fence = Some((marker.to_string(), trimmed.trim_end_matches(['\n', '\r']).to_string()));
            fence_indent = line.len() - trimmed.len();
            fence_lines = 0;
            if !cur.is_empty() && !cur.ends_with('\n') {
                cur.push('\n');
            }
            cur.push_str(trimmed);
            continue;
        }
        if trimmed.trim().is_empty() {
            // A blank line ends a block, unless a heading waits for its block.
            if !heading_pending {
                flush(&mut cur, &mut blocks);
                cut_pending = false;
            }
            continue;
        }
        if cut_pending && !line.starts_with("    ") && !line.starts_with('\t') {
            flush(&mut cur, &mut blocks);
            cut_pending = false;
        }
        let heading = is_heading(trimmed);
        if heading && !heading_pending {
            flush(&mut cur, &mut blocks);
            cut_pending = false;
            heading_pending = true;
        } else if !heading {
            heading_pending = false;
        }
        cur.push_str(line);
        if cur.len() >= wall && line.ends_with('\n') && !trimmed.starts_with('|') {
            cut_pending = true;
        }
    }
    flush(&mut cur, &mut blocks);
    if blocks.is_empty() {
        return vec![(String::new(), false)];
    }

    // Merge small neighbours; a fenced block's lines stay whole, so merging joins at block
    // edges only, with the blank line Markdown needs between them.
    let mut rows: Vec<String> = Vec::with_capacity(blocks.len());
    for b in blocks {
        match rows.last_mut() {
            Some(last) if last.len() + b.len() < MERGE_BELOW_BYTES => {
                last.push_str("\n\n");
                last.push_str(&b);
            }
            _ => rows.push(b),
        }
    }
    rows.into_iter()
        .map(|s| {
            let md = needs_markdown(&s);
            (s, md)
        })
        .collect()
}

/// The list edits that turn the rows `old` into `new` (each a row key), as
/// `PortalList::splice_items` takes them, applied in order: `(at, removed, added)` in list
/// ids, the history bar (`old_bar`/`new_bar`) at id 0. Rows after an edit keep their widget
/// and measured height under their new id, so the rows on screen stay where they are when
/// older ones leave the front or newer ones arrive at the end. A grown end is left to
/// `set_item_range` (it follows the end when the list was tailing). `None`: nothing in
/// common, start over.
fn plan_splices(old: &[u64], new: &[u64], old_bar: bool, new_bar: bool) -> Option<Vec<(usize, usize, usize)>> {
    if old.is_empty() || new.is_empty() {
        return None;
    }
    let mut ops = Vec::new();
    match (old_bar, new_bar) {
        (true, false) => ops.push((0, 1, 0)),
        (false, true) => ops.push((0, 0, 1)),
        _ => {}
    }
    let off = usize::from(new_bar);
    let (mut a, mut b) = (0usize, 0usize);
    if old[0] != new[0] {
        if let Some(j) = old.iter().position(|k| *k == new[0]) {
            ops.push((off, j, 0)); // older rows left the front
            a = j;
        } else if let Some(j) = new.iter().position(|k| *k == old[0]) {
            ops.push((off, 0, j)); // earlier rows came in at the front
            b = j;
        } else {
            return None;
        }
    }
    let (oa, nb) = (&old[a..], &new[b..]);
    let p = oa.iter().zip(nb).take_while(|(x, y)| x == y).count();
    let max_s = oa.len().min(nb.len()) - p;
    let s = oa.iter().rev().zip(nb.iter().rev()).take(max_s).take_while(|(x, y)| x == y).count();
    let removed = oa.len() - p - s;
    let added = nb.len() - p - s;
    let at = off + b + p;
    if s == 0 {
        // The end changed: the ids both sides have are rebound in place; extra old rows go,
        // extra new ones come with the range.
        if removed > added {
            ops.push((at + added, removed - added, 0));
        }
    } else if removed > 0 || added > 0 {
        ops.push((at, removed, added));
    }
    Some(ops)
}

script_mod! {
    use mod.prelude.widgets_internal.*
    use mod.widgets.*

    mod.widgets.ChatFeedMdBody = mod.widgets.Markdown{
        width: Fill height: Fit
        padding: Inset{top: 0 right: 0 bottom: 3 left: 0}
        font_size: 15.0
        font_color: #xeeeeee
        paragraph_spacing: 8
        pre_code_spacing: 6
        heading_base_scale: 1.35
        use_math_widget: false
        draw_text +: { color: #xeeeeee }
        text_style_normal +: { font_size: 15.0 }
        text_style_italic +: { font_size: 15.0 }
        text_style_bold +: { font_size: 15.0 }
        text_style_bold_italic +: { font_size: 15.0 }
        text_style_fixed +: { font_size: 13.5 }
        code_layout +: { padding: Inset{left: 10 right: 10 top: 8 bottom: 8} }
        quote_layout +: { padding: Inset{left: 10 right: 10 top: 6 bottom: 6} }
        draw_block +: {
            code_color: #x101520
            quote_bg_color: #x151d2b
            quote_fg_color: #x93c5fd
            table_header_bg_color: #x232f46
            table_border_color: #x334155
            sep_color: #x2d3748
        }
    }

    mod.widgets.ChatFeedToolMdBody = mod.widgets.ChatFeedMdBody{
        font_size: 13.5
        font_color: #xb8c2d1
        draw_text +: { color: #xb8c2d1 }
        text_style_normal +: { font_size: 13.5 }
        text_style_italic +: { font_size: 13.5 }
        text_style_bold +: { font_size: 13.5 }
        text_style_bold_italic +: { font_size: 13.5 }
        text_style_fixed +: { font_size: 12.5 }
    }

    mod.widgets.ChatFeedPlainBody = mod.widgets.Label{
        width: Fill height: Fit
        padding: 0
        draw_text +: {
            color: #xeeeeee
            text_style +: { font_size: 15.0 line_spacing: 1.25 }
        }
    }

    mod.widgets.ChatFeedToolPlainBody = mod.widgets.Label{
        width: Fill height: Fit
        padding: 0
        draw_text +: {
            color: #xb8c2d1
            text_style +: { font_size: 13.5 line_spacing: 1.22 }
        }
    }

    mod.widgets.ChatFeedHdr = mod.widgets.Label{
        width: Fill height: Fit
        padding: 0
        draw_text +: {
            color: #x3ddc84
            text_style +: { font_size: 13.0 }
        }
    }

    mod.widgets.ChatCardBase = mod.widgets.RoundedYView{
        width: Fill height: Fit flow: Down spacing: 4
        new_batch: false show_bg: true clip_x: false clip_y: false
        margin: Inset{top: 0 bottom: 8 left: 0 right: 0}
        padding: Inset{top: 9 bottom: 9 left: 13 right: 13}
        draw_bg.color: #x1c2230
        draw_bg.border_radius: vec2(15.0 15.0)
    }

    mod.widgets.ChatFeedList = mod.widgets.PortalList{
        width: Fill height: Fill
        flow: Down
        auto_tail: true
        keep_end_on_resize: true
        keep_nearby_items: true
        reuse_items: true
        drag_scrolling: true
        grab_key_focus: false
        bounce_at_start: true
        bounce_at_end: true

        Empty := mod.widgets.View{
            width: Fill height: Fit
            padding: Inset{top: 18 bottom: 18 left: 4 right: 4}
            note := mod.widgets.Label{
                width: Fill height: Fit padding: 0
                draw_text +: { color: #x9aa4b2 text_style +: { font_size: 13.0 } }
                text: "No messages yet."
            }
        }

        HistoryBar := mod.widgets.View{
            width: Fill height: Fit flow: Right spacing: 6
            margin: Inset{bottom: 8}
            more_btn := mod.widgets.ButtonFlat{
                width: Fill height: 36 margin: 0
                draw_bg.color: #x1c2230 draw_bg.color_focus: #x1c2230 draw_bg.color_hover: #x263246
                draw_bg.border_radius: 10.0
                draw_text.color: #xeeeeee draw_text.text_style.font_size: 14.0
                text: "Show earlier"
            }
            all_btn := mod.widgets.ButtonFlat{
                width: Fit height: 36 margin: 0
                padding: Inset{left: 14 right: 14 top: 0 bottom: 0}
                draw_bg.color: #x1c2230 draw_bg.color_focus: #x1c2230 draw_bg.color_hover: #x263246
                draw_bg.border_radius: 10.0
                draw_text.color: #xeeeeee draw_text.text_style.font_size: 14.0
                text: "Show all"
            }
        }

        // --- SAY (agent / default card) ---
        SaySinglePlain := mod.widgets.ChatCardBase{
            hdr := mod.widgets.ChatFeedHdr{ draw_text +: { color: #x3ddc84 } }
            body := mod.widgets.ChatFeedPlainBody{}
        }
        SaySingleMd := mod.widgets.ChatCardBase{
            hdr := mod.widgets.ChatFeedHdr{ draw_text +: { color: #x3ddc84 } }
            body := mod.widgets.ChatFeedMdBody{}
        }
        SayTopPlain := mod.widgets.ChatCardBase{
            margin: Inset{bottom: 1} padding: Inset{top: 9 bottom: 5 left: 13 right: 13}
            draw_bg.border_radius: vec2(15.0 2.0)
            hdr := mod.widgets.ChatFeedHdr{ draw_text +: { color: #x3ddc84 } }
            body := mod.widgets.ChatFeedPlainBody{}
        }
        SayTopMd := mod.widgets.ChatCardBase{
            margin: Inset{bottom: 1} padding: Inset{top: 9 bottom: 5 left: 13 right: 13}
            draw_bg.border_radius: vec2(15.0 2.0)
            hdr := mod.widgets.ChatFeedHdr{ draw_text +: { color: #x3ddc84 } }
            body := mod.widgets.ChatFeedMdBody{}
        }
        SayMidPlain := mod.widgets.ChatCardBase{
            margin: Inset{bottom: 1} padding: Inset{top: 5 bottom: 5 left: 13 right: 13}
            draw_bg.border_radius: vec2(2.0 2.0)
            body := mod.widgets.ChatFeedPlainBody{}
        }
        SayMidMd := mod.widgets.ChatCardBase{
            margin: Inset{bottom: 1} padding: Inset{top: 5 bottom: 5 left: 13 right: 13}
            draw_bg.border_radius: vec2(2.0 2.0)
            body := mod.widgets.ChatFeedMdBody{}
        }
        SayBotPlain := mod.widgets.ChatCardBase{
            margin: Inset{bottom: 8} padding: Inset{top: 5 bottom: 9 left: 13 right: 13}
            draw_bg.border_radius: vec2(2.0 15.0)
            body := mod.widgets.ChatFeedPlainBody{}
        }
        SayBotMd := mod.widgets.ChatCardBase{
            margin: Inset{bottom: 8} padding: Inset{top: 5 bottom: 9 left: 13 right: 13}
            draw_bg.border_radius: vec2(2.0 15.0)
            body := mod.widgets.ChatFeedMdBody{}
        }

        // --- YOU (user prompt bubble) ---
        YouSinglePlain := mod.widgets.ChatCardBase{
            margin: Inset{left: 22 bottom: 8} draw_bg.color: #x1e2c42
            hdr := mod.widgets.ChatFeedHdr{ draw_text +: { color: #x7db3ff } }
            body := mod.widgets.ChatFeedPlainBody{}
        }
        YouSingleMd := mod.widgets.ChatCardBase{
            margin: Inset{left: 22 bottom: 8} draw_bg.color: #x1e2c42
            hdr := mod.widgets.ChatFeedHdr{ draw_text +: { color: #x7db3ff } }
            body := mod.widgets.ChatFeedMdBody{}
        }
        YouTopPlain := mod.widgets.ChatCardBase{
            margin: Inset{left: 22 bottom: 1} padding: Inset{top: 9 bottom: 5 left: 13 right: 13}
            draw_bg.color: #x1e2c42 draw_bg.border_radius: vec2(15.0 2.0)
            hdr := mod.widgets.ChatFeedHdr{ draw_text +: { color: #x7db3ff } }
            body := mod.widgets.ChatFeedPlainBody{}
        }
        YouTopMd := mod.widgets.ChatCardBase{
            margin: Inset{left: 22 bottom: 1} padding: Inset{top: 9 bottom: 5 left: 13 right: 13}
            draw_bg.color: #x1e2c42 draw_bg.border_radius: vec2(15.0 2.0)
            hdr := mod.widgets.ChatFeedHdr{ draw_text +: { color: #x7db3ff } }
            body := mod.widgets.ChatFeedMdBody{}
        }
        YouMidPlain := mod.widgets.ChatCardBase{
            margin: Inset{left: 22 bottom: 1} padding: Inset{top: 5 bottom: 5 left: 13 right: 13}
            draw_bg.color: #x1e2c42 draw_bg.border_radius: vec2(2.0 2.0)
            body := mod.widgets.ChatFeedPlainBody{}
        }
        YouMidMd := mod.widgets.ChatCardBase{
            margin: Inset{left: 22 bottom: 1} padding: Inset{top: 5 bottom: 5 left: 13 right: 13}
            draw_bg.color: #x1e2c42 draw_bg.border_radius: vec2(2.0 2.0)
            body := mod.widgets.ChatFeedMdBody{}
        }
        YouBotPlain := mod.widgets.ChatCardBase{
            margin: Inset{left: 22 bottom: 8} padding: Inset{top: 5 bottom: 9 left: 13 right: 13}
            draw_bg.color: #x1e2c42 draw_bg.border_radius: vec2(2.0 15.0)
            body := mod.widgets.ChatFeedPlainBody{}
        }
        YouBotMd := mod.widgets.ChatCardBase{
            margin: Inset{left: 22 bottom: 8} padding: Inset{top: 5 bottom: 9 left: 13 right: 13}
            draw_bg.color: #x1e2c42 draw_bg.border_radius: vec2(2.0 15.0)
            body := mod.widgets.ChatFeedMdBody{}
        }

        // --- TOOL (compact muted card) ---
        ToolSinglePlain := mod.widgets.ChatCardBase{
            spacing: 3 margin: Inset{bottom: 8} padding: Inset{top: 7 bottom: 7 left: 11 right: 11}
            draw_bg.color: #x151923 draw_bg.border_radius: vec2(12.0 12.0)
            hdr := mod.widgets.ChatFeedHdr{ draw_text +: { color: #x8b95a5 } }
            body := mod.widgets.ChatFeedToolPlainBody{}
        }
        ToolSingleMd := mod.widgets.ChatCardBase{
            spacing: 3 margin: Inset{bottom: 8} padding: Inset{top: 7 bottom: 7 left: 11 right: 11}
            draw_bg.color: #x151923 draw_bg.border_radius: vec2(12.0 12.0)
            hdr := mod.widgets.ChatFeedHdr{ draw_text +: { color: #x8b95a5 } }
            body := mod.widgets.ChatFeedToolMdBody{}
        }
        ToolTopPlain := mod.widgets.ChatCardBase{
            spacing: 3 margin: Inset{bottom: 1} padding: Inset{top: 7 bottom: 4 left: 11 right: 11}
            draw_bg.color: #x151923 draw_bg.border_radius: vec2(12.0 2.0)
            hdr := mod.widgets.ChatFeedHdr{ draw_text +: { color: #x8b95a5 } }
            body := mod.widgets.ChatFeedToolPlainBody{}
        }
        ToolTopMd := mod.widgets.ChatCardBase{
            spacing: 3 margin: Inset{bottom: 1} padding: Inset{top: 7 bottom: 4 left: 11 right: 11}
            draw_bg.color: #x151923 draw_bg.border_radius: vec2(12.0 2.0)
            hdr := mod.widgets.ChatFeedHdr{ draw_text +: { color: #x8b95a5 } }
            body := mod.widgets.ChatFeedToolMdBody{}
        }
        ToolMidPlain := mod.widgets.ChatCardBase{
            spacing: 3 margin: Inset{bottom: 1} padding: Inset{top: 4 bottom: 4 left: 11 right: 11}
            draw_bg.color: #x151923 draw_bg.border_radius: vec2(2.0 2.0)
            body := mod.widgets.ChatFeedToolPlainBody{}
        }
        ToolMidMd := mod.widgets.ChatCardBase{
            spacing: 3 margin: Inset{bottom: 1} padding: Inset{top: 4 bottom: 4 left: 11 right: 11}
            draw_bg.color: #x151923 draw_bg.border_radius: vec2(2.0 2.0)
            body := mod.widgets.ChatFeedToolMdBody{}
        }
        ToolBotPlain := mod.widgets.ChatCardBase{
            spacing: 3 margin: Inset{bottom: 8} padding: Inset{top: 4 bottom: 7 left: 11 right: 11}
            draw_bg.color: #x151923 draw_bg.border_radius: vec2(2.0 12.0)
            body := mod.widgets.ChatFeedToolPlainBody{}
        }
        ToolBotMd := mod.widgets.ChatCardBase{
            spacing: 3 margin: Inset{bottom: 8} padding: Inset{top: 4 bottom: 7 left: 11 right: 11}
            draw_bg.color: #x151923 draw_bg.border_radius: vec2(2.0 12.0)
            body := mod.widgets.ChatFeedToolMdBody{}
        }
    }

    mod.widgets.ChatFeedBase = #(ChatFeed::register_widget(vm))
    mod.widgets.ChatFeed = set_type_default() do mod.widgets.ChatFeedBase{
        width: Fill
        height: Fill
        store_key: "chat"
        empty_text: "No messages yet."
        split_bytes: 2200
    }
    mod.widgets.VirtualFeed = mod.widgets.ChatFeed{}
}

#[derive(Script, WidgetRef, WidgetRegister)]
pub struct ChatFeed {
    #[source]
    source: ScriptObjectRef,
    #[uid]
    uid: WidgetUid,
    #[walk]
    walk: Walk,

    #[live]
    store_key: ArcStringMut,
    #[live]
    items: ScriptValue,
    #[live]
    empty_text: ArcStringMut,
    #[live(2200usize)]
    split_bytes: usize,
    #[live]
    on_more: ScriptFnRef,
    #[live]
    on_all: ScriptFnRef,
    #[live]
    on_item_tap: ScriptFnRef,

    #[rust]
    list: WidgetRef,
    #[rust]
    list_heap_key: usize,
    #[rust]
    seen_store_ver: u64,
    #[rust]
    has_inline_items: bool,

    #[rust]
    agent_id: String,
    #[rust]
    hidden_before: usize,
    #[rust]
    rows: Vec<FlatRow>,
    #[rust]
    rows_fingerprint: u64,
    #[rust]
    pending_range_sync: bool,
    #[rust]
    pending_reset_tail: bool,
    /// List edits (`plan_splices`) the next draw applies before it sets the range.
    #[rust]
    pending_splices: Vec<(usize, usize, usize)>,

    /// Per-message split cache: `msg_hash -> Rc<Vec<(String, bool)>>` so untouched messages
    /// are never re-scanned or re-split when new messages arrive.
    #[rust]
    split_cache: HashMap<u64, Rc<Vec<(String, bool)>>>,
    /// Per-instantiated row `WidgetUid -> row.hash` so `existed` rows skip `set_text` on scroll.
    #[rust]
    applied_hashes: HashMap<WidgetUid, u64>,
    #[rust]
    press_pos: Option<DVec2>,
}

impl ScriptHook for ChatFeed {
    fn on_after_apply(
        &mut self,
        vm: &mut ScriptVm,
        _apply: &Apply,
        _scope: &mut Scope,
        value: ScriptValue,
    ) {
        let hk = vm.heap_key();
        if self.list.is_empty() || self.list_heap_key != hk {
            let list_val = script_eval!(vm, { mod.widgets.ChatFeedList{} });
            let list_ref = WidgetRef::script_from_value(vm, list_val);
            vm.cx_mut()
                .widget_tree_insert_child(self.uid, live_id!(list), list_ref.clone());
            self.list = list_ref;
            self.list_heap_key = hk;
            self.applied_hashes.clear();
            self.pending_range_sync = true;
            self.pending_reset_tail = true;
        }

        // Optional generic `VirtualFeed{ items: [...] }` inline Splash array support.
        let _ = value;
        let items_val = self.items;
        if !items_val.is_nil() {
            let mut raw_elems: Vec<ScriptValue> = Vec::new();
            let mut had_collection = false;
            if let Some(arr) = items_val.as_array() {
                had_collection = true;
                let len = vm.array_len(arr);
                raw_elems.reserve(len);
                for idx in 0..len {
                    raw_elems.push(vm.array_index_trap(arr, idx));
                }
            } else if let Some(arr_obj) = items_val.as_object() {
                if arr_obj != ScriptObject::ZERO {
                    had_collection = true;
                    vm.vec_with(arr_obj, |_vm, vec| {
                        raw_elems.extend(vec.iter().map(|kv| kv.value));
                    });
                }
            }
            if had_collection {
                let mut parsed = Vec::with_capacity(raw_elems.len());
                for (idx, elem) in raw_elems.into_iter().enumerate() {
                    if let Some(item_obj) = elem.as_object() {
                        let get_str = |vm: &mut ScriptVm, k: LiveId| -> Option<String> {
                            let v = vm.value(item_obj, k);
                            if v.is_nil() || v.is_err() {
                                let ks = vm.new_string(&k.to_string());
                                let v2 = vm.value(item_obj, ks);
                                (!v2.is_nil() && !v2.is_err())
                                    .then(|| vm.cast_to_owned_string(v2, ""))
                                    .flatten()
                            } else {
                                vm.cast_to_owned_string(v, "")
                            }
                        };
                        let role_s = get_str(vm, live_id!(role)).unwrap_or_default();
                        let label = get_str(vm, live_id!(label))
                            .or_else(|| get_str(vm, live_id!(title)))
                            .unwrap_or_default();
                        let ts = get_str(vm, live_id!(ts)).unwrap_or_default();
                        let hdr = if label.is_empty() && ts.is_empty() {
                            role_s.clone()
                        } else if ts.is_empty() {
                            label
                        } else if label.is_empty() {
                            ts
                        } else {
                            format!("{label}  {ts}")
                        };
                        let text = get_str(vm, live_id!(text))
                            .or_else(|| get_str(vm, live_id!(body)))
                            .unwrap_or_default();
                        let id = get_str(vm, live_id!(id)).unwrap_or_default();
                        parsed.push(FeedMsg {
                            id,
                            role: RoleKind::from_str(&role_s),
                            hdr,
                            text,
                            msg_index: idx,
                        });
                    } else if let Some(s) = vm.cast_to_owned_string(elem, "") {
                        parsed.push(FeedMsg {
                            id: String::new(),
                            role: RoleKind::Say,
                            hdr: String::new(),
                            text: s,
                            msg_index: idx,
                        });
                    }
                }
                self.has_inline_items = true;
                self.rebuild_rows("", 0, &parsed);
            }
        }
    }
}

impl WidgetNode for ChatFeed {
    fn widget_uid(&self) -> WidgetUid {
        self.uid
    }

    fn walk(&mut self, _cx: &mut Cx) -> Walk {
        self.walk
    }

    fn area(&self) -> Area {
        self.list.area()
    }

    fn redraw(&mut self, cx: &mut Cx) {
        self.list.redraw(cx);
    }

    fn children(&self, visit: &mut dyn FnMut(LiveId, WidgetRef)) {
        if !self.list.is_empty() {
            visit(live_id!(list), self.list.clone());
        }
    }

    fn cancel_children_impl(&self, visit: &mut dyn FnMut(LiveId, WidgetRef)) -> bool {
        if !self.list.is_empty() {
            visit(live_id!(list), self.list.clone());
            true
        } else {
            false
        }
    }
}

impl ChatFeed {
    fn parse_json_value(val: &JsonValue) -> (String, usize, Vec<FeedMsg>) {
        match val {
            JsonValue::Object(map) => {
                let agent = map
                    .get("agent")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let hidden_before = match map.get("hidden_before") {
                    Some(JsonValue::U64(n)) => *n as usize,
                    Some(JsonValue::I64(n)) if *n > 0 => *n as usize,
                    Some(JsonValue::F64(f)) if *f > 0.0 => *f as usize,
                    _ => 0,
                };
                let arr = map
                    .get("msgs")
                    .or_else(|| map.get("items"))
                    .and_then(|v| v.as_array());
                let msgs = arr
                    .map(|a| Self::parse_json_msgs(a))
                    .unwrap_or_default();
                (agent, hidden_before, msgs)
            }
            JsonValue::Array(arr) => (String::new(), 0, Self::parse_json_msgs(arr)),
            _ => (String::new(), 0, Vec::new()),
        }
    }

    fn parse_json_msgs(arr: &[JsonValue]) -> Vec<FeedMsg> {
        let mut out = Vec::with_capacity(arr.len());
        for (idx, item) in arr.iter().enumerate() {
            match item {
                JsonValue::String(s) => out.push(FeedMsg {
                    id: String::new(),
                    role: RoleKind::Say,
                    hdr: String::new(),
                    text: s.clone(),
                    msg_index: idx,
                }),
                JsonValue::Object(m) => {
                    let role_s = m.get("role").and_then(|v| v.as_str()).unwrap_or("say");
                    let label = m
                        .get("label")
                        .or_else(|| m.get("title"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(role_s);
                    let ts = m.get("ts").and_then(|v| v.as_str()).unwrap_or("");
                    let hdr = if ts.is_empty() {
                        label.to_string()
                    } else if label.is_empty() {
                        ts.to_string()
                    } else {
                        format!("{label}  {ts}")
                    };
                    let text = m
                        .get("text")
                        .or_else(|| m.get("body"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    out.push(FeedMsg {
                        id,
                        role: RoleKind::from_str(role_s),
                        hdr,
                        text,
                        msg_index: idx,
                    });
                }
                _ => {}
            }
        }
        out
    }

    fn sync_store_if_needed(&mut self) -> bool {
        if self.has_inline_items {
            return false;
        }
        let key = self.store_key.as_ref();
        if key.is_empty() {
            return false;
        }
        let Some((ver, val)) = read_store(key, self.seen_store_ver) else {
            return false;
        };
        self.seen_store_ver = ver;
        let (agent, hidden_before, msgs) = Self::parse_json_value(&val);
        self.rebuild_rows(&agent, hidden_before, &msgs)
    }

    fn rebuild_rows(
        &mut self,
        agent: &str,
        hidden_before: usize,
        msgs: &[FeedMsg],
    ) -> bool {
        let split_target = self.split_bytes.max(600);
        let mut new_rows = Vec::with_capacity(msgs.len().saturating_add(8));
        let mut used_msg_hashes = Vec::with_capacity(msgs.len());
        let mut fp = fnv1a(agent.as_bytes(), FNV_INIT);
        fp = fnv1a(&hidden_before.to_le_bytes(), fp);
        fp = fnv1a(&split_target.to_le_bytes(), fp);
        // A message without an id is known by what it says, and by how many times the same
        // words came before it.
        let mut seen_content: HashMap<u64, u32> = HashMap::new();

        for m in msgs {
            let mut mh = fnv1a(&[m.role.tag()], FNV_INIT);
            mh = fnv1a(m.text.as_bytes(), mh);
            mh = fnv1a(&split_target.to_le_bytes(), mh);
            used_msg_hashes.push(mh);
            let msg_key = if m.id.is_empty() {
                let n = seen_content.entry(mh).or_insert(0);
                *n += 1;
                fnv1a(&n.to_le_bytes(), fnv1a(m.hdr.as_bytes(), mh))
            } else {
                fnv1a(m.id.as_bytes(), 0x9e3779b97f4a7c15)
            };

            let slices = if let Some(cached) = self.split_cache.get(&mh) {
                Rc::clone(cached)
            } else {
                let built = Rc::new(split_message_rows(&m.text, split_target));
                self.split_cache.insert(mh, Rc::clone(&built));
                built
            };

            let n = slices.len();
            for (i, (chunk_text, is_md)) in slices.iter().enumerate() {
                let pos = if n == 1 {
                    SlicePos::Single
                } else if i == 0 {
                    SlicePos::Top
                } else if i + 1 == n {
                    SlicePos::Bot
                } else {
                    SlicePos::Mid
                };
                let hdr = if matches!(pos, SlicePos::Single | SlicePos::Top) {
                    m.hdr.clone()
                } else {
                    String::new()
                };
                let mut rh = fnv1a(&[m.role.tag(), pos.tag(), u8::from(*is_md)], FNV_INIT);
                rh = fnv1a(hdr.as_bytes(), rh);
                rh = fnv1a(chunk_text.as_bytes(), rh);
                fp = fnv1a(&rh.to_le_bytes(), fp);

                new_rows.push(FlatRow {
                    key: fnv1a(&(i as u64).to_le_bytes(), msg_key),
                    role: m.role,
                    pos,
                    is_md: *is_md,
                    hdr,
                    body: chunk_text.clone(),
                    msg_index: m.msg_index,
                    hash: rh,
                });
            }
        }

        if self.split_cache.len() > used_msg_hashes.len() + 64 {
            self.split_cache
                .retain(|k, _| used_msg_hashes.contains(k));
        }

        if fp == self.rows_fingerprint && self.rows.len() == new_rows.len() {
            return false;
        }

        let agent_changed = !self.agent_id.is_empty() && self.agent_id != agent;
        let old_has_bar = self.hidden_before > 0 && !self.rows.is_empty();
        let new_has_bar = hidden_before > 0 && !new_rows.is_empty();
        let plan = if !agent_changed {
            let old_keys: Vec<u64> = self.rows.iter().map(|r| r.key).collect();
            let new_keys: Vec<u64> = new_rows.iter().map(|r| r.key).collect();
            plan_splices(&old_keys, &new_keys, old_has_bar, new_has_bar)
        } else {
            None
        };

        self.agent_id = agent.to_string();
        self.hidden_before = hidden_before;
        self.rows = new_rows;
        self.rows_fingerprint = fp;
        self.pending_range_sync = true;
        match plan {
            // Two updates before one draw queue both plans. The first one's grown end has no
            // op (set_item_range adds it at the draw), so an end op of the second can name ids
            // past the list's range_end: splice_items clamps it, and set_item_range(0, total)
            // then sets the count, with the rows there rebound by hash. Harmless.
            Some(ops) => self.pending_splices.extend(ops),
            None => {
                self.pending_reset_tail = true;
                self.pending_splices.clear();
            }
        }
        true
    }

    fn display_item_count(&self) -> usize {
        if self.rows.is_empty() {
            1 // Empty placeholder row
        } else if self.hidden_before > 0 {
            1 + self.rows.len() // HistoryBar at 0 + rows
        } else {
            self.rows.len()
        }
    }

    fn draw_rows(&mut self, cx: &mut Cx2d, list: &mut PortalList) {
        let total = self.display_item_count();
        if self.pending_range_sync {
            self.pending_range_sync = false;
            if self.pending_reset_tail {
                self.pending_reset_tail = false;
                self.pending_splices.clear();
                list.set_tail_range(true);
                list.set_item_range(cx, 0, total);
                // Also when the count happens to be the same (another agent's chat): the range
                // didn't change, so set_item_range moved nothing.
                list.set_first_id_and_scroll(total.saturating_sub(1), 0.0);
            } else {
                for (at, removed, added) in self.pending_splices.drain(..) {
                    list.splice_items(cx, at, removed, added);
                }
                list.set_item_range(cx, 0, total);
            }
        } else {
            list.set_item_range(cx, 0, total);
        }

        if self.rows.is_empty() {
            while let Some(item_id) = list.next_visible_item(cx) {
                if item_id > 0 {
                    continue;
                }
                let item = list.item(cx, 0, id!(Empty));
                let empty_txt = if self.empty_text.as_ref().is_empty() {
                    "No messages yet."
                } else {
                    self.empty_text.as_ref()
                };
                item.widget(&**cx, ids!(note)).set_text(cx, empty_txt);
                item.draw_all(cx, &mut Scope::empty());
            }
            return;
        }

        let has_bar = self.hidden_before > 0;
        while let Some(item_id) = list.next_visible_item(cx) {
            if item_id >= total {
                continue;
            }
            if has_bar && item_id == 0 {
                let (item, existed) = list.item_with_existed(cx, 0, id!(HistoryBar));
                let bar_hash = fnv1a(&self.hidden_before.to_le_bytes(), 0x517cc1b727220a95);
                let uid = item.widget_uid();
                if !existed || self.applied_hashes.get(&uid) != Some(&bar_hash) {
                    let more_txt = format!("Show earlier ({} more)", self.hidden_before);
                    item.widget(&**cx, ids!(more_btn)).set_text(cx, &more_txt);
                    self.applied_hashes.insert(uid, bar_hash);
                }
                item.draw_all(cx, &mut Scope::empty());
                continue;
            }

            let row_idx = if has_bar { item_id - 1 } else { item_id };
            let Some(row) = self.rows.get(row_idx) else {
                continue;
            };
            let (item, existed) = list.item_with_existed(cx, item_id, row.template_id());
            let uid = item.widget_uid();
            if !existed || self.applied_hashes.get(&uid) != Some(&row.hash) {
                if row.has_header() {
                    item.widget(&**cx, ids!(hdr)).set_text(cx, &row.hdr);
                }
                item.widget(&**cx, ids!(body)).set_text(cx, &row.body);
                self.applied_hashes.insert(uid, row.hash);
            }
            item.draw_all(cx, &mut Scope::empty());
        }
    }
}

impl Widget for ChatFeed {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if let Event::Signal = event {
            if self.sync_store_if_needed() {
                self.redraw(cx);
            }
        }
        if self.list.is_empty() {
            return;
        }

        // Track non-scroll tap coordinates for optional `on_item_tap`.
        match event {
            Event::MouseDown(e) if e.button == MouseButton::PRIMARY => {
                self.press_pos = Some(e.abs);
            }
            Event::TouchUpdate(e) => {
                if let Some(t) = e.touches.iter().find(|t| {
                    matches!(
                        t.state,
                        makepad_widgets::makepad_platform::event::TouchState::Start
                    )
                }) {
                    self.press_pos = Some(t.abs);
                }
            }
            _ => {}
        }

        let actions = cx.capture_actions(|cx| {
            self.list.handle_event(cx, event, scope);
        });

        if !actions.is_empty() && self.hidden_before > 0 && !self.rows.is_empty() {
            let bar_opt = self
                .list
                .borrow::<PortalList>()
                .and_then(|list| list.get_item(0).map(|(_, w)| w));
            if let Some(bar) = bar_opt {
                if bar.button(&*cx, ids!(more_btn)).clicked(&actions) {
                    cx.widget_to_script_call(
                        self.uid,
                        NIL,
                        self.source.clone(),
                        self.on_more.clone(),
                        &[],
                    );
                }
                if bar.button(&*cx, ids!(all_btn)).clicked(&actions) {
                    cx.widget_to_script_call(
                        self.uid,
                        NIL,
                        self.source.clone(),
                        self.on_all.clone(),
                        &[],
                    );
                }
            }
        }

        if self.on_item_tap.as_object() != ScriptObject::ZERO {
            let up_abs = match event {
                Event::MouseUp(e) if e.button == MouseButton::PRIMARY => Some(e.abs),
                Event::TouchUpdate(e) => e
                    .touches
                    .iter()
                    .find(|t| {
                        matches!(
                            t.state,
                            makepad_widgets::makepad_platform::event::TouchState::Stop
                        )
                    })
                    .map(|t| t.abs),
                _ => None,
            };
            if let (Some(down), Some(up)) = (self.press_pos, up_abs) {
                self.press_pos = None;
                let dist = ((up.x - down.x).powi(2) + (up.y - down.y).powi(2)).sqrt();
                let tapped_msg = if dist < 10.0 {
                    self.list.borrow::<PortalList>().and_then(|list| {
                        if list.was_scrolling() {
                            return None;
                        }
                        for (&entry_id, wi) in list.items().iter() {
                            if wi.widget.area().rect(cx).contains(up) {
                                let has_bar = self.hidden_before > 0 && !self.rows.is_empty();
                                if has_bar && entry_id == 0 {
                                    return None;
                                }
                                let r_idx = if has_bar { entry_id - 1 } else { entry_id };
                                return self.rows.get(r_idx).map(|r| r.msg_index);
                            }
                        }
                        None
                    })
                } else {
                    None
                };
                if let Some(msg_idx) = tapped_msg {
                    cx.widget_to_script_call(
                        self.uid,
                        NIL,
                        self.source.clone(),
                        self.on_item_tap.clone(),
                        &[ScriptValue::from_f64(msg_idx as f64)],
                    );
                }
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let _ = self.sync_store_if_needed();
        if self.list.is_empty() {
            return DrawStep::done();
        }
        if self.pending_reset_tail {
            self.list.as_portal_list().stop_scrolling();
        }
        while self.list.draw_walk(cx, scope, walk).is_step() {
            let list_ref = self.list.clone();
            if let Some(mut list) = list_ref.borrow_mut::<PortalList>() {
                self.draw_rows(cx, &mut list);
            };
        }
        DrawStep::done()
    }
}

/// Installed into every Splash isolate (`ChatFeed` and generic alias `VirtualFeed`).
pub fn isolate_mod(vm: &mut ScriptVm) {
    self::script_mod(vm);
    script_eval!(vm, {
        mod.prelude.widgets = {
            ..mod.prelude.widgets,
            ChatFeed: mod.widgets.ChatFeed,
            VirtualFeed: mod.widgets.VirtualFeed,
        }
    });
}
