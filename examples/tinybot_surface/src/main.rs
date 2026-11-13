//! tinybot's live surface (not for upstream): a full-screen app that shows what an agent
//! describes over a local Unix socket (text, toggles, rows of buttons) and keeps the user's
//! taps until the agent's next call collects them. No network: the only way in is
//! `$XDG_RUNTIME_DIR/tinybot/surface.sock`, same user only. The protocol and the `tb-surface`
//! command are described in the README next to this crate.
//!
//! The widgets are a fixed set compiled into the app; a description is data, checked as a
//! whole before the screen changes (`wire.rs`). The socket has its own thread (`socket.rs`).
//! At rest nothing runs: no timer, no frame callback.

pub use makepad_widgets;

use makepad_widgets::widget_tree::CxWidgetExt;
use makepad_widgets::*;
use std::collections::HashMap;

#[cfg(unix)]
mod socket;
mod wire;

/// What the surface settles before `main`. Makepad starts three things in every app that a
/// pager's home screen has no use for. Each starts when the `Cx` or its HAL session is made,
/// before any code of the app runs, and each has an environment switch, so the surface sets
/// its defaults here:
///
/// - `MAKEPAD_REMOTE=0`: no remote-control endpoint (a same-user Unix socket in the temp
///   directory and its thread); the surface's one way in is its own socket. Left unset when
///   the app is started with `--remote` (tests on the desktop).
/// - `MAKEPAD_GAMEPAD=0`: no game controller reader, whose worker scans /dev/input every
///   second.
/// - `NO_AT_BRIDGE=1`: no AT-SPI accessibility bridge, whose worker tries the session bus
///   again every five seconds when the accessibility bus does not answer.
///
/// A variable the environment already sets is never overwritten, so a launcher can turn any
/// of them back on. glibc calls every `.init_array` function with the process's argc, argv
/// and envp, in an executable and in a library a launcher loads alike.
///
/// It also limits glibc's malloc to one arena (`mallopt(M_ARENA_MAX, 1)`), while the process
/// still has one thread. glibc otherwise gives threads arenas of their own, 64 MB
/// reservations that each keep what their busiest moment needed: the app's thread, the
/// renderer's and the pool's each held 16 to 34 MB on the phone after the GL driver had
/// compiled the shaders (PSS 135 MB with the default, 89 MB with one arena, the m2's
/// measurement of bundle n). `TB_SURFACE_ARENA_MAX=N` chooses another limit (0: glibc's own
/// choice); a `GLIBC_TUNABLES` that sets `glibc.malloc.arena_max`, or `MALLOC_ARENA_MAX`,
/// is left to decide.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod before_main {
    use std::ffi::{c_char, c_int, CStr};

    extern "C" {
        fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int;
        fn getenv(name: *const c_char) -> *const c_char;
        fn mallopt(param: c_int, value: c_int) -> c_int;
    }

    /// glibc's malloc.h
    const M_ARENA_MAX: c_int = -8;
    /// one arena unless told otherwise: the least memory; the allocator's lock is then shared
    const ARENA_MAX: c_int = 1;

    fn env(name: &CStr) -> Option<&'static [u8]> {
        let value = unsafe { getenv(name.as_ptr()) };
        (!value.is_null()).then(|| unsafe { CStr::from_ptr(value) }.to_bytes())
    }

    /// The arena limit to set, or `None` when the environment has its own or asks for glibc's.
    fn arena_max() -> Option<c_int> {
        let tunable = b"glibc.malloc.arena_max";
        if env(c"GLIBC_TUNABLES").is_some_and(|v| v.windows(tunable.len()).any(|w| w == tunable))
            || env(c"MALLOC_ARENA_MAX").is_some()
        {
            return None;
        }
        let asked = env(c"TB_SURFACE_ARENA_MAX").and_then(|v| std::str::from_utf8(v).ok()?.trim().parse::<c_int>().ok());
        match asked {
            Some(0) => None,
            Some(n) if n > 0 => Some(n),
            _ => Some(ARENA_MAX),
        }
    }

    /// Sets `name` unless the environment already has it.
    fn default_env(name: &CStr, value: &CStr) {
        unsafe { setenv(name.as_ptr(), value.as_ptr(), 0) };
    }

    fn started_with_remote(argc: c_int, argv: *const *const c_char) -> bool {
        if argv.is_null() {
            return false;
        }
        for i in 1..argc as isize {
            let arg = unsafe { *argv.offset(i) };
            if arg.is_null() {
                break;
            }
            let arg = unsafe { CStr::from_ptr(arg) }.to_bytes();
            if arg == b"--remote" || arg.starts_with(b"--remote=") {
                return true;
            }
        }
        false
    }

    extern "C" fn before_main(argc: c_int, argv: *const *const c_char, _envp: *const *const c_char) {
        if let Some(n) = arena_max() {
            unsafe { mallopt(M_ARENA_MAX, n) };
        }
        default_env(c"MAKEPAD_GAMEPAD", c"0");
        default_env(c"NO_AT_BRIDGE", c"1");
        if !started_with_remote(argc, argv) {
            default_env(c"MAKEPAD_REMOTE", c"0");
        }
    }

    #[used]
    #[link_section = ".init_array"]
    static BEFORE_MAIN: extern "C" fn(c_int, *const *const c_char, *const *const c_char) = before_main;
}

