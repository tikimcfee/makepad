//! tinybot's measurement scene (not for upstream). A pager-like screen for a 384x807 phone
//! window: a header, a list of 300 messages (avatar, name, two lines of text, time), a text
//! input with a Send button. What redraws is scripted, so a run measures one thing:
//!
//!   TB_SCENE=rest    (default) nothing animates: the at-rest cost of a drawn window
//!   TB_SCENE=scroll  the list scrolls by TB_SCENE_PX (default 4) px every frame for
//!                    TB_SCENE_SECS (default 10) s, then rests: the 60 fps cost of text and shapes
//!   TB_SCENE=pulse   one full redraw every second for TB_SCENE_SECS s: the cost of single frames
//!
//! Marks on stderr, `[tb-scene] mark NAME mono_ms=... cpu_user_ms= cpu_sys_ms= pss_kb=...`
//! (CLOCK_MONOTONIC, as the harness's own marks; getrusage of the whole process; smaps_rollup):
//! startup, first-draw, mode-start, mode-end. In rest mode mode-start comes 3 s after startup
//! (the first frames settled) and mode-end TB_SCENE_SECS later: rest CPU = the difference of
//! the two marks over that time. While animating, every 60 frames:
//! `[tb-scene] frames N interval_ms min/avg/max` from the frame times Makepad hands out.
use makepad_widgets::*;

app_main!(App);

pub const ROWS: usize = 300;
const ROW_H: f64 = 72.0;

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}
#[repr(C)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i64,
}
/// struct rusage on 64-bit Linux: two timevals, then 14 longs
#[repr(C)]
struct Rusage {
    utime: Timeval,
    stime: Timeval,
    rest: [i64; 14],
}
extern "C" {
    fn clock_gettime(clock: i32, ts: *mut Timespec) -> i32;
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
}
/// this process's (user, system) CPU in ms, all threads
fn cpu_ms() -> (f64, f64) {
    let mut r = Rusage { utime: Timeval { tv_sec: 0, tv_usec: 0 }, stime: Timeval { tv_sec: 0, tv_usec: 0 }, rest: [0; 14] };
    unsafe { getrusage(0, &mut r) };
    let ms = |t: &Timeval| t.tv_sec as f64 * 1e3 + t.tv_usec as f64 / 1e3;
    (ms(&r.utime), ms(&r.stime))
}
/// Pss, Pss_Anon, Pss_File of this process in kB (smaps_rollup)
fn pss_kb() -> String {
    let text = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    let get = |k: &str| text.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).unwrap_or("?").to_string();
    format!("pss_kb={} pss_anon_kb={} pss_file_kb={}", get("Pss:"), get("Pss_Anon:"), get("Pss_File:"))
}
fn mono_ms() -> f64 {
    let mut t = Timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { clock_gettime(1, &mut t) };
    t.tv_sec as f64 * 1e3 + t.tv_nsec as f64 / 1e6
}
/// every thread's (name, user+system CPU in ms) from /proc/self/task (clock ticks of 10 ms)
fn thread_cpu() -> String {
    let mut out = Vec::new();
    if let Ok(dir) = std::fs::read_dir("/proc/self/task") {
        for e in dir.flatten() {
            let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else { continue };
            let (Some(o), Some(c)) = (stat.find('('), stat.rfind(')')) else { continue };
            let name = stat[o + 1..c].replace(' ', "_");
            let f: Vec<&str> = stat[c + 2..].split_whitespace().collect();
            let ticks = f.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            if ticks > 0 { out.push(format!("{name}={}", ticks * 10)); }
        }
    }
    out.sort();
    out.join(" ")
}
fn mark(name: &str) {
    let (u, s) = cpu_ms();
    eprintln!("[tb-scene] mark {name} mono_ms={:.3} cpu_user_ms={u:.1} cpu_sys_ms={s:.1} {} threads_ms: {}", mono_ms(), pss_kb(), thread_cpu());
}

