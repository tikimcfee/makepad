//! tb-shell: one JSON store, one Splash isolate per surface, a strip and a
//! guard the shell draws itself, and a Unix socket that speaks
//! newline-delimited JSON.
//!
//! A write from anywhere (a client's `set`, a tap's `set(k, v)`) is a JSON
//! merge patch on the one store. The shell applies it, hands the new values
//! of the touched top-level keys to each surface that reads them (the
//! prelude's `__apply` re-renders `main`), and tells the watchers.

pub use makepad_widgets;

mod io;
mod paneltrace;
mod shell;
mod sched;
mod split;
mod stream;

use io::{Msg, Peer};
use makepad_micro_serde::{DeJson, JsonValue, SerJson};
use makepad_widgets::makepad_platform::log_ring;
use makepad_widgets::splash_host::take_splash_host_requests;
use makepad_widgets::*;
use shell::{GuardView, Shell};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};

app_main!(App);

const PRELUDE: &str = include_str!("prelude.splash");
const BASE: &str = include_str!("../lib/base.splash");
/// A key only the shell writes.
const SHELL_KEY: &str = "shell";
/// Lines ahead of a body: Splash's own prefix, the prelude, the library.
fn lines_ahead() -> usize {
    3 + PRELUDE.lines().count() + BASE.lines().count()
}

script_mod! {
    use mod.prelude.widgets.*

    startup() do #(App::script_component(vm)){
        ui: Root{
            main_window := Window{
                window.title: "tb-shell"
                window.inner_size: vec2(384, 854)
                pass.clear_color: #x0b0e14
                body +: {
                    shell := mod.widgets.Shell{}
                }
            }
        }
    }
}

/// Which top-level store keys a body reads, from a scan of its text.
#[derive(Clone)]
enum Reads {
    All,
    Keys(Vec<String>),
}

impl Reads {
    fn scan(body: &str) -> Reads {
        let b = body.as_bytes();
        let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        let mut keys = Vec::new();
        let mut at = 0;
        while let Some(off) = body[at..].find("state") {
            let i = at + off;
            at = i + 5;
            if i > 0 && (ident(b[i - 1]) || b[i - 1] == b'.') {
                continue;
            }
            match b.get(at) {
                Some(b'[') => return Reads::All,
                Some(b'.') => {
                    let end = b[at + 1..].iter().position(|c| !ident(*c)).map_or(b.len(), |p| at + 1 + p);
                    let key = &body[at + 1..end];
                    if !key.is_empty() && !keys.iter().any(|k| k == key) {
                        keys.push(key.to_string());
                    }
                }
                _ => {}
            }
        }
        Reads::Keys(keys)
    }

    fn any(&self, touched: &[String]) -> bool {
        match self {
            Reads::All => true,
            Reads::Keys(keys) => keys.iter().any(|k| touched.contains(k)),
        }
    }
}

/// Shader code in a body: an anonymous `fn(` (how a draw shader's methods
/// are written) or a `pixel:` / `vertex:` key. Agents may not send it until a
/// per-surface frame-time watch exists; bodies we wrote and tried are trusted.
fn shader_text(body: &str) -> Option<String> {
    let b = body.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    for (i, _) in body.match_indices("fn") {
        let before_ok = i == 0 || !ident(b[i - 1]);
        let rest = body[i + 2..].trim_start();
        if before_ok && rest.starts_with('(') {
            return Some(snippet(body, i));
        }
    }
    for key in ["pixel", "vertex"] {
        for (i, _) in body.match_indices(key) {
            let before_ok = i == 0 || !ident(b[i - 1]);
            let after = &body[i + key.len()..];
            let after_ok = after.chars().next().map_or(false, |c| !c.is_ascii_alphanumeric() && c != '_');
            if before_ok && after_ok && after.trim_start().starts_with(':') {
                return Some(snippet(body, i));
            }
        }
    }
    None
}

fn snippet(body: &str, at: usize) -> String {
    let line = body[..at].lines().count().max(1);
    let text = body[at..].lines().next().unwrap_or("");
    format!("line {line}: {}", text.chars().take(40).collect::<String>())
}

struct WatchSpec {
    surfaces: Option<Vec<String>>,
    keys: Option<Vec<String>>,
}

impl WatchSpec {
    fn wants_surface(&self, surface: &str) -> bool {
        match (&self.surfaces, &self.keys) {
            (Some(s), _) => s.iter().any(|s| s == surface),
            (None, Some(_)) => false,
            (None, None) => true,
        }
    }
    fn wants_keys(&self, touched: &[String]) -> bool {
        match (&self.keys, &self.surfaces) {
            (Some(keys), _) => keys.iter().any(|k| touched.contains(k)),
            (None, Some(_)) => false,
            (None, None) => true,
        }
    }
}

struct Conn {
    tx: Sender<String>,
    peer: Peer,
    watch: Option<WatchSpec>,
}

struct Guard {
    conn: u64,
    view: GuardView,
}

