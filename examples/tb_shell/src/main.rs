//! tb-shell: one JSON store, one Splash isolate per surface, a strip the
//! shell draws itself, and a Unix socket that speaks newline-delimited JSON.
//!
//! A write from anywhere (a client's `set`, a tap's `set(k, v)`) is a JSON
//! merge patch on the one store. The shell applies it, hands the new values
//! of the touched top-level keys to each surface that reads them (the
//! prelude's `__apply` re-renders `main`), and tells the watchers.

pub use makepad_widgets;

use makepad_micro_serde::{DeJson, JsonValue, SerJson};
use makepad_widgets::*;
use makepad_widgets::splash_host::take_splash_host_requests;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::mpsc::{channel, Receiver, Sender};

app_main!(App);

const PRELUDE: &str = include_str!("prelude.splash");
const BASE: &str = include_str!("../lib/base.splash");
/// Keys only the shell writes (the marks and the guard live here later).
const SHELL_KEY: &str = "shell";

script_mod! {
    use mod.prelude.widgets.*

    mod.widgets.ShellBase = #(Shell::register_widget(vm))
    mod.widgets.Shell = set_type_default() do mod.widgets.ShellBase {
        width: Fill
        height: Fill
        strip_height: 28.0
        draw_strip +: { color: #x05070b }
        draw_label +: {
            color: #x9aa4b2
            text_style: theme.font_regular{ font_size: 10 }
        }
    }

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

/// The strip on top (drawn in the last overlay tier, above every popup a
/// body can open) and the `main` slot under it, which shows the front surface.
#[derive(Script, ScriptHook, WidgetRef, WidgetRegister)]
pub struct Shell {
    #[source] source: ScriptObjectRef,
    #[uid] uid: WidgetUid,
    #[walk] walk: Walk,
    #[live] draw_strip: DrawColor,
    #[live] draw_label: DrawText,
    #[live] strip_list: DrawList2d,
    #[live] strip_height: f64,
    #[rust] area: Area,
    #[rust] surfaces: Vec<(String, WidgetRef)>,
    #[rust] front: String,
}

impl Widget for Shell {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        for (name, splash) in &self.surfaces {
            // Input goes to the front surface only; hidden ones still get time.
            if *name == self.front || matches!(event, Event::Signal | Event::Timer(_) | Event::NextFrame(_)) {
                splash.handle_event(cx, event, scope);
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let layout = Layout { padding: Inset { top: self.strip_height, ..Inset::default() }, ..Layout::flow_down() };
        cx.begin_turtle(walk, layout);
        let rect = cx.turtle().rect();
        if let Some((_, splash)) = self.surfaces.iter().find(|(n, _)| *n == self.front) {
            splash.draw_walk_all(cx, scope, Walk::fill());
        }
        self.strip_list.begin_overlay_last(cx);
        self.draw_strip.draw_abs(cx, Rect { pos: rect.pos, size: dvec2(rect.size.x, self.strip_height) });
        let front = if self.front.is_empty() { "-" } else { self.front.as_str() };
        let label = format!("tb-shell   {front}   {} surfaces", self.surfaces.len());
        self.draw_label.draw_abs(cx, rect.pos + dvec2(12.0, 8.0), &label);
        self.strip_list.end(cx);
        cx.end_turtle_with_area(&mut self.area);
        DrawStep::done()
    }

}

impl WidgetNode for Shell {
    fn widget_uid(&self) -> WidgetUid {
        self.uid
    }
    fn walk(&mut self, _cx: &mut Cx) -> Walk {
        self.walk
    }
    fn area(&self) -> Area {
        self.area
    }
    fn redraw(&mut self, cx: &mut Cx) {
        self.area.redraw(cx);
    }
    fn children(&self, visit: &mut dyn FnMut(LiveId, WidgetRef)) {
        for (name, splash) in &self.surfaces {
            visit(LiveId::from_str(name), splash.clone());
        }
    }
}

impl Shell {
    fn surface(&self, name: &str) -> Option<WidgetRef> {
        self.surfaces.iter().find(|(n, _)| n == name).map(|(_, w)| w.clone())
    }

    fn add(&mut self, cx: &mut Cx, name: &str) -> WidgetRef {
        let splash = cx.with_vm(|vm| {
            let value = script_eval!(vm, { use mod.prelude.widgets.* Splash{width: Fill height: Fill} });
            WidgetRef::script_from_value(vm, value)
        });
        if let Some(mut inner) = splash.borrow_mut::<Splash>() {
            inner.set_host_io_only(true);
            inner.set_debug_name(name);
            inner.set_host_tag(cx, Some(name.to_string()));
        }
        self.surfaces.push((name.to_string(), splash.clone()));
        cx.widget_tree_mark_dirty(self.uid);
        splash
    }

    fn remove(&mut self, cx: &mut Cx, name: &str) -> bool {
        let Some(at) = self.surfaces.iter().position(|(n, _)| n == name) else { return false };
        let (_, splash) = self.surfaces.remove(at);
        splash.set_text(cx, ""); // an empty body reclaims the isolate and its timers
        cx.widget_tree_mark_dirty(self.uid);
        cx.redraw_all();
        true
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

enum Msg {
    Open { conn: u64, tx: Sender<String> },
    Line { conn: u64, line: String },
    Closed { conn: u64 },
}

struct Watch {
    surfaces: Option<Vec<String>>,
    keys: Option<Vec<String>>,
}

struct Conn {
    tx: Sender<String>,
    watch: Option<Watch>,
}

#[derive(Default)]
struct State {
    store: HashMap<String, JsonValue>,
    rx: Option<Receiver<Msg>>,
    conns: HashMap<u64, Conn>,
    reads: HashMap<String, Reads>,
    /// Surfaces in the order they were last shown; closing the front one
    /// brings back the one before it.
    order: Vec<String>,
}

#[derive(Script, ScriptHook)]
pub struct App {
    #[live] ui: WidgetRef,
    #[rust] st: State,
}

impl AppMain for App {
    fn script_mod(vm: &mut ScriptVm) -> ScriptValue {
        crate::makepad_widgets::script_mod(vm);
        self::script_mod(vm)
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        if let Event::Startup = event {
            self.st.rx = Some(listen(&socket_path()));
        }
        if let Event::Signal = event {
            self.drain_socket(cx);
        }
        self.drain_host_requests(cx);
        self.ui.handle_event(cx, event, &mut Scope::empty());
        self.drain_host_requests(cx);
    }
}

fn socket_path() -> String {
    if let Ok(p) = std::env::var("TB_SHELL_SOCK") {
        return p;
    }
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    format!("{dir}/tb-shell.sock")
}

/// One accept thread; per connection a reader (lines in) and a writer (lines
/// out). The UI thread only ever does non-blocking sends to the writers.
fn listen(path: &str) -> Receiver<Msg> {
    let (tx, rx) = channel();
    let _ = std::fs::remove_file(path);
    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            error!("tb-shell: bind {path}: {e}; no clients can reach this shell");
            return rx;
        }
    };
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    log!("tb-shell: listening on {path}");
    std::thread::spawn(move || {
        for (conn, stream) in (1u64..).zip(listener.incoming()) {
            let Ok(stream) = stream else { continue };
            let Ok(mut out) = stream.try_clone() else { continue };
            let (wtx, wrx) = channel::<String>();
            std::thread::spawn(move || {
                for line in wrx {
                    if out.write_all(line.as_bytes()).and_then(|_| out.write_all(b"\n")).is_err() {
                        break;
                    }
                }
            });
            let _ = tx.send(Msg::Open { conn, tx: wtx });
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    let _ = tx.send(Msg::Line { conn, line });
                    SignalToUI::set_ui_signal();
                }
                let _ = tx.send(Msg::Closed { conn });
                SignalToUI::set_ui_signal();
            });
        }
    });
    rx
}