app_main!(App);

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    // A row of buttons made from data: one of the three templates per button, `per_line`
    // to a line, further buttons on further lines.
    mod.widgets.ButtonRowBase = #(ButtonRow::register_widget(vm))
    mod.widgets.ButtonRow = set_type_default() do mod.widgets.ButtonRowBase{
        width: Fill
        height: Fit
        flow: Right
        spacing: 8
        per_line: 3
        Primary := ButtonPrimary{
            width: Fill
            height: 46
            text: ""
        }
        Secondary := Button{
            width: Fill
            height: 46
            text: ""
        }
        Danger := ButtonDanger{
            width: Fill
            height: 46
            text: ""
        }
    }

    mod.widgets.SurfaceListBase = #(SurfaceList::register_widget(vm))
    mod.widgets.SurfaceList = set_type_default() do mod.widgets.SurfaceListBase{
        width: Fill
        height: Fill
        flow: Down

        list := PortalList{
            width: Fill
            height: Fill

            Heading := View{
                width: Fill
                height: Fit
                padding: Inset{left: 16 right: 16 top: 14 bottom: 4}
                label := Label{
                    width: Fill
                    text: ""
                    draw_text.color: #fff
                    draw_text.text_style.font_size: 15
                }
            }
            Text := View{
                width: Fill
                height: Fit
                padding: Inset{left: 16 right: 16 top: 6 bottom: 6}
                label := Label{
                    width: Fill
                    text: ""
                    draw_text.color: #ccc
                    draw_text.text_style.font_size: 11
                }
            }
            Toggle := View{
                width: Fill
                height: Fit
                padding: Inset{left: 16 right: 16 top: 8 bottom: 8}
                toggle := Toggle{
                    width: Fill
                    text: ""
                    draw_text.text_style.font_size: 11
                }
            }
            Row := View{
                width: Fill
                height: Fit
                flow: Down
                spacing: 6
                padding: Inset{left: 16 right: 16 top: 8 bottom: 8}
                buttons := mod.widgets.ButtonRow{}
                working := Label{
                    visible: false
                    width: Fill
                    text: ""
                    draw_text.color: #8a8
                    draw_text.text_style.font_size: 10
                }
            }
            Divider := View{
                width: Fill
                height: Fit
                padding: Inset{left: 16 right: 16 top: 8 bottom: 8}
                SolidView{
                    width: Fill
                    height: 1
                    draw_bg.color: #666
                }
            }
        }
    }

    startup() do #(App::script_component(vm)){
        ui: Root{
            main_window := Window{
                window.inner_size: vec2(360, 720)
                window.title: "tinybot"
                body +: {
                    // the system bars sit over the window's edges: stay inside the safe area
                    padding: Inset{top: mod.widgets.SAFE_INSET_PAD_TOP right: mod.widgets.SAFE_INSET_PAD_RIGHT bottom: mod.widgets.SAFE_INSET_PAD_BOTTOM left: mod.widgets.SAFE_INSET_PAD_LEFT}
                    flow: Down
                    // nothing to show: tinybot's home, one line
                    idle := View{
                        width: Fill
                        height: Fill
                        align: Align{x: 0.5 y: 0.5}
                        idle_line := Label{
                            text: "tinybot · idle"
                            draw_text.color: #888
                            draw_text.text_style.font_size: 12
                        }
                    }
                    surface := View{
                        visible: false
                        width: Fill
                        height: Fill
                        flow: Down
                        header := SolidView{
                            width: Fill
                            height: 48
                            padding: Inset{left: 16 right: 16}
                            align: Align{y: 0.5}
                            draw_bg.color: #202428
                            title := Label{
                                width: Fill
                                text: "tinybot"
                                draw_text.color: #fff
                                draw_text.text_style.font_size: 14
                            }
                        }
                        items := mod.widgets.SurfaceList{}
                    }
                }
            }
        }
    }
}