const NAMES: [&str; 6] = ["user", "tinybot-m2", "building", "optimizer", "phoc", "presenter"];
const WORDS: [&str; 24] = [
    "the", "panel", "draws", "a", "frame", "when", "something", "changed", "and", "rests", "otherwise",
    "fence", "buffer", "glyph", "atlas", "touch", "keyboard", "scroll", "commit", "vsync", "presenter",
    "shader", "cache", "launch",
];
fn body(i: usize) -> String {
    let n = 10 + (i * 7) % 14;
    (0..n).map(|k| WORDS[(i * 31 + k * 17) % WORDS.len()]).collect::<Vec<_>>().join(" ")
}

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*

    mod.widgets.MessageListBase = #(MessageList::register_widget(vm))
    mod.widgets.MessageList = set_type_default() do mod.widgets.MessageListBase{
        width: Fill
        height: Fill
        flow: Down
        list := PortalList{
            width: Fill
            height: Fill
            Row := View{
                width: Fill
                height: 72
                flow: Right
                spacing: 10
                padding: Inset{left: 10 right: 10 top: 8 bottom: 8}
                avatar := RoundedView{
                    width: 40
                    height: 40
                    draw_bg.color: #3a6ea5
                    draw_bg.border_radius: 20.0
                }
                View{
                    width: Fill
                    height: Fill
                    flow: Down
                    spacing: 2
                    View{
                        width: Fill
                        height: Fit
                        flow: Right
                        name := Label{
                            width: Fill
                            text: ""
                            draw_text.color: #fff
                            draw_text.text_style.font_size: 11
                        }
                        time := Label{
                            text: ""
                            draw_text.color: #888
                            draw_text.text_style.font_size: 9
                        }
                    }
                    text := Label{
                        width: Fill
                        text: ""
                        draw_text.color: #ccc
                        draw_text.text_style.font_size: 10
                    }
                }
            }
        }
    }

    startup() do #(App::script_component(vm)){
        ui: Root{
            main_window := Window{
                window.inner_size: vec2(384, 807)
                body +: {
                    View{
                        width: Fill
                        height: Fill
                        flow: Down
                        View{
                            width: Fill
                            height: 48
                            padding: Inset{left: 12 right: 12}
                            align: Align{y: 0.5}
                            show_bg: true
                            draw_bg.color: #202428
                            title := Label{
                                width: Fill
                                text: "tinybot"
                                draw_text.color: #fff
                                draw_text.text_style.font_size: 14
                            }
                            status := Label{
                                text: "rest"
                                draw_text.color: #8a8
                                draw_text.text_style.font_size: 10
                            }
                        }
                        messages := mod.widgets.MessageList{}
                        View{
                            width: Fill
                            height: Fit
                            flow: Right
                            spacing: 8
                            padding: 8
                            align: Align{y: 0.5}
                            input := TextInput{
                                width: Fill
                                height: 44
                                empty_text: "Message"
                            }
                            send := Button{
                                text: "Send"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Script, ScriptHook, Widget)]
pub struct MessageList {
    #[deref]
    view: View,
    #[rust]
    drawn: bool,
}

impl Widget for MessageList {
    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        while let Some(item) = self.view.draw_walk(cx, scope, walk).step() {
            if let Some(mut list) = item.borrow_mut::<PortalList>() {
                list.set_item_range(cx, 0, ROWS);
                while let Some(i) = list.next_visible_item(cx) {
                    if i >= ROWS {
                        continue;
                    }
                    let row = list.item(cx, i, id!(Row));
                    row.label(cx, ids!(name)).set_text(cx, NAMES[i % NAMES.len()]);
                    row.label(cx, ids!(time)).set_text(cx, &format!("{:02}:{:02}", 8 + i / 60 % 12, i % 60));
                    row.label(cx, ids!(text)).set_text(cx, &body(i));
                    row.draw_all(cx, &mut Scope::empty());
                }
            }
        }
        if !self.drawn {
            self.drawn = true;
            mark("first-draw");
        }
        DrawStep::done()
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Rest,
    Scroll,
    Pulse,
}

#[derive(Script, ScriptHook)]
pub struct App {
    #[live]
    ui: WidgetRef,
    #[rust]
    mode: Option<Mode>,
    #[rust]
    px: f64,
    #[rust]
    secs: f64,
    #[rust]
    next_frame: NextFrame,
    #[rust]
    started: Option<f64>,
    #[rust]
    offset: f64,
    #[rust]
    frames: u64,
    #[rust]
    last_time: Option<f64>,
    #[rust]
    intervals: Vec<f64>,
    #[rust]
    pulses: u64,
    #[rust]
    timer: Timer,
}

impl App {
    fn report(&mut self) {
        if self.intervals.is_empty() {
            return;
        }
        let min = self.intervals.iter().cloned().fold(f64::MAX, f64::min);
        let max = self.intervals.iter().cloned().fold(0.0, f64::max);
        let avg = self.intervals.iter().sum::<f64>() / self.intervals.len() as f64;
        eprintln!("[tb-scene] frames {} interval_ms {:.2}/{:.2}/{:.2} mono_ms={:.3}", self.frames, min, avg, max, mono_ms());
        self.intervals.clear();
    }
}

impl MatchEvent for App {
    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions) {
        if self.ui.button(cx, ids!(send)).clicked(actions) {
            mark("send");
            self.ui.text_input(cx, ids!(input)).set_text(cx, "");
        }
    }
}

impl AppMain for App {
    fn script_mod(vm: &mut ScriptVm) -> ScriptValue {
        makepad_widgets::script_mod(vm);
        self::script_mod(vm)
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        if let Event::Startup = event {
            mark("startup");
            let env = |k: &str| std::env::var(k).ok();
            let mode = match env("TB_SCENE").as_deref() {
                Some("scroll") => Mode::Scroll,
                Some("pulse") => Mode::Pulse,
                _ => Mode::Rest,
            };
            self.mode = Some(mode);
            self.px = env("TB_SCENE_PX").and_then(|v| v.parse().ok()).unwrap_or(4.0);
            self.secs = env("TB_SCENE_SECS").and_then(|v| v.parse().ok()).unwrap_or(10.0);
            let name = match mode { Mode::Rest => "rest", Mode::Scroll => "scroll", Mode::Pulse => "pulse" };
            eprintln!("[tb-scene] mode {name} px {} secs {}", self.px, self.secs);
            self.ui.label(cx, ids!(status)).set_text(cx, name);
            match mode {
                Mode::Scroll => self.next_frame = cx.new_next_frame(),
                // a timer, not a frame callback per frame: between pulses the app must be idle
                Mode::Pulse => { mark("mode-start"); self.timer = cx.start_interval(1.0); }
                // first a settle (the first frames, shader compiles, glyph rasterizing), then the window
                Mode::Rest => self.timer = cx.start_timeout(3.0),
            }
        }
        if let Event::NextFrame(ne) = event {
            if ne.set.contains(&self.next_frame) {
                let t = ne.time;
                let start = *self.started.get_or_insert_with(|| {
                    mark("mode-start");
                    t
                });
                if let Some(last) = self.last_time {
                    self.intervals.push((t - last) * 1e3);
                }
                self.last_time = Some(t);
                self.frames += 1;
                let list = self.ui.portal_list(cx, ids!(list));
                match self.mode {
                    Some(Mode::Scroll) => {
                        self.offset += self.px;
                        let total = ROWS as f64 * ROW_H - 600.0;
                        if self.offset > total {
                            self.offset = 0.0;
                        }
                        let id = (self.offset / ROW_H) as usize;
                        list.set_first_id_and_scroll(id, -(self.offset - id as f64 * ROW_H));
                        list.redraw(cx);
                    }
                    _ => {}
                }
                if self.frames % 60 == 0 {
                    self.report();
                }
                if t - start < self.secs {
                    self.next_frame = cx.new_next_frame();
                } else {
                    self.report();
                    mark("mode-end");
                }
            }
        }
        if self.timer.is_event(event).is_some() && self.mode == Some(Mode::Rest) {
            if self.started.is_none() {
                self.started = Some(0.0);
                mark("mode-start");
                self.timer = cx.start_timeout(self.secs);
            } else {
                mark("mode-end");
            }
        } else if self.timer.is_event(event).is_some() {
            self.pulses += 1;
            self.ui.redraw(cx);
            eprintln!("[tb-scene] pulse {} mono_ms={:.3}", self.pulses, mono_ms());
            if self.pulses as f64 >= self.secs {
                cx.stop_timer(self.timer);
                mark("mode-end");
            }
        }
        self.match_event(cx, event);
        self.ui.handle_event(cx, event, &mut Scope::empty());
    }
}
