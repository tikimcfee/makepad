//! Event-driven `g3d-termd` status follower and non-blocking action bridge for `tb-shell`.
//!
//! Follows `status_follow` push events on `"default"` (interactive shells) and `"agents"`
//! (`gomuxai` agent sessions) without polling and without auto-spawning daemons just to watch.
//! Drops all follower connections immediately on `panel_off` (`0` asleep wake-ups) and
//! re-establishes them on `panel_on`.

use crate::termd_wire::{
    self, create_terminal, j_bool, j_get, j_i64, j_obj, j_str, j_u64, kill_terminal,
    paste_preview, paste_terminal, read_frame, send_key_terminal, socket_path, DialError,
    PasteOutcome, SyncClient, FRAME_ERROR, FRAME_EVENT,
};
use makepad_micro_serde::{DeJson, JsonValue, SerJson};
use makepad_widgets::SignalToUI;
use std::collections::HashMap;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const WATCHED_NS: [&str; 2] = ["default", "agents"];

#[derive(Clone, Debug)]
pub struct TermEntry {
    pub ns: String,
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub pwd: String,
    pub cmd: String,
    pub pid: i64,
    pub cols: u64,
    pub rows: u64,
    pub alt: bool,
    pub created_at: u64,
    pub label: String,
}

impl TermEntry {
    pub fn to_json(&self) -> JsonValue {
        j_obj(vec![
            ("ns", JsonValue::String(self.ns.clone())),
            ("id", JsonValue::String(self.id.clone())),
            ("title", JsonValue::String(self.title.clone())),
            ("cwd", JsonValue::String(self.cwd.clone())),
            ("pwd", JsonValue::String(self.pwd.clone())),
            ("cmd", JsonValue::String(self.cmd.clone())),
            ("pid", JsonValue::I64(self.pid)),
            ("cols", JsonValue::U64(self.cols)),
            ("rows", JsonValue::U64(self.rows)),
            ("alt", JsonValue::Bool(self.alt)),
            ("exited", JsonValue::Bool(false)),
            ("exit_code", JsonValue::Null),
            ("label", JsonValue::String(self.label.clone())),
        ])
    }
}

pub enum BridgeEvent {
    Status {
        ns: String,
        terminals: Vec<TermEntry>,
    },
    Created {
        ns: String,
        id: String,
        focus_view: bool,
    },
    Killed {
        ns: String,
        id: String,
    },
    PasteUnsafe {
        ns: String,
        id: String,
        text: String,
        enter: bool,
        lines: usize,
        preview: String,
    },
    PasteDone,
    Error(String),
}

enum ActionJob {
    Create {
        ns: String,
        id: Option<String>,
        cwd: Option<String>,
        cmd: Option<String>,
        cols: u16,
        rows: u16,
        focus_view: bool,
    },
    Key {
        ns: String,
        id: String,
        spec: String,
        ctrl: bool,
        alt: bool,
        shift: bool,
    },
    Paste {
        ns: String,
        id: String,
        text: String,
        enter: bool,
        allow_unsafe: bool,
    },
    Kill {
        ns: String,
        id: String,
    },
}

#[derive(Clone)]
struct StagedPaste {
    ns: String,
    id: String,
    text: String,
    enter: bool,
    lines: usize,
    preview: String,
}

pub struct TermdBridge {
    paused: Arc<AtomicBool>,
    active_followers: Arc<Mutex<HashMap<String, UnixStream>>>,
    ev_tx: Sender<BridgeEvent>,
    ev_rx: Receiver<BridgeEvent>,
    act_tx: Sender<ActionJob>,

    by_ns: HashMap<String, Vec<TermEntry>>,
    pub active_ns: String,
    pub active_id: String,
    pub cols_mode: String,
    pub ctrl_latched: bool,
    pub kill_confirm: String,
    pub picker_open: bool,
    pub last_error: String,
    pub local_host: String,
    staged_paste: Option<StagedPaste>,
}