/// One button the row holds: the widget and the template it was made from.
struct RowButton {
    widget: WidgetRef,
    template: LiveId,
}

/// The buttons of one surface row. They are made on demand from the `Primary`, `Secondary`
/// and `Danger` templates and kept by (position, style), so a row widget the list hands to
/// another row later reuses them.
#[derive(Script, Widget)]
pub struct ButtonRow {
    #[uid]
    uid: WidgetUid,
    #[source]
    source: ScriptObjectRef,
    #[walk]
    walk: Walk,
    #[layout]
    layout: Layout,
    /// buttons on one line; a longer row continues on the next line
    #[live(3)]
    per_line: usize,
    #[redraw]
    #[rust]
    area: Area,
    #[rust]
    templates: HashMap<LiveId, ScriptObjectRef>,
    #[rust]
    made: HashMap<LiveId, RowButton>,
    /// the buttons of the row shown now, in order
    #[rust]
    shown: Vec<LiveId>,
}

impl ScriptHook for ButtonRow {
    fn on_before_apply(&mut self, _vm: &mut ScriptVm, apply: &Apply, _scope: &mut Scope, _value: ScriptValue) {
        if apply.is_reload() {
            self.templates.clear();
        }
    }

    fn on_after_apply(&mut self, vm: &mut ScriptVm, apply: &Apply, scope: &mut Scope, value: ScriptValue) {
        // the named items of the object are the templates (as in FlatList and PortalList)
        if !apply.is_eval() {
            if let Some(obj) = value.as_object() {
                vm.vec_with(obj, |vm, vec| {
                    for kv in vec {
                        if let Some(id) = kv.key.as_id() {
                            if let Some(template) = kv.value.as_object() {
                                self.templates.insert(id, vm.new_object_ref(template));
                            }
                        }
                    }
                });
            }
        }
        if apply.is_reload() {
            for button in self.made.values_mut() {
                if let Some(template) = self.templates.get(&button.template) {
                    let template: ScriptValue = template.as_object().into();
                    button.widget.script_apply(vm, apply, scope, template);
                }
            }
        }
    }
}

impl ButtonRow {
    /// Makes this the row of `buttons`, armed or locked. Called while the list draws.
    pub fn set_buttons(&mut self, cx: &mut Cx, buttons: &[wire::Button], armed: bool) {
        self.shown.clear();
        for (index, button) in buttons.iter().enumerate() {
            let (template, style) = match button.style {
                wire::Style::Primary => (id!(Primary), 0),
                wire::Style::Secondary => (id!(Secondary), 1),
                wire::Style::Danger => (id!(Danger), 2),
            };
            let key = LiveId(((index as u64) << 2) | style);
            if !self.made.contains_key(&key) {
                let Some(source) = self.templates.get(&template) else {
                    continue;
                };
                let source: ScriptValue = source.as_object().into();
                let widget = cx.with_vm(|vm| WidgetRef::script_from_value(vm, source));
                cx.widget_tree_insert_child(self.uid, key, widget.clone());
                self.made.insert(key, RowButton { widget, template });
            }
            let widget = &self.made[&key].widget;
            widget.set_text(cx, &button.text);
            // a locked button takes no press (enabled) and fades to its disabled look
            widget.as_button().set_enabled(cx, armed);
            if widget.disabled(cx) == armed {
                widget.set_disabled(cx, !armed);
            }
            self.shown.push(key);
        }
    }

