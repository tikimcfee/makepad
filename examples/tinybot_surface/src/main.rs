//! tinybot's live surface (not for upstream): a full-screen app that shows what an agent
//! describes over a local Unix socket (text, toggles, steppers, text fields, readouts, rows
//! of buttons) and keeps the user's taps until the agent's next call collects them. No network: the only way in is
//! `$XDG_RUNTIME_DIR/tinybot/surface.sock`, same user only. The protocol and the `tb-surface`
//! command are described in the README next to this crate.
//!
//! The widgets are a fixed set compiled into the app; a description is data, checked as a
//! whole before the screen changes (`wire.rs`). The socket has its own thread (`socket.rs`).
//! At rest nothing runs: no timer, no frame callback. The two timers there are run only
//! while the user holds a stepper's button or a `confirm` button is armed, and two more only
//! while the voice strip shows a text.

pub use makepad_widgets;

use makepad_widgets::makepad_platform::event;
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
            // one of a fixed list of strings: ‹ value ›
            Stepper := View{
                width: Fill
                height: Fit
                flow: Right
                spacing: 8
                align: Align{y: 0.5}
                padding: Inset{left: 16 right: 16 top: 6 bottom: 6}
                label := Label{
                    width: Fill
                    text: ""
                    draw_text.color: #ccc
                    draw_text.text_style.font_size: 11
                }
                less := Button{
                    width: 46
                    height: 42
                    text: "‹"
                    draw_text.text_style.font_size: 16
                }
                value := Label{
                    width: 104
                    align: Align{x: 0.5}
                    text: ""
                    draw_text.color: #fff
                    draw_text.text_style.font_size: 11
                }
                more := Button{
                    width: 46
                    height: 42
                    text: "›"
                    draw_text.text_style.font_size: 16
                }
            }
            // a text field; what is typed is sent as it is
            Number := View{
                width: Fill
                height: Fit
                flow: Right
                spacing: 12
                align: Align{y: 0.5}
                padding: Inset{left: 16 right: 16 top: 6 bottom: 6}
                label := Label{
                    width: Fill
                    text: ""
                    draw_text.color: #ccc
                    draw_text.text_style.font_size: 11
                }
                input := TextInput{
                    width: 150
                    height: Fit
                    empty_text: ""
                    draw_text.text_style.font_size: 11
                }
            }
            // a label and a value the agent changes in place
            Readout := View{
                width: Fill
                height: Fit
                flow: Right
                spacing: 12
                align: Align{y: 0.5}
                padding: Inset{left: 16 right: 16 top: 6 bottom: 6}
                label := Label{
                    width: Fit
                    text: ""
                    draw_text.color: #ccc
                    draw_text.text_style.font_size: 11
                }
                value := Label{
                    width: Fill
                    text: ""
                    draw_text.color: #fff
                    draw_text +: {
                        text_style: theme.font_code{font_size: 11}
                    }
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
                    flow: Overlay
                    // the pages, and under them the voice strip while there is one: it takes
                    // its room from the page above, so it never lies over anything
                    column := View{
                        width: Fill
                        height: Fill
                        flow: Down
                        pages := View{
                            width: Fill
                            height: Fill
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
                            // root's layer: the panel runner's pages
                            surface := View{
                                visible: false
                                width: Fill
                                height: Fill
                                flow: Down
                                header := SolidView{
                                    width: Fill
                                    height: 48
                                    // the right end is the dots' place
                                    padding: Inset{left: 16 right: 126}
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
                            // the agent's layer. The band is the app's own: a page cannot
                            // change it or leave it out, so the user sees whose page this is.
                            agent := View{
                                visible: false
                                width: Fill
                                height: Fill
                                flow: Down
                                band := SolidView{
                                    width: Fill
                                    height: 44
                                    flow: Right
                                    spacing: 8
                                    // the right end is the dots' place
                                    padding: Inset{left: 16 right: 126}
                                    align: Align{y: 0.5}
                                    draw_bg.color: #6a3d00
                                    who := Label{
                                        width: Fill
                                        text: "agent"
                                        draw_text.color: #fff
                                        draw_text.text_style.font_size: 10
                                    }
                                    agent_home := Button{
                                        visible: false
                                        width: Fit
                                        height: 34
                                        text: "‹ Home"
                                    }
                                }
                                agent_header := SolidView{
                                    width: Fill
                                    height: 40
                                    padding: Inset{left: 16 right: 16}
                                    align: Align{y: 0.5}
                                    draw_bg.color: #202428
                                    agent_title := Label{
                                        width: Fill
                                        text: ""
                                        draw_text.color: #fff
                                        draw_text.text_style.font_size: 13
                                    }
                                }
                                agent_items := mod.widgets.SurfaceList{}
                            }
                        }
                        // The voice strip: what the voice service hears, as it comes. The band
                        // and its tag are the app's own; the text is two lines, the newest
                        // words. It takes no touch. (The sizes here are VOICE_* in the code.)
                        voice := SolidView{
                            visible: false
                            width: Fill
                            height: 58
                            flow: Right
                            spacing: 8
                            padding: Inset{left: 12 right: 12 top: 9 bottom: 9}
                            draw_bg.color: #16324a
                            voice_tag := Label{
                                width: 40
                                padding: Inset{top: 0 right: 0 bottom: 0 left: 0}
                                text: "voice"
                                draw_text.color: #8fc1e8
                                draw_text.text_style.font_size: 9
                            }
                            View{
                                width: Fill
                                height: Fit
                                flow: Down
                                spacing: 2
                                voice_1 := Label{
                                    width: Fill
                                    padding: Inset{top: 0 right: 0 bottom: 0 left: 0}
                                    max_lines: 1
                                    text: ""
                                    draw_text.color: #fff
                                    draw_text.text_style.font_size: 12
                                }
                                voice_2 := Label{
                                    width: Fill
                                    padding: Inset{top: 0 right: 0 bottom: 0 left: 0}
                                    max_lines: 1
                                    text: ""
                                    draw_text.color: #fff
                                    draw_text.text_style.font_size: 12
                                }
                            }
                        }
                    }
                    // over every page of either layer: lit while the microphone or the
                    // speaker stream runs. No page can cover or draw them.
                    dots := View{
                        width: Fill
                        height: 44
                        flow: Right
                        spacing: 10
                        align: Align{x: 1.0 y: 0.5}
                        padding: Inset{right: 14}
                        mic := Label{
                            text: ""
                            draw_text.color: #ff5a4a
                            draw_text.text_style.font_size: 10
                        }
                        play := Label{
                            text: ""
                            draw_text.color: #5ad06a
                            draw_text.text_style.font_size: 10
                        }
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
    /// Makes this the row of `buttons`, armed or locked. `confirming` is the `confirm` button
    /// that waits for its second tap: it is drawn as a danger button that says so. Called
    /// while the list draws.
    pub fn set_buttons(&mut self, cx: &mut Cx, buttons: &[wire::Button], armed: bool, confirming: Option<usize>) {
        self.shown.clear();
        for (index, button) in buttons.iter().enumerate() {
            let confirming = confirming == Some(index);
            let (template, style) = match button.style {
                _ if confirming => (id!(Danger), 2),
                wire::Style::Primary => (id!(Primary), 0),
                wire::Style::Secondary => (id!(Secondary), 1),
                wire::Style::Danger => (id!(Danger), 2),
            };
            // a button that does not lock its row is not locked with it either
            let armed = armed || !button.lock;
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
            if confirming {
                widget.set_text(cx, &format!("tap again: {}", button.text));
            } else {
                widget.set_text(cx, &button.text);
            }
            // a locked button takes no press (enabled) and fades to its disabled look; a
            // `dim` one fades and still takes presses
            widget.as_button().set_enabled(cx, armed);
            let faded = !armed || (button.dim && !confirming);
            if widget.disabled(cx) != faded {
                widget.set_disabled(cx, faded);
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
    Heading { id: String, text: String },
    Text { id: String, text: String },
    Toggle { id: String, text: String },
    Stepper { id: String, text: String, values: Vec<String> },
    Number { id: String, text: String },
    Readout { id: String, text: String, value: String },
    /// `tapped`: the button that was pressed. The row is locked from then until the agent
    /// sends the next surface, arms the row with an `update`, or clears.
    Row { id: String, buttons: Vec<wire::Button>, tapped: Option<usize> },
    Divider,
}

/// What the user set in a toggle, a stepper (a position in its values) or a number.
enum Held {
    Bool(bool),
    Step(usize),
    Text(String),
}

/// A button press on its way to the agent.
pub struct Pressed {
    pub surface: String,
    pub rev: u64,
    pub id: String,
    pub values: Vec<(String, wire::Value)>,
}

/// A press on a stepper's button that has not ended yet.
struct HeldStep {
    entry: usize,
    /// -1 or 1
    by: isize,
    /// the steps the hold has made so far
    repeats: usize,
}

/// A stepper's button held down: the first repeat comes after this long,
const REPEAT_DELAY: f64 = 0.45;
/// and the following ones this far apart.
const REPEAT_EVERY: f64 = 0.1;

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
    /// what the user set, by item id
    #[rust]
    values: HashMap<String, Held>,
    /// the `confirm` button that waits for its second tap: (entry, button)
    #[rust]
    confirming: Option<(usize, usize)>,
    /// one shot: the armed `confirm` button gives up
    #[rust]
    confirm_timer: Timer,
    /// the stepper button the pointer is down on
    #[rust]
    held: Option<HeldStep>,
    /// one shot, started again after each repeat while the button is held
    #[rust]
    repeat_timer: Timer,
}

impl SurfaceList {
    /// Replaces what is shown. A toggle, stepper or number keeps what the user set when the
    /// surface id is the same and the description gives no value; every row is armed again.
    pub fn set_spec(&mut self, cx: &mut Cx, spec: wire::Spec) {
        let same = spec.surface == self.surface;
        let kept: HashMap<String, wire::Value> = if same { self.tap_values().into_iter().collect() } else { HashMap::new() };
        self.disarm(cx);
        self.stop_repeat(cx);
        self.values.clear();
        self.entries.clear();
        for item in spec.items {
            self.entries.push(match item {
                wire::Item::Heading { id, text } => Entry::Heading { id, text },
                wire::Item::Text { id, text } => Entry::Text { id, text },
                wire::Item::Divider => Entry::Divider,
                wire::Item::Row { id, buttons } => Entry::Row { id, buttons, tapped: None },
                wire::Item::Readout { id, text, value } => Entry::Readout { id, text, value },
                wire::Item::Toggle { id, text, value } => {
                    let kept = match kept.get(&id) {
                        Some(wire::Value::Bool(on)) => Some(*on),
                        _ => None,
                    };
                    self.values.insert(id.clone(), Held::Bool(value.or(kept).unwrap_or(false)));
                    Entry::Toggle { id, text }
                }
                wire::Item::Stepper { id, text, values, value } => {
                    // the value the user chose, if the new list still has it
                    let kept = match kept.get(&id) {
                        Some(wire::Value::Text(was)) => values.iter().position(|v| v == was),
                        _ => None,
                    };
                    self.values.insert(id.clone(), Held::Step(value.or(kept).unwrap_or(0)));
                    Entry::Stepper { id, text, values }
                }
                wire::Item::Number { id, text, value } => {
                    let kept = match kept.get(&id) {
                        Some(wire::Value::Text(was)) => Some(was.clone()),
                        _ => None,
                    };
                    self.values.insert(id.clone(), Held::Text(value.or(kept).unwrap_or_default()));
                    Entry::Number { id, text }
                }
            });
        }
        self.surface = spec.surface;
        self.rev = spec.rev;
        if !same {
            self.view.portal_list(cx, ids!(list)).set_first_id_and_scroll(0, 0.0);
        }
        self.view.redraw(cx);
    }

    /// Changes labels and readouts in place and arms rows again. What the user set, the armed
    /// `confirm` button and the scroll position stay. The socket thread has checked the ids.
    pub fn update(&mut self, cx: &mut Cx, update: wire::Update) {
        for change in update.changes {
            for entry in &mut self.entries {
                match entry {
                    Entry::Heading { id, text } | Entry::Text { id, text } if *id == change.id => {
                        if let Some(new) = &change.text {
                            text.clone_from(new);
                        }
                    }
                    Entry::Readout { id, text, value } if *id == change.id => {
                        if let Some(new) = &change.text {
                            text.clone_from(new);
                        }
                        if let Some(new) = &change.value {
                            value.clone_from(new);
                        }
                    }
                    _ => {}
                }
            }
        }
        for name in update.arm {
            for entry in &mut self.entries {
                if let Entry::Row { id, buttons, tapped } = entry {
                    if *id == name || buttons.iter().any(|b| b.id == name) {
                        *tapped = None;
                    }
                }
            }
        }
        self.view.redraw(cx);
    }

    pub fn clear(&mut self, cx: &mut Cx) {
        self.disarm(cx);
        self.stop_repeat(cx);
        self.entries.clear();
        self.values.clear();
        self.surface.clear();
        self.rev = 0;
        self.view.redraw(cx);
    }

    /// The toggles, steppers and numbers as they are now, in the order they are shown.
    fn tap_values(&self) -> Vec<(String, wire::Value)> {
        let mut out = Vec::new();
        for entry in &self.entries {
            let value = match entry {
                Entry::Toggle { id, .. } => match self.values.get(id) {
                    Some(Held::Bool(on)) => (id, wire::Value::Bool(*on)),
                    _ => continue,
                },
                Entry::Stepper { id, values, .. } => match self.values.get(id) {
                    Some(Held::Step(at)) if *at < values.len() => (id, wire::Value::Text(values[*at].clone())),
                    _ => continue,
                },
                Entry::Number { id, .. } => match self.values.get(id) {
                    Some(Held::Text(text)) => (id, wire::Value::Text(text.clone())),
                    _ => continue,
                },
                _ => continue,
            };
            out.push((value.0.clone(), value.1));
        }
        out
    }

    /// The armed `confirm` button, if any, is an ordinary button again.
    fn disarm(&mut self, cx: &mut Cx) {
        if self.confirming.take().is_some() {
            cx.stop_timer(self.confirm_timer);
            self.view.redraw(cx);
        }
    }

    /// Ends a hold on a stepper's button. `true` when the hold has stepped already.
    fn stop_repeat(&mut self, cx: &mut Cx) -> bool {
        cx.stop_timer(self.repeat_timer);
        self.held.take().is_some_and(|held| held.repeats > 0)
    }

    /// Moves a stepper one value down or up. `false` at the end of its values: a stepper
    /// does not wrap around, so the lowest value is never one press from the highest.
    fn step(&mut self, cx: &mut Cx, entry: usize, by: isize) -> bool {
        let Some(Entry::Stepper { id, values, .. }) = self.entries.get(entry) else {
            return false;
        };
        let at = match self.values.get(id) {
            Some(Held::Step(at)) => *at,
            _ => 0,
        };
        let next = at.saturating_add_signed(by).min(values.len().saturating_sub(1));
        if next == at {
            return false;
        }
        self.values.insert(id.clone(), Held::Step(next));
        self.view.redraw(cx);
        true
    }

    /// Takes what the user did out of `actions`: toggles, typing, stepper presses and button
    /// presses. A pressed row locks; what comes back goes to the agent.
    pub fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions) -> Vec<Pressed> {
        let list = self.view.portal_list(cx, ids!(list));
        let mut clicked = Vec::new();
        for (index, item) in list.items_with_actions(actions) {
            match self.entries.get(index) {
                Some(Entry::Toggle { id, .. }) => {
                    if let Some(value) = item.check_box(cx, ids!(toggle)).changed(actions) {
                        self.values.insert(id.clone(), Held::Bool(value));
                    }
                }
                Some(Entry::Number { id, .. }) => {
                    if let Some(text) = item.text_input(cx, ids!(input)).changed(actions) {
                        self.values.insert(id.clone(), Held::Text(text));
                    }
                }
                Some(Entry::Stepper { .. }) => {
                    for (button, by) in [(item.button(cx, ids!(less)), -1), (item.button(cx, ids!(more)), 1)] {
                        // a held button repeats; a short press is one step when it is let go
                        if button.pressed(actions) {
                            self.stop_repeat(cx);
                            self.held = Some(HeldStep { entry: index, by, repeats: 0 });
                            self.repeat_timer = cx.start_timeout(REPEAT_DELAY);
                        }
                        // a click with no press before it comes from a press made without a
                        // pointer (accessibility, the remote): one step as well
                        if button.clicked(actions) && !self.stop_repeat(cx) {
                            self.step(cx, index, by);
                        }
                        if button.released(actions) {
                            self.stop_repeat(cx);
                        }
                    }
                }
                Some(Entry::Row { .. }) => {
                    let hit = item.widget(cx, ids!(buttons)).borrow::<ButtonRow>().and_then(|row| row.clicked(actions));
                    if let Some(button) = hit {
                        clicked.push((index, button));
                    }
                }
                _ => {}
            }
        }
        let mut ids = Vec::new();
        for (index, k) in clicked {
            let Some(Entry::Row { buttons, tapped, .. }) = self.entries.get(index) else {
                continue;
            };
            let Some(button) = buttons.get(k) else {
                continue;
            };
            if tapped.is_some() && button.lock {
                continue;
            }
            if button.confirm && self.confirming != Some((index, k)) {
                // the first tap: armed until the second one or the timeout
                self.disarm(cx);
                self.confirming = Some((index, k));
                self.confirm_timer = cx.start_timeout(wire::CONFIRM_SECONDS);
                self.view.redraw(cx);
                continue;
            }
            // a tap on any button ends another button's wait for its second tap
            self.disarm(cx);
            if let Some(Entry::Row { buttons, tapped, .. }) = self.entries.get_mut(index) {
                if buttons[k].lock {
                    *tapped = Some(k);
                }
                ids.push(buttons[k].id.clone());
            }
        }
        if ids.is_empty() {
            return Vec::new();
        }
        self.view.redraw(cx);
        let values = self.tap_values();
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
                        Entry::Heading { text, .. } => {
                            let row = list.item(cx, index, id!(Heading));
                            row.label(cx, ids!(label)).set_text(cx, text);
                            row
                        }
                        Entry::Text { text, .. } => {
                            let row = list.item(cx, index, id!(Text));
                            row.label(cx, ids!(label)).set_text(cx, text);
                            row
                        }
                        Entry::Readout { text, value, .. } => {
                            let row = list.item(cx, index, id!(Readout));
                            row.label(cx, ids!(label)).set_text(cx, text);
                            row.label(cx, ids!(value)).set_text(cx, value);
                            row
                        }
                        Entry::Stepper { id, text, values } => {
                            let row = list.item(cx, index, id!(Stepper));
                            row.label(cx, ids!(label)).set_text(cx, text);
                            let at = match self.values.get(id) {
                                Some(Held::Step(at)) => *at,
                                _ => 0,
                            };
                            row.label(cx, ids!(value)).set_text(cx, values.get(at).map_or("", |v| v.as_str()));
                            // a button at the end of the values fades; a press there does nothing
                            for (button, end) in [(ids!(less), 0), (ids!(more), values.len().saturating_sub(1))] {
                                let button = row.widget(cx, button);
                                if button.disabled(cx) != (at == end) {
                                    button.set_disabled(cx, at == end);
                                }
                            }
                            row
                        }
                        Entry::Number { id, text } => {
                            let row = list.item(cx, index, id!(Number));
                            row.label(cx, ids!(label)).set_text(cx, text);
                            let input = row.text_input(cx, ids!(input));
                            let held = match self.values.get(id) {
                                Some(Held::Text(held)) => held.as_str(),
                                _ => "",
                            };
                            // only when it differs: typing must not be answered with a reset
                            if input.text() != held {
                                input.set_text(cx, held);
                            }
                            row
                        }
                        Entry::Divider => list.item(cx, index, id!(Divider)),
                        Entry::Toggle { id, text } => {
                            let row = list.item(cx, index, id!(Toggle));
                            let toggle = row.check_box(cx, ids!(toggle));
                            toggle.set_text(text);
                            let value = matches!(self.values.get(id), Some(Held::Bool(true)));
                            if toggle.active(cx) != value {
                                toggle.set_active(cx, value, Animate::No);
                            }
                            row
                        }
                        Entry::Row { buttons, tapped, .. } => {
                            let row = list.item(cx, index, id!(Row));
                            let confirming = self.confirming.filter(|(entry, _)| *entry == index).map(|(_, button)| button);
                            if let Some(mut line) = row.widget(cx, ids!(buttons)).borrow_mut::<ButtonRow>() {
                                line.set_buttons(cx, buttons, tapped.is_none(), confirming);
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
        if self.confirm_timer.is_event(event).is_some() {
            self.disarm(cx);
        }
        if self.repeat_timer.is_event(event).is_some() {
            if let Some((entry, by)) = self.held.as_ref().map(|held| (held.entry, held.by)) {
                // at the end of the values the hold has nothing more to do
                if self.step(cx, entry, by) {
                    self.repeat_timer = cx.start_timeout(REPEAT_EVERY);
                }
                if let Some(held) = &mut self.held {
                    held.repeats += 1;
                }
            }
        }
        // the repeat ends when the pointer comes up, also when the button itself never
        // hears of it; the button's own click or release then ends the hold
        let up = match event {
            Event::MouseUp(_) => true,
            Event::TouchUpdate(e) => e.touches.iter().all(|t| matches!(t.state, event::TouchState::Stop | event::TouchState::Cancel)),
            _ => false,
        };
        if up {
            cx.stop_timer(self.repeat_timer);
        }
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
    /// the service manager has been told the app is up
    #[rust]
    ready_sent: bool,
    /// the layer on screen; `None` is the idle line
    #[rust]
    front: Option<wire::Layer>,
    /// root's layer exists: an agent's page has the way back to it
    #[rust]
    has_root: bool,
    #[rust]
    dots: Dots,
    /// one shot, started again by each look while the dots are watched
    #[rust]
    dots_timer: Timer,
    /// what the dots show: (microphone, speaker)
    #[rust]
    lit: (bool, bool),
    #[rust]
    voice: Voice,
}

/// The voice strip: one text of the voice client's, in a band under the page.
#[derive(Default)]
struct Voice {
    /// the newest word from the client that the band does not show yet: a text, or `None`
    /// for no band
    next: Option<Option<String>>,
    /// the text the band shows; `None`: no band
    shown: Option<String>,
    /// when the band was last changed
    at: Option<std::time::Instant>,
    /// one shot: the newest text waits for `VOICE_APART` to pass since the last change
    turn: Timer,
    waits: bool,
    /// one shot, started again by every text: the client has gone quiet
    quiet: Timer,
}

/// The least time between two changes of the voice strip: about eight a second. Texts that
/// come faster are not queued; the newest one is shown when its turn comes.
const VOICE_APART: f64 = 0.125;
/// A text the client has not replaced or sent again for this long is taken away by the app:
/// a client that hangs cannot leave words standing.
const VOICE_SECONDS: f64 = 15.0;
/// The room the band's text does not have: the band's padding left and right, the tag and
/// the space after it (the numbers of `voice` in the layout).
const VOICE_BESIDE: f64 = 12.0 + 12.0 + 40.0 + 8.0;

/// How the microphone and speaker dots are watched.
#[derive(Default, PartialEq)]
enum Dots {
    /// the kernel has no stream status files: nothing to watch until a surface comes
    #[default]
    Absent,
    /// looked at once a second
    Watched,
    /// the phone's panel is off: not looked at until the app is woken by something else
    Dark,
}

/// The kernel's state of a PCM stream: `state: RUNNING` while it is open and running. The
/// files exist with CONFIG_SND_VERBOSE_PROCFS. `None`: no such file.
fn stream_runs(env: &str, default: &str) -> Option<bool> {
    let path = std::env::var_os(env).unwrap_or_else(|| default.into());
    std::fs::read_to_string(path).ok().map(|status| status.contains("state: RUNNING"))
}

/// How often the dots are looked at while the panel is on.
const DOTS_SECONDS: f64 = 1.0;


/// Tells the service manager that the app is up, for a unit of `Type=notify`: one datagram
/// `READY=1` to the socket `NOTIFY_SOCKET` names (systemd's sd_notify protocol). Sent once
/// the surface socket is bound and the first frame is drawn, so that what waits for the
/// surface waits on the unit's state and not on a line in a log. Without `NOTIFY_SOCKET`
/// (started by hand) nothing is sent.
fn notify_ready() {
    #[cfg(target_os = "linux")]
    {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::net::{SocketAddr, UnixDatagram};
        let Some(name) = std::env::var_os("NOTIFY_SOCKET") else {
            return;
        };
        // '@name' is a socket in the abstract namespace, '/path' one in the file system
        let addr = match name.as_bytes() {
            [b'@', name @ ..] => SocketAddr::from_abstract_name(name),
            [b'/', ..] => SocketAddr::from_pathname(&name),
            _ => return,
        };
        let sent = addr.and_then(|addr| UnixDatagram::unbound()?.send_to_addr(b"READY=1", &addr));
        if let Err(e) = sent {
            eprintln!("[tinybot-surface] cannot tell the service manager the app is ready: {e}");
        }
    }
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

    /// Shows the layer in front, or the idle line.
    fn show(&mut self, cx: &mut Cx) {
        self.ui.view(cx, ids!(surface)).set_visible(cx, self.front == Some(wire::Layer::Root));
        self.ui.view(cx, ids!(agent)).set_visible(cx, self.front == Some(wire::Layer::Agent));
        self.ui.view(cx, ids!(idle)).set_visible(cx, self.front.is_none());
        self.ui.widget(cx, ids!(agent_home)).set_visible(cx, self.has_root);
        self.ui.redraw(cx);
    }

    /// A `confirm` button waits for its second tap.
    fn confirm_armed(&self, cx: &mut Cx) -> bool {
        [ids!(items), ids!(agent_items)]
            .into_iter()
            .any(|list| self.ui.widget(cx, list).borrow::<SurfaceList>().is_some_and(|list| list.confirming.is_some()))
    }

    /// The last two lines of `text` as the band's labels will break it: the newest words.
    fn voice_lines(&self, cx: &mut Cx, text: &str) -> (String, String) {
        let width = self.ui.view(cx, ids!(column)).area().rect(cx).size.x;
        // before the first frame there is no width yet: the window's own
        let width = (if width > VOICE_BESIDE { width } else { 360.0 } - VOICE_BESIDE) as f32;
        let label = self.ui.label(cx, ids!(voice_1));
        let Some(mut label) = label.borrow_mut() else {
            return (text.to_string(), String::new());
        };
        // laid out with the label's own text style, so the lines are the ones it would
        // break; all of them (the label sets its one-line limit again when it draws)
        label.draw_text.max_lines = 0;
        let laid = label.draw_text.layout(cx, 0.0, 0.0, Some(width), true, Align::default(), text);
        let mut rows = laid.rows.iter().rev().map(|row| row.text.as_str().trim().to_string()).filter(|row| !row.is_empty());
        let last = rows.next().unwrap_or_default();
        match rows.next() {
            Some(before) => (before, last),
            None => (last, String::new()),
        }
    }

    /// Brings the band to the voice client's newest word, when that may be done now.
    /// Called after every event while a word waits, so each of the three reasons to wait
    /// ends by itself: the panel comes on, the armed button is tapped or gives up, the
    /// text's turn comes.
    fn voice_flush(&mut self, cx: &mut Cx) {
        let Some(next) = &self.voice.next else {
            return;
        };
        // nothing is drawn for a panel that is off: the newest word waits for the screen
        if self.dots == Dots::Dark {
            return;
        }
        // The band takes its room from the foot of the page, and gives it back: neither
        // while a `confirm` button waits for its second tap, which must stay where it is
        // and in sight for the three seconds it is armed.
        if next.is_some() != self.voice.shown.is_some() && self.confirm_armed(cx) {
            return;
        }
        if let Some(since) = self.voice.at.map(|at| at.elapsed().as_secs_f64()) {
            if since < VOICE_APART {
                if !self.voice.waits {
                    self.voice.turn = cx.start_timeout(VOICE_APART - since);
                    self.voice.waits = true;
                }
                return;
            }
        }
        let Some(next) = self.voice.next.take() else {
            return;
        };
        // the same text again keeps it on screen (the client is alive) and draws nothing
        if next == self.voice.shown {
            return;
        }
        let comes_or_goes = next.is_some() != self.voice.shown.is_some();
        let (first, second) = match &next {
            Some(text) => self.voice_lines(cx, text),
            None => Default::default(),
        };
        self.ui.label(cx, ids!(voice_1)).set_text(cx, &first);
        self.ui.label(cx, ids!(voice_2)).set_text(cx, &second);
        if comes_or_goes {
            // the page above gets shorter or longer: all of it is laid out again
            self.ui.view(cx, ids!(voice)).set_visible(cx, next.is_some());
            self.ui.redraw(cx);
        } else {
            self.ui.view(cx, ids!(voice)).redraw(cx);
        }
        // for a count of the band's changes and the size of what it was given; never the words
        if std::env::var_os("TINYBOT_SURFACE_LOG").is_some() {
            let what = if next.is_none() { "gone" } else if comes_or_goes { "shown" } else { "changed" };
            eprintln!("[tinybot-surface] voice strip {what} ({} bytes)", next.as_ref().map_or(0, |text| text.len()));
        }
        self.voice.shown = next;
        self.voice.at = Some(std::time::Instant::now());
    }

    /// One look at the microphone and speaker streams, and the next one a second later.
    /// Not while the phone's panel is off, and not without the kernel's status files: then
    /// no timer runs, and the app is at rest as before. `drawing`: called at the start of a
    /// frame, which will show what is found here.
    fn look_at_dots(&mut self, cx: &mut Cx, drawing: bool) {
        cx.stop_timer(self.dots_timer);
        let panel = std::env::var_os("TINYBOT_PANEL_STATE").unwrap_or_else(|| "/sys/kernel/tinybot_panel/state".into());
        if std::fs::read_to_string(panel).is_ok_and(|state| state.trim() == "off") {
            self.dots = Dots::Dark;
            return;
        }
        let mic = stream_runs("TINYBOT_MIC_STATUS", "/proc/asound/card1/pcm1c/sub0/status");
        let play = stream_runs("TINYBOT_PLAY_STATUS", "/proc/asound/card1/pcm0p/sub0/status");
        let lit = (mic == Some(true), play == Some(true));
        if lit != self.lit {
            self.lit = lit;
            self.ui.label(cx, ids!(mic)).set_text(cx, if lit.0 { "● mic" } else { "" });
            self.ui.label(cx, ids!(play)).set_text(cx, if lit.1 { "● out" } else { "" });
            if !drawing {
                self.ui.redraw(cx);
            }
        }
        if mic.is_none() && play.is_none() {
            self.dots = Dots::Absent;
        } else {
            self.dots = Dots::Watched;
            self.dots_timer = cx.start_timeout(DOTS_SECONDS);
        }
    }
}

impl MatchEvent for App {
    fn handle_startup(&mut self, cx: &mut Cx) {
        self.start_socket();
        self.settle = cx.start_timeout(SETTLE_SECONDS);
        self.settle_again = true;
        self.look_at_dots(cx, false);
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
            // a new surface (or none) may free memory a moment later; an update in place
            // does not, and a readout that changes every few seconds must not run a timer
            if matches!(message, socket::ToUi::Set(..) | socket::ToUi::Clear(_)) {
                cx.stop_timer(self.settle);
                self.settle = cx.start_timeout(SETTLE_SECONDS);
                // the status files may have come with a sound card that was not there before
                if self.dots == Dots::Absent {
                    self.look_at_dots(cx, false);
                }
            }
            let list_of = |layer: wire::Layer| match layer {
                wire::Layer::Root => ids!(items),
                wire::Layer::Agent => ids!(agent_items),
            };
            match message {
                socket::ToUi::Update(layer, update) => {
                    if let Some(title) = &update.title {
                        let title = if title.is_empty() { "tinybot" } else { title.as_str() };
                        match layer {
                            wire::Layer::Root => self.ui.label(cx, ids!(title)).set_text(cx, title),
                            wire::Layer::Agent => self.ui.label(cx, ids!(agent_title)).set_text(cx, title),
                        }
                    }
                    if let Some(mut list) = self.ui.widget(cx, list_of(layer)).borrow_mut::<SurfaceList>() {
                        list.update(cx, update);
                    }
                }
                socket::ToUi::Set(layer, spec, who) => {
                    let title = if spec.title.is_empty() { "tinybot" } else { spec.title.as_str() };
                    match layer {
                        wire::Layer::Root => self.ui.label(cx, ids!(title)).set_text(cx, title),
                        wire::Layer::Agent => {
                            self.ui.label(cx, ids!(agent_title)).set_text(cx, title);
                            // the name is the process's own word for itself; the uid is the kernel's
                            self.ui.label(cx, ids!(who)).set_text(cx, &format!("agent · {who}"));
                        }
                    }
                    if let Some(mut list) = self.ui.widget(cx, list_of(layer)).borrow_mut::<SurfaceList>() {
                        list.set_spec(cx, spec);
                    }
                }
                socket::ToUi::Clear(layer) => {
                    if let Some(mut list) = self.ui.widget(cx, list_of(layer)).borrow_mut::<SurfaceList>() {
                        list.clear(cx);
                    }
                }
                socket::ToUi::Front(front, has_root) => {
                    self.front = front;
                    self.has_root = has_root;
                    self.show(cx);
                }
                // shown by `voice_flush`, after this event; of several the newest counts
                socket::ToUi::Voice(text) => {
                    cx.stop_timer(self.voice.quiet);
                    if text.is_some() {
                        self.voice.quiet = cx.start_timeout(VOICE_SECONDS);
                    }
                    self.voice.next = Some(text);
                }
            }
        }
    }

    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions) {
        // each layer's presses go to that layer's clients
        let mut pressed = Vec::new();
        for (layer, list) in [(wire::Layer::Root, ids!(items)), (wire::Layer::Agent, ids!(agent_items))] {
            let list = self.ui.widget(cx, list);
            let presses = list.borrow_mut::<SurfaceList>().map(|mut list| list.handle_actions(cx, actions)).unwrap_or_default();
            pressed.extend(presses.into_iter().map(|press| (layer, press)));
        }
        // the band's way back from an agent's page: at once, no round trip
        let home = self.has_root && self.front == Some(wire::Layer::Agent) && self.ui.button(cx, ids!(agent_home)).clicked(actions);
        if home {
            self.front = Some(wire::Layer::Root);
            self.show(cx);
        }
        #[cfg(unix)]
        if let Some(link) = &self.link {
            for (layer, p) in pressed {
                link.send(socket::ToIo::Tap { layer, surface: p.surface, rev: p.rev, id: p.id, values: p.values });
            }
            if home {
                link.send(socket::ToIo::Home);
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
        if self.dots_timer.is_event(event).is_some() {
            self.look_at_dots(cx, false);
        } else if matches!(event, Event::Draw(_)) && self.dots != Dots::Absent {
            // The mic dot is a privacy signal: every frame the app draws shows the streams
            // as they are at that moment, the first frame after the screen comes on too.
            // Two small reads, before the frame is drawn, only when one is drawn anyway.
            self.look_at_dots(cx, true);
        } else if self.dots == Dots::Dark && matches!(event, Event::Signal | Event::MouseDown(_) | Event::TouchUpdate(_)) {
            // something woke the app while the panel was off: it may be on again
            self.look_at_dots(cx, false);
        }
        if self.settle.is_event(event).is_some() {
            release_free_memory();
            if std::mem::take(&mut self.settle_again) {
                self.settle = cx.start_timeout(SETTLE_AGAIN_SECONDS);
            }
        }
        if self.voice.quiet.is_event(event).is_some() {
            self.voice.next = Some(None);
        }
        if self.voice.turn.is_event(event).is_some() {
            self.voice.waits = false;
        }
        self.match_event(cx, event);
        self.ui.handle_event(cx, event, &mut Scope::empty());
        if self.voice.next.is_some() {
            self.voice_flush(cx);
        }
        // the first frame is drawn, and the socket was bound at startup (or the app left)
        if matches!(event, Event::Draw(_)) && !std::mem::replace(&mut self.ready_sent, true) {
            notify_ready();
        }
    }
}