impl TermdBridge {
    pub fn new(panel_off: bool) -> Self {
        let (ev_tx, ev_rx) = channel();
        let (act_tx, act_rx) = channel::<ActionJob>();
        let paused = Arc::new(AtomicBool::new(panel_off));

        let ev_tx_act = ev_tx.clone();
        let _ = std::thread::Builder::new()
            .name("tb-termd-act".into())
            .spawn(move || action_worker(act_rx, ev_tx_act));

        let bridge = Self {
            paused,
            active_followers: Arc::new(Mutex::new(HashMap::new())),
            ev_tx,
            ev_rx,
            act_tx,
            by_ns: HashMap::new(),
            active_ns: "default".into(),
            active_id: String::new(),
            cols_mode: "48".into(),
            ctrl_latched: false,
            kill_confirm: String::new(),
            picker_open: false,
            last_error: String::new(),
            local_host: termd_wire::local_hostname(),
            staged_paste: None,
        };
        if !panel_off {
            bridge.probe_disconnected();
        }
        bridge
    }

    pub fn set_panel_off(&mut self, panel_off: bool) {
        if panel_off {
            self.paused.store(true, Ordering::SeqCst);
            if let Ok(mut map) = self.active_followers.lock() {
                for (_, sock) in map.drain() {
                    let _ = sock.shutdown(Shutdown::Both);
                }
            }
        } else {
            self.paused.store(false, Ordering::SeqCst);
            self.probe_disconnected();
        }
    }

    /// Spawns a `status_follow` reader for any watched namespace whose socket exists on disk
    /// and is not already being followed. Never spawns a `g3d-termd` daemon.
    pub fn probe_disconnected(&self) {
        if self.paused.load(Ordering::SeqCst) {
            return;
        }
        for ns in WATCHED_NS {
            let already = self
                .active_followers
                .lock()
                .map_or(false, |m| m.contains_key(ns));
            if already {
                continue;
            }
            if !socket_path(ns).exists() {
                continue;
            }
            let ns_owned = ns.to_string();
            let paused = Arc::clone(&self.paused);
            let followers = Arc::clone(&self.active_followers);
            let tx = self.ev_tx.clone();
            let _ = std::thread::Builder::new()
                .name(format!("tb-termd-{ns}"))
                .spawn(move || follow_ns_worker(ns_owned, paused, followers, tx));
        }
    }

    fn all_sorted(&self) -> Vec<TermEntry> {
        let mut out = Vec::new();
        for ns in WATCHED_NS {
            if let Some(list) = self.by_ns.get(ns) {
                let mut sorted = list.clone();
                sorted.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
                out.extend(sorted);
            }
        }
        for (ns, list) in &self.by_ns {
            if !WATCHED_NS.contains(&ns.as_str()) {
                let mut sorted = list.clone();
                sorted.sort_by(|a, b| a.id.cmp(&b.id));
                out.extend(sorted);
            }
        }
        out
    }

    fn ensure_valid_active(&mut self, all: &[TermEntry]) {
        if all.is_empty() {
            if self.active_ns == "default" {
                self.active_id.clear();
            }
            return;
        }
        let exists = all
            .iter()
            .any(|t| t.ns == self.active_ns && t.id == self.active_id);
        if !exists {
            // Prefer newest in `"default"`, else first available.
            let pick = all
                .iter()
                .rev()
                .find(|t| t.ns == "default")
                .or_else(|| all.first());
            if let Some(t) = pick {
                self.active_ns = t.ns.clone();
                self.active_id = t.id.clone();
            }
        }
    }