    /// The position of the button that was clicked, if one of this row was.
    pub fn clicked(&self, actions: &Actions) -> Option<usize> {
        self.shown.iter().position(|key| self.made.get(key).is_some_and(|b| b.widget.as_button().clicked(actions)))
    }
}

impl Widget for ButtonRow {
    fn semantics(&self, cx: &Cx, node: &mut a11y_tree::Node) {
        let _ = cx;
        node.role = a11y_tree::Role::Generic;
    }

    fn visit_cancel(&self, visit: &mut dyn FnMut(LiveId, WidgetRef)) -> bool {
        for key in &self.shown {
            if let Some(button) = self.made.get(key) {
                visit(*key, button.widget.clone());
            }
        }
        true
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        // only the buttons on screen: one kept for another row has no place to be hit
        for key in &self.shown {
            if let Some(button) = self.made.get(key) {
                button.widget.handle_event(cx, event, scope);
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let lines = Layout { flow: Flow::Down, spacing: self.layout.spacing, ..Default::default() };
        cx.begin_turtle(walk, lines);
        for line in self.shown.chunks(self.per_line.max(1)) {
            cx.begin_turtle(Walk::fill_fit(), self.layout);
            // the buttons of a line share its width: the Fill ones are sized once the
            // others have taken their part (as View does for its children)
            let mut fills = Vec::new();
            for key in line {
                if let Some(button) = self.made.get(key) {
                    let walk = button.widget.walk(cx);
                    match cx.defer_walk_turtle(walk) {
                        Some(fill) => fills.push((button, fill)),
                        None => button.widget.draw_walk_all(cx, scope, walk),
                    }
                }
            }
            for (button, mut fill) in fills {
                let walk = fill.resolve(cx);
                button.widget.draw_walk_all(cx, scope, walk);
            }
            cx.end_turtle();
        }
        cx.end_turtle_with_area(&mut self.area);
        DrawStep::done()
    }
}

/// One line of the list.
enum Entry {
    Heading(String),
    Text(String),
    Toggle { id: String, text: String },
    /// `tapped`: the button that was pressed. The row is locked from then until the agent
    /// sends the next surface or clears.
    Row { buttons: Vec<wire::Button>, tapped: Option<usize> },
    Divider,
}

/// A button press on its way to the agent.
pub struct Pressed {
    pub surface: String,
    pub rev: u64,
    pub id: String,
    pub values: Vec<(String, bool)>,
}

/// The surface on screen: the list and the state the list's recycled rows are drawn from.
#[derive(Script, ScriptHook, Widget)]
pub struct SurfaceList {
    #[deref]
    view: View,
    #[rust]
    surface: String,
    #[rust]
    rev: u64,
    #[rust]
    entries: Vec<Entry>,
    /// the toggles' values by id
    #[rust]
    values: HashMap<String, bool>,
}

impl SurfaceList {
    /// Replaces what is shown. The toggles keep what the user set when the surface id is the
    /// same and the description gives no value; every row is armed again.
    pub fn set_spec(&mut self, cx: &mut Cx, spec: wire::Spec) {
        let same = spec.surface == self.surface;
        let mut values = HashMap::new();
        self.entries.clear();
        for item in spec.items {
            self.entries.push(match item {
                wire::Item::Heading(text) => Entry::Heading(text),
                wire::Item::Text(text) => Entry::Text(text),
                wire::Item::Divider => Entry::Divider,
                wire::Item::Row(buttons) => Entry::Row { buttons, tapped: None },
                wire::Item::Toggle { id, text, value } => {
                    let kept = if same { self.values.get(&id).copied() } else { None };
                    values.insert(id.clone(), value.or(kept).unwrap_or(false));
                    Entry::Toggle { id, text }
                }
            });
        }
        self.values = values;
        self.surface = spec.surface;
        self.rev = spec.rev;
        if !same {
            self.view.portal_list(cx, ids!(list)).set_first_id_and_scroll(0, 0.0);
        }
        self.view.redraw(cx);
    }

    pub fn clear(&mut self, cx: &mut Cx) {
        self.entries.clear();
        self.values.clear();
        self.surface.clear();
        self.rev = 0;
        self.view.redraw(cx);
    }

    /// Takes the toggles' changes and the button presses out of `actions`. A pressed row
    /// locks; what comes back goes to the agent.
    pub fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions) -> Vec<Pressed> {
        let list = self.view.portal_list(cx, ids!(list));
        let mut ids = Vec::new();
        for (index, item) in list.items_with_actions(actions) {
            match self.entries.get_mut(index) {
                Some(Entry::Toggle { id, .. }) => {
                    if let Some(value) = item.check_box(cx, ids!(toggle)).changed(actions) {
                        self.values.insert(id.clone(), value);
                    }
                }
                Some(Entry::Row { buttons, tapped }) if tapped.is_none() => {
                    let hit = item.widget(cx, ids!(buttons)).borrow::<ButtonRow>().and_then(|row| row.clicked(actions));
                    if let Some(button) = hit.and_then(|k| buttons.get(k).map(|b| (k, b))) {
                        *tapped = Some(button.0);
                        ids.push(button.1.id.clone());
                    }
                }
                _ => {}
            }
        }
        if ids.is_empty() {
            return Vec::new();
        }
        self.view.redraw(cx);
        // the toggles as they are now, in the order they are shown
        let values: Vec<(String, bool)> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                Entry::Toggle { id, .. } => Some((id.clone(), self.values.get(id).copied().unwrap_or(false))),
                _ => None,
            })
            .collect();
        ids.into_iter()
            .map(|id| Pressed { surface: self.surface.clone(), rev: self.rev, id, values: values.clone() })
            .collect()
    }
}

