//! tb-shell-chat: lightweight native Rust owner of `state.chat` for `views/chat.splash`.
//!
//! Replaces the 144 MB Perl helper with < 2 MB RSS and zero wake-ups when `chat` is hidden:
//!   - Parses JSON lines one at a time and retains only compact pre-truncated fields
//!     (at most 300 events per target, <= 16 targets).
//!   - Runs `agent transcript <short> --follow` (or `agent events` for remote targets) ONLY
//!     while `chat` is in front; stops child followers immediately on `hidden`.
//!   - Refreshes `state.chat` (`preview` for `Home`) on start, when `target` or target's
//!     `agents` status changes, and when `home` is `shown`.

#[path = "../page_ipc.rs"]
mod page_ipc;

use makepad_micro_serde::{DeJson, JsonValue, SerJson};
use page_ipc::{connect_and_watch_ext, env_opt, get_keys_sync, obj, set_key_sync, terminate_recorded_pid, ShellWriter};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;

const DEFAULT_SHOWN_MSGS: usize = 120;
const MORE_STEP_MSGS: usize = 200;
const MAX_TARGETS: usize = 16;

fn max_cached_events() -> usize {
    env_opt("TB_CHAT_MAX_EVENTS")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
}

fn clean_text(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_string()
}

#[repr(C)]
#[derive(Default)]
struct Tm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    tm_gmtoff: i64,
    tm_zone: *const std::ffi::c_char,
}

extern "C" {
    fn localtime_r(timep: *const i64, result: *mut Tm) -> *mut Tm;
}

/// Converts Howard Hinnant's civil `(year, month 1..12, day 1..31)` UTC to days since 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Formats an ISO-8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SS[.fff]Z`) as local `HH:MM`.
fn fmt_hhmm(ts: Option<&str>) -> String {
    let Some(s) = ts else { return String::new() };
    if s.len() < 20 || !s.ends_with('Z') || s.as_bytes().get(10) != Some(&b'T') {
        return String::new();
    }
    let year: i64 = match s[0..4].parse() { Ok(v) => v, Err(_) => return String::new() };
    let mon: i64 = match s[5..7].parse() { Ok(v) => v, Err(_) => return String::new() };
    let day: i64 = match s[8..10].parse() { Ok(v) => v, Err(_) => return String::new() };
    let hour: i64 = match s[11..13].parse() { Ok(v) => v, Err(_) => return String::new() };
    let min: i64 = match s[14..16].parse() { Ok(v) => v, Err(_) => return String::new() };
    let sec: i64 = match s[17..19].parse() { Ok(v) => v, Err(_) => return String::new() };
    let epoch = days_from_civil(year, mon, day) * 86_400 + hour * 3600 + min * 60 + sec;
    let mut tm = Tm::default();
    let ok = unsafe { !localtime_r(&epoch, &mut tm).is_null() };
    if ok {
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    } else {
        format!("{hour:02}:{min:02}")
    }
}