    pub fn to_store_json(&mut self) -> JsonValue {
        let all = self.all_sorted();
        self.ensure_valid_active(&all);
        let default_count = all.iter().filter(|t| t.ns == "default").count() as u64;
        let agents_count = all.iter().filter(|t| t.ns == "agents").count() as u64;
        let total = all.len() as u64;
        let terms_json: Vec<JsonValue> = all.iter().map(TermEntry::to_json).collect();

        let pending_json = match &self.staged_paste {
            Some(sp) => j_obj(vec![
                ("ns", JsonValue::String(sp.ns.clone())),
                ("id", JsonValue::String(sp.id.clone())),
                ("lines", JsonValue::U64(sp.lines as u64)),
                ("preview", JsonValue::String(sp.preview.clone())),
                ("enter", JsonValue::Bool(sp.enter)),
            ]),
            None => JsonValue::Null,
        };

        j_obj(vec![
            ("terminals", JsonValue::Array(terms_json)),
            ("count", JsonValue::U64(total)),
            ("default_count", JsonValue::U64(default_count)),
            ("agents_count", JsonValue::U64(agents_count)),
            (
                "active",
                j_obj(vec![
                    ("ns", JsonValue::String(self.active_ns.clone())),
                    ("id", JsonValue::String(self.active_id.clone())),
                ]),
            ),
            ("cols_mode", JsonValue::String(self.cols_mode.clone())),
            ("ctrl_latched", JsonValue::Bool(self.ctrl_latched)),
            ("kill_confirm", JsonValue::String(self.kill_confirm.clone())),
            ("picker_open", JsonValue::Bool(self.picker_open)),
            ("pending_paste", pending_json),
            ("last_error", JsonValue::String(self.last_error.clone())),
            ("local_host", JsonValue::String(self.local_host.clone())),
        ])
    }

    /// Drains pending background `BridgeEvent`s. Returns `(store_changed, want_front_terminal)`.
    pub fn drain_events(&mut self) -> (bool, bool) {
        let mut changed = false;
        let mut want_front = false;
        while let Ok(ev) = self.ev_rx.try_recv() {
            match ev {
                BridgeEvent::Status { ns, terminals } => {
                    self.by_ns.insert(ns, terminals);
                    changed = true;
                }
                BridgeEvent::Created { ns, id, focus_view } => {
                    self.active_ns = ns;
                    self.active_id = id;
                    self.picker_open = false;
                    self.kill_confirm.clear();
                    self.last_error.clear();
                    if focus_view {
                        want_front = true;
                    }
                    self.probe_disconnected();
                    changed = true;
                }
                BridgeEvent::Killed { ns, id } => {
                    if let Some(list) = self.by_ns.get_mut(&ns) {
                        list.retain(|t| t.id != id);
                    }
                    self.kill_confirm.clear();
                    self.last_error.clear();
                    changed = true;
                }
                BridgeEvent::PasteUnsafe {
                    ns,
                    id,
                    text,
                    enter,
                    lines,
                    preview,
                } => {
                    self.staged_paste = Some(StagedPaste {
                        ns,
                        id,
                        text,
                        enter,
                        lines,
                        preview,
                    });
                    changed = true;
                }
                BridgeEvent::PasteDone => {
                    if self.staged_paste.take().is_some() {
                        changed = true;
                    }
                    if !self.last_error.is_empty() {
                        self.last_error.clear();
                        changed = true;
                    }
                }
                BridgeEvent::Error(msg) => {
                    self.last_error = msg;
                    changed = true;
                }
            }
        }
        (changed, want_front)
    }

    fn cols_u16(&self) -> u16 {
        self.cols_mode.parse::<u16>().unwrap_or(48).clamp(20, 240)
    }

