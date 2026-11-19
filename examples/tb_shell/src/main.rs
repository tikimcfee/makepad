//! tb-shell: one JSON store, one Splash isolate per surface, a 3-zone protected
//! strip and root GUARD drawn by the shell, general 1..N `Stage` multitasking,
//! and a Unix socket speaking newline-delimited JSON.

pub use makepad_widgets;

mod chat_feed;
mod io;
mod jit_slot;
mod library;
mod paneltrace;
mod sched;
mod shell;
mod split;
mod stage;
mod stream;
mod termd_bridge;
mod termd_view;
mod termd_wire;

use io::{Msg, Peer};
use makepad_micro_serde::{DeJson, JsonValue, SerJson};
use makepad_widgets::makepad_platform::{
    log_ring,
    perf_monitor::{PerfMonitorFrame, PERF_CHANNEL_DRAW, PERF_CHANNEL_GPU},
};
use makepad_widgets::splash_host::take_splash_host_requests;
use makepad_widgets::*;
use shell::{GuardView, Shell, ShellAction};
use stage::{SlotMode, Stage, StageMode, StageSlot};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

app_main!(
    App,
    font_assets: [
        "makepad_widgets/resources/jetbrains_mono_variable.ttf",
        "makepad_widgets/resources/fa-solid-900.ttf",
        "makepad_widgets/resources/NotoColorEmoji.ttf",
        INTER_FONT_ASSET,
        MATH_VIEW_FONT_ASSET,
    ]
);

const PRELUDE: &str = include_str!("prelude.splash");
const BASE: &str = include_str!("../lib/base.splash");
/// Keys only the shell writes.
const SHELL_KEY: &str = "shell";
const SURFACES_KEY: &str = "surfaces";
const WORKSPACES_KEY: &str = "workspaces";
/// Maximum hidden warm isolates kept in memory before LRU parking.
const MAX_WARM_HIDDEN: usize = 4;

#[repr(C)]
#[derive(Default)]
struct LocalTm {
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
    fn localtime_r(timep: *const i64, result: *mut LocalTm) -> *mut LocalTm;
}

fn now_hhmm_and_rem_secs() -> (String, f64) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let rem = (60 - (secs.rem_euclid(60))) as f64 + 0.05;
    let mut tm = LocalTm::default();
    let ok = unsafe { !localtime_r(&secs, &mut tm).is_null() };
    let hhmm = if ok {
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    } else {
        format!("{:02}:{:02}", (secs / 3600).rem_euclid(24), (secs / 60).rem_euclid(60))
    };
    (hhmm, rem)
}

/// Lines ahead of a body: Splash's own prefix, the prelude, and `base_and_lib` lines.
fn lines_ahead(base_and_lib_lines: usize) -> usize {
    3 + PRELUDE.lines().count() + base_and_lib_lines
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
    tx: std::sync::mpsc::Sender<String>,
    peer: Peer,
    watch: Option<WatchSpec>,
}

struct Guard {
    conn: u64,
    view: GuardView,
}

#[derive(Default)]
struct Sources {
    panel: Option<(String, String)>,
    panel_read: Option<Instant>,
    mic_status: Option<String>,
    speaker_state: Option<String>,
    wifi_state: String,
    power_state: String,
}

#[derive(Default)]
struct SurfaceWatch {
    cpu_frames: u32,
    gpu_boundaries: u32,
    last_painted: u64,
    pending_render_ms: f64,
    over_hits: u32,
    last_over_at: Option<Instant>,
    verify_redraws_left: u8,
}

#[derive(Default)]
struct State {
    store: HashMap<String, JsonValue>,
    rx: Option<Receiver<Msg>>,
    conns: HashMap<u64, Conn>,
    reads: HashMap<String, Reads>,
    bodies: HashMap<String, String>,
    stage: Stage,
    prev_stage_before_switcher: Option<Stage>,
    visible: BTreeSet<String>,
    parked: HashSet<String>,
    warm_lru: VecDeque<String>,
    slot_prelude_cache: HashMap<String, String>,
    slot_dims: HashMap<String, (f64, f64)>,
    front_history: Vec<String>,
    owned: HashMap<u64, Vec<String>>,
    guards: Vec<Guard>,
    trusted: HashSet<String>,
    sources: Sources,
    panel_off: bool,
    log_cursor: u64,
    split: split::Split,
    trace: paneltrace::PanelTrace,
    render_pending: Vec<(String, WidgetRef)>,
    stale: HashSet<String>,
    render_frame: NextFrame,
    watch_frame: NextFrame,
    views: HashSet<String>,
    notified: bool,
    placement: sched::Placement,
    note_timer: Timer,
    mic_timer: Timer,
    clock_timer: Timer,
    panel_wake_timer: Timer,
    panel_wake_ticks: u8,
    mic_unreadable: bool,
    persist: library::StorePersistence,
    surfaces_json: String,
    workspaces_json: String,
    frame_watch: HashMap<String, SurfaceWatch>,
    frame_budget_ms: f64,
    agent_shaders_on: bool,
    pending_watch_shown: Vec<(u64, String)>,
    termd: Option<termd_bridge::TermdBridge>,
}

#[derive(Script, ScriptHook)]
pub struct App {
    #[live] ui: WidgetRef,
    #[rust] st: State,
}