/// Where the panel state and the recording mark come from: kernel state, never the store.
#[derive(Default)]
struct Sources {
    /// The panel state file and the content that means asleep. It gives no
    /// change event and is never polled: it is read when the shell is awake
    /// anyway (an event), at most every quarter second.
    panel: Option<(String, String)>,
    panel_read: Option<std::time::Instant>,
    /// The capture stream's status file ("closed" when no capture is open),
    /// read again on each open or close in the sound device directory.
    mic_status: Option<String>,
}

#[derive(Default)]
struct State {
    store: HashMap<String, JsonValue>,
    rx: Option<Receiver<Msg>>,
    conns: HashMap<u64, Conn>,
    reads: HashMap<String, Reads>,
    bodies: HashMap<String, String>,
    /// The views that were in front before the current one, newest last: closing
    /// the front surface brings back the newest that still exists, else home.
    front_history: Vec<String>,
    /// Surfaces a connection showed with "owned": true (a question): they close with it.
    owned: HashMap<u64, Vec<String>>,
    guards: Vec<Guard>,
    guard_uids: HashSet<u32>,
    trusted: HashSet<String>,
    sources: Sources,
    panel_off: bool,
    log_cursor: u64,
    split: split::Split,
    trace: paneltrace::PanelTrace,
    /// Surfaces whose `on_render` runs at the next frame: at most once a frame each,
    /// with the newest state, however many updates arrive before it.
    render_pending: Vec<WidgetRef>,
    /// Hidden surfaces fed since they last rendered: they render when they come to the front.
    stale: Vec<WidgetRef>,
    render_frame: NextFrame,
    /// Surfaces loaded from the views folder (the rest are the agents').
    views: HashSet<String>,
    notified: bool,
    placement: sched::Placement,
    /// The note's end (a one-shot timer, only while a note is up).
    note_timer: Timer,
}

#[derive(Script, ScriptHook)]
pub struct App {
    #[live] ui: WidgetRef,
    #[rust] st: State,
}