    /// Handles Splash `emit("termd_*", data)` events from `terminal.splash` / `agents.splash`.
    /// Returns `(handled, store_changed, want_front_terminal)`.
    pub fn handle_ui_emit(&mut self, name: &str, data: &JsonValue) -> (bool, bool, bool) {
        match name {
            "termd_create" => {
                let ns = j_str(j_get(data, "ns")).unwrap_or("default").to_string();
                let cwd = j_str(j_get(data, "cwd")).and_then(|s| {
                    if s == "@workspace" {
                        if Path::new("/userdata/tinybot").is_dir() {
                            Some("/userdata/tinybot".to_string())
                        } else {
                            termd_wire::default_shell_cwd()
                        }
                    } else if s.is_empty() {
                        None
                    } else {
                        Some(s.to_string())
                    }
                });
                let cmd = j_str(j_get(data, "cmd"))
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let id = j_str(j_get(data, "id"))
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let focus_view = j_bool(j_get(data, "focus")).unwrap_or(true);
                let _ = self.act_tx.send(ActionJob::Create {
                    ns,
                    id,
                    cwd,
                    cmd,
                    cols: self.cols_u16(),
                    rows: 32,
                    focus_view,
                });
                (true, false, focus_view)
            }
            "termd_spawn_agent" => {
                let backend = j_str(j_get(data, "backend")).unwrap_or("agy");
                let safe_bin = match backend {
                    "claude" => "claude",
                    _ => "agy",
                };
                let id = termd_wire::gen_term_id(safe_bin);
                let cwd = if Path::new("/userdata/tinybot").is_dir() {
                    Some("/userdata/tinybot".to_string())
                } else {
                    termd_wire::default_shell_cwd()
                };
                let script = format!(
                    "export PATH=\"$HOME/.local/bin:/usr/local/bin:$PATH\"; \
                     if command -v {safe_bin} >/dev/null 2>&1; then {safe_bin}; \
                     else echo '{safe_bin} CLI not found on PATH; interactive shell ready.'; fi; \
                     exec \"${{SHELL:-/bin/bash}}\" -l"
                );
                let _ = self.act_tx.send(ActionJob::Create {
                    ns: "default".into(),
                    id: Some(id),
                    cwd,
                    cmd: Some(script),
                    cols: self.cols_u16(),
                    rows: 32,
                    focus_view: true,
                });
                (true, false, true)
            }
            "termd_select" => {
                let ns = j_str(j_get(data, "ns")).unwrap_or("default");
                let id = j_str(j_get(data, "id")).unwrap_or("");
                if !id.is_empty() {
                    self.active_ns = ns.to_string();
                    self.active_id = id.to_string();
                }
                self.picker_open = false;
                self.kill_confirm.clear();
                let focus = j_bool(j_get(data, "focus")).unwrap_or(false);
                (true, true, focus)
            }
            "termd_picker_toggle" => {
                self.picker_open = !self.picker_open;
                (true, true, false)
            }
            "termd_cycle" => {
                let all = self.all_sorted();
                if all.len() > 1 {
                    let cur = all
                        .iter()
                        .position(|t| t.ns == self.active_ns && t.id == self.active_id)
                        .unwrap_or(0);
                    let next = &all[(cur + 1) % all.len()];
                    self.active_ns = next.ns.clone();
                    self.active_id = next.id.clone();
                    self.kill_confirm.clear();
                    return (true, true, false);
                }
                (true, false, false)
            }
            "termd_focus_agent" => {
                self.probe_disconnected();
                let name = j_str(j_get(data, "name")).unwrap_or("");
                let session = j_str(j_get(data, "session")).unwrap_or("");
                let candidate = self
                    .by_ns
                    .get("agents")
                    .and_then(|list| {
                        list.iter().find(|t| {
                            (!session.is_empty() && t.id == session)
                                || (!name.is_empty()
                                    && (t.id == name || t.id == format!("agent-{name}")))
                        })
                    })
                    .map(|t| ("agents".to_string(), t.id.clone()))
                    .unwrap_or_else(|| {
                        let fallback_id = if !session.is_empty() {
                            session.to_string()
                        } else {
                            name.to_string()
                        };
                        ("agents".to_string(), fallback_id)
                    });
                if !candidate.1.is_empty() {
                    self.active_ns = candidate.0;
                    self.active_id = candidate.1;
                }
                self.picker_open = false;
                self.kill_confirm.clear();
                (true, true, true)
            }
            "termd_cols_toggle" => {
                self.cols_mode = if self.cols_mode == "80" {
                    "48".into()
                } else {
                    "80".into()
                };
                (true, true, false)
            }
            "termd_ctrl_toggle" => {
                self.ctrl_latched = !self.ctrl_latched;
                (true, true, false)
            }
            "termd_key" => {
                let spec = j_str(j_get(data, "key")).unwrap_or("").to_string();
                let explicit_ctrl = j_bool(j_get(data, "ctrl")).unwrap_or(false);
                let alt = j_bool(j_get(data, "alt")).unwrap_or(false);
                let shift = j_bool(j_get(data, "shift")).unwrap_or(false);
                let use_ctrl = explicit_ctrl || self.ctrl_latched;
                let mut store_changed = false;
                if self.ctrl_latched {
                    self.ctrl_latched = false;
                    store_changed = true;
                }
                if !spec.is_empty() && !self.active_id.is_empty() {
                    let _ = self.act_tx.send(ActionJob::Key {
                        ns: self.active_ns.clone(),
                        id: self.active_id.clone(),
                        spec,
                        ctrl: use_ctrl,
                        alt,
                        shift,
                    });
                }
                (true, store_changed, false)
            }
            "termd_kill_arm" => {
                if !self.active_id.is_empty() {
                    let tag = format!("{}:{}", self.active_ns, self.active_id);
                    if self.kill_confirm == tag {
                        self.kill_confirm.clear();
                    } else {
                        self.kill_confirm = tag;
                    }
                    return (true, true, false);
                }
                (true, false, false)
            }
            "termd_kill_cancel" => {
                self.kill_confirm.clear();
                (true, true, false)
            }
            "termd_kill_confirm" => {
                if !self.active_id.is_empty() {
                    let _ = self.act_tx.send(ActionJob::Kill {
                        ns: self.active_ns.clone(),
                        id: self.active_id.clone(),
                    });
                }
                self.kill_confirm.clear();
                (true, true, false)
            }
            "termd_confirm_paste" => {
                if let Some(sp) = self.staged_paste.take() {
                    let _ = self.act_tx.send(ActionJob::Paste {
                        ns: sp.ns,
                        id: sp.id,
                        text: sp.text,
                        enter: sp.enter,
                        allow_unsafe: true,
                    });
                    return (true, true, false);
                }
                (true, false, false)
            }
            "termd_cancel_paste" => {
                if self.staged_paste.take().is_some() {
                    return (true, true, false);
                }
                (true, false, false)
            }
            _ => (false, false, false),
        }
    }