fn obj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn strings(v: Option<&JsonValue>) -> Option<Vec<String>> {
    Some(v?.as_array()?.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
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

/// Error locations count from the top of the surface's own body.
fn body_errors(errors: Vec<String>, name: &str) -> Vec<JsonValue> {
    let ahead = 3 + PRELUDE.lines().count() + BASE.lines().count();
    let tag = format!("splash:{name}:");
    errors
        .into_iter()
        .map(|e| {
            let Some(at) = e.find(&tag) else { return JsonValue::String(e) };
            let rest = &e[at + tag.len()..];
            let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
            match rest[..digits].parse::<usize>() {
                Ok(line) if line > ahead => JsonValue::String(format!("{}{}{}", &e[..at + tag.len()], line - ahead, &rest[digits..])),
                Ok(_) => JsonValue::String(format!("{e} (in the prelude or library)")),
                Err(_) => JsonValue::String(e),
            }
        })
        .collect()
}

impl App {
    fn shell(&self, cx: &Cx) -> WidgetRef {
        self.ui.widget(cx, ids!(shell))
    }

    fn drain_socket(&mut self, cx: &mut Cx) {
        let Some(rx) = self.st.rx.take() else { return };
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Open { conn, tx } => {
                    self.st.conns.insert(conn, Conn { tx, watch: None });
                }
                Msg::Closed { conn } => {
                    self.st.conns.remove(&conn);
                }
                Msg::Line { conn, line } => {
                    let reply = match JsonValue::deserialize_json(&line) {
                        Ok(req) => self.request(cx, conn, &req),
                        Err(e) => Err(format!("bad json: {e:?}")),
                    };
                    let reply = match reply {
                        Ok(mut v) => {
                            if let JsonValue::Object(m) = &mut v {
                                m.entry("ok".into()).or_insert(JsonValue::Bool(true));
                            }
                            v
                        }
                        Err(e) => obj(vec![("ok", JsonValue::Bool(false)), ("error", JsonValue::String(e))]),
                    };
                    if let Some(c) = self.st.conns.get(&conn) {
                        let _ = c.tx.send(reply.serialize_json());
                    }
                }
            }
        }
        self.st.rx = Some(rx);
    }

    fn request(&mut self, cx: &mut Cx, conn: u64, req: &JsonValue) -> Result<JsonValue, String> {
        let op = req.get("op").and_then(|v| v.as_str()).ok_or("missing op")?;
        let surface = req.get("surface").and_then(|v| v.as_str());
        match op {
            "show" => {
                let name = surface.ok_or("show needs surface")?;
                let body = req.get("body").and_then(|v| v.as_str()).ok_or("show needs body")?;
                let front = req.get("front") != Some(&JsonValue::Bool(false));
                self.show(cx, name, body, front)
            }
            "set" => {
                let patch = req.get("patch").ok_or("set needs patch")?;
                if !matches!(patch, JsonValue::Object(_)) {
                    return Err("patch must be an object".into());
                }
                if patch.get(SHELL_KEY).is_some() {
                    return Err(format!("the key {SHELL_KEY} is the shell's"));
                }
                self.apply(cx, patch, &format!("client:{conn}"));
                Ok(obj(vec![]))
            }
            "get" => {
                let keys = strings(req.get("keys"));
                let state = self.st.store.iter()
                    .filter(|(k, _)| keys.as_ref().map_or(true, |ks| ks.contains(k)))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Ok(obj(vec![("state", JsonValue::Object(state))]))
            }
            "watch" => {
                let watch = Watch { surfaces: strings(req.get("surfaces")), keys: strings(req.get("keys")) };
                self.st.conns.get_mut(&conn).ok_or("no connection")?.watch = Some(watch);
                Ok(obj(vec![]))
            }
            "close" => {
                let name = surface.ok_or("close needs surface")?;
                self.close(cx, name)
            }
            _ => Err(format!("unknown op {op}")),
        }
    }

    fn show(&mut self, cx: &mut Cx, name: &str, body: &str, front: bool) -> Result<JsonValue, String> {
        if name.is_empty() {
            return Err("empty surface name".into());
        }
        let started = std::time::Instant::now();
        let shell = self.shell(cx);
        let Some(mut sh) = shell.borrow_mut::<Shell>() else { return Err("no shell".into()) };
        let (splash, created) = match sh.surface(name) {
            Some(w) => (w, false),
            None => (sh.add(cx, name), true),
        };
        drop(sh);
        splash.set_text(cx, &format!("{PRELUDE}{BASE}{body}"));
        let errors = body_errors(splash.as_splash().last_errors(), name);
        if !errors.is_empty() {
            if created {
                shell.borrow_mut::<Shell>().map(|mut sh| sh.remove(cx, name));
            }
            // The previous body (if any) keeps drawing.
            let error = JsonValue::String("body did not evaluate".into());
            return Ok(obj(vec![("ok", JsonValue::Bool(false)), ("error", error), ("errors", JsonValue::Array(errors))]));
        }
        let reads = Reads::scan(body);
        self.st.reads.insert(name.to_string(), reads.clone());
        self.st.order.retain(|n| n != name);
        self.st.order.push(name.to_string());
        // `on_render` does not run on first show: feed it the keys it reads.
        let keys: Vec<String> = self.st.store.keys().filter(|k| reads.any(std::slice::from_ref(k))).cloned().collect();
        self.feed(cx, &splash, &keys);
        if front {
            self.apply(cx, &obj(vec![("front", JsonValue::String(name.into()))]), "shell");
        }
        cx.redraw_all();
        let reads = match reads {
            Reads::All => JsonValue::String("*".into()),
            Reads::Keys(k) => JsonValue::Array(k.into_iter().map(JsonValue::String).collect()),
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
        self.st.order.retain(|n| n != name);
        if self.st.store.get("front").and_then(|v| v.as_str()) == Some(name) {
            let back = self.st.order.last().map_or(JsonValue::Null, |n| JsonValue::String(n.clone()));
            self.apply(cx, &obj(vec![("front", back)]), "shell");
        }
        Ok(obj(vec![]))
    }

    /// Hands a surface the current values of `keys` and re-renders it.
    fn feed(&mut self, cx: &mut Cx, splash: &WidgetRef, keys: &[String]) {
        let vals = keys.iter()
            .map(|k| (k.clone(), self.st.store.get(k).cloned().unwrap_or(JsonValue::Null)))
            .collect();
        let text = JsonValue::Object(vals).serialize_json();
        // Warm the widget tree from here: while the Splash is borrowed below,
        // the tree cannot read its children, and `ui.main` would not resolve.
        let _ = splash.widget(cx, ids!(main));
        if let Some(mut inner) = splash.borrow_mut::<Splash>() {
            inner.call_script_fn_with_strings(cx, live_id!(__apply), &[&text]);
        }
    }

    /// The one write path: merge, re-render the readers, tell the watchers.
    fn apply(&mut self, cx: &mut Cx, patch: &JsonValue, by: &str) {
        let JsonValue::Object(top) = patch else { return };
        let touched: Vec<String> = top.keys().cloned().collect();
        for (k, v) in top {
            if *v == JsonValue::Null {
                self.st.store.remove(k);
            } else {
                merge(self.st.store.entry(k.clone()).or_insert(JsonValue::Null), v);
            }
        }
        let shell = self.shell(cx);
        if touched.iter().any(|k| k == "front") {
            let front = self.st.store.get("front").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if let Some(mut sh) = shell.borrow_mut::<Shell>() {
                sh.front = front;
            }
            cx.redraw_all();
        }
        let readers: Vec<(String, WidgetRef)> = shell.borrow::<Shell>().map_or(vec![], |sh| sh.surfaces.clone());
        for (name, splash) in readers {
            if self.st.reads.get(&name).map_or(false, |r| r.any(&touched)) {
                let keys: Vec<String> = match &self.st.reads[&name] {
                    Reads::All => touched.clone(),
                    Reads::Keys(ks) => touched.iter().filter(|k| ks.contains(k)).cloned().collect(),
                };
                self.feed(cx, &splash, &keys);
            }
        }
        let ev = obj(vec![("ev", "set".into()), ("patch", patch.clone()), ("by", by.into())]).serialize_json();
        for c in self.st.conns.values() {
            let Some(w) = &c.watch else { continue };
            let wants = match (&w.keys, &w.surfaces) {
                (Some(keys), _) => keys.iter().any(|k| touched.contains(k)),
                (None, Some(_)) => false,
                (None, None) => true,
            };
            if wants {
                let _ = c.tx.send(ev.clone());
            }
        }
    }

    fn emit(&mut self, surface: &str, ev: JsonValue) {
        let line = ev.serialize_json();
        for c in self.st.conns.values() {
            let Some(w) = &c.watch else { continue };
            let wants = match (&w.surfaces, &w.keys) {
                (Some(s), _) => s.iter().any(|s| s == surface),
                (None, Some(_)) => false,
                (None, None) => true,
            };
            if wants {
                let _ = c.tx.send(line.clone());
            }
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
                        if key == SHELL_KEY {
                            log!("tb-shell: surface {surface} may not write {SHELL_KEY}");
                            continue;
                        }
                        let value = args.get("value").cloned().unwrap_or(JsonValue::Null);
                        self.apply(cx, &obj(vec![(key, value)]), &format!("surface:{surface}"));
                    }
                    "emit" => {
                        let name = args.get("name").cloned().unwrap_or(JsonValue::Null);
                        let data = args.get("data").cloned().unwrap_or(JsonValue::Null);
                        let ev = obj(vec![("ev", "emit".into()), ("surface", surface.as_str().into()), ("name", name), ("data", data)]);
                        self.emit(&surface, ev);
                    }
                    other => log!("tb-shell: surface {surface} asked for {other}: refused"),
                }
            }
        }
    }
}