impl AppMain for App {
    fn script_mod(vm: &mut ScriptVm) -> ScriptValue {
        crate::makepad_widgets::script_mod(vm);
        // StreamView in every surface's isolate (before the first one is made).
        makepad_widgets::widget_async::register_splash_isolate_mod(stream::isolate_mod);
        crate::shell::script_mod(vm);
        self::script_mod(vm)
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        if let Event::Startup = event {
            self.start(cx);
        }
        self.st.split.event_start();
        self.st.trace.event(event);
        self.check_panel(cx);
        if self.st.render_frame.is_event(event).is_some() {
            self.render_pending(cx);
        }
        if let Event::Signal = event {
            if self.st.split.on() {
                let t = std::time::Instant::now();
                self.drain_socket(cx);
                let dt = t.elapsed();
                self.st.split.add_drain(dt);
            } else {
                self.drain_socket(cx);
            }
        }
        self.drain_host_requests(cx);
        if let Event::Draw(_) = event {
            self.ui.handle_event(cx, event, &mut Scope::empty());
        } else {
            let ui = &self.ui;
            self.st.split.timed(4, || ui.handle_event(cx, event, &mut Scope::empty()));
        }
        self.drain_host_requests(cx);
        if let Event::Draw(_) = event {
            self.notify_ready();
        }
        if self.st.note_timer.is_event(event).is_some() {
            self.clear_note(cx);
        }
        let home = self.shell(cx).borrow_mut::<Shell>().map_or(false, |mut sh| sh.take_go_home());
        if home {
            self.clear_note(cx);
        }
        if home && self.st.guards.is_empty() && self.st.bodies.contains_key("home") {
            self.apply(cx, &obj(vec![("front", "home".into())]), "shell");
        }
        let answer = self.shell(cx).borrow_mut::<Shell>().and_then(|mut sh| sh.take_answer());
        if let Some(yes) = answer {
            self.answer_guard(cx, yes);
        }
        self.errors_to_watchers();
        self.st.split.event_end(event);
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn obj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn strings(v: Option<&JsonValue>) -> Option<Vec<String>> {
    Some(v?.as_array()?.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
}

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// RFC 7386: objects merge, everything else replaces, null deletes.
fn merge(target: &mut JsonValue, patch: &JsonValue) {
    let JsonValue::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !matches!(target, JsonValue::Object(_)) {
        *target = JsonValue::Object(HashMap::new());
    }
    let JsonValue::Object(target) = target else { unreachable!() };
    for (k, v) in patch {
        if *v == JsonValue::Null {
            target.remove(k);
        } else {
            merge(target.entry(k.clone()).or_insert(JsonValue::Null), v);
        }
    }
}

/// Parses a log location `splash:<surface>:<line>:<col>` into body-relative
/// numbers (line 0: in the prelude or the library).
fn locate(text: &str, name: &str) -> Option<(usize, usize, String)> {
    let tag = format!("splash:{name}:");
    let at = text.find(&tag)?;
    let rest = &text[at + tag.len()..];
    let mut parts = rest.splitn(3, |c: char| c == ':' || c == ' ');
    let line: usize = parts.next()?.parse().ok()?;
    let col: usize = parts.next()?.trim_end_matches(':').parse().ok().unwrap_or(0);
    let message = rest.split_once(" - ").map(|(_, m)| m).or_else(|| rest.split_once(": ").map(|(_, m)| m)).unwrap_or(rest).to_string();
    let line = line.checked_sub(lines_ahead()).unwrap_or(0);
    Some((line, col, message))
}

/// A surface's name shows in the strip: none may start with "guard", the word the
/// strip uses for the shell's own guard.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && !name.to_ascii_lowercase().starts_with("guard")
}

impl App {
    fn shell(&self, cx: &Cx) -> WidgetRef {
        self.ui.widget(cx, ids!(shell))
    }

    fn start(&mut self, cx: &mut Cx) {
        let (tx, rx) = io::new_channel();
        let path = env("TB_SHELL_SOCK").unwrap_or_else(|| format!("{}/tb-shell.sock", env("XDG_RUNTIME_DIR").unwrap_or("/tmp".into())));
        io::listen(&path, tx.clone());
        self.st.rx = Some(rx);
        self.st.guard_uids = env("TB_SHELL_GUARD_UIDS").unwrap_or("0".into()).split(',').filter_map(|u| u.trim().parse().ok()).collect();
        // Bodies we wrote and tried: their shader text is allowed.
        let trusted = env("TB_SHELL_TRUSTED").or_else(|| {
            std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("trusted").to_string_lossy().to_string()))
        });
        if let Some(dir) = trusted {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                if let Ok(body) = std::fs::read_to_string(entry.path()) {
                    self.st.trusted.insert(body);
                }
            }
            log!("tb-shell: {} trusted bodies from {dir}", self.st.trusted.len());
        }
        // The panel and the marks: files and device nodes, watched by inotify.
        let s = &mut self.st.sources;
        if let Some(file) = env("TB_SHELL_PANEL_FILE") {
            s.panel = Some((file, env("TB_SHELL_PANEL_OFF").unwrap_or("off".into())));
        }
        if let (Some(dir), Some(status)) = (env("TB_SHELL_MIC_DIR"), env("TB_SHELL_MIC_STATUS")) {
            io::watch_capture(&dir, tx);
            s.mic_status = Some(status);
        }
        self.st.log_cursor = log_ring::read_since(u64::MAX, 0).0;
        self.st.split = split::Split::from_env();
        self.st.trace = paneltrace::PanelTrace::from_env();
        self.guard_input(cx);
        self.st.placement = sched::Placement::from_env();
        self.st.placement.apply();
        self.read_mic(cx);
        let before = Self::surfaces_file().and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default();
        self.load_views(cx);
        self.left_out(cx, &before);
    }

    /// The views folder (TB_SHELL_VIEWS, else `views/` beside the binary): every
    /// `NAME.splash` becomes surface NAME; `home` comes to the front.
    fn load_views(&mut self, cx: &mut Cx) {
        let dir = env("TB_SHELL_VIEWS").map(std::path::PathBuf::from).or_else(|| {
            std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("views")))
        });
        let Some(dir) = dir else { return };
        let mut files: Vec<_> = std::fs::read_dir(&dir).into_iter().flatten().flatten().map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |x| x == "splash")).collect();
        files.sort();
        for path in files {
            let name = path.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let Ok(body) = std::fs::read_to_string(&path) else { continue };
            // A view before it is shown, so the agents' surfaces file never lists it.
            self.st.views.insert(name.clone());
            match self.show(cx, &name, &body, false) {
                Ok(reply) if reply.get("ok") != Some(&JsonValue::Bool(false)) => {
                    log!("tb-shell: view {name} shown in {} ms", reply.get("ms").map_or(String::new(), |m| m.serialize_json()));
                }
                Ok(reply) => {
                    self.st.views.remove(&name);
                    error!("tb-shell: view {name}: {}", reply.serialize_json())
                }
                Err(e) => {
                    self.st.views.remove(&name);
                    error!("tb-shell: view {name}: {e}")
                }
            }
        }
        log!("tb-shell: {} views from {}", self.st.views.len(), dir.display());
        if self.st.views.contains("home") {
            self.apply(cx, &obj(vec![("front", "home".into())]), "shell");
        }
    }

    /// The agents' surfaces are not brought back after a restart (a body that
    /// crashed the shell would crash it again). Their names live in a file in
    /// XDG_RUNTIME_DIR (memory, not disk); after a restart the strip names them.
    fn surfaces_file() -> Option<String> {
        env("XDG_RUNTIME_DIR").map(|d| format!("{d}/tb-shell.agent-surfaces"))
    }

    /// `before`: the file as the last run left it. The shell's own views are never named;
    /// the note goes on a tap on the strip or after ten seconds.
    fn left_out(&mut self, cx: &mut Cx, before: &str) {
        let names: Vec<&str> = before.split_whitespace().filter(|n| !self.st.views.contains(*n)).collect();
        if !names.is_empty() {
            let note = format!("left out: {}", names.join(", "));
            log!("tb-shell: after a restart, {note}");
            if let Some(mut sh) = self.shell(cx).borrow_mut::<Shell>() {
                sh.note = note;
            }
            self.st.note_timer = cx.start_timeout(10.0);
        }
        self.write_surfaces_file();
    }

    fn clear_note(&mut self, cx: &mut Cx) {
        cx.stop_timer(self.st.note_timer);
        if let Some(mut sh) = self.shell(cx).borrow_mut::<Shell>() {
            if !sh.note.is_empty() {
                sh.note.clear();
                cx.redraw_all();
            }
        }
    }

    /// systemd's Type=notify: READY=1 once the first frame is drawn.
    fn notify_ready(&mut self) {
        if std::mem::replace(&mut self.st.notified, true) {
            return;
        }
        // GL threads the driver started with the first frame get placed too.
        self.st.placement.apply();
        let Some(path) = env("NOTIFY_SOCKET") else { return };
        use std::os::linux::net::SocketAddrExt;
        let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else { return };
        let sent = match path.strip_prefix('@') {
            Some(name) => std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).and_then(|a| sock.send_to_addr(b"READY=1", &a)),
            None => sock.send_to(b"READY=1", &path),
        };
        if let Err(e) = sent {
            error!("tb-shell: READY=1 to {path}: {e}");
        }
    }

    fn write_surfaces_file(&self) {
        let Some(file) = Self::surfaces_file() else { return };
        let mut names: Vec<&String> = self.st.bodies.keys().filter(|n| !self.st.views.contains(*n)).collect();
        names.sort();
        let _ = std::fs::write(&file, names.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(" "));
    }

    /// Who may answer a guard: the --remote endpoint decides whether input can be
    /// injected; root's test switch, a root-owned file at a fixed path (a path given
    /// by env would let any process of the user point it at a root-owned file), lets
    /// injected taps count, for the proof scripts. The log and the strip say so.
    fn guard_input(&mut self, cx: &mut Cx) {
        const TEST_TAPS: &str = "/etc/tb-shell/guard-test-taps";
        let remote_on = std::env::args().any(|a| a.starts_with("--remote")) || env("MAKEPAD_REMOTE").is_some();
        let roots = |p: &str| {
            use std::os::unix::fs::MetadataExt;
            std::fs::symlink_metadata(p).map_or(false, |m| m.uid() == 0 && m.mode() & 0o022 == 0)
        };
        let test_taps = roots(TEST_TAPS) && roots("/etc/tb-shell");
        if test_taps {
            log!("tb-shell: TEST SWITCH ON ({TEST_TAPS}, root's): injected input may answer a guard");
        }
        log!("tb-shell: guard answers: {}", if !remote_on { "the device only (no --remote endpoint)" } else if test_taps { "any input (test switch)" } else { "the device only (injected input through --remote is ignored)" });
        if let Some(mut sh) = self.shell(cx).borrow_mut::<Shell>() {
            sh.guard_input.remote_on = remote_on;
            sh.guard_input.test_taps = test_taps;
            sh.marks.test_taps = test_taps;
        }
    }

    /// The recording mark, from the kernel's capture status (on start and on
    /// each open or close in the sound device directory).
    fn read_mic(&mut self, cx: &mut Cx) {
        let mic = self.st.sources.mic_status.as_deref().and_then(read_trim).map_or(false, |t| t != "closed");
        if let Some(mut sh) = self.shell(cx).borrow_mut::<Shell>() {
            if sh.marks.mic != mic {
                sh.marks.mic = mic;
                cx.redraw_all();
            }
        }
    }

    fn check_panel(&mut self, cx: &mut Cx) {
        let s = &mut self.st.sources;
        let Some((file, off_value)) = &s.panel else { return };
        if s.panel_read.map_or(false, |t| t.elapsed().as_millis() < 250) {
            return;
        }
        s.panel_read = Some(std::time::Instant::now());
        let value = read_trim(file);
        let off = value.as_deref() == Some(off_value.as_str());
        self.st.trace.read(value.as_deref());
        if off != self.st.panel_off {
            self.panel(cx, !off);
        }
    }

    /// Panel asleep: every isolate is torn down, so no timer runs and nothing
    /// asks for a frame. Awake: every surface is shown again from its body
    /// and the store, which holds all the state a view has.
    fn panel(&mut self, cx: &mut Cx, on: bool) {
        self.st.panel_off = !on;
        self.st.trace.changed();
        log!("tb-shell: panel {} at {}", if on { "on: surfaces back" } else { "off: surfaces stopped" }, paneltrace::stamp());
        let shell = self.shell(cx);
        let surfaces: Vec<(String, WidgetRef)> = shell.borrow::<Shell>().map_or(vec![], |sh| sh.surfaces.clone());
        for (name, splash) in surfaces {
            if on {
                let body = self.st.bodies.get(&name).cloned().unwrap_or_default();
                splash.set_text(cx, &format!("{PRELUDE}{BASE}{body}"));
                self.feed_all(cx, &name, &splash);
            } else {
                splash.set_text(cx, "");
            }
        }
        cx.redraw_all();
        let ev = obj(vec![("ev", "panel".into()), ("on", JsonValue::Bool(on))]).serialize_json();
        for c in self.st.conns.values().filter(|c| c.watch.is_some()) {
            let _ = c.tx.send(ev.clone());
        }
    }

    fn drain_socket(&mut self, cx: &mut Cx) {
        let Some(rx) = self.st.rx.take() else { return };
        let mut sources_changed = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Open { conn, tx, peer } => {
                    self.st.conns.insert(conn, Conn { tx, peer, watch: None });
                }
                Msg::Closed { conn } => {
                    self.st.conns.remove(&conn);
                    // A question whose asker is gone goes too.
                    for name in self.st.owned.remove(&conn).unwrap_or_default() {
                        let _ = self.close(cx, &name);
                    }
                    // A guard whose asker is gone is withdrawn, never answered.
                    let raised = self.st.guards.first().map(|g| g.conn);
                    self.st.guards.retain(|g| g.conn != conn);
                    if raised == Some(conn) {
                        self.raise_next_guard(cx);
                    }
                }
                Msg::Changed => sources_changed = true,
                Msg::Line { conn, line } => {
                    if self.st.split.on() && (line.contains("\"op\":\"set\"") || line.contains("\"op\":\"push\"")) {
                        self.st.split.begin();
                        self.st.split.add_count("req-bytes", line.len() as f64);
                    }
                    let parsed = self.st.split.timed(0, || JsonValue::deserialize_json(&line));
                    let reply = match parsed {
                        Ok(req) => self.request(cx, conn, &req),
                        Err(e) => Err(format!("bad json: {e:?}")),
                    };
                    let reply = match reply {
                        Ok(None) => continue, // answered later (a guard)
                        Ok(Some(mut v)) => {
                            if let JsonValue::Object(m) = &mut v {
                                m.entry("ok".into()).or_insert(JsonValue::Bool(true));
                            }
                            v
                        }
                        Err(e) => obj(vec![("ok", JsonValue::Bool(false)), ("error", JsonValue::String(e))]),
                    };
                    let t = std::time::Instant::now();
                    self.send(conn, reply.serialize_json());
                    self.st.split.add_named("reply", t.elapsed());
                }
            }
        }
        self.st.rx = Some(rx);
        if sources_changed {
            self.read_mic(cx);
        }
    }

    fn send(&self, conn: u64, line: String) {
        if let Some(c) = self.st.conns.get(&conn) {
            let _ = c.tx.send(line);
        }
    }

    fn request(&mut self, cx: &mut Cx, conn: u64, req: &JsonValue) -> Result<Option<JsonValue>, String> {
        let t = std::time::Instant::now();
        if !matches!(req.get("op").and_then(|v| v.as_str()), Some("set") | Some("push")) {
            self.st.split.taint();
        }
        let r = self.request_inner(cx, conn, req);
        self.st.split.add_named("request", t.elapsed());
        r
    }

    fn request_inner(&mut self, cx: &mut Cx, conn: u64, req: &JsonValue) -> Result<Option<JsonValue>, String> {
        let op = req.get("op").and_then(|v| v.as_str()).ok_or("missing op")?;
        let surface = req.get("surface").and_then(|v| v.as_str());
        let peer = self.st.conns.get(&conn).map(|c| c.peer.clone()).ok_or("no connection")?;
        match op {
            "show" => {
                let name = surface.ok_or("show needs surface")?;
                let body = req.get("body").and_then(|v| v.as_str()).ok_or("show needs body")?;
                let front = req.get("front") != Some(&JsonValue::Bool(false));
                let reply = self.show(cx, name, body, front)?;
                if req.get("owned") == Some(&JsonValue::Bool(true)) && reply.get("ok") != Some(&JsonValue::Bool(false)) {
                    self.st.owned.entry(conn).or_default().push(name.to_string());
                }
                Ok(Some(reply))
            }
            "set" => {
                let patch = req.get("patch").ok_or("set needs patch")?;
                if !matches!(patch, JsonValue::Object(_)) {
                    return Err("patch must be an object".into());
                }
                if let Some(key) = self.refused_key(patch) {
                    return Err(format!("the key {key} is the shell's"));
                }
                self.apply(cx, patch, &format!("client:{}", peer.label()));
                // "reply": false: a service's stream of updates takes no answers (errors still come).
                if req.get("reply") == Some(&JsonValue::Bool(false)) {
                    return Ok(None);
                }
                Ok(Some(obj(vec![])))
            }
            "push" => {
                // A stream, not the store: no merge, no script, no reply when it is taken.
                let name = req.get("stream").and_then(|v| v.as_str()).ok_or("push needs stream")?;
                let values: Vec<f32> = req.get("values").and_then(|v| v.as_array()).ok_or("push needs values")?
                    .iter().map(|v| match v {
                        JsonValue::F64(f) => *f as f32,
                        JsonValue::I64(i) => *i as f32,
                        JsonValue::U64(u) => *u as f32,
                        _ => 0.0,
                    }).collect();
                if values.len() > stream::MAX_VALUES {
                    log!("tb-shell: push to {name}: {} values, a view shows the first {}", values.len(), stream::MAX_VALUES);
                }
                stream::push(name, values, req.get("append") == Some(&JsonValue::Bool(true)));
                Ok(None)
            }
            "get" => {
                let keys = strings(req.get("keys"));
                let state = self.st.store.iter()
                    .filter(|(k, _)| keys.as_ref().map_or(true, |ks| ks.contains(k)))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Ok(Some(obj(vec![("state", JsonValue::Object(state))])))
            }
            "watch" => {
                let spec = WatchSpec { surfaces: strings(req.get("surfaces")), keys: strings(req.get("keys")) };
                self.st.conns.get_mut(&conn).ok_or("no connection")?.watch = Some(spec);
                Ok(Some(obj(vec![])))
            }
            "close" => {
                let name = surface.ok_or("close needs surface")?;
                self.close(cx, name).map(Some)
            }
            "guard" => {
                if !self.st.guard_uids.contains(&peer.uid) {
                    return Err(format!("guard: uid {} may not raise a guard", peer.uid));
                }
                let text = req.get("text").and_then(|v| v.as_str()).ok_or("guard needs text")?.to_string();
                let yes = req.get("yes").and_then(|v| v.as_str()).unwrap_or("Proceed").to_string();
                let no = req.get("no").and_then(|v| v.as_str()).unwrap_or("Cancel").to_string();
                let view = GuardView { text, who: format!("asked by {}", peer.label()), yes, no };
                self.st.guards.push(Guard { conn, view });
                if self.st.guards.len() == 1 {
                    self.raise_next_guard(cx);
                }
                Ok(None)
            }
            _ => Err(format!("unknown op {op}")),
        }
    }

    /// The shell's own key, and `front` while a guard is up.
    fn refused_key(&self, patch: &JsonValue) -> Option<&'static str> {
        if patch.get(SHELL_KEY).is_some() {
            return Some(SHELL_KEY);
        }
        if !self.st.guards.is_empty() && patch.get("front").is_some() {
            return Some("front");
        }
        None
    }

    fn raise_next_guard(&mut self, cx: &mut Cx) {
        let view = self.st.guards.first().map(|g| g.view.clone());
        if let Some(mut sh) = self.shell(cx).borrow_mut::<Shell>() {
            sh.set_guard(cx, view);
        }
    }

    /// Only a tap on the raised guard gets here; it answers the asker's own connection.
    fn answer_guard(&mut self, cx: &mut Cx, yes: bool) {
        if self.st.guards.is_empty() {
            return;
        }
        let g = self.st.guards.remove(0);
        let choice = if yes { &g.view.yes } else { &g.view.no };
        let drawn = self.shell(cx).borrow::<Shell>().and_then(|sh| sh.guard_drawn_ms).map_or(JsonValue::Null, |ms| JsonValue::F64((ms * 10.0).round() / 10.0));
        let reply = obj(vec![("ok", JsonValue::Bool(true)), ("answer", if yes { "yes" } else { "no" }.into()), ("choice", choice.as_str().into()), ("drawn_ms", drawn)]);
        self.send(g.conn, reply.serialize_json());
        self.raise_next_guard(cx);
    }

    fn show(&mut self, cx: &mut Cx, name: &str, body: &str, front: bool) -> Result<JsonValue, String> {
        if !valid_name(name) {
            return Err("a surface name is 1 to 64 of A-Z a-z 0-9 - _ . and does not start with \"guard\"".into());
        }
        if !self.st.trusted.contains(body) {
            if let Some(at) = shader_text(body) {
                return Err(format!("shader text is not taken from agents yet (no frame-time watch): {at}"));
            }
        }
        let started = std::time::Instant::now();
        let shell = self.shell(cx);
        let Some(mut sh) = shell.borrow_mut::<Shell>() else { return Err("no shell".into()) };
        let (splash, created) = match sh.surface(name) {
            Some(w) => (w, false),
            None => (sh.add(cx, name), true),
        };
        drop(sh);
        if !self.st.panel_off {
            splash.set_text(cx, &format!("{PRELUDE}{BASE}{body}"));
            let errors: Vec<JsonValue> = splash.as_splash().last_errors().into_iter().map(|e| match locate(&e, name) {
                Some((line, col, msg)) => obj(vec![("line", (line as u64).into()), ("col", (col as u64).into()), ("message", msg.into())]),
                None => obj(vec![("message", e.into())]),
            }).collect();
            if !errors.is_empty() {
                if created {
                    shell.borrow_mut::<Shell>().map(|mut sh| sh.remove(cx, name));
                }
                // The previous body (if any) keeps drawing.
                return Ok(obj(vec![("ok", JsonValue::Bool(false)), ("error", "body did not evaluate".into()), ("errors", JsonValue::Array(errors))]));
            }
        }
        self.st.bodies.insert(name.to_string(), body.to_string());
        self.write_surfaces_file();
        self.st.reads.insert(name.to_string(), Reads::scan(body));
        if !self.st.panel_off {
            self.feed_all(cx, name, &splash);
        }
        // Behind a raised guard the front does not change.
        if front && self.st.guards.is_empty() {
            self.apply(cx, &obj(vec![("front", JsonValue::String(name.into()))]), "shell");
        }
        cx.redraw_all();
        let reads = match &self.st.reads[name] {
            Reads::All => JsonValue::String("*".into()),
            Reads::Keys(k) => JsonValue::Array(k.iter().map(|k| k.as_str().into()).collect()),
        };
        // Evaluation and the first feed; the frame that draws it comes after.
        let ms = JsonValue::F64((started.elapsed().as_secs_f64() * 1e4).round() / 10.0);
        Ok(obj(vec![("errors", JsonValue::Array(vec![])), ("reads", reads), ("ms", ms)]))
    }

    fn close(&mut self, cx: &mut Cx, name: &str) -> Result<JsonValue, String> {
        let removed = self.shell(cx).borrow_mut::<Shell>().map_or(false, |mut sh| sh.remove(cx, name));
        if !removed {
            return Err(format!("no surface {name}"));
        }
        self.st.reads.remove(name);
        self.st.bodies.remove(name);
        self.write_surfaces_file();
        self.st.front_history.retain(|n| n != name);
        if self.st.store.get("front").and_then(|v| v.as_str()) == Some(name) {
            // Back to what was in front before it; if that is gone too, home.
            let back = self.st.front_history.iter().rev().find(|n| self.st.bodies.contains_key(*n)).cloned()
                .or_else(|| self.st.bodies.contains_key("home").then(|| "home".to_string()));
            let back = back.map_or(JsonValue::Null, JsonValue::String);
            self.apply(cx, &obj(vec![("front", back)]), "shell");
        }
        Ok(obj(vec![]))
    }

    /// `on_render` does not run on first show: hand a surface every key it reads.
    fn feed_all(&mut self, cx: &mut Cx, name: &str, splash: &WidgetRef) {
        let Some(reads) = self.st.reads.get(name).cloned() else { return };
        let keys: Vec<String> = self.st.store.keys().filter(|k| reads.any(std::slice::from_ref(k))).cloned().collect();
        self.feed(cx, splash, &keys);
    }

    /// Hands a surface the current values of `keys` and re-renders it.
    fn feed(&mut self, cx: &mut Cx, splash: &WidgetRef, keys: &[String]) {
        let t = std::time::Instant::now();
        let r = self.feed_inner(cx, splash, keys);
        self.st.split.add_named("feed", t.elapsed());
        r
    }

    fn feed_inner(&mut self, cx: &mut Cx, splash: &WidgetRef, keys: &[String]) {
        let vals = keys.iter()
            .map(|k| (k.clone(), self.st.store.get(k).cloned().unwrap_or(JsonValue::Null)))
            .collect();
        let text = JsonValue::Object(vals).serialize_json();
        // Warm the widget tree from here: while the Splash is borrowed below,
        // the tree cannot read its children, and `ui.main` would not resolve.
        let split = &mut self.st.split;
        split.timed(5, || { let _ = splash.widget(cx, ids!(main)); });
        // The values are assigned now; `on_render` and the reload of its result
        // wait for the next frame (render_pending).
        if let Some(mut inner) = splash.borrow_mut::<Splash>() {
            let split = &mut self.st.split;
            if split.on() {
                // The bare cost of a host-to-script call carrying this text, for the split.
                split.add_count("assign-bytes", text.len() as f64);
                let t = std::time::Instant::now();
                inner.call_script_fn_with_strings(cx, live_id!(__noop), &[&text]);
                split.add_named("noop-call", t.elapsed());
            }
            split.timed(2, || inner.call_script_fn_with_strings(cx, live_id!(__assign), &[&text]));
        }
        if !self.st.render_pending.iter().any(|w| w.widget_uid() == splash.widget_uid()) {
            self.st.render_pending.push(splash.clone());
        }
        self.st.render_frame = cx.new_next_frame();
    }

    /// The frame has come: each surface fed since the last one renders once.
    fn render_pending(&mut self, cx: &mut Cx) {
        let front = self.st.store.get("front").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let front = self.shell(cx).borrow::<Shell>().and_then(|sh| sh.surface(&front));
        for splash in std::mem::take(&mut self.st.render_pending) {
            // Only the front surface renders; a hidden one waits until it is shown.
            if front.as_ref().map_or(true, |f| f.widget_uid() != splash.widget_uid()) {
                if !self.st.stale.iter().any(|w| w.widget_uid() == splash.widget_uid()) {
                    self.st.stale.push(splash);
                }
                continue;
            }
            let _ = splash.widget(cx, ids!(main)); // the tree warm-up, as in feed
            if let Some(mut inner) = splash.borrow_mut::<Splash>() {
                self.st.split.timed(3, || inner.call_script_fn(cx, live_id!(__render), &[]));
            }
        }
    }

    /// The one write path: merge, re-render the readers, tell the watchers.
    fn apply(&mut self, cx: &mut Cx, patch: &JsonValue, by: &str) {
        let t = std::time::Instant::now();
        let r = self.apply_inner(cx, patch, by);
        self.st.split.add_named("apply", t.elapsed());
        r
    }

    fn apply_inner(&mut self, cx: &mut Cx, patch: &JsonValue, by: &str) {
        let JsonValue::Object(top) = patch else { return };
        let touched: Vec<String> = top.keys().cloned().collect();
        let store = &mut self.st.store;
        self.st.split.timed(1, || {
            for (k, v) in top {
                if *v == JsonValue::Null {
                    store.remove(k);
                } else {
                    merge(store.entry(k.clone()).or_insert(JsonValue::Null), v);
                }
            }
        });
        let t = std::time::Instant::now();
        let shell = self.shell(cx);
        self.st.split.add_named("shell-lookup", t.elapsed());
        if touched.iter().any(|k| k == "front") {
            let front = self.st.store.get("front").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let shown = shell.borrow::<Shell>().and_then(|sh| sh.surface(&front));
            let before = shell.borrow::<Shell>().map(|sh| sh.front.clone()).unwrap_or_default();
            if let Some(mut sh) = shell.borrow_mut::<Shell>() {
                sh.front = front.clone();
            }
            // A service does its work only while its view is looked at: `shown` and `hidden`
            // go to the watchers of those surfaces.
            if before != front {
                self.st.front_history.retain(|n| *n != front && *n != before);
                if !before.is_empty() {
                    self.st.front_history.push(before.clone());
                }
                if !before.is_empty() {
                    self.to_surface_watchers(&before, obj(vec![("ev", "hidden".into()), ("surface", before.as_str().into())]));
                }
                if !front.is_empty() {
                    self.to_surface_watchers(&front, obj(vec![("ev", "shown".into()), ("surface", front.as_str().into())]));
                }
            }
            // A surface that was fed while hidden renders now, with the newest state.
            if let Some(shown) = shown {
                if let Some(at) = self.st.stale.iter().position(|w| w.widget_uid() == shown.widget_uid()) {
                    self.st.stale.remove(at);
                    self.st.render_pending.push(shown);
                    self.st.render_frame = cx.new_next_frame();
                }
            }
            cx.redraw_all();
        }
        // Asleep, nothing renders: a surface gets the whole store when the panel wakes.
        if !self.st.panel_off {
            let readers: Vec<(String, WidgetRef)> = shell.borrow::<Shell>().map_or(vec![], |sh| sh.surfaces.clone());
            for (name, splash) in readers {
                let keys: Vec<String> = match self.st.reads.get(&name) {
                    Some(Reads::All) => touched.clone(),
                    Some(Reads::Keys(ks)) => touched.iter().filter(|k| ks.contains(k)).cloned().collect(),
                    None => continue,
                };
                if !keys.is_empty() {
                    self.feed(cx, &splash, &keys);
                }
            }
        }
        let ev = obj(vec![("ev", "set".into()), ("patch", patch.clone()), ("by", by.into())]).serialize_json();
        for c in self.st.conns.values() {
            if c.watch.as_ref().map_or(false, |w| w.wants_keys(&touched)) {
                let _ = c.tx.send(ev.clone());
            }
        }
    }

    fn to_surface_watchers(&self, surface: &str, ev: JsonValue) {
        let line = ev.serialize_json();
        for c in self.st.conns.values() {
            if c.watch.as_ref().map_or(false, |w| w.wants_surface(surface)) {
                let _ = c.tx.send(line.clone());
            }
        }
    }

    /// Script errors land in the process log, wherever they happen (a render,
    /// a tap, a timer). Those of a surface go to its watchers as `error` events.
    fn errors_to_watchers(&mut self) {
        let (cursor, lines) = log_ring::read_since(self.st.log_cursor, usize::MAX);
        if lines.is_empty() {
            return;
        }
        self.st.log_cursor = cursor;
        for l in lines {
            if !matches!(l.level, LogLevel::Error | LogLevel::Panic) {
                continue;
            }
            let Some(at) = l.text.find("splash:") else { continue };
            let name: String = l.text[at + 7..].chars().take_while(|c| *c != ':').collect();
            if !self.st.bodies.contains_key(&name) {
                continue;
            }
            let Some((line, col, message)) = locate(&l.text, &name) else { continue };
            let ev = obj(vec![("ev", "error".into()), ("surface", name.as_str().into()), ("line", (line as u64).into()), ("col", (col as u64).into()), ("message", message.into())]);
            self.to_surface_watchers(&name, ev);
        }
    }

    /// `set`, `emit` (and `front`, which is a `set`) are the only doors out
    /// of a body. Anything else a body asks for is refused by silence.
    fn drain_host_requests(&mut self, cx: &mut Cx) {
        loop {
            let reqs = take_splash_host_requests();
            if reqs.is_empty() {
                return;
            }
            for req in reqs {
                let surface = req.app_tag.clone();
                if surface.is_empty() {
                    continue;
                }
                let args = JsonValue::deserialize_json(&req.args_json).unwrap_or(JsonValue::Null);
                match req.service.as_str() {
                    "set" => {
                        let Some(key) = args.get("key").and_then(|k| k.as_str()) else { continue };
                        let patch = obj(vec![(key, args.get("value").cloned().unwrap_or(JsonValue::Null))]);
                        if let Some(key) = self.refused_key(&patch) {
                            log!("tb-shell: surface {surface} may not write {key} now");
                            continue;
                        }
                        self.st.trace.body_write();
                        self.apply(cx, &patch, &format!("surface:{surface}"));
                    }
                    "emit" => {
                        let name = args.get("name").cloned().unwrap_or(JsonValue::Null);
                        let data = args.get("data").cloned().unwrap_or(JsonValue::Null);
                        let ev = obj(vec![("ev", "emit".into()), ("surface", surface.as_str().into()), ("name", name), ("data", data)]);
                        self.to_surface_watchers(&surface, ev);
                    }
                    other => log!("tb-shell: surface {surface} asked for {other}: refused"),
                }
            }
        }
    }
}