    /// Handles socket `"op": "termd"` notifications from `tbui termd` (`activated`, `stage_paste`, `probe`).
    /// Returns `(reply_json, store_changed, want_front_terminal)`.
    pub fn handle_socket_op(&mut self, req: &JsonValue) -> Result<(JsonValue, bool, bool), String> {
        let sub = j_str(j_get(req, "sub")).unwrap_or("probe");
        match sub {
            "activated" => {
                let ns = j_str(j_get(req, "ns")).unwrap_or("default").to_string();
                let id = j_str(j_get(req, "id")).unwrap_or("").to_string();
                let focus = j_bool(j_get(req, "focus")).unwrap_or(false);
                if !id.is_empty() {
                    self.active_ns = ns;
                    self.active_id = id;
                    self.picker_open = false;
                    self.kill_confirm.clear();
                }
                self.probe_disconnected();
                Ok((j_obj(vec![]), true, focus))
            }
            "stage_paste" => {
                let ns = j_str(j_get(req, "ns")).unwrap_or("default").to_string();
                let id = j_str(j_get(req, "id")).unwrap_or("").to_string();
                let text = j_str(j_get(req, "text")).unwrap_or("").to_string();
                let enter = j_bool(j_get(req, "enter")).unwrap_or(false);
                let (lines, preview) = paste_preview(&text);
                self.staged_paste = Some(StagedPaste {
                    ns,
                    id,
                    text,
                    enter,
                    lines,
                    preview,
                });
                Ok((j_obj(vec![("staged", JsonValue::Bool(true))]), true, false))
            }
            "probe" => {
                self.probe_disconnected();
                Ok((j_obj(vec![]), false, false))
            }
            other => Err(format!("unknown termd socket sub-op {other:?}")),
        }
    }
}