fn trunc_chars(text: &str, max_chars: usize) -> String {
    let clean = text.replace("\r\n", "\n");
    let trimmed = clean.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = trimmed.chars().take(keep).collect();
    out.push_str("...");
    out
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn resolve_agent_bin() -> String {
    if let Some(a) = env_opt("AGENT") {
        return a;
    }
    if let Some(u) = env_opt("USER") {
        let p = format!("/userdata/{u}/agent/agent");
        if Path::new(&p).is_file() {
            return p;
        }
    }
    if let Some(h) = env_opt("HOME") {
        for rel in ["agent/agent", ".local/bin/agent"] {
            let p = PathBuf::from(&h).join(rel);
            if p.is_file() {
                return p.to_string_lossy().to_string();
            }
        }
    }
    for entry in std::fs::read_dir("/userdata").into_iter().flatten().flatten() {
        let p = entry.path().join("agent/agent");
        if p.is_file() {
            return p.to_string_lossy().to_string();
        }
    }
    "agent".to_string()
}

fn local_hostname() -> String {
    if let Some(h) = env_opt("TB_CHAT_HOST") {
        return h;
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "tinybot".to_string())
}

#[derive(Clone)]
enum EvKind {
    Prompt { label: String, text: String },
    Say { text: String },
    Tool { name: String, st: String, line: String, text: String },
    Think,
    Other,
}

#[derive(Clone)]
struct CompactEvent {
    abandoned: bool,
    provisional: bool,
    ts: String,
    kind: EvKind,
}

#[derive(Default)]
struct TargetCache {
    order: VecDeque<String>,
    by_id: HashMap<String, CompactEvent>,
    last_at: i64,
    session_id: String,
    path: String,
    file_size: Option<u64>,
    status: String,
}

impl TargetCache {
    fn new() -> Self {
        Self { last_at: -1, ..Default::default() }
    }

    fn reset_transcript(&mut self) {
        self.order.clear();
        self.by_id.clear();
        self.last_at = -1;
        self.session_id.clear();
        self.path.clear();
        self.file_size = None;
    }

    fn drop_provisional(&mut self) {
        let mut kept = VecDeque::with_capacity(self.order.len());
        for id in self.order.drain(..) {
            let is_prov = self.by_id.get(&id).is_some_and(|e| e.provisional);
            if is_prov {
                self.by_id.remove(&id);
            } else {
                kept.push_back(id);
            }
        }
        self.order = kept;
    }
}

fn json_bool(v: Option<&JsonValue>) -> bool {
    matches!(v, Some(JsonValue::Bool(true)) | Some(JsonValue::I64(1)) | Some(JsonValue::U64(1)))
}

fn json_i64(v: Option<&JsonValue>) -> Option<i64> {
    match v? {
        JsonValue::I64(n) => Some(*n),
        JsonValue::U64(n) => Some(*n as i64),
        JsonValue::F64(f) => Some(*f as i64),
        JsonValue::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn parse_prompt(p: &JsonValue) -> Option<EvKind> {
    let origin = p.get("origin").and_then(|v| v.as_str()).unwrap_or("human");
    if origin == "system" {
        return None;
    }
    let raw = p.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
    if raw.is_empty() {
        return None;
    }
    let (label, body) = if let Some(rest) = raw.strip_prefix("[from ") {
        if let Some((who, after)) = rest.split_once(']') {
            let text = after.trim();
            if text.is_empty() {
                return None;
            }
            let lbl = if who.starts_with("a person on ") {
                "You".to_string()
            } else if who.contains("session") {
                who.to_string()
            } else {
                who.split('@').next().unwrap_or(who).to_string()
            };
            (lbl, text)
        } else {
            ("You".to_string(), raw)
        }
    } else if origin == "command" {
        ("Command".to_string(), raw)
    } else {
        ("You".to_string(), raw)
    };
    Some(EvKind::Prompt { label, text: clean_text(body) })
}

struct IngestOutcome {
    dirty: bool,
    need_full_reread: bool,
}

fn ingest_line(c: &mut TargetCache, line: &str) -> IngestOutcome {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('{') {
        return IngestOutcome { dirty: false, need_full_reread: false };
    }
    let Ok(ev_obj) = JsonValue::deserialize_json(trimmed) else {
        return IngestOutcome { dirty: false, need_full_reread: false };
    };
    let ty = ev_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if ty == "session" {
        let mut dirty = false;
        let mut need_full = false;
        let sid = ev_obj.get("session_id").and_then(|v| v.as_str()).unwrap_or("");
        if !sid.is_empty() && !c.session_id.is_empty() && sid != c.session_id {
            c.reset_transcript();
            c.session_id = sid.to_string();
            need_full = true;
            dirty = true;
        } else if !sid.is_empty() {
            c.session_id = sid.to_string();
        }
        if let Some(p) = ev_obj.get("path").and_then(|v| v.as_str()).filter(|p| !p.is_empty()) {
            c.path = p.to_string();
            c.file_size = std::fs::metadata(p).ok().map(|m| m.len());
        }
        if let Some(st) = ev_obj.get("status").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            if st != c.status {
                c.status = st.to_string();
                dirty = true;
            }
        }
        return IngestOutcome { dirty, need_full_reread: need_full };
    }
    if ty != "event" {
        return IngestOutcome { dirty: false, need_full_reread: false };
    }
    let Some(id) = ev_obj.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
        return IngestOutcome { dirty: false, need_full_reread: false };
    };
    let provisional = json_bool(ev_obj.get("provisional"));
    if !provisional {
        if let Some(at) = json_i64(ev_obj.get("at")).filter(|&a| a >= 0) {
            if at > c.last_at {
                c.last_at = at;
            }
        }
    }
    if !c.path.is_empty() {
        if let Ok(m) = std::fs::metadata(&c.path) {
            c.file_size = Some(m.len());
        }
    }

    let abandoned = json_bool(ev_obj.get("abandoned"));
    let ts = fmt_hhmm(ev_obj.get("ts").and_then(|v| v.as_str()));
    let kind_str = ev_obj.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let kind = match kind_str {
        "prompt" => ev_obj.get("prompt").and_then(parse_prompt).unwrap_or(EvKind::Other),
        "say" => {
            let txt = ev_obj.get("say").and_then(|s| s.get("text")).and_then(|v| v.as_str()).unwrap_or("").trim();
            if txt.is_empty() {
                EvKind::Other
            } else {
                EvKind::Say { text: clean_text(txt) }
            }
        }
        "tool" => {
            if let Some(t) = ev_obj.get("tool") {
                let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("tool").to_string();
                let detail = t.get("detail").and_then(|v| v.as_str()).unwrap_or("");
                let st = t.get("result").and_then(|r| r.get("status")).and_then(|v| v.as_str()).unwrap_or("pending").to_string();
                let raw_line = if detail.is_empty() { name.clone() } else { format!("{name}: {detail}") };
                let line = trunc_chars(&raw_line, 120);
                let text = clean_text(if detail.is_empty() { &name } else { detail });
                EvKind::Tool { name, st, line, text }
            } else {
                EvKind::Other
            }
        }
        "think" => EvKind::Think,
        _ => EvKind::Other,
    };

    if !c.by_id.contains_key(id) {
        c.order.push_back(id.to_string());
        let cap = max_cached_events();
        if cap > 0 && c.order.len() > cap {
            if let Some(old) = c.order.pop_front() {
                c.by_id.remove(&old);
            }
        }
    }
    c.by_id.insert(id.to_string(), CompactEvent { abandoned, provisional, ts, kind });
    IngestOutcome { dirty: true, need_full_reread: false }
}

fn build_chat_value(c: &TargetCache, agent_short: &str, show_tools: bool, shown_limit: usize) -> JsonValue {
    let mut msgs: Vec<JsonValue> = Vec::new();
    let mut last_say = String::new();
    let mut last_say_idx: isize = -1;
    let mut activity = String::new();

    for (idx, id) in c.order.iter().enumerate() {
        let Some(ev) = c.by_id.get(id) else { continue };
        if ev.abandoned {
            continue;
        }
        let idx_isize = idx as isize;
        match &ev.kind {
            EvKind::Prompt { label, text } => {
                msgs.push(obj(vec![
                    ("role", "you".into()),
                    ("label", label.clone().into()),
                    ("ts", ev.ts.clone().into()),
                    ("text", text.clone().into()),
                ]));
                activity.clear();
            }
            EvKind::Say { text } => {
                msgs.push(obj(vec![
                    ("role", "say".into()),
                    ("label", agent_short.into()),
                    ("ts", ev.ts.clone().into()),
                    ("text", text.clone().into()),
                ]));
                last_say = text.clone();
                last_say_idx = idx_isize;
                activity.clear();
            }
            EvKind::Tool { name, st, line, text } => {
                if show_tools {
                    msgs.push(obj(vec![
                        ("role", "tool".into()),
                        ("label", format!("{name} ({st})").into()),
                        ("ts", ev.ts.clone().into()),
                        ("text", text.clone().into()),
                    ]));
                }
                if st == "pending" || idx_isize > last_say_idx {
                    let prefix = if st == "pending" { "running " } else { "ran " };
                    activity = format!("{prefix}{line}");
                }
            }
            EvKind::Think => {
                if idx_isize > last_say_idx && activity.is_empty() {
                    activity = "thinking...".to_string();
                }
            }
            EvKind::Other => {}
        }
    }

    if c.status != "busy" && c.status != "processing" && !activity.starts_with("running ") {
        activity.clear();
    }
    let total_msgs = msgs.len();
    let hidden_before = if shown_limit > 0 && total_msgs > shown_limit {
        let drop_n = total_msgs - shown_limit;
        msgs.drain(..drop_n);
        drop_n
    } else {
        0
    };

    let collapsed = collapse_ws(&last_say);
    let preview = if collapsed.is_empty() {
        "no replies yet".to_string()
    } else {
        trunc_chars(&collapsed, 46)
    };
    let st_ui = match c.status.as_str() {
        "processing" => "busy",
        "waiting" => "error",
        other => other,
    };

    obj(vec![
        ("agent", agent_short.into()),
        ("status", st_ui.into()),
        ("tools", JsonValue::Bool(show_tools)),
        ("preview", preview.into()),
        ("activity", activity.into()),
        ("total", JsonValue::U64(total_msgs as u64)),
        ("hidden_before", JsonValue::U64(hidden_before as u64)),
        ("msgs", JsonValue::Array(msgs)),
    ])
}

enum ChatMsg {
    Socket(JsonValue),
    SocketClosed,
    FollowerLine { gen_id: u64, line: String },
    FollowerEof { gen_id: u64, used_from: bool, saw_any: bool },
    RemoteEvLine { gen_id: u64, line: String },
}

struct ChildFollower {
    pid: u32,
}

impl Drop for ChildFollower {
    fn drop(&mut self) {
        terminate_recorded_pid(self.pid, None);
    }
}

struct ChatState {
    agent_bin: String,
    me_host: String,
    cache: HashMap<String, TargetCache>,
    target_id: String,
    agent_short: String,
    agent_host: String,
    show_tools: bool,
    shown_limit: usize,
    chat_visible: bool,
    last_pushed: String,
    gen_counter: Arc<AtomicU64>,
    follower: Option<ChildFollower>,
    remote_ev: Option<ChildFollower>,
}

impl ChatState {
    fn new(agent_bin: String, me_host: String) -> Self {
        let default_id = format!("tinybot-phone@{me_host}");
        let mut cache = HashMap::new();
        cache.insert(default_id.clone(), TargetCache::new());
        Self {
            agent_bin,
            agent_short: "tinybot-phone".into(),
            agent_host: me_host.clone(),
            me_host,
            cache,
            target_id: default_id,
            show_tools: false,
            shown_limit: DEFAULT_SHOWN_MSGS,
            chat_visible: false,
            last_pushed: String::new(),
            gen_counter: Arc::new(AtomicU64::new(0)),
            follower: None,
            remote_ev: None,
        }
    }

    fn is_local(&self) -> bool {
        self.agent_host == self.me_host
    }

    fn cur_mut(&mut self) -> &mut TargetCache {
        if !self.cache.contains_key(&self.target_id) && self.cache.len() >= MAX_TARGETS {
            if let Some(k) = self.cache.keys().next().cloned() {
                self.cache.remove(&k);
            }
        }
        self.cache.entry(self.target_id.clone()).or_insert_with(TargetCache::new)
    }

    fn set_target(&mut self, raw_id: Option<&str>, agents_arr: Option<&Vec<JsonValue>>) -> bool {
        let mut chosen = raw_id.unwrap_or("").trim().to_string();
        let default_local = format!("tinybot-phone@{}", self.me_host);
        if chosen.is_empty() {
            if let Some(arr) = agents_arr {
                if arr.iter().any(|a| a.get("id").and_then(|v| v.as_str()) == Some(default_local.as_str())) {
                    chosen = default_local.clone();
                } else if let Some(first) = arr.first().and_then(|a| a.get("id")).and_then(|v| v.as_str()) {
                    chosen = first.to_string();
                }
            }
        }
        if chosen.is_empty() {
            chosen = default_local;
        }
        let changed = chosen != self.target_id;
        if changed {
            self.shown_limit = DEFAULT_SHOWN_MSGS;
        }
        self.target_id = chosen;
        if let Some((short, host)) = self.target_id.split_once('@') {
            self.agent_short = short.to_string();
            self.agent_host = host.to_string();
        } else {
            self.agent_short = self.target_id.clone();
            self.agent_host = self.me_host.clone();
        }
        let tid = self.target_id.clone();
        let c = self.cur_mut();
        if let Some(arr) = agents_arr {
            for a in arr {
                if a.get("id").and_then(|v| v.as_str()) == Some(tid.as_str()) {
                    if let Some(st) = a.get("status").and_then(|v| v.as_str()) {
                        c.status = st.to_string();
                    }
                    break;
                }
            }
        }
        changed
    }

    fn run_ingest_cmd(&mut self, args: &[&str]) -> (bool, bool) {
        let Ok(mut child) = Command::new(&self.agent_bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return (false, false);
        };
        let mut need_full = false;
        if let Some(out) = child.stdout.take() {
            let c = self.cur_mut();
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                let r = ingest_line(c, &line);
                need_full |= r.need_full_reread;
            }
        }
        let ok = child.wait().map_or(false, |s| s.success());
        (ok, need_full)
    }

    fn snapshot_fetch(&mut self, force_full: bool) {
        if self.is_local() {
            let (last_at, unchanged) = {
                let c = self.cur_mut();
                let same_sz = !force_full
                    && c.last_at >= 0
                    && !c.path.is_empty()
                    && std::fs::metadata(&c.path).ok().map(|m| m.len()) == c.file_size
                    && c.file_size.is_some();
                if same_sz {
                    return;
                }
                if force_full {
                    c.reset_transcript();
                } else if c.last_at >= 0 {
                    c.drop_provisional();
                }
                (c.last_at, false)
            };
            let _ = unchanged;
            let short = self.agent_short.clone();
            let (ok, need_full) = if last_at >= 0 {
                let from_str = last_at.to_string();
                self.run_ingest_cmd(&["transcript", &short, "--from", &from_str])
            } else {
                self.run_ingest_cmd(&["transcript", &short])
            };
            if (!ok || need_full) && last_at >= 0 {
                self.cur_mut().reset_transcript();
                let _ = self.run_ingest_cmd(&["transcript", &short]);
            }
        } else {
            if force_full {
                self.cur_mut().reset_transcript();
            }
            let tid = self.target_id.clone();
            let _ = self.run_ingest_cmd(&["logs", &tid, "--lines", "2000", "--json"]);
        }
    }

    fn push_if_changed(&mut self, writer: &ShellWriter, force: bool) -> Result<(), String> {
        let short = self.agent_short.clone();
        let tools = self.show_tools;
        let limit = self.shown_limit;
        let val = {
            let c = self.cur_mut();
            build_chat_value(c, &short, tools, limit)
        };
        let ser = val.serialize_json();
        if !force && ser == self.last_pushed {
            return Ok(());
        }
        writer.patch_key("chat", val)?;
        self.last_pushed = ser;
        Ok(())
    }

    fn stop_streams(&mut self) {
        self.gen_counter.fetch_add(1, Ordering::SeqCst);
        self.follower = None;
        self.remote_ev = None;
    }

    fn sync_streams(&mut self, force_full: bool, tx: &Sender<ChatMsg>) {
        self.stop_streams();
        if !self.chat_visible {
            return;
        }
        let gen_id = self.gen_counter.fetch_add(1, Ordering::SeqCst) + 1;
        if self.is_local() {
            let last_at = {
                let c = self.cur_mut();
                if force_full {
                    c.reset_transcript();
                } else if c.last_at >= 0 {
                    c.drop_provisional();
                }
                c.last_at
            };
            let mut cmd = Command::new(&self.agent_bin);
            cmd.arg("transcript").arg(&self.agent_short);
            let used_from = if last_at >= 0 {
                cmd.arg("--from").arg(last_at.to_string());
                true
            } else {
                false
            };
            cmd.arg("--follow").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
            if let Ok(mut child) = cmd.spawn() {
                let pid = child.id();
                if let Some(out) = child.stdout.take() {
                    let tx = tx.clone();
                    let gen_ref = self.gen_counter.clone();
                    let _ = std::thread::Builder::new().name("tb-chat-follow".into()).spawn(move || {
                        let mut saw_any = false;
                        for line in BufReader::new(out).lines() {
                            if gen_ref.load(Ordering::Relaxed) != gen_id {
                                break;
                            }
                            let Ok(line) = line else { break };
                            saw_any = true;
                            let _ = tx.send(ChatMsg::FollowerLine { gen_id, line });
                        }
                        let _ = child.wait();
                        if gen_ref.load(Ordering::Relaxed) == gen_id {
                            let _ = tx.send(ChatMsg::FollowerEof { gen_id, used_from, saw_any });
                        }
                    });
                }
                self.follower = Some(ChildFollower { pid });
            }
        } else {
            self.snapshot_fetch(force_full);
            let mut cmd = Command::new(&self.agent_bin);
            cmd.args(["events", "--all-hosts", "--json", "-q"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            if let Ok(mut child) = cmd.spawn() {
                let pid = child.id();
                if let Some(out) = child.stdout.take() {
                    let tx = tx.clone();
                    let gen_ref = self.gen_counter.clone();
                    let _ = std::thread::Builder::new().name("tb-chat-events".into()).spawn(move || {
                        for line in BufReader::new(out).lines() {
                            if gen_ref.load(Ordering::Relaxed) != gen_id {
                                break;
                            }
                            let Ok(line) = line else { break };
                            let _ = tx.send(ChatMsg::RemoteEvLine { gen_id, line });
                        }
                        let _ = child.wait();
                    });
                }
                self.remote_ev = Some(ChildFollower { pid });
            }
        }
    }
}

fn main() {
    let mut st = ChatState::new(resolve_agent_bin(), local_hostname());

    if std::env::args().any(|a| a == "--once") {
        if let Ok(init) = get_keys_sync(&["target", "agents"]) {
            st.set_target(
                init.get("target").and_then(|v| v.as_str()),
                init.get("agents").and_then(|v| v.as_array()),
            );
        }
        st.snapshot_fetch(true);
        let short = st.agent_short.clone();
        let tools = st.show_tools;
        let limit = st.shown_limit;
        let val = build_chat_value(st.cur_mut(), &short, tools, limit);
        if let Err(e) = set_key_sync("chat", val) {
            eprintln!("tb-shell-chat: {e}");
            std::process::exit(1);
        }
        return;
    }

    let (_, writer, reader) = match connect_and_watch_ext(&["chat", "home"], &["target", "agents"]) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tb-shell-chat: {e}");
            std::process::exit(1);
        }
    };

    if let Ok(init) = get_keys_sync(&["target", "agents", "front"]) {
        st.set_target(
            init.get("target").and_then(|v| v.as_str()),
            init.get("agents").and_then(|v| v.as_array()),
        );
        st.chat_visible = init.get("front").and_then(|v| v.as_str()) == Some("chat");
    }
    st.snapshot_fetch(true);
    if let Err(e) = st.push_if_changed(&writer, true) {
        eprintln!("tb-shell-chat: initial push: {e}");
        std::process::exit(1);
    }

    let (tx, rx) = mpsc::channel::<ChatMsg>();
    {
        let tx = tx.clone();
        let _ = std::thread::Builder::new().name("tb-chat-sock".into()).spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if let Ok(ev) = JsonValue::deserialize_json(line.trim()) {
                    let _ = tx.send(ChatMsg::Socket(ev));
                }
            }
            let _ = tx.send(ChatMsg::SocketClosed);
        });
    }

    if st.chat_visible {
        st.sync_streams(false, &tx);
    }

    while let Ok(msg) = rx.recv() {
        let mut dirty = false;
        match msg {
            ChatMsg::SocketClosed => {
                st.stop_streams();
                break;
            }
            ChatMsg::FollowerLine { gen_id, line } => {
                if gen_id != st.gen_counter.load(Ordering::Relaxed) {
                    continue;
                }
                let out = ingest_line(st.cur_mut(), &line);
                if out.need_full_reread {
                    st.sync_streams(true, &tx);
                }
                dirty |= out.dirty;
            }
            ChatMsg::FollowerEof { gen_id, used_from, saw_any } => {
                if gen_id == st.gen_counter.load(Ordering::Relaxed) && used_from && !saw_any && st.chat_visible && st.is_local() {
                    st.sync_streams(true, &tx);
                }
            }
            ChatMsg::RemoteEvLine { gen_id, line } => {
                if gen_id != st.gen_counter.load(Ordering::Relaxed) {
                    continue;
                }
                if let Ok(e) = JsonValue::deserialize_json(line.trim()) {
                    let ename = e.get("agent").or_else(|| e.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                    let ehost = e.get("host").and_then(|v| v.as_str()).unwrap_or(&st.me_host);
                    let full_id = format!("{ename}@{ehost}");
                    if full_id == st.target_id || ename == st.target_id {
                        if let Some(ns) = e.get("status").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                            let c = st.cur_mut();
                            if c.status != ns {
                                c.status = ns.to_string();
                                dirty = true;
                            }
                            if matches!(ns, "idle" | "waiting" | "error") {
                                st.snapshot_fetch(false);
                                dirty = true;
                            }
                        }
                    }
                }
            }
            ChatMsg::Socket(ev) => {
                let kind = ev.get("ev").and_then(|v| v.as_str()).unwrap_or("");
                let surf = ev.get("surface").and_then(|v| v.as_str()).unwrap_or("");
                match kind {
                    "shown" if surf == "chat" => {
                        st.chat_visible = true;
                        st.sync_streams(false, &tx);
                        dirty = true;
                    }
                    "hidden" if surf == "chat" => {
                        st.chat_visible = false;
                        st.stop_streams();
                    }
                    "shown" if surf == "home" && !st.chat_visible => {
                        st.snapshot_fetch(false);
                        dirty = true;
                    }
                    "panel" if ev.get("on") == Some(&JsonValue::Bool(true)) && !st.chat_visible => {
                        st.snapshot_fetch(false);
                        dirty = true;
                    }
                    "emit" if surf == "chat" => {
                        match ev.get("name").and_then(|v| v.as_str()).unwrap_or("") {
                            "refresh" => {
                                if st.chat_visible {
                                    st.sync_streams(true, &tx);
                                } else {
                                    st.snapshot_fetch(true);
                                }
                                dirty = true;
                            }
                            "toggle_tools" => {
                                st.show_tools = !st.show_tools;
                                dirty = true;
                            }
                            "more" => {
                                st.shown_limit = st.shown_limit.saturating_add(MORE_STEP_MSGS);
                                dirty = true;
                            }
                            "all" => {
                                st.shown_limit = 0;
                                dirty = true;
                            }
                            _ => {}
                        }
                    }
                    "set" => {
                        if let Some(patch) = ev.get("patch") {
                            if let Some(arr) = patch.get("agents").and_then(|v| v.as_array()) {
                                let tid = st.target_id.clone();
                                for a in arr {
                                    if a.get("id").and_then(|v| v.as_str()) == Some(tid.as_str()) {
                                        let ns = a.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                        let prev = st.cur_mut().status.clone();
                                        if ns != prev {
                                            st.cur_mut().status = ns.clone();
                                            dirty = true;
                                            if !st.chat_visible && matches!(ns.as_str(), "idle" | "waiting" | "error") {
                                                st.snapshot_fetch(false);
                                            }
                                        }
                                        break;
                                    }
                                }
                            }
                            if let Some(t) = patch.get("target") {
                                if st.set_target(t.as_str(), None) {
                                    dirty = true;
                                    if st.chat_visible {
                                        st.sync_streams(false, &tx);
                                    } else {
                                        st.stop_streams();
                                        st.snapshot_fetch(false);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if dirty {
            if let Err(e) = st.push_if_changed(&writer, false) {
                eprintln!("tb-shell-chat: {e}");
                st.stop_streams();
                break;
            }
        }
    }
}