impl Widget for SurfaceList {
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        while let Some(item) = self.view.draw_walk(cx, scope, walk).step() {
            if let Some(mut list) = item.borrow_mut::<PortalList>() {
                list.set_item_range(cx, 0, self.entries.len());
                while let Some(index) = list.next_visible_item(cx) {
                    let Some(entry) = self.entries.get(index) else {
                        continue;
                    };
                    let row = match entry {
                        Entry::Heading(text) => {
                            let row = list.item(cx, index, id!(Heading));
                            row.label(cx, ids!(label)).set_text(cx, text);
                            row
                        }
                        Entry::Text(text) => {
                            let row = list.item(cx, index, id!(Text));
                            row.label(cx, ids!(label)).set_text(cx, text);
                            row
                        }
                        Entry::Divider => list.item(cx, index, id!(Divider)),
                        Entry::Toggle { id, text } => {
                            let row = list.item(cx, index, id!(Toggle));
                            let toggle = row.check_box(cx, ids!(toggle));
                            toggle.set_text(text);
                            let value = self.values.get(id).copied().unwrap_or(false);
                            if toggle.active(cx) != value {
                                toggle.set_active(cx, value, Animate::No);
                            }
                            row
                        }
                        Entry::Row { buttons, tapped } => {
                            let row = list.item(cx, index, id!(Row));
                            if let Some(mut line) = row.widget(cx, ids!(buttons)).borrow_mut::<ButtonRow>() {
                                line.set_buttons(cx, buttons, tapped.is_none());
                            }
                            let working = row.label(cx, ids!(working));
                            match tapped.and_then(|k| buttons.get(k)) {
                                Some(button) => {
                                    working.set_text(cx, &format!("{} · working…", button.text));
                                    working.set_visible(cx, true);
                                }
                                None => working.set_visible(cx, false),
                            }
                            row
                        }
                    };
                    row.draw_all(cx, &mut Scope::empty());
                }
            }
        }
        DrawStep::done()
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }
}

#[derive(Script, ScriptHook)]
pub struct App {
    #[live]
    ui: WidgetRef,
    #[cfg(unix)]
    #[rust]
    link: Option<socket::Link>,
    /// one shot, a moment after startup and after each new surface: see `release_free_memory`
    #[rust]
    settle: Timer,
    /// the release after startup runs a second time, for a device that compiles slowly
    #[rust]
    settle_again: bool,
}