fn action_worker(rx: Receiver<ActionJob>, ev_tx: Sender<BridgeEvent>) {
    while let Ok(job) = rx.recv() {
        match job {
            ActionJob::Create {
                ns,
                id,
                cwd,
                cmd,
                cols,
                rows,
                focus_view,
            } => match create_terminal(
                &ns,
                id.as_deref(),
                cwd.as_deref(),
                cmd.as_deref(),
                cols,
                rows,
            ) {
                Ok(rep) => {
                    let created_id = j_str(j_get(&rep, "id")).unwrap_or("").to_string();
                    let _ = ev_tx.send(BridgeEvent::Created {
                        ns,
                        id: created_id,
                        focus_view,
                    });
                    SignalToUI::set_ui_signal();
                }
                Err(e) => {
                    let _ = ev_tx.send(BridgeEvent::Error(format!("create: {e}")));
                    SignalToUI::set_ui_signal();
                }
            },
            ActionJob::Key {
                ns,
                id,
                spec,
                ctrl,
                alt,
                shift,
            } => {
                if let Err(e) = send_key_terminal(&ns, Some(&id), &spec, ctrl, alt, shift) {
                    let _ = ev_tx.send(BridgeEvent::Error(format!("key {spec}: {e}")));
                    SignalToUI::set_ui_signal();
                }
            }
            ActionJob::Paste {
                ns,
                id,
                text,
                enter,
                allow_unsafe,
            } => match paste_terminal(&ns, Some(&id), &text, enter, allow_unsafe) {
                Ok((resolved_id, PasteOutcome::UnsafePaste { lines, preview })) => {
                    let _ = ev_tx.send(BridgeEvent::PasteUnsafe {
                        ns,
                        id: resolved_id,
                        text,
                        enter,
                        lines,
                        preview,
                    });
                    SignalToUI::set_ui_signal();
                }
                Ok((_, PasteOutcome::Written { .. })) => {
                    let _ = ev_tx.send(BridgeEvent::PasteDone);
                    SignalToUI::set_ui_signal();
                }
                Err(e) => {
                    let _ = ev_tx.send(BridgeEvent::Error(format!("paste: {e}")));
                    SignalToUI::set_ui_signal();
                }
            },
            ActionJob::Kill { ns, id } => match kill_terminal(&ns, Some(&id), None) {
                Ok(killed_id) => {
                    let _ = ev_tx.send(BridgeEvent::Killed { ns, id: killed_id });
                    SignalToUI::set_ui_signal();
                }
                Err(e) => {
                    let _ = ev_tx.send(BridgeEvent::Error(format!("kill: {e}")));
                    SignalToUI::set_ui_signal();
                }
            },
        }
    }
}

fn parse_status_terminals(ns: &str, doc: &JsonValue) -> Vec<TermEntry> {
    let Some(JsonValue::Array(arr)) = j_get(doc, "terminals") else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let Some(id) = j_str(j_get(item, "id")) else {
            continue;
        };
        let title = j_str(j_get(item, "title")).unwrap_or("").to_string();
        let cwd = j_str(j_get(item, "cwd")).unwrap_or("").to_string();
        let pwd = j_str(j_get(item, "pwd")).unwrap_or("").to_string();
        let cmd = match j_get(item, "argv") {
            Some(JsonValue::Array(argv)) => argv
                .first()
                .and_then(|v| j_str(Some(v)))
                .map(|p| {
                    Path::new(p)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(p)
                        .trim_start_matches('-')
                        .to_string()
                })
                .unwrap_or_else(|| "shell".into()),
            _ => "shell".into(),
        };
        let pid = j_i64(j_get(item, "pid")).unwrap_or(0);
        let cols = j_u64(j_get(item, "cols")).unwrap_or(80);
        let rows = j_u64(j_get(item, "rows")).unwrap_or(24);
        let alt = j_bool(j_get(item, "alt")).unwrap_or(false);
        let created_at = j_u64(j_get(item, "created_at")).unwrap_or(0);

        let short_title = if !title.is_empty() {
            title.chars().take(24).collect::<String>()
        } else if !cmd.is_empty() {
            cmd.clone()
        } else {
            id.to_string()
        };
        let label = if ns == "default" {
            format!("{id} ({short_title})")
        } else {
            format!("{ns}:{id}")
        };

        out.push(TermEntry {
            ns: ns.to_string(),
            id: id.to_string(),
            title,
            cwd,
            pwd,
            cmd,
            pid,
            cols,
            rows,
            alt,
            created_at,
            label,
        });
    }
    out
}