impl AppMain for App {
    fn script_mod(vm: &mut ScriptVm) -> ScriptValue {
        crate::makepad_widgets::script_mod(vm);
        chat_feed::script_mod(vm);
        termd_view::script_mod(vm);
        makepad_widgets::widget_async::register_splash_isolate_mod(stream::isolate_mod);
        makepad_widgets::widget_async::register_splash_isolate_mod(chat_feed::isolate_mod);
        makepad_widgets::widget_async::register_splash_isolate_mod(termd_view::isolate_mod);
        crate::shell::script_mod(vm);
        self::script_mod(vm)
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        if let Event::Startup = event {
            self.start(cx);
        }
        if let Event::Shutdown = event {
            if let Some(mut sh) = self.shell(cx).borrow_mut::<Shell>() {
                sh.jit.shutdown_all();
            }
            cx.perf_monitor.set_enabled(false);
            library::CrashGuard::mark_clean_shutdown();
            return;
        }
        self.st.split.event_start();
        self.st.trace.event(event);
        self.check_panel(cx);
        if self.st.render_frame.is_event(event).is_some() {
            self.render_pending(cx);
        }
        if let Event::Signal = event {
            if self.st.split.on() {
                let t = Instant::now();
                self.drain_socket(cx);
                let dt = t.elapsed();
                self.st.split.add_drain(dt);
            } else {
                self.drain_socket(cx);
            }
            self.drain_termd_events(cx);
            if !self.st.panel_off {
                let has_vis_jit = self.shell(cx).borrow::<Shell>().map_or(false, |sh| {
                    self.st.visible.iter().any(|n| sh.jit.contains(n))
                });
                if has_vis_jit {
                    cx.redraw_all();
                }
            }
        }
        self.drain_host_requests(cx);
        if let Event::Draw(_) = event {
            self.ui.handle_event(cx, event, &mut Scope::empty());
            self.after_draw_watch(cx);
            self.notify_ready();
        } else {
            let ui = &self.ui;
            self.st.split.timed(4, || ui.handle_event(cx, event, &mut Scope::empty()));
        }
        self.drain_gpu_watch(cx);
        self.drain_host_requests(cx);
        if self.st.mic_timer.is_event(event).is_some() {
            self.mic_timer(cx);
        }
        if self.st.note_timer.is_event(event).is_some() {
            self.clear_note(cx);
        }
        if self.st.clock_timer.is_event(event).is_some() {
            if !self.st.panel_off {
                self.update_strip_status(cx);
                self.arm_clock_timer(cx);
            } else {
                cx.stop_timer(self.st.clock_timer);
            }
        }
        if self.st.panel_wake_timer.is_event(event).is_some() {
            if self.st.panel_wake_ticks > 0 && !self.st.panel_off {
                self.st.panel_wake_ticks -= 1;
                cx.redraw_all();
            }
            if self.st.panel_wake_ticks == 0 || self.st.panel_off {
                cx.stop_timer(self.st.panel_wake_timer);
                self.st.panel_wake_ticks = 0;
            }
        }
        let shell_actions = self.shell(cx).borrow_mut::<Shell>().map_or(vec![], |mut sh| sh.take_actions());
        if !shell_actions.is_empty() {
            self.handle_shell_actions(cx, shell_actions);
        }
        // Check if any hidden surface started a timer inside an event handler and park it.
        if !matches!(event, Event::Draw(_)) && !self.st.panel_off {
            self.park_timer_bearing_hidden(cx);
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

fn locate_with_ahead(text: &str, name: &str, ahead: usize) -> Option<(usize, usize, String)> {
    let tag = format!("splash:{name}:");
    let at = text.find(&tag)?;
    let rest = &text[at + tag.len()..];
    let mut parts = rest.splitn(3, |c: char| c == ':' || c == ' ');
    let line: usize = parts.next()?.parse().ok()?;
    let col: usize = parts.next()?.trim_end_matches(':').parse().ok().unwrap_or(0);
    let message = rest
        .split_once(" - ")
        .map(|(_, m)| m)
        .or_else(|| rest.split_once(": ").map(|(_, m)| m))
        .unwrap_or(rest)
        .to_string();
    let line = line.checked_sub(ahead).unwrap_or(0);
    Some((line, col, message))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

fn has_isolate_timers(cx: &mut Cx, splash: &WidgetRef) -> bool {
    let Some(hk) = splash.borrow_mut::<Splash>().and_then(|mut s| s.isolate_heap_key(cx)) else {
        return false;
    };
    cx.script_data.timers.timers.iter().any(|t| t.callback.heap_key() == hk)
}

fn notify_screen_inhibit(cmd: &str) {
    let path = env("TB_SCREEN_SOCK").unwrap_or_else(|| {
        format!("{}/tb-screen.sock", env("XDG_RUNTIME_DIR").unwrap_or_else(|| "/run/user/8000".into()))
    });
    if let Ok(mut sock) = UnixStream::connect(path) {
        let _ = sock.set_write_timeout(Some(Duration::from_millis(80)));
        let _ = sock.write_all(format!("{cmd}\n").as_bytes());
    }
}

impl App {
    fn shell(&self, cx: &Cx) -> WidgetRef {
        self.ui.widget(cx, ids!(shell))
    }

    fn full_source(&self, body: &str) -> String {
        let lib = self.st.persist.base_and_lib_source();
        format!("{PRELUDE}{lib}{body}")
    }

    fn current_ahead(&self) -> usize {
        lines_ahead(self.st.persist.base_and_lib_lines())
    }

    fn is_watched_surface(&self, name: &str) -> bool {
        self.st.frame_budget_ms > 0.0
            && !self.st.views.contains(name)
            && self
                .st
                .bodies
                .get(name)
                .map_or(false, |b| !b.starts_with("__jit__:") && !self.st.trusted.contains(b))
    }

    fn sync_gpu_timer_for_visible(&mut self, cx: &mut Cx) {
        let watched_vis: Vec<String> = self
            .st
            .visible
            .iter()
            .filter(|n| self.is_watched_surface(n))
            .cloned()
            .collect();
        let want = !self.st.panel_off && !watched_vis.is_empty();
        cx.perf_monitor.set_enabled(false);
        if want {
            cx.perf_monitor.set_enabled(true);
            let painted = cx.perf_monitor.frames_painted();
            for name in watched_vis {
                let w = self.st.frame_watch.entry(name).or_default();
                w.last_painted = painted;
                w.gpu_boundaries = 0;
            }
        }
    }

    fn reload_trusted(&mut self) {
        self.st.trusted.clear();
        if let Some(dir) = library::resolve_ui_subdir("TB_SHELL_TRUSTED", "trusted") {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                if let Ok(body) = std::fs::read_to_string(entry.path()) {
                    self.st.trusted.insert(body);
                }
            }
            log!("tb-shell: {} trusted bodies from {}", self.st.trusted.len(), dir.display());
        }
    }

    fn arm_clock_timer(&mut self, cx: &mut Cx) {
        cx.stop_timer(self.st.clock_timer);
        if !self.st.panel_off {
            let (_, rem) = now_hhmm_and_rem_secs();
            self.st.clock_timer = cx.start_timeout(rem);
        }
    }

    fn update_strip_status(&mut self, cx: &mut Cx) {
        let (hhmm, _) = now_hhmm_and_rem_secs();
        let wifi_tag = self
            .st
            .store
            .get("wifi")
            .and_then(|w| w.get("state"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| {
                std::fs::read_to_string(&self.st.sources.wifi_state).ok().and_then(|s| {
                    s.lines()
                        .find_map(|l| l.trim().strip_prefix("state=").map(|v| v.trim().to_string()))
                })
            });
        let wifi_short = match wifi_tag.as_deref() {
            Some("online") => Some("wifi"),
            Some("connecting") => Some("wifi.."),
            Some("failed") => Some("wifi!"),
            _ => None,
        };

        let bat_pct = self
            .st
            .store
            .get("battery")
            .and_then(|b| b.get("pct"))
            .and_then(|v| match v {
                JsonValue::I64(n) => Some(*n),
                JsonValue::U64(n) => Some(*n as i64),
                JsonValue::F64(f) => Some(f.round() as i64),
                _ => None,
            })
            .or_else(|| {
                std::fs::read_to_string(&self.st.sources.power_state).ok().and_then(|s| {
                    s.lines()
                        .find_map(|l| l.trim().strip_prefix("soc=").and_then(|v| v.trim().parse::<i64>().ok()))
                })
            });

        let mut parts = Vec::with_capacity(3);
        if let Some(w) = wifi_short {
            parts.push(w.to_string());
        }
        if let Some(p) = bat_pct {
            parts.push(format!("{p}%"));
        }
        parts.push(hhmm);
        let next = parts.join(" · ");
        if let Some(mut sh) = self.shell(cx).borrow_mut::<Shell>() {
            if sh.status_right != next {
                sh.status_right = next;
                cx.redraw_all();
            }
        }
    }

    fn start(&mut self, cx: &mut Cx) {
        let (tx, rx) = io::new_channel();
        let path = env("TB_SHELL_SOCK").unwrap_or_else(|| format!("{}/tb-shell.sock", env("XDG_RUNTIME_DIR").unwrap_or("/tmp".into())));
        io::listen(&path, tx.clone());
        self.st.rx = Some(rx);
        self.st.frame_budget_ms = env("TB_SHELL_FRAME_BUDGET_MS").and_then(|v| v.parse().ok()).unwrap_or(120.0);
        self.st.agent_shaders_on = !matches!(env("TB_SHELL_AGENT_SHADERS").as_deref(), Some("0") | Some("false"));
        self.st.persist = library::StorePersistence::load(BASE);
        if !self.st.persist.pieces.is_empty() {
            log!("tb-shell: {} library pieces from {}", self.st.persist.pieces.len(), self.st.persist.lib_dir.display());
        }

        self.reload_trusted();

        let s = &mut self.st.sources;
        if let Some(file) = env("TB_SHELL_PANEL_FILE") {
            io::watch_panel_sysfs(&file, tx.clone());
            s.panel = Some((file, env("TB_SHELL_PANEL_OFF").unwrap_or("off".into())));
        }
        if let (Some(dir), Some(status)) = (env("TB_SHELL_MIC_DIR"), env("TB_SHELL_MIC_STATUS")) {
            io::watch_capture(&dir, tx.clone());
            s.mic_status = Some(status);
        }
        let spk = env("TB_SHELL_SPEAKER_STATE").unwrap_or_else(|| "/run/tinybot/speaker-state".into());
        io::watch_speaker_state(&spk, tx);
        s.speaker_state = Some(spk);
        s.wifi_state = env("TB_WIFI_STATE").unwrap_or_else(|| "/run/tinybot/wifi-state".into());
        s.power_state = env("TB_POWER_STATE").unwrap_or_else(|| "/run/tinybot/power-state".into());

        self.st.log_cursor = log_ring::read_since(u64::MAX, 0).0;
        self.st.split = split::Split::from_env();
        self.st.trace = paneltrace::PanelTrace::from_env();
        self.st.placement = sched::Placement::from_env();
        self.st.placement.apply();
        self.mic_timer(cx);
        self.read_loud(cx);
        self.update_strip_status(cx);
        self.arm_clock_timer(cx);
        self.st.termd = Some(termd_bridge::TermdBridge::new(self.st.panel_off));
        self.sync_termd_store(cx, false);

        let (prev_unclean, crashes_60s) = library::CrashGuard::check_on_start();
        let before = Self::surfaces_file().and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or_default();
        self.load_views(cx);

        if crashes_60s >= 2 {
            let mut skipped: Vec<String> = self.st.persist.saved_screen_names();
            for n in before.split_whitespace() {
                if !self.st.views.contains(n) && !skipped.iter().any(|s| s == n) {
                    skipped.push(n.to_string());
                }
            }
            error!("tb-shell: {crashes_60s} crashes within 60 s: skipping saved agent screens");
            self.left_out(cx, &skipped.join(" "));
        } else {
            self.load_saved_screens(cx);
            if prev_unclean {
                self.left_out(cx, &before);
            } else {
                self.write_surfaces_file();
            }
        }
        self.sync_surfaces_key(cx);
        self.sync_workspaces_key(cx);
    }

    fn load_views(&mut self, cx: &mut Cx) {
        let Some(dir) = library::resolve_ui_subdir("TB_SHELL_VIEWS", "views") else { return };
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |x| x == "splash"))
            .collect();
        files.sort();
        for path in files {
            let name = path.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let Ok(body) = std::fs::read_to_string(&path) else { continue };
            self.st.views.insert(name.clone());
            match self.show(cx, &name, &body, false) {
                Ok(reply) if reply.get("ok") != Some(&JsonValue::Bool(false)) => {
                    log!("tb-shell: view {name} shown in {} ms", reply.get("ms").map_or(String::new(), |m| m.serialize_json()));
                }
                Ok(reply) => {
                    self.st.views.remove(&name);
                    error!("tb-shell: view {name}: {}", reply.serialize_json());
                }
                Err(e) => {
                    self.st.views.remove(&name);
                    error!("tb-shell: view {name}: {e}");
                }
            }
        }
        log!("tb-shell: {} views from {}", self.st.views.len(), dir.display());
        if self.st.views.contains("home") {
            self.apply(cx, &obj(vec![("front", "home".into())]), "shell");
        }
    }

    fn load_saved_screens(&mut self, cx: &mut Cx) {
        let saved = self.st.persist.load_saved_screens();
        if saved.is_empty() {
            return;
        }
        let mut count = 0usize;
        for (name, body) in saved {
            if self.st.views.contains(&name) {
                continue;
            }
            match self.show(cx, &name, &body, false) {
                Ok(reply) if reply.get("ok") != Some(&JsonValue::Bool(false)) => {
                    count += 1;
                }
                Ok(reply) => error!("tb-shell: saved screen {name}: {}", reply.serialize_json()),
                Err(e) => error!("tb-shell: saved screen {name}: {e}"),
            }
        }
        if count > 0 {
            log!("tb-shell: restored {count} saved screen(s) from {}", self.st.persist.screens_dir.display());
        }
    }

    fn sync_surfaces_key(&mut self, cx: &mut Cx) {
        let mut names: Vec<String> = self
            .st
            .bodies
            .keys()
            .filter(|n| !matches!(n.as_str(), "home" | "stage" | "power"))
            .cloned()
            .collect();
        names.sort_by(|a, b| {
            let a_sys = self.st.views.contains(a);
            let b_sys = self.st.views.contains(b);
            b_sys.cmp(&a_sys).then_with(|| a.cmp(b))
        });
        let items: Vec<JsonValue> = names
            .into_iter()
            .map(|name| {
                let is_sys = self.st.views.contains(&name);
                let is_jit = self.st.bodies.get(&name).is_some_and(|b| b.starts_with("__jit__:"));
                let saved = is_sys || self.st.persist.is_saved(&name);
                let title = self.st.persist.title_of(&name).unwrap_or(&name).to_string();
                let kind = if is_sys { "system" } else if is_jit { "jit" } else { "agent" };
                obj(vec![
                    ("name", JsonValue::String(name)),
                    ("kind", JsonValue::String(kind.into())),
                    ("saved", JsonValue::Bool(saved)),
                    ("title", JsonValue::String(title)),
                ])
            })
            .collect();
        let arr = JsonValue::Array(items);
        let ser = arr.serialize_json();
        if ser == self.st.surfaces_json {
            return;
        }
        self.st.surfaces_json = ser;
        self.apply(cx, &obj(vec![(SURFACES_KEY, arr)]), "shell");
    }

    fn sync_workspaces_key(&mut self, cx: &mut Cx) {
        let arr = JsonValue::Array(self.st.persist.ls_workspaces());
        let ser = arr.serialize_json();
        if ser == self.st.workspaces_json {
            return;
        }
        self.st.workspaces_json = ser;
        self.apply(cx, &obj(vec![(WORKSPACES_KEY, arr)]), "shell");
    }

    fn surfaces_file() -> Option<String> {
        env("TB_SHELL_RUNTIME_DIR")
            .or_else(|| env("XDG_RUNTIME_DIR"))
            .map(|d| format!("{d}/tb-shell.agent-surfaces"))
    }

    fn left_out(&mut self, cx: &mut Cx, before: &str) {
        let names: Vec<&str> = before
            .split_whitespace()
            .filter(|n| !self.st.views.contains(*n) && !self.st.bodies.contains_key(*n))
            .collect();
        if !names.is_empty() {
            let note = format!("left out: {}", names.join(", "));
            log!("tb-shell: after a restart, {note}");
            self.show_strip_note_secs(cx, note, 10.0);
        }
        self.write_surfaces_file();
    }

    fn show_strip_note(&mut self, cx: &mut Cx, note: String) {
        self.show_strip_note_secs(cx, note, 10.0);
    }

    fn show_strip_note_secs(&mut self, cx: &mut Cx, note: String, secs: f64) {
        if let Some(mut sh) = self.shell(cx).borrow_mut::<Shell>() {
            sh.note = note;
            cx.redraw_all();
        }
        cx.stop_timer(self.st.note_timer);
        if secs > 0.0 {
            self.st.note_timer = cx.start_timeout(secs.clamp(0.5, 300.0));
        }
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

    fn notify_ready(&mut self) {
        if std::mem::replace(&mut self.st.notified, true) {
            return;
        }
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
        let mut names: Vec<&String> = self
            .st
            .bodies
            .iter()
            .filter(|(n, b)| !self.st.views.contains(*n) && !b.starts_with("__jit__:"))
            .map(|(n, _)| n)
            .collect();
        names.sort();
        let _ = std::fs::write(&file, names.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(" "));
    }

    fn read_mic(&mut self, cx: &mut Cx) {
        let Some(status) = self.st.sources.mic_status.clone() else { return };
        let text = read_trim(&status);
        if text.is_none() && !self.st.mic_unreadable {
            error!("tb-shell: cannot read {status}: the recording mark shows off");
        }
        self.st.mic_unreadable = text.is_none();
        let mic = text.as_deref().map_or(false, |t| t != "closed");
        if let Some(mut sh) = self.shell(cx).borrow_mut::<Shell>() {
            if sh.marks.mic != mic {
                sh.marks.mic = mic;
                log!("tb-shell: recording mark {} (status: {:?}) at {}", if mic { "ON" } else { "off" }, text, paneltrace::stamp());
                cx.redraw_all();
            }
        }
    }

    fn mic_event(&mut self, cx: &mut Cx) {
        self.read_mic(cx);
        cx.stop_timer(self.st.mic_timer);
        self.st.mic_timer = cx.start_timeout(0.3);
    }

    fn mic_timer(&mut self, cx: &mut Cx) {
        self.read_mic(cx);
        let on = self.shell(cx).borrow::<Shell>().map_or(false, |sh| sh.marks.mic);
        if on {
            self.st.mic_timer = cx.start_timeout(2.0);
        }
    }

    fn read_loud(&mut self, cx: &mut Cx) {
        let Some(path) = self.st.sources.speaker_state.clone() else { return };
        let text = std::fs::read_to_string(&path).ok();
        let state_val = text.as_deref().and_then(|s| {
            s.lines()
                .map(str::trim)
                .find_map(|l| l.strip_prefix("state=").map(str::trim))
        });
        let loud = matches!(state_val, Some("playing") | Some("holding") | Some("parking"));
        if let Some(mut sh) = self.shell(cx).borrow_mut::<Shell>() {
            if sh.marks.loud != loud {
                sh.marks.loud = loud;
                log!("tb-shell: loud mark {} (state: {:?}) at {}", if loud { "ON" } else { "off" }, state_val, paneltrace::stamp());
                cx.redraw_all();
            }
        }
    }

    /// Records CPU frame time across all drawn slots and syncs measured slot sizes into `slot` prelude.
    fn after_draw_watch(&mut self, cx: &mut Cx) {
        let (drawn_list, sizes) = self.shell(cx).borrow_mut::<Shell>().map_or((vec![], vec![]), |mut sh| {
            (std::mem::take(&mut sh.last_draw_ms), std::mem::take(&mut sh.last_slot_sizes))
        });
        let mut slot_dirty = false;
        for (name, w, h) in sizes {
            let prev = self.st.slot_dims.insert(name.clone(), (w, h));
            if prev.map_or(true, |(pw, ph)| (pw - w).abs() > 2.0 || (ph - h).abs() > 2.0) {
                slot_dirty = true;
            }
        }
        if slot_dirty && !self.st.panel_off {
            self.sync_visible_slot_preludes(cx);
        }

        let mut request_verify = false;
        for (name, draw_ms) in drawn_list {
            if !self.is_watched_surface(&name) {
                continue;
            }
            let (frame_idx, cpu_ms, verify_redraw) = {
                let w = self.st.frame_watch.entry(name.clone()).or_default();
                w.cpu_frames = w.cpu_frames.saturating_add(1);
                let total = std::mem::take(&mut w.pending_render_ms) + draw_ms;
                let vr = if !self.st.panel_off && w.verify_redraws_left > 0 {
                    w.verify_redraws_left -= 1;
                    true
                } else {
                    false
                };
                (w.cpu_frames, total, vr)
            };
            request_verify |= verify_redraw;
            if frame_idx >= 2 {
                self.check_frame_sample(cx, &name, cpu_ms, "cpu");
            }
        }
        if request_verify {
            cx.redraw_all();
            self.st.watch_frame = cx.new_next_frame();
        }
    }

    fn drain_gpu_watch(&mut self, cx: &mut Cx) {
        if !cx.perf_monitor.enabled() {
            return;
        }
        let watched: Vec<String> = self
            .st
            .visible
            .iter()
            .filter(|n| self.is_watched_surface(n))
            .cloned()
            .collect();
        let culprit = if watched.len() == 1 {
            watched[0].clone()
        } else if watched.len() > 1 {
            let shader_bearers: Vec<&String> = watched
                .iter()
                .filter(|n| {
                    self.st
                        .bodies
                        .get(*n)
                        .map_or(false, |b| b.contains("pixel:") || b.contains("vertex:"))
                })
                .collect();
            if shader_bearers.len() == 1 {
                shader_bearers[0].clone()
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        if culprit.is_empty() {
            return;
        }
        let painted = cx.perf_monitor.frames_painted();
        let prev = self.st.frame_watch.get(&culprit).map_or(painted, |w| w.last_painted);
        if painted <= prev {
            return;
        }
        let delta = (painted - prev) as usize;
        if let Some(w) = self.st.frame_watch.get_mut(&culprit) {
            w.last_painted = painted;
        }
        let mut ring: Vec<PerfMonitorFrame> = Vec::new();
        cx.perf_monitor.read(&mut ring);
        if ring.is_empty() {
            return;
        }
        let take_n = delta.min(ring.len());
        let slice = &ring[ring.len() - take_n..];
        for f in slice {
            let b_idx = {
                let w = self.st.frame_watch.entry(culprit.clone()).or_default();
                w.gpu_boundaries = w.gpu_boundaries.saturating_add(1);
                w.gpu_boundaries
            };
            if b_idx <= 2 {
                continue;
            }
            let gpu_ms = f.channel_us[PERF_CHANNEL_GPU.0] as f64 / 1000.0;
            let encode_ms = f.channel_us[PERF_CHANNEL_DRAW.0] as f64 / 1000.0;
            let sample_ms = gpu_ms.max(encode_ms);
            let label = if gpu_ms >= encode_ms { "gpu" } else { "encode" };
            if sample_ms > 0.0 {
                self.check_frame_sample(cx, &culprit, sample_ms, label);
                if !self.st.bodies.contains_key(&culprit) {
                    break;
                }
            }
        }
    }

    fn check_frame_sample(&mut self, cx: &mut Cx, name: &str, ms: f64, kind: &str) {
        let budget = self.st.frame_budget_ms;
        if budget <= 0.0 || !self.st.bodies.contains_key(name) {
            return;
        }
        let trip = {
            let w = self.st.frame_watch.entry(name.to_string()).or_default();
            if ms > budget {
                let recent = w.last_over_at.map_or(false, |t| t.elapsed().as_secs_f64() <= 3.0);
                w.over_hits = if recent { w.over_hits + 1 } else { 1 };
                w.last_over_at = Some(Instant::now());
                log!("tb-shell: surface {name} slow {kind} frame: {ms:.1} ms (budget {budget:.0} ms, hit {})", w.over_hits);
                w.over_hits >= 2
            } else {
                if w.last_over_at.map_or(false, |t| t.elapsed().as_secs_f64() > 3.0) {
                    w.over_hits = 0;
                }
                false
            }
        };
        if trip {
            let msg = format!("frame budget exceeded ({kind} {ms:.1} ms > {budget:.0} ms)");
            error!("tb-shell: surface {name}: {msg}; evicting slot");
            let ms_round = (ms * 10.0).round() / 10.0;
            let ev = obj(vec![
                ("ev", "error".into()),
                ("surface", name.into()),
                ("error", msg.clone().into()),
                ("message", msg.into()),
                ("ms", JsonValue::F64(ms_round)),
                ("budget_ms", JsonValue::F64(budget)),
            ]);
            self.to_surface_watchers(name, ev);
            let _ = self.close_surface(cx, name, true);
            self.show_strip_note(cx, format!("stopped slow: {name}"));
        }
    }

    fn check_panel(&mut self, cx: &mut Cx) {
        self.check_panel_inner(cx, false);
    }

    fn check_panel_inner(&mut self, cx: &mut Cx, force: bool) {
        let s = &mut self.st.sources;
        let Some((file, off_value)) = &s.panel else { return };
        if !force && s.panel_read.map_or(false, |t| t.elapsed().as_millis() < 250) {
            return;
        }
        s.panel_read = Some(Instant::now());
        let value = read_trim(file);
        let off = value.as_deref() == Some(off_value.as_str());
        self.st.trace.read(value.as_deref());
        if off != self.st.panel_off {
            self.panel(cx, !off);
        }
    }

    fn panel(&mut self, cx: &mut Cx, on: bool) {
        self.st.panel_off = !on;
        self.st.trace.changed();
        log!("tb-shell: panel {} at {}", if on { "on: surfaces back" } else { "off: surfaces stopped" }, paneltrace::stamp());
        cx.stop_timer(self.st.panel_wake_timer);
        self.st.panel_wake_ticks = 0;

        if on {
            if let Some(ref mut tb) = self.st.termd {
                tb.set_panel_off(false);
            }
            self.update_strip_status(cx);
            self.arm_clock_timer(cx);
            self.sync_visibility_and_lifecycle(cx);
            self.render_pending(cx);
            cx.redraw_all();
            self.st.panel_wake_ticks = 2;
            self.st.panel_wake_timer = cx.start_interval(0.18);
        } else {
            if let Some(ref mut tb) = self.st.termd {
                tb.set_panel_off(true);
            }
            cx.stop_timer(self.st.clock_timer);
            let shell = self.shell(cx);
            let surfaces: Vec<(String, WidgetRef)> = shell.borrow::<Shell>().map_or(vec![], |sh| sh.surfaces.clone());
            for (name, splash) in surfaces {
                splash.set_text(cx, "");
                self.st.parked.insert(name);
            }
            self.st.slot_prelude_cache.clear();
            self.st.render_pending.clear();
            self.sync_visibility_and_lifecycle(cx);
        }

        let ev = obj(vec![("ev", "panel".into()), ("on", JsonValue::Bool(on))]).serialize_json();
        for c in self.st.conns.values().filter(|c| c.watch.is_some()) {
            let _ = c.tx.send(ev.clone());
        }
    }

    /// Ensures a surface's isolate is evaluated and fed with current store & slot prelude.
    fn ensure_surface_live(&mut self, cx: &mut Cx, name: &str) {
        if self.st.panel_off || !self.st.parked.remove(name) {
            return;
        }
        let Some(body) = self.st.bodies.get(name).cloned() else { return };
        let Some(splash) = self.shell(cx).borrow::<Shell>().and_then(|sh| sh.surface(name)) else { return };
        let code = self.full_source(&body);
        if let Some(w) = self.st.frame_watch.get_mut(name) {
            w.cpu_frames = 0;
            w.gpu_boundaries = 0;
        }
        self.st.slot_prelude_cache.remove(name);
        splash.set_text(cx, &code);
        self.feed_slot_for(cx, name, &splash);
        self.feed_all(cx, name, &splash);
    }

    fn feed_slot_for(&mut self, cx: &mut Cx, name: &str, splash: &WidgetRef) -> bool {
        let total = self.st.stage.slots.len();
        let stage_mode = self.st.stage.mode;
        let (mode_str, pinned) = self
            .st
            .stage
            .slots
            .iter()
            .find(|s| s.surface == name)
            .map(|s| (s.prelude_mode(stage_mode, total), s.pinned))
            .unwrap_or(("full", false));
        let (w, h) = self.st.slot_dims.get(name).copied().unwrap_or((384.0, 822.0));
        let w_r = (w * 2.0).round() * 0.5;
        let h_r = (h * 2.0).round() * 0.5;
        let json = obj(vec![
            ("mode", mode_str.into()),
            ("pinned", JsonValue::Bool(pinned)),
            ("width", JsonValue::F64(w_r)),
            ("height", JsonValue::F64(h_r)),
        ])
        .serialize_json();
        if self.st.slot_prelude_cache.get(name) == Some(&json) {
            return false;
        }
        self.st.slot_prelude_cache.insert(name.to_string(), json.clone());
        let _ = splash.widget(cx, ids!(main));
        if let Some(mut inner) = splash.borrow_mut::<Splash>() {
            inner.call_script_fn_with_strings(cx, live_id!(__slot), &[&json]);
        }
        self.queue_render(cx, name, splash);
        true
    }

    fn sync_visible_slot_preludes(&mut self, cx: &mut Cx) {
        let vis: Vec<String> = self.st.visible.iter().cloned().collect();
        let shell = self.shell(cx);
        for name in vis {
            if self.st.parked.contains(&name) {
                continue;
            }
            if let Some(splash) = shell.borrow::<Shell>().and_then(|sh| sh.surface(&name)) {
                self.feed_slot_for(cx, &name, &splash);
            }
        }
    }

    fn park_surface(&mut self, cx: &mut Cx, name: &str) {
        if self.st.parked.contains(name) {
            return;
        }
        if let Some(splash) = self.shell(cx).borrow::<Shell>().and_then(|sh| sh.surface(name)) {
            splash.set_text(cx, "");
        }
        self.st.parked.insert(name.to_string());
        self.st.slot_prelude_cache.remove(name);
        self.st.stale.remove(name);
    }

    fn park_timer_bearing_hidden(&mut self, cx: &mut Cx) {
        let shell = self.shell(cx);
        let surfaces: Vec<(String, WidgetRef)> = shell.borrow::<Shell>().map_or(vec![], |sh| sh.surfaces.clone());
        let mut to_park = Vec::new();
        for (name, splash) in surfaces {
            if self.st.visible.contains(&name) || self.st.parked.contains(&name) {
                continue;
            }
            if has_isolate_timers(cx, &splash) {
                log!("tb-shell: parking hidden timer-bearing surface {name}");
                to_park.push(name);
            }
        }
        for name in to_park {
            self.park_surface(cx, &name);
            self.st.warm_lru.retain(|n| n != &name);
        }
    }

    fn enforce_hidden_lifecycle(&mut self, cx: &mut Cx) {
        self.park_timer_bearing_hidden(cx);
        while self.st.warm_lru.len() > MAX_WARM_HIDDEN {
            if let Some(oldest) = self.st.warm_lru.pop_front() {
                if !self.st.visible.contains(&oldest) {
                    self.park_surface(cx, &oldest);
                }
            }
        }
    }

    /// Diffs `visible` set, emits `shown`/`hidden`, unparks newly visible isolates,
    /// updates `slot` preludes, and parks hidden timer-bearing / excess LRU isolates.
    fn sync_visibility_and_lifecycle(&mut self, cx: &mut Cx) {
        let next_vis = self.st.stage.visible_surfaces(self.st.panel_off);
        let added: Vec<String> = next_vis.difference(&self.st.visible).cloned().collect();
        let removed: Vec<String> = self.st.visible.difference(&next_vis).cloned().collect();

        for s in &removed {
            if !self.st.panel_off && !self.st.parked.contains(s) {
                self.st.warm_lru.retain(|x| x != s);
                self.st.warm_lru.push_back(s.clone());
            }
            self.to_surface_watchers(s, obj(vec![("ev", "hidden".into()), ("surface", s.as_str().into())]));
        }

        self.st.visible = next_vis;

        for s in &added {
            self.st.warm_lru.retain(|x| x != s);
            self.ensure_surface_live(cx, s);
            if self.st.stale.remove(s) {
                if let Some(splash) = self.shell(cx).borrow::<Shell>().and_then(|sh| sh.surface(s)) {
                    self.queue_render(cx, s, &splash);
                }
            }
            self.to_surface_watchers(s, obj(vec![("ev", "shown".into()), ("surface", s.as_str().into())]));
        }

        if !self.st.panel_off {
            self.sync_visible_slot_preludes(cx);
            self.enforce_hidden_lifecycle(cx);
        }
        if let Some(sh) = self.shell(cx).borrow::<Shell>() {
            sh.jit.sync_visibility(&self.st.visible, self.st.panel_off);
        }
        termd_view::sync_global_visibility(&self.st.visible, &self.st.bodies, self.st.panel_off);
        if !self.st.panel_off && added.iter().any(|s| matches!(s.as_str(), "terminal" | "agents" | "home")) {
            if let Some(ref tb) = self.st.termd {
                tb.probe_disconnected();
            }
        }
        self.sync_gpu_timer_for_visible(cx);
    }

    fn commit_stage(&mut self, cx: &mut Cx, by: &str) {
        let bodies_ref = &self.st.bodies;
        self.st.stage.slots.retain(|s| bodies_ref.contains_key(&s.surface));
        self.st.stage.ensure_valid_focus(self.st.bodies.contains_key("home"));

        let focus = self.st.stage.focus.clone();
        let prev_focus = self.st.store.get("front").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if prev_focus != focus && !prev_focus.is_empty() && prev_focus != "stage" && prev_focus != "power" {
            self.st.front_history.retain(|n| *n != focus && *n != prev_focus);
            self.st.front_history.push(prev_focus);
        }

        if let Some(mut sh) = self.shell(cx).borrow_mut::<Shell>() {
            if sh.stage.mode != self.st.stage.mode || sh.stage.slots.len() != self.st.stage.slots.len() {
                sh.stage_scroll_y = 0.0;
            }
            sh.stage = self.st.stage.clone();
        }

        if focus != "stage" {
            self.st.prev_stage_before_switcher = None;
        }
        let prev_stage_json = if focus == "stage" {
            self.st.prev_stage_before_switcher.as_ref().map_or(JsonValue::Null, Stage::to_json)
        } else {
            JsonValue::Null
        };

        let stage_json = self.st.stage.to_json();
        self.st.store.insert("stage".into(), stage_json.clone());
        self.st.store.insert("prev_stage".into(), prev_stage_json.clone());
        self.st.store.insert("front".into(), JsonValue::String(focus.clone()));

        self.sync_visibility_and_lifecycle(cx);

        // Notify store readers of `stage` / `front`
        let touched = vec!["stage".to_string(), "prev_stage".to_string(), "front".to_string()];
        if !self.st.panel_off {
            let readers: Vec<(String, WidgetRef)> = self.shell(cx).borrow::<Shell>().map_or(vec![], |sh| sh.surfaces.clone());
            for (name, splash) in readers {
                if self.st.parked.contains(&name) {
                    continue;
                }
                let keys: Vec<String> = match self.st.reads.get(&name) {
                    Some(Reads::All) => touched.clone(),
                    Some(Reads::Keys(ks)) => touched.iter().filter(|k| ks.contains(k)).cloned().collect(),
                    None => continue,
                };
                if !keys.is_empty() {
                    self.feed(cx, &name, &splash, &keys);
                }
            }
        }

        let patch = obj(vec![("front", JsonValue::String(focus)), ("stage", stage_json)]);
        let ev = obj(vec![("ev", "set".into()), ("patch", patch), ("by", by.into())]).serialize_json();
        for c in self.st.conns.values() {
            if c.watch.as_ref().map_or(false, |w| w.wants_keys(&touched)) {
                let _ = c.tx.send(ev.clone());
            }
        }
        cx.redraw_all();
    }

    fn handle_shell_actions(&mut self, cx: &mut Cx, actions: Vec<ShellAction>) {
        if !self.st.guards.is_empty() {
            return;
        }
        for act in actions {
            match act {
                ShellAction::GoHome => {
                    self.clear_note(cx);
                    if self.st.bodies.contains_key("home") {
                        if self.st.stage.focus == "home" && self.st.stage.slots.len() > 1 {
                            self.st.stage.mode = StageMode::Stack;
                            self.st.stage.slots = vec![StageSlot::fill("home")];
                            self.st.stage.focus = "home".into();
                        } else {
                            self.st.stage.apply_front("home");
                        }
                        self.commit_stage(cx, "shell");
                    }
                }
                ShellAction::ToggleStageView => {
                    if self.st.bodies.contains_key("stage") {
                        if self.st.stage.focus == "stage" {
                            if let Some(prev) = self.st.prev_stage_before_switcher.take() {
                                self.st.stage = prev;
                            } else if self.st.bodies.contains_key("home") {
                                self.st.stage.apply_front("home");
                            }
                        } else {
                            self.st.prev_stage_before_switcher = Some(self.st.stage.clone());
                            self.st.stage.apply_front("stage");
                        }
                        self.commit_stage(cx, "shell");
                    }
                }
                ShellAction::CycleTask(dir) => {
                    self.cycle_task(cx, dir, "shell");
                }
                ShellAction::FocusSlot(name) => {
                    if self.st.stage.slots.iter().any(|s| s.surface == name) {
                        self.st.stage.focus = name;
                        self.commit_stage(cx, "shell");
                    }
                }
                ShellAction::ToggleHeader(name) => {
                    if let Some(s) = self.st.stage.slots.iter_mut().find(|s| s.surface == name) {
                        s.mode = if s.mode == SlotMode::Header {
                            if s.pinned { SlotMode::Compact } else { SlotMode::Fill }
                        } else {
                            SlotMode::Header
                        };
                        self.commit_stage(cx, "shell");
                    }
                }
                ShellAction::CycleSlotMode(name) => {
                    let cur = self.st.stage.slots.iter().find(|s| s.surface == name).map(|s| s.mode);
                    match cur {
                        Some(SlotMode::Header) | Some(SlotMode::Fill) if self.st.stage.slots.len() > 1 => {
                            if cur == Some(SlotMode::Fill) {
                                self.st.stage.solo(&name);
                            } else if let Some(s) = self.st.stage.slots.iter_mut().find(|s| s.surface == name) {
                                s.mode = SlotMode::Compact;
                            }
                            self.commit_stage(cx, "shell");
                        }
                        Some(SlotMode::Compact) => {
                            if let Some(s) = self.st.stage.slots.iter_mut().find(|s| s.surface == name) {
                                s.mode = SlotMode::Fill;
                            }
                            self.commit_stage(cx, "shell");
                        }
                        _ => {}
                    }
                }
                ShellAction::RemoveSlot(name) => {
                    if self.st.stage.remove_surface(&name, self.st.bodies.contains_key("home")) {
                        self.commit_stage(cx, "shell");
                    }
                }
            }
        }
    }

    fn cycle_task(&mut self, cx: &mut Cx, dir: i32, by: &str) {
        if self.st.stage.mode == StageMode::Deck && self.st.stage.slots.len() > 1 {
            let cur = self.st.stage.slots.iter().position(|s| s.surface == self.st.stage.focus).unwrap_or(0);
            let n = self.st.stage.slots.len() as isize;
            let next_idx = (cur as isize + dir as isize).rem_euclid(n) as usize;
            self.st.stage.focus = self.st.stage.slots[next_idx].surface.clone();
            self.commit_stage(cx, by);
            return;
        }
        let mut order: Vec<String> = self
            .st
            .bodies
            .keys()
            .filter(|n| !matches!(n.as_str(), "stage" | "power"))
            .cloned()
            .collect();
        order.sort_by(|a, b| {
            let rank = |s: &str| match s {
                "home" => 0,
                "chat" => 1,
                "voice" => 2,
                "agents" => 3,
                "battery" => 4,
                "sound" => 5,
                "display" => 6,
                "wifi" => 7,
                _ => 8,
            };
            rank(a).cmp(&rank(b)).then_with(|| a.cmp(b))
        });
        if order.is_empty() {
            return;
        }
        let cur = order.iter().position(|s| *s == self.st.stage.focus).unwrap_or(0);
        let n = order.len() as isize;
        let next_idx = (cur as isize + dir as isize).rem_euclid(n) as usize;
        self.st.stage.apply_front(&order[next_idx]);
        self.commit_stage(cx, by);
    }

    fn drain_socket(&mut self, cx: &mut Cx) {
        let Some(rx) = self.st.rx.take() else { return };
        let mut mic_changed = false;
        let mut loud_changed = false;
        let mut panel_changed = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Open { conn, tx, peer } => {
                    self.st.conns.insert(conn, Conn { tx, peer, watch: None });
                }
                Msg::Closed { conn } => {
                    self.st.conns.remove(&conn);
                    for name in self.st.owned.remove(&conn).unwrap_or_default() {
                        let _ = self.close_surface(cx, &name, false);
                    }
                    let raised = self.st.guards.first().map(|g| g.conn);
                    let had_guards = !self.st.guards.is_empty();
                    self.st.guards.retain(|g| g.conn != conn);
                    if raised == Some(conn) {
                        self.raise_next_guard(cx);
                    } else if had_guards && self.st.guards.is_empty() {
                        notify_screen_inhibit("inhibit off");
                    }
                }
                Msg::MicChanged => mic_changed = true,
                Msg::LoudChanged => loud_changed = true,
                Msg::PanelChanged => panel_changed = true,
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
                        Ok(None) => continue,
                        Ok(Some(mut v)) => {
                            if let JsonValue::Object(m) = &mut v {
                                m.entry("ok".into()).or_insert(JsonValue::Bool(true));
                            }
                            v
                        }
                        Err(e) => obj(vec![("ok", JsonValue::Bool(false)), ("error", JsonValue::String(e))]),
                    };
                    let t = Instant::now();
                    self.send(conn, reply.serialize_json());
                    for (wconn, ev_line) in std::mem::take(&mut self.st.pending_watch_shown) {
                        self.send(wconn, ev_line);
                    }
                    self.st.split.add_named("reply", t.elapsed());
                }
            }
        }
        self.st.rx = Some(rx);
        if mic_changed {
            self.mic_event(cx);
        }
        if loud_changed {
            self.read_loud(cx);
        }
        if panel_changed {
            self.check_panel_inner(cx, true);
        }
    }

    fn send(&self, conn: u64, line: String) {
        if let Some(c) = self.st.conns.get(&conn) {
            let _ = c.tx.send(line);
        }
    }

    fn request(&mut self, cx: &mut Cx, conn: u64, req: &JsonValue) -> Result<Option<JsonValue>, String> {
        let t = Instant::now();
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
                if reply.get("ok") != Some(&JsonValue::Bool(false)) {
                    if req.get("owned") == Some(&JsonValue::Bool(true)) {
                        self.st.owned.entry(conn).or_default().push(name.to_string());
                    }
                    self.sync_surfaces_key(cx);
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
                if req.get("reply") == Some(&JsonValue::Bool(false)) {
                    return Ok(None);
                }
                Ok(Some(obj(vec![])))
            }
            "stage" => self.stage_op(cx, req, &format!("client:{}", peer.label())).map(Some),
            "note" => {
                let text = req.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let secs = stage::json_num(req.get("secs")).unwrap_or(10.0);
                if text.is_empty() {
                    self.clear_note(cx);
                } else {
                    self.show_strip_note_secs(cx, text, secs);
                }
                Ok(Some(obj(vec![])))
            }
            "push" => {
                let name = req.get("stream").and_then(|v| v.as_str()).ok_or("push needs stream")?;
                let values: Vec<f32> = req
                    .get("values")
                    .and_then(|v| v.as_array())
                    .ok_or("push needs values")?
                    .iter()
                    .map(|v| match v {
                        JsonValue::F64(f) => *f as f32,
                        JsonValue::I64(i) => *i as f32,
                        JsonValue::U64(u) => *u as f32,
                        _ => 0.0,
                    })
                    .collect();
                if values.len() > stream::MAX_VALUES {
                    log!("tb-shell: push to {name}: {} values, a view shows the first {}", values.len(), stream::MAX_VALUES);
                }
                let flag = |k: &str| req.get(k) == Some(&JsonValue::Bool(true));
                stream::push(name, values, flag("append"), flag("clear"));
                Ok(None)
            }
            "get" => {
                let keys = strings(req.get("keys"));
                let state = self
                    .st
                    .store
                    .iter()
                    .filter(|(k, _)| keys.as_ref().map_or(true, |ks| ks.contains(k)))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Ok(Some(obj(vec![("state", JsonValue::Object(state))])))
            }
            "watch" => {
                let spec = WatchSpec { surfaces: strings(req.get("surfaces")), keys: strings(req.get("keys")) };
                if !self.st.panel_off {
                    for vis in &self.st.visible {
                        if spec.wants_surface(vis) {
                            let ev = obj(vec![("ev", "shown".into()), ("surface", vis.as_str().into())]).serialize_json();
                            self.st.pending_watch_shown.push((conn, ev));
                        }
                    }
                }
                self.st.conns.get_mut(&conn).ok_or("no connection")?.watch = Some(spec);
                Ok(Some(obj(vec![])))
            }
            "close" => {
                let name = surface.ok_or("close needs surface")?;
                if req.get("unsave") == Some(&JsonValue::Bool(true)) {
                    let _ = self.st.persist.unsave_screen(name)?;
                }
                self.close_surface(cx, name, false).map(Some)
            }
            "save" => {
                let name = surface.ok_or("save needs surface")?;
                if self.st.views.contains(name) {
                    return Err(format!("{name} is a built-in view"));
                }
                let body = self.st.bodies.get(name).cloned().ok_or_else(|| format!("no surface {name}"))?;
                let title = req.get("title").and_then(|v| v.as_str());
                self.st.persist.save_screen(name, &body, title)?;
                self.sync_surfaces_key(cx);
                Ok(Some(obj(vec![("saved", name.into())])))
            }
            "unsave" => {
                let name = surface.ok_or("unsave needs surface")?;
                let removed = self.st.persist.unsave_screen(name)?;
                self.sync_surfaces_key(cx);
                Ok(Some(obj(vec![("unsaved", name.into()), ("removed", JsonValue::Bool(removed))])))
            }
            "lib" => self.lib_op(cx, req).map(Some),
            "reload" => self.reload_op(cx, req).map(Some),
            "termd" => self.termd_socket_op(cx, req).map(Some),
            "jit" => {
                let name = surface.ok_or("jit needs surface")?;
                if !valid_name(name) {
                    return Err("invalid surface name".into());
                }
                if self.st.views.contains(name) {
                    return Err(format!("{name} is a built-in view"));
                }
                let package = req
                    .get("package")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .ok_or("jit needs package")?;
                let dir = req.get("dir").and_then(|v| v.as_str());
                let src_store = req.get("src").and_then(|v| v.as_str());
                let fps = stage::json_num(req.get("fps")).unwrap_or(24.0).clamp(1.0, 60.0);
                let front = req.get("front").map_or(true, |v| v == &JsonValue::Bool(true));

                // Remove any prior Splash isolate of the same name before installing the JIT slot.
                if let Some(mut sh) = self.shell(cx).borrow_mut::<Shell>() {
                    if let Some(at) = sh.surfaces.iter().position(|(n, _)| n == name) {
                        let (_, sp) = sh.surfaces.remove(at);
                        sp.set_text(cx, "");
                    }
                }
                let will_vis = !self.st.panel_off && (front || self.st.visible.contains(name));
                let inst = jit_slot::JitInstance::spawn(name, package, dir, src_store, fps, will_vis)?;
                if let Some(mut sh) = self.shell(cx).borrow_mut::<Shell>() {
                    sh.jit.insert(name.to_string(), inst);
                }
                self.st.bodies.insert(name.to_string(), format!("__jit__:{package}"));
                self.st.parked.remove(name);
                self.st.stale.remove(name);
                if req.get("ephemeral") == Some(&JsonValue::Bool(true)) {
                    let list = self.st.owned.entry(conn).or_default();
                    if !list.iter().any(|n| n == name) {
                        list.push(name.to_string());
                    }
                }
                if front {
                    self.st.stage.apply_front(name);
                }
                self.commit_stage(cx, "jit");
                self.sync_surfaces_key(cx);
                Ok(Some(obj(vec![
                    ("surface", name.into()),
                    ("package", package.into()),
                    ("fps", JsonValue::F64(fps)),
                ])))
            }
            "confirm" | "guard" => {
                let text = req.get("text").and_then(|v| v.as_str()).ok_or("confirm needs text")?.to_string();
                let yes = req.get("yes").and_then(|v| v.as_str()).unwrap_or("Proceed").to_string();
                let no = req.get("no").and_then(|v| v.as_str()).unwrap_or("Cancel").to_string();
                let who = req
                    .get("who")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("asked by {}", peer.label()));
                let view = GuardView { text, who, yes, no };
                self.st.guards.push(Guard { conn, view });
                if self.st.guards.len() == 1 {
                    self.raise_next_guard(cx);
                }
                Ok(None)
            }
            _ => Err(format!("unknown op {op}")),
        }
    }

    fn stage_op(&mut self, cx: &mut Cx, req: &JsonValue, by: &str) -> Result<JsonValue, String> {
        let sub = req.get("sub").and_then(|v| v.as_str()).unwrap_or("get");
        if sub == "get" {
            return Ok(obj(vec![("stage", self.st.stage.to_json())]));
        }
        if !self.st.guards.is_empty() && !matches!(sub, "save" | "unsave") {
            return Err("cannot modify stage while a guard is up".into());
        }
        let check_exists = |s: &str, bodies: &HashMap<String, String>| -> Result<(), String> {
            if bodies.contains_key(s) {
                Ok(())
            } else {
                Err(format!("no surface {s}"))
            }
        };

        if self.st.stage.focus == "stage" && matches!(sub, "add" | "rm" | "pin" | "unpin" | "solo") {
            if let Some(prev) = self.st.prev_stage_before_switcher.take() {
                self.st.stage = prev;
            }
        }

        match sub {
            "stack" | "grid" | "deck" | "set" => {
                if sub == "set" {
                    if let Some(st_obj) = req.get("stage") {
                        let parsed = Stage::from_json(st_obj).ok_or("invalid stage json")?;
                        for s in &parsed.slots {
                            check_exists(&s.surface, &self.st.bodies)?;
                        }
                        self.st.stage = parsed;
                        self.commit_stage(cx, by);
                        return Ok(obj(vec![("stage", self.st.stage.to_json())]));
                    }
                }
                let specs = req.get("specs").or_else(|| req.get("slots")).and_then(|v| v.as_array()).ok_or("stage needs specs array")?;
                let mut slots = Vec::new();
                for item in specs {
                    let slot = StageSlot::from_json(item).ok_or("invalid slot spec")?;
                    check_exists(&slot.surface, &self.st.bodies)?;
                    if !slots.iter().any(|x: &StageSlot| x.surface == slot.surface) {
                        slots.push(slot);
                    }
                }
                if slots.is_empty() {
                    return Err("stage requires at least one slot".into());
                }
                self.st.stage.mode = StageMode::from_str(sub);
                self.st.stage.slots = slots;
                if let Some(f) = req.get("focus").and_then(|v| v.as_str()).filter(|f| self.st.stage.slots.iter().any(|s| s.surface == *f)) {
                    self.st.stage.focus = f.to_string();
                } else {
                    self.st.stage.focus.clear();
                }
                self.st.stage.ensure_valid_focus(false);
                self.commit_stage(cx, by);
            }
            "add" => {
                let name = req.get("surface").and_then(|v| v.as_str()).ok_or("stage add needs surface")?;
                check_exists(name, &self.st.bodies)?;
                let mode = req.get("mode").and_then(|v| v.as_str()).map(SlotMode::from_str).unwrap_or(SlotMode::Fill);
                let px = stage::json_num(req.get("px")).map(|p| p.clamp(36.0, 720.0));
                let weight = stage::json_num(req.get("weight")).unwrap_or(1.0).clamp(0.1, 20.0);
                // If currently on `stage` or `home` alone, replace `stage` switcher so adding from switcher shows the real work!
                self.st.stage.slots.retain(|s| s.surface != "stage");
                if let Some(existing) = self.st.stage.slots.iter_mut().find(|s| s.surface == name) {
                    existing.mode = mode;
                    existing.px = px;
                    existing.weight = weight;
                } else {
                    self.st.stage.slots.push(StageSlot {
                        surface: name.to_string(),
                        mode,
                        weight,
                        px,
                        pinned: false,
                        ephemeral: false,
                    });
                }
                self.st.stage.focus = name.to_string();
                self.commit_stage(cx, by);
            }
            "rm" => {
                let name = req.get("surface").and_then(|v| v.as_str()).ok_or("stage rm needs surface")?;
                self.st.stage.remove_surface(name, self.st.bodies.contains_key("home"));
                self.commit_stage(cx, by);
            }
            "pin" => {
                let name = req.get("surface").and_then(|v| v.as_str()).ok_or("stage pin needs surface")?;
                check_exists(name, &self.st.bodies)?;
                let mode = req.get("mode").and_then(|v| v.as_str()).map(SlotMode::from_str).unwrap_or(SlotMode::Compact);
                let px = stage::json_num(req.get("px")).map(|p| p.clamp(36.0, 720.0));
                let ephemeral = req.get("ephemeral") == Some(&JsonValue::Bool(true));
                if let Some(existing) = self.st.stage.slots.iter_mut().find(|s| s.surface == name) {
                    // If user already manually pinned it (`!existing.ephemeral`), don't downgrade to `ephemeral: true`!
                    if !existing.pinned {
                        existing.ephemeral = ephemeral;
                    } else if !ephemeral {
                        existing.ephemeral = false;
                    }
                    existing.pinned = true;
                    existing.mode = mode;
                    if px.is_some() {
                        existing.px = px;
                    }
                } else {
                    let insert_at = self.st.stage.slots.iter().take_while(|s| s.pinned).count();
                    self.st.stage.slots.insert(
                        insert_at,
                        StageSlot {
                            surface: name.to_string(),
                            mode,
                            weight: 1.0,
                            px,
                            pinned: true,
                            ephemeral,
                        },
                    );
                }
                if self.st.stage.mode == StageMode::Deck {
                    self.st.stage.mode = StageMode::Stack;
                }
                self.st.stage.ensure_valid_focus(self.st.bodies.contains_key("home"));
                self.commit_stage(cx, by);
            }
            "unpin" => {
                let name = req.get("surface").and_then(|v| v.as_str()).ok_or("stage unpin needs surface")?;
                let ephemeral_only = req.get("ephemeral_only") == Some(&JsonValue::Bool(true));
                if let Some(idx) = self.st.stage.slots.iter().position(|s| s.surface == name) {
                    let is_ephem = self.st.stage.slots[idx].ephemeral;
                    if ephemeral_only && !is_ephem {
                        return Ok(obj(vec![("stage", self.st.stage.to_json()), ("kept_manual_pin", JsonValue::Bool(true))]));
                    }
                    if self.st.stage.slots.len() > 1 || is_ephem {
                        self.st.stage.slots.remove(idx);
                    } else {
                        self.st.stage.slots[idx].pinned = false;
                        self.st.stage.slots[idx].ephemeral = false;
                        self.st.stage.slots[idx].mode = SlotMode::Fill;
                    }
                    self.st.stage.ensure_valid_focus(self.st.bodies.contains_key("home"));
                    self.commit_stage(cx, by);
                }
            }
            "solo" => {
                let name = req.get("surface").and_then(|v| v.as_str()).ok_or("stage solo needs surface")?;
                check_exists(name, &self.st.bodies)?;
                if req.get("clear_pins") == Some(&JsonValue::Bool(true)) {
                    self.st.stage.mode = StageMode::Stack;
                    self.st.stage.slots = vec![StageSlot::fill(name)];
                    self.st.stage.focus = name.to_string();
                } else {
                    self.st.stage.solo(name);
                }
                self.commit_stage(cx, by);
            }
            "focus" => {
                let name = req.get("surface").and_then(|v| v.as_str()).ok_or("stage focus needs surface")?;
                if self.st.stage.slots.iter().any(|s| s.surface == name) {
                    self.st.stage.focus = name.to_string();
                    self.commit_stage(cx, by);
                } else {
                    check_exists(name, &self.st.bodies)?;
                    self.st.stage.apply_front(name);
                    self.commit_stage(cx, by);
                }
            }
            "next" => self.cycle_task(cx, 1, by),
            "prev" => self.cycle_task(cx, -1, by),
            "save" => {
                let name = req.get("name").and_then(|v| v.as_str()).ok_or("stage save needs name")?;
                if !valid_name(name) {
                    return Err("invalid workspace name".into());
                }
                let title = req.get("title").and_then(|v| v.as_str());
                self.st.persist.save_workspace(name, title, &self.st.stage.to_json())?;
                self.sync_workspaces_key(cx);
                return Ok(obj(vec![("saved", name.into()), ("stage", self.st.stage.to_json())]));
            }
            "open" => {
                let name = req.get("name").and_then(|v| v.as_str()).ok_or("stage open needs name")?;
                let saved = self.st.persist.load_workspace(name).ok_or_else(|| format!("no workspace {name}"))?;
                let mut parsed = Stage::from_json(&saved).ok_or("corrupt workspace json")?;
                parsed.slots.retain(|s| self.st.bodies.contains_key(&s.surface));
                if parsed.slots.is_empty() {
                    return Err(format!("workspace {name} has no active surfaces"));
                }
                parsed.ensure_valid_focus(false);
                self.st.stage = parsed;
                self.commit_stage(cx, by);
            }
            "unsave" => {
                let name = req.get("name").and_then(|v| v.as_str()).ok_or("stage unsave needs name")?;
                let removed = self.st.persist.unsave_workspace(name)?;
                self.sync_workspaces_key(cx);
                return Ok(obj(vec![("unsaved", name.into()), ("removed", JsonValue::Bool(removed))]));
            }
            _ => return Err(format!("unknown stage subcommand {sub}")),
        }
        Ok(obj(vec![("stage", self.st.stage.to_json())]))
    }

    fn lib_op(&mut self, cx: &mut Cx, req: &JsonValue) -> Result<JsonValue, String> {
        let sub = req.get("sub").and_then(|v| v.as_str()).ok_or("lib needs sub (ls | cat | put | rm)")?;
        match sub {
            "ls" => {
                let unused_days = match req.get("unused_days") {
                    Some(JsonValue::U64(n)) => Some(*n),
                    Some(JsonValue::I64(n)) if *n >= 0 => Some(*n as u64),
                    Some(JsonValue::F64(f)) if *f >= 0.0 => Some(*f as u64),
                    _ if req.get("unused") == Some(&JsonValue::Bool(true)) => Some(30),
                    _ => None,
                };
                Ok(obj(vec![("pieces", JsonValue::Array(self.st.persist.ls_pieces(unused_days)))]))
            }
            "cat" => {
                let name = req.get("name").and_then(|v| v.as_str()).ok_or("lib cat needs name")?;
                let body = self.st.persist.cat_piece(name).ok_or_else(|| format!("no library piece {name}"))?;
                Ok(obj(vec![("name", name.into()), ("body", body.into())]))
            }
            "put" => {
                let name = req.get("name").and_then(|v| v.as_str()).ok_or("lib put needs name")?;
                let body = req.get("body").and_then(|v| v.as_str()).ok_or("lib put needs body")?;
                if !library::valid_piece_name(name) {
                    return Err("a library piece name is 1..64 of A-Z a-z 0-9 _ - (starting with a letter or _, not \"base\")".into());
                }
                if !self.st.agent_shaders_on {
                    if let Some(at) = shader_text(body) {
                        return Err(format!("shader text is off (TB_SHELL_AGENT_SHADERS=0): {at}"));
                    }
                }
                let other_lib = self.st.persist.base_and_lib_except(name);
                let ahead = lines_ahead(other_lib.lines().count());
                let check_code = format!("{PRELUDE}{other_lib}{body}\nView{{}}\n");
                let raw_errors = makepad_widgets::splash::validate_splash_body_with_host_io(cx, &check_code);
                let _ = take_splash_host_requests();
                if !raw_errors.is_empty() {
                    let errors: Vec<JsonValue> = raw_errors
                        .into_iter()
                        .map(|e| match locate_with_ahead(&e, "validating", ahead) {
                            Some((line, col, msg)) => obj(vec![("line", (line as u64).into()), ("col", (col as u64).into()), ("message", msg.into())]),
                            None => obj(vec![("message", e.into())]),
                        })
                        .collect();
                    return Ok(obj(vec![
                        ("ok", JsonValue::Bool(false)),
                        ("error", "library piece did not validate".into()),
                        ("errors", JsonValue::Array(errors)),
                    ]));
                }
                self.st.persist.put_piece(name, body)?;
                let dependents: Vec<(String, String)> = self
                    .st
                    .bodies
                    .iter()
                    .filter(|(_, b)| library::ident_in(b, name))
                    .map(|(s, b)| (s.clone(), b.clone()))
                    .collect();
                let mut reshown = Vec::new();
                for (surf, surf_body) in dependents {
                    if let Ok(r) = self.show(cx, &surf, &surf_body, false) {
                        if r.get("ok") != Some(&JsonValue::Bool(false)) {
                            reshown.push(JsonValue::String(surf));
                        }
                    }
                }
                Ok(obj(vec![("name", name.into()), ("reshown", JsonValue::Array(reshown))]))
            }
            "rm" => {
                let name = req.get("name").and_then(|v| v.as_str()).ok_or("lib rm needs name")?;
                let force = req.get("force") == Some(&JsonValue::Bool(true));
                if !force {
                    let mut users: Vec<&str> = self
                        .st
                        .bodies
                        .iter()
                        .filter(|(_, b)| library::ident_in(b, name))
                        .map(|(s, _)| s.as_str())
                        .collect();
                    users.sort_unstable();
                    if !users.is_empty() {
                        return Err(format!("library piece {name} is in use by active surface(s): {} (close them or pass --force)", users.join(", ")));
                    }
                }
                self.st.persist.rm_piece(name)?;
                Ok(obj(vec![("removed", name.into())]))
            }
            _ => Err(format!("unknown lib subcommand {sub}")),
        }
    }

    fn reload_op(&mut self, cx: &mut Cx, req: &JsonValue) -> Result<JsonValue, String> {
        let Some(dir) = library::resolve_ui_subdir("TB_SHELL_VIEWS", "views") else {
            return Err("no views directory found".into());
        };
        let prev_sys_base = self.st.persist.sys_base_source.clone();
        let prev_pieces = self.st.persist.pieces.clone();
        self.st.persist.reload_all(BASE);
        self.reload_trusted();

        let target = req.get("view").and_then(|v| v.as_str()).filter(|s| !s.is_empty() && *s != "*");
        if let Some(name) = target {
            if !valid_name(name) {
                self.st.persist.sys_base_source = prev_sys_base;
                self.st.persist.pieces = prev_pieces;
                return Err("invalid view name".into());
            }
            let path = dir.join(format!("{name}.splash"));
            let body = match std::fs::read_to_string(&path) {
                Ok(b) => b,
                Err(e) => {
                    self.st.persist.sys_base_source = prev_sys_base;
                    self.st.persist.pieces = prev_pieces;
                    return Err(format!("{}: {e}", path.display()));
                }
            };
            let was_view = self.st.views.insert(name.to_string());
            let reply = match self.show(cx, name, &body, false) {
                Ok(r) => r,
                Err(e) => {
                    if !was_view {
                        self.st.views.remove(name);
                    }
                    self.st.persist.sys_base_source = prev_sys_base;
                    self.st.persist.pieces = prev_pieces;
                    return Err(e);
                }
            };
            if reply.get("ok") == Some(&JsonValue::Bool(false)) {
                if !was_view {
                    self.st.views.remove(name);
                }
                self.st.persist.sys_base_source = prev_sys_base;
                self.st.persist.pieces = prev_pieces;
                return Ok(reply);
            }
            self.sync_surfaces_key(cx);
            return Ok(obj(vec![
                ("view", name.into()),
                ("dir", dir.to_string_lossy().to_string().into()),
                ("ms", reply.get("ms").cloned().unwrap_or(JsonValue::Null)),
                ("errors", JsonValue::Array(vec![])),
            ]));
        }

        // Atomic `--all` validation pass first so a broken base.splash / view never half-commits!
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |x| x == "splash"))
            .collect();
        files.sort();
        let ahead = self.current_ahead();
        let mut view_sources: Vec<(String, String)> = Vec::new();
        let mut err_map: HashMap<String, JsonValue> = HashMap::new();
        for path in files {
            let Some(name) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
            let Ok(body) = std::fs::read_to_string(&path) else { continue };
            let full = self.full_source(&body);
            let raw_errors = makepad_widgets::splash::validate_splash_body_with_host_io(cx, &full);
            let _ = take_splash_host_requests();
            if !raw_errors.is_empty() {
                let errors: Vec<JsonValue> = raw_errors
                    .into_iter()
                    .map(|e| match locate_with_ahead(&e, "validating", ahead) {
                        Some((line, col, msg)) => obj(vec![("line", (line as u64).into()), ("col", (col as u64).into()), ("message", msg.into())]),
                        None => obj(vec![("message", e.into())]),
                    })
                    .collect();
                err_map.insert(name, JsonValue::Array(errors));
            } else {
                view_sources.push((name, body));
            }
        }
        if !err_map.is_empty() {
            self.st.persist.sys_base_source = prev_sys_base;
            self.st.persist.pieces = prev_pieces;
            return Ok(obj(vec![
                ("ok", JsonValue::Bool(false)),
                ("error", "one or more views did not evaluate".into()),
                ("reloaded", JsonValue::Array(vec![])),
                ("errors", JsonValue::Object(err_map)),
            ]));
        }

        let mut reloaded = Vec::new();
        for (name, body) in view_sources {
            self.st.views.insert(name.clone());
            if let Ok(reply) = self.show(cx, &name, &body, false) {
                if reply.get("ok") != Some(&JsonValue::Bool(false)) {
                    reloaded.push(JsonValue::String(name));
                }
            }
        }
        let agent_bodies: Vec<(String, String)> = self
            .st
            .bodies
            .iter()
            .filter(|(n, b)| !self.st.views.contains(*n) && !b.starts_with("__jit__:"))
            .map(|(n, b)| (n.clone(), b.clone()))
            .collect();
        for (name, body) in agent_bodies {
            if let Ok(r) = self.show(cx, &name, &body, false) {
                if r.get("ok") != Some(&JsonValue::Bool(false)) {
                    reloaded.push(JsonValue::String(name));
                }
            }
        }
        self.sync_surfaces_key(cx);
        Ok(obj(vec![
            ("dir", dir.to_string_lossy().to_string().into()),
            ("reloaded", JsonValue::Array(reloaded)),
        ]))
    }

    fn refused_key(&self, patch: &JsonValue) -> Option<&'static str> {
        if patch.get(SHELL_KEY).is_some() {
            return Some(SHELL_KEY);
        }
        if patch.get(SURFACES_KEY).is_some() {
            return Some(SURFACES_KEY);
        }
        if patch.get(WORKSPACES_KEY).is_some() {
            return Some(WORKSPACES_KEY);
        }
        if patch.get("stage").is_some() {
            return Some("stage");
        }
        if !self.st.guards.is_empty() && patch.get("front").is_some() {
            return Some("front");
        }
        None
    }

    fn raise_next_guard(&mut self, cx: &mut Cx) {
        let view = self.st.guards.first().map(|g| g.view.clone());
        if view.is_some() {
            notify_screen_inhibit("inhibit 60");
        } else {
            notify_screen_inhibit("inhibit off");
        }
        if let Some(mut sh) = self.shell(cx).borrow_mut::<Shell>() {
            sh.set_guard(cx, view);
        }
    }

    fn answer_guard(&mut self, cx: &mut Cx, yes: bool) {
        if self.st.guards.is_empty() {
            return;
        }
        let g = self.st.guards.remove(0);
        let choice = if yes { &g.view.yes } else { &g.view.no };
        let drawn = self
            .shell(cx)
            .borrow::<Shell>()
            .and_then(|sh| sh.guard_drawn_ms)
            .map_or(JsonValue::Null, |ms| JsonValue::F64((ms * 10.0).round() / 10.0));
        let reply = obj(vec![
            ("ok", JsonValue::Bool(true)),
            ("answer", if yes { "yes" } else { "no" }.into()),
            ("choice", choice.as_str().into()),
            ("drawn_ms", drawn),
        ]);
        self.send(g.conn, reply.serialize_json());
        self.raise_next_guard(cx);
    }

    fn show(&mut self, cx: &mut Cx, name: &str, body: &str, front: bool) -> Result<JsonValue, String> {
        if !valid_name(name) {
            return Err("a surface name is 1 to 64 of A-Z a-z 0-9 - _ . and does not start with \"guard\"".into());
        }
        let has_shader = shader_text(body);
        if !self.st.trusted.contains(body) && !self.st.agent_shaders_on {
            if let Some(at) = &has_shader {
                return Err(format!("shader text is off (TB_SHELL_AGENT_SHADERS=0): {at}"));
            }
        }
        let started = Instant::now();
        let shell = self.shell(cx);
        let Some(mut sh) = shell.borrow_mut::<Shell>() else { return Err("no shell".into()) };
        let (splash, created) = match sh.surface(name) {
            Some(w) => (w, false),
            None => (sh.add(cx, name), true),
        };
        drop(sh);

        let ahead = self.current_ahead();
        let full = self.full_source(body);
        let will_be_visible = !self.st.panel_off && (front || self.st.visible.contains(name));

        // Always validate in throwaway isolate when panel is off, replacing an existing surface,
        // or showing a background (`--back`) surface that starts parked!
        if !created || self.st.panel_off || !will_be_visible {
            let raw_errors = makepad_widgets::splash::validate_splash_body_with_host_io(cx, &full);
            let _ = take_splash_host_requests();
            if !raw_errors.is_empty() {
                let errors: Vec<JsonValue> = raw_errors
                    .into_iter()
                    .map(|e| match locate_with_ahead(&e, "validating", ahead) {
                        Some((line, col, msg)) => obj(vec![("line", (line as u64).into()), ("col", (col as u64).into()), ("message", msg.into())]),
                        None => obj(vec![("message", e.into())]),
                    })
                    .collect();
                if created {
                    shell.borrow_mut::<Shell>().map(|mut sh| sh.remove(cx, name));
                }
                return Ok(obj(vec![
                    ("ok", JsonValue::Bool(false)),
                    ("error", "body did not evaluate".into()),
                    ("errors", JsonValue::Array(errors)),
                ]));
            }
        }

        if will_be_visible {
            if !created {
                splash.set_text(cx, "");
            }
            splash.set_text(cx, &full);
            let errors: Vec<JsonValue> = splash
                .as_splash()
                .last_errors()
                .into_iter()
                .map(|e| match locate_with_ahead(&e, name, ahead) {
                    Some((line, col, msg)) => obj(vec![("line", (line as u64).into()), ("col", (col as u64).into()), ("message", msg.into())]),
                    None => obj(vec![("message", e.into())]),
                })
                .collect();
            if !errors.is_empty() {
                if created {
                    shell.borrow_mut::<Shell>().map(|mut sh| sh.remove(cx, name));
                }
                return Ok(obj(vec![
                    ("ok", JsonValue::Bool(false)),
                    ("error", "body did not evaluate".into()),
                    ("errors", JsonValue::Array(errors)),
                ]));
            }
            self.st.parked.remove(name);
            self.st.slot_prelude_cache.remove(name);
        } else {
            // Keep hidden/asleep surface parked until brought onto the visible stage!
            splash.set_text(cx, "");
            self.st.parked.insert(name.to_string());
            self.st.slot_prelude_cache.remove(name);
        }

        self.st.bodies.insert(name.to_string(), body.to_string());
        self.st.persist.record_usage(body);
        self.write_surfaces_file();
        self.st.reads.insert(name.to_string(), Reads::scan(body));

        if self.is_watched_surface(name) {
            let verify_redraws_left = if has_shader.is_some() { 3 } else { 0 };
            self.st.frame_watch.insert(
                name.to_string(),
                SurfaceWatch { verify_redraws_left, ..Default::default() },
            );
        } else {
            self.st.frame_watch.remove(name);
        }

        if will_be_visible {
            self.feed_slot_for(cx, name, &splash);
            self.feed_all(cx, name, &splash);
        }
        if front && self.st.guards.is_empty() {
            self.st.stage.apply_front(name);
            self.commit_stage(cx, "shell");
        } else {
            self.sync_gpu_timer_for_visible(cx);
        }
        cx.redraw_all();
        let reads = match &self.st.reads[name] {
            Reads::All => JsonValue::String("*".into()),
            Reads::Keys(k) => JsonValue::Array(k.iter().map(|k| k.as_str().into()).collect()),
        };
        let ms = JsonValue::F64((started.elapsed().as_secs_f64() * 1e4).round() / 10.0);
        Ok(obj(vec![("errors", JsonValue::Array(vec![])), ("reads", reads), ("ms", ms)]))
    }

    fn close_surface(&mut self, cx: &mut Cx, name: &str, force_home: bool) -> Result<JsonValue, String> {
        let removed = self.shell(cx).borrow_mut::<Shell>().map_or(false, |mut sh| sh.remove(cx, name));
        if !removed {
            return Err(format!("no surface {name}"));
        }
        self.st.reads.remove(name);
        self.st.bodies.remove(name);
        self.st.parked.remove(name);
        self.st.stale.remove(name);
        self.st.warm_lru.retain(|n| n != name);
        self.st.slot_prelude_cache.remove(name);
        self.st.slot_dims.remove(name);
        self.st.frame_watch.remove(name);
        self.write_surfaces_file();
        self.st.front_history.retain(|n| n != name);

        let was_in_stage = self.st.stage.remove_surface(name, false);
        if was_in_stage {
            if self.st.stage.slots.is_empty() {
                let back = if force_home && self.st.bodies.contains_key("home") {
                    Some("home".to_string())
                } else {
                    self.st
                        .front_history
                        .iter()
                        .rev()
                        .find(|n| self.st.bodies.contains_key(*n))
                        .cloned()
                        .or_else(|| self.st.bodies.contains_key("home").then(|| "home".to_string()))
                };
                if let Some(b) = back {
                    self.st.stage.slots.push(StageSlot::fill(b.clone()));
                    self.st.stage.focus = b;
                }
            }
            self.commit_stage(cx, "shell");
        }
        self.sync_surfaces_key(cx);
        Ok(obj(vec![]))
    }

    fn queue_render(&mut self, cx: &mut Cx, name: &str, splash: &WidgetRef) {
        if !self.st.render_pending.iter().any(|(n, _)| n == name) {
            self.st.render_pending.push((name.to_string(), splash.clone()));
        }
        self.st.render_frame = cx.new_next_frame();
    }

    fn feed_all(&mut self, cx: &mut Cx, name: &str, splash: &WidgetRef) {
        let Some(reads) = self.st.reads.get(name).cloned() else { return };
        let keys: Vec<String> = self.st.store.keys().filter(|k| reads.any(std::slice::from_ref(k))).cloned().collect();
        self.feed(cx, name, splash, &keys);
    }

    fn feed(&mut self, cx: &mut Cx, name: &str, splash: &WidgetRef, keys: &[String]) {
        let t = Instant::now();
        self.feed_inner(cx, name, splash, keys);
        self.st.split.add_named("feed", t.elapsed());
    }

    fn feed_inner(&mut self, cx: &mut Cx, name: &str, splash: &WidgetRef, keys: &[String]) {
        let body = self.st.bodies.get(name).map(|s| s.as_str()).unwrap_or("");
        let reads_dot_msgs = body.contains(".msgs");
        let has_feed_widget = body.contains("ChatFeed") || body.contains("VirtualFeed");
        let vals = keys
            .iter()
            .map(|k| {
                let strip_msgs = !reads_dot_msgs && (k == "chat" || has_feed_widget);
                let v = match self.st.store.get(k) {
                    Some(JsonValue::Object(m)) if strip_msgs && m.contains_key("msgs") => {
                        let filtered = m
                            .iter()
                            .filter(|(mk, _)| mk.as_str() != "msgs")
                            .map(|(mk, mv)| (mk.clone(), mv.clone()))
                            .collect();
                        JsonValue::Object(filtered)
                    }
                    Some(other) => other.clone(),
                    None => JsonValue::Null,
                };
                (k.clone(), v)
            })
            .collect();
        let text = JsonValue::Object(vals).serialize_json();
        let split = &mut self.st.split;
        split.timed(5, || {
            let _ = splash.widget(cx, ids!(main));
        });
        if let Some(mut inner) = splash.borrow_mut::<Splash>() {
            let split = &mut self.st.split;
            if split.on() {
                split.add_count("assign-bytes", text.len() as f64);
                let t = Instant::now();
                inner.call_script_fn_with_strings(cx, live_id!(__noop), &[&text]);
                split.add_named("noop-call", t.elapsed());
            }
            split.timed(2, || inner.call_script_fn_with_strings(cx, live_id!(__assign), &[&text]));
        }
        self.queue_render(cx, name, splash);
    }

    fn render_pending(&mut self, cx: &mut Cx) {
        let mut rendered = false;
        for (name, splash) in std::mem::take(&mut self.st.render_pending) {
            if self.st.parked.contains(&name) {
                continue;
            }
            if !self.st.visible.contains(&name) {
                self.st.stale.insert(name);
                continue;
            }
            let _ = splash.widget(cx, ids!(main));
            let t0 = Instant::now();
            if let Some(mut inner) = splash.borrow_mut::<Splash>() {
                self.st.split.timed(3, || inner.call_script_fn(cx, live_id!(__render), &[]));
                rendered = true;
            }
            let render_ms = t0.elapsed().as_secs_f64() * 1000.0;
            if self.is_watched_surface(&name) {
                self.st.frame_watch.entry(name).or_default().pending_render_ms += render_ms;
            }
        }
        if rendered {
            cx.redraw_all();
        }
    }

    fn apply(&mut self, cx: &mut Cx, patch: &JsonValue, by: &str) {
        let t = Instant::now();
        self.apply_inner(cx, patch, by);
        self.st.split.add_named("apply", t.elapsed());
    }

    fn apply_inner(&mut self, cx: &mut Cx, patch: &JsonValue, by: &str) {
        let JsonValue::Object(top) = patch else { return };
        let touched: Vec<String> = top.keys().cloned().collect();
        let store = &mut self.st.store;
        self.st.split.timed(1, || {
            for (k, v) in top {
                if *v == JsonValue::Null {
                    store.remove(k);
                    chat_feed::remove_store(k);
                } else {
                    let feed_relevant = match v {
                        JsonValue::Object(m) => {
                            m.contains_key("msgs")
                                || m.contains_key("items")
                                || m.contains_key("hidden_before")
                                || m.contains_key("agent")
                        }
                        JsonValue::Array(_) => true,
                        _ => false,
                    };
                    let entry = store.entry(k.clone()).or_insert(JsonValue::Null);
                    merge(entry, v);
                    if feed_relevant {
                        chat_feed::publish_store(k, entry);
                    }
                }
            }
        });

        let t = Instant::now();
        let shell = self.shell(cx);
        self.st.split.add_named("shell-lookup", t.elapsed());

        if touched.iter().any(|k| k == "battery" || k == "wifi") && !self.st.panel_off {
            self.update_strip_status(cx);
        }

        if touched.iter().any(|k| k == "front") {
            let front = self.st.store.get("front").and_then(|v| v.as_str()).unwrap_or("").to_string();
            self.st.stage.apply_front(&front);
            let stage_json = self.st.stage.to_json();
            self.st.store.insert("stage".into(), stage_json);
            if let Some(mut sh) = shell.borrow_mut::<Shell>() {
                sh.stage = self.st.stage.clone();
            }
            self.sync_visibility_and_lifecycle(cx);
            cx.redraw_all();
        }

        if !self.st.panel_off {
            let readers: Vec<(String, WidgetRef)> = shell.borrow::<Shell>().map_or(vec![], |sh| sh.surfaces.clone());
            for (name, splash) in readers {
                if self.st.parked.contains(&name) {
                    continue;
                }
                let keys: Vec<String> = match self.st.reads.get(&name) {
                    Some(Reads::All) => touched.clone(),
                    Some(Reads::Keys(ks)) => touched.iter().filter(|k| ks.contains(k)).cloned().collect(),
                    None => continue,
                };
                if !keys.is_empty() {
                    self.feed(cx, &name, &splash, &keys);
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

    fn errors_to_watchers(&mut self) {
        let (cursor, lines) = log_ring::read_since(self.st.log_cursor, usize::MAX);
        if lines.is_empty() {
            return;
        }
        self.st.log_cursor = cursor;
        let ahead = self.current_ahead();
        for l in lines {
            if !matches!(l.level, LogLevel::Error | LogLevel::Panic) {
                continue;
            }
            let Some(at) = l.text.find("splash:") else { continue };
            let name: String = l.text[at + 7..].chars().take_while(|c| *c != ':').collect();
            if !self.st.bodies.contains_key(&name) {
                continue;
            }
            let Some((line, col, message)) = locate_with_ahead(&l.text, &name, ahead) else { continue };
            let ev = obj(vec![
                ("ev", "error".into()),
                ("surface", name.as_str().into()),
                ("line", (line as u64).into()),
                ("col", (col as u64).into()),
                ("message", message.into()),
            ]);
            self.to_surface_watchers(&name, ev);
        }
    }

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
                    "stage" => {
                        if let Err(e) = self.stage_op(cx, &args, &format!("surface:{surface}")) {
                            log!("tb-shell: surface {surface} stage op refused: {e}");
                        }
                    }
                    "emit" => {
                        let name_str = args.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let name = args.get("name").cloned().unwrap_or(JsonValue::Null);
                        let data = args.get("data").cloned().unwrap_or(JsonValue::Null);
                        if !name_str.is_empty() {
                            self.handle_termd_ui_emit(cx, &name_str, &data);
                        }
                        let ev = obj(vec![("ev", "emit".into()), ("surface", surface.as_str().into()), ("name", name), ("data", data)]);
                        self.to_surface_watchers(&surface, ev);
                    }
                    other => log!("tb-shell: surface {surface} asked for {other}: refused"),
                }
            }
        }
    }

    fn sync_termd_store(&mut self, cx: &mut Cx, front_terminal: bool) {
        let Some(ref mut tb) = self.st.termd else { return };
        let val = tb.to_store_json();
        if front_terminal && self.st.bodies.contains_key("terminal") {
            let patch = obj(vec![
                ("termd", val),
                ("front", JsonValue::String("terminal".into())),
            ]);
            self.apply(cx, &patch, "termd");
        } else {
            let patch = obj(vec![("termd", val)]);
            self.apply(cx, &patch, "termd");
        }
    }

    fn drain_termd_events(&mut self, cx: &mut Cx) {
        if self.st.panel_off {
            return;
        }
        let outcome = self.st.termd.as_mut().map(|tb| tb.drain_events());
        if let Some((changed, want_front)) = outcome {
            if changed || want_front {
                self.sync_termd_store(cx, want_front);
            }
        }
    }

    fn handle_termd_ui_emit(&mut self, cx: &mut Cx, name: &str, data: &JsonValue) {
        let outcome = self
            .st
            .termd
            .as_mut()
            .map(|tb| tb.handle_ui_emit(name, data));
        if let Some((handled, store_changed, want_front)) = outcome {
            if handled {
                if store_changed || want_front {
                    self.sync_termd_store(cx, want_front);
                } else if want_front && self.st.bodies.contains_key("terminal") {
                    self.apply(cx, &obj(vec![("front", "terminal".into())]), "termd");
                }
            }
        }
    }

    fn termd_socket_op(&mut self, cx: &mut Cx, req: &JsonValue) -> Result<JsonValue, String> {
        let Some(ref mut tb) = self.st.termd else {
            return Err("termd bridge not initialized".into());
        };
        let (reply, store_changed, want_front) = tb.handle_socket_op(req)?;
        if store_changed || want_front {
            self.sync_termd_store(cx, want_front);
        }
        Ok(reply)
    }
}