/// Gives the memory the allocator holds but no longer uses back to the system. Starting up
/// and showing a kind of widget for the first time compile scripts and shaders; what that
/// needed for a moment is freed, but glibc keeps freed memory inside its arenas, and the
/// process stays as large as its busiest moment. Called once things have settled.
/// On linux-node with the GLES backend and no driver shader cache this returns about 30 MB of
/// 310 MB after startup and takes 1.4 ms; `TINYBOT_SURFACE_LOG=1` prints the time it took.
fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        let start = std::time::Instant::now();
        unsafe { malloc_trim(0) };
        if std::env::var_os("TINYBOT_SURFACE_LOG").is_some() {
            eprintln!("[tinybot-surface] malloc_trim took {:.1} ms", start.elapsed().as_secs_f64() * 1e3);
        }
    }
}

/// How long after startup or a new surface the memory is released: past the first frames and
/// the shader compiles they start.
const SETTLE_SECONDS: f64 = 3.0;
/// The second release after startup.
const SETTLE_AGAIN_SECONDS: f64 = 10.0;

impl App {
    #[cfg(unix)]
    fn start_socket(&mut self) {
        let (path, default_place) = socket::socket_path();
        match socket::start(path.clone(), default_place) {
            Ok(link) => {
                eprintln!("[tinybot-surface] socket={}", link.path.display());
                self.link = Some(link);
            }
            Err(e) => {
                // a second surface would only fight the first one for the screen
                eprintln!("[tinybot-surface] cannot serve {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }

    #[cfg(not(unix))]
    fn start_socket(&mut self) {
        eprintln!("[tinybot-surface] no Unix sockets on this platform: the surface stays idle");
    }

    /// Shows either the surface or the idle line.
    fn show(&mut self, cx: &mut Cx, surface: bool) {
        self.ui.view(cx, ids!(surface)).set_visible(cx, surface);
        self.ui.view(cx, ids!(idle)).set_visible(cx, !surface);
        self.ui.redraw(cx);
    }
}

impl MatchEvent for App {
    fn handle_startup(&mut self, cx: &mut Cx) {
        self.start_socket();
        self.settle = cx.start_timeout(SETTLE_SECONDS);
        self.settle_again = true;
    }

    #[cfg(unix)]
    fn handle_signal(&mut self, cx: &mut Cx) {
        // only the newest message matters to the screen, but each is applied in order: the
        // toggles a surface keeps depend on the one before it
        loop {
            let message = match &self.link {
                Some(link) => link.from_io.try_recv(),
                None => return,
            };
            let Ok(message) = message else {
                return;
            };
            cx.stop_timer(self.settle);
            self.settle = cx.start_timeout(SETTLE_SECONDS);
            let items = self.ui.widget(cx, ids!(items));
            match message {
                socket::ToUi::Set(spec) => {
                    let title = if spec.title.is_empty() { "tinybot" } else { spec.title.as_str() };
                    self.ui.label(cx, ids!(title)).set_text(cx, title);
                    if let Some(mut list) = items.borrow_mut::<SurfaceList>() {
                        list.set_spec(cx, spec);
                    }
                    self.show(cx, true);
                }
                socket::ToUi::Clear => {
                    if let Some(mut list) = items.borrow_mut::<SurfaceList>() {
                        list.clear(cx);
                    }
                    self.show(cx, false);
                }
            }
        }
    }

    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions) {
        let items = self.ui.widget(cx, ids!(items));
        let pressed = items.borrow_mut::<SurfaceList>().map(|mut list| list.handle_actions(cx, actions)).unwrap_or_default();
        #[cfg(unix)]
        if let Some(link) = &self.link {
            for p in pressed {
                link.send(socket::ToIo::Tap { surface: p.surface, rev: p.rev, id: p.id, values: p.values });
            }
        }
        #[cfg(not(unix))]
        let _ = pressed;
    }

    fn handle_shutdown(&mut self, _cx: &mut Cx) {
        #[cfg(unix)]
        if let Some(link) = self.link.take() {
            socket::remove(&link.path);
        }
    }
}

impl AppMain for App {
    fn script_mod(vm: &mut ScriptVm) -> ScriptValue {
        crate::makepad_widgets::script_mod(vm);
        self::script_mod(vm)
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        if self.settle.is_event(event).is_some() {
            release_free_memory();
            if std::mem::take(&mut self.settle_again) {
                self.settle = cx.start_timeout(SETTLE_AGAIN_SECONDS);
            }
        }
        self.match_event(cx, event);
        self.ui.handle_event(cx, event, &mut Scope::empty());
    }
}