fn follow_ns_worker(
    ns: String,
    paused: Arc<AtomicBool>,
    followers: Arc<Mutex<HashMap<String, UnixStream>>>,
    ev_tx: Sender<BridgeEvent>,
) {
    let mut backoff = Duration::from_millis(120);
    while !paused.load(Ordering::SeqCst) {
        let mut client = match SyncClient::dial(&ns, "tb-shell-status/0.1") {
            Ok(c) => c,
            Err(DialError::NotRunning(_)) | Err(DialError::UnsafeSocket(_)) => {
                let _ = ev_tx.send(BridgeEvent::Status {
                    ns: ns.clone(),
                    terminals: Vec::new(),
                });
                SignalToUI::set_ui_signal();
                break;
            }
            Err(_) => {
                if paused.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(2));
                continue;
            }
        };
        backoff = Duration::from_millis(120);

        if let Ok(ctrl_sock) = client.writer.try_clone() {
            if let Ok(mut map) = followers.lock() {
                map.insert(ns.clone(), ctrl_sock);
            }
        }
        if paused.load(Ordering::SeqCst) {
            let _ = client.writer.shutdown(Shutdown::Both);
            break;
        }

        if client.welcome.has("status") {
            let req = j_obj(vec![
                ("req", JsonValue::U64(1)),
                ("op", JsonValue::String("status_follow".into())),
                ("interval_ms", JsonValue::U64(400)),
            ])
            .serialize_json();
            if client
                .writer
                .write_all(&termd_wire::encode_frame(
                    termd_wire::FRAME_REQUEST,
                    0,
                    req.as_bytes(),
                ))
                .is_err()
            {
                break;
            }
        } else {
            // Fallback for 1.0 daemon without status_follow: one-shot list.
            if let Ok(rep) = client.request("list", vec![]) {
                let terms = parse_status_terminals(&ns, &rep);
                let _ = ev_tx.send(BridgeEvent::Status {
                    ns: ns.clone(),
                    terminals: terms,
                });
                SignalToUI::set_ui_signal();
            }
            break;
        }

        use std::io::Write;
        let mut daemon_exiting = false;
        let mut saw_restart = false;

        loop {
            if paused.load(Ordering::SeqCst) {
                break;
            }
            let frame = match read_frame(&mut client.reader, client.welcome.max_frame) {
                Ok(f) => f,
                Err(_) => break,
            };
            match frame.ftype {
                FRAME_EVENT if frame.stream == 0 => {
                    let Ok(s) = std::str::from_utf8(&frame.payload) else {
                        continue;
                    };
                    let Ok(doc) = JsonValue::deserialize_json(s) else {
                        continue;
                    };
                    if j_str(j_get(&doc, "ev")) != Some("status") {
                        continue;
                    }
                    if let Some(d) = j_get(&doc, "daemon") {
                        if j_bool(j_get(d, "exiting")) == Some(true) {
                            daemon_exiting = true;
                        }
                    }
                    let terms = parse_status_terminals(&ns, &doc);
                    let _ = ev_tx.send(BridgeEvent::Status {
                        ns: ns.clone(),
                        terminals: terms,
                    });
                    SignalToUI::set_ui_signal();
                    if daemon_exiting {
                        break;
                    }
                }
                FRAME_ERROR => {
                    if let Ok(s) = std::str::from_utf8(&frame.payload) {
                        if let Ok(val) = JsonValue::deserialize_json(s) {
                            let err_obj = j_get(&val, "error").unwrap_or(&val);
                            if j_str(j_get(err_obj, "code")) == Some("restart") {
                                saw_restart = true;
                            }
                        }
                    }
                    break;
                }
                _ => {}
            }
        }

        if let Ok(mut map) = followers.lock() {
            map.remove(&ns);
        }

        if paused.load(Ordering::SeqCst) || daemon_exiting {
            if daemon_exiting {
                let _ = ev_tx.send(BridgeEvent::Status {
                    ns: ns.clone(),
                    terminals: Vec::new(),
                });
                SignalToUI::set_ui_signal();
            }
            break;
        }

        if saw_restart {
            std::thread::sleep(Duration::from_millis(80));
            continue;
        }
        // Unexpected EOF: check if socket still exists; if gone, clear & stop.
        std::thread::sleep(Duration::from_millis(120));
        if !socket_path(&ns).exists() {
            let _ = ev_tx.send(BridgeEvent::Status {
                ns: ns.clone(),
                terminals: Vec::new(),
            });
            SignalToUI::set_ui_signal();
            break;
        }
    }

    if let Ok(mut map) = followers.lock() {
        map.remove(&ns);
    }
}
