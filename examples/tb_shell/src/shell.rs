//! The window's one Rust widget: the strip on top, the front surface under it,
//! and the guard. The strip and the guard draw in the last overlay tier, above
//! every popup, modal or tooltip a body can open; while a guard is up no input
//! reaches any surface.

use makepad_widgets::*;
use makepad_widgets::makepad_platform::event::TouchState;

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
        draw_mark +: {
            color: #xffffff
            text_style: theme.font_bold{ font_size: 10 }
        }
        draw_veil +: { color: #x0b0e14 }
        draw_yes +: { color: #x1c2230 }
        draw_no +: { color: #x1c2230 }
        draw_guard_text +: {
            color: #xeeeeee
            text_style: theme.font_regular{ font_size: 17 }
        }
        draw_guard_small +: {
            color: #x9aa4b2
            text_style: theme.font_regular{ font_size: 12 }
        }
    }
}

/// What a raised guard shows. Only the shell makes one.
#[derive(Clone)]
pub struct GuardView {
    pub text: String,
    pub who: String,
    pub yes: String,
    pub no: String,
}

#[derive(Clone, Copy, Default, PartialEq)]
pub struct Marks {
    pub mic: bool,
    pub loud: bool,
}

#[derive(Script, ScriptHook, WidgetRef, WidgetRegister)]
pub struct Shell {
    #[source] source: ScriptObjectRef,
    #[uid] uid: WidgetUid,
    #[walk] walk: Walk,
    #[live] draw_strip: DrawColor,
    #[live] draw_label: DrawText,
    #[live] draw_mark: DrawText,
    #[live] draw_veil: DrawColor,
    #[live] draw_yes: DrawColor,
    #[live] draw_no: DrawColor,
    #[live] draw_guard_text: DrawText,
    #[live] draw_guard_small: DrawText,
    #[live] top_list: DrawList2d,
    #[live] strip_height: f64,
    #[rust] area: Area,
    #[rust] pub surfaces: Vec<(String, WidgetRef)>,
    #[rust] pub front: String,
    #[rust] pub marks: Marks,
    #[rust] guard: Option<GuardView>,
    #[rust] armed: bool,
    #[rust] arm_timer: Timer,
    #[rust] pressed: Option<bool>,
    #[rust] raised_at: Option<std::time::Instant>,
    /// How long the raised guard took to reach its first draw (the starve proof's read).
    #[rust] pub guard_drawn_ms: Option<f64>,
    #[rust] yes_rect: Rect,
    #[rust] no_rect: Rect,
    /// The person's answer to the raised guard, for the app to take.
    #[rust] answer: Option<bool>,
    #[rust] strip_rect: Rect,
    /// A tap on the strip: the app brings `home` to the front.
    #[rust] go_home: bool,
    /// A short note the strip shows after the front's name (the shell's own words).
    #[rust] pub note: String,
}

/// Input events: while a guard is up none of these reach a surface.
fn is_input(event: &Event) -> bool {
    !matches!(event, Event::Signal | Event::Timer(_) | Event::NextFrame(_) | Event::Startup | Event::Draw(_))
}

impl Widget for Shell {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if self.guard.is_some() {
            if self.arm_timer.is_event(event).is_some() {
                self.armed = true;
                cx.redraw_all();
            }
            // The guard reads the raw pointer against its own rectangles, not the
            // hit system: a body's open popup holds the hit sweep and would turn
            // the guard's buttons away.
            let (down, up) = match event {
                Event::MouseDown(e) if e.button == MouseButton::PRIMARY => (Some(e.abs), None),
                Event::MouseUp(e) if e.button == MouseButton::PRIMARY => (None, Some(e.abs)),
                Event::TouchUpdate(e) => {
                    let start = e.touches.iter().find(|t| t.state == TouchState::Start).map(|t| t.abs);
                    let stop = e.touches.iter().find(|t| t.state == TouchState::Stop).map(|t| t.abs);
                    (start, stop)
                }
                _ => (None, None),
            };
            let button_at = |p: Vec2d| [(true, self.yes_rect), (false, self.no_rect)].into_iter().find(|(_, r)| r.contains(p)).map(|(yes, _)| yes);
            if let Some(p) = down {
                // A press that began before the guard was armed is not an answer.
                self.pressed = if self.armed { button_at(p) } else { None };
            }
            if let Some(p) = up {
                if self.armed && self.pressed.is_some() && button_at(p) == self.pressed {
                    self.answer = self.pressed;
                }
                self.pressed = None;
            }
        }
        // A tap on the strip, read from the raw pointer like the guard's: home.
        if self.guard.is_none() {
            let up = match event {
                Event::MouseUp(e) if e.button == MouseButton::PRIMARY => Some(e.abs),
                Event::TouchUpdate(e) => e.touches.iter().find(|t| t.state == TouchState::Stop).map(|t| t.abs),
                _ => None,
            };
            if up.map_or(false, |p| self.strip_rect.contains(p)) {
                self.go_home = true;
            }
        }
        let gated = self.guard.is_some() && is_input(event);
        for (name, splash) in &self.surfaces {
            // Input goes to the front surface only, and to none under a guard;
            // every surface still gets time.
            let input_ok = *name == self.front && !gated;
            if input_ok || !is_input(event) {
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
        self.top_list.begin_overlay_last(cx);
        if let Some(guard) = self.guard.clone() {
            self.draw_guard(cx, rect, &guard);
            if let Some(at) = self.raised_at.take() {
                let ms = at.elapsed().as_secs_f64() * 1000.0;
                self.guard_drawn_ms = Some(ms);
                log!("tb-shell: guard drawn {ms:.1} ms after it was raised");
            }
        }
        self.draw_strip(cx, rect);
        self.top_list.end(cx);
        cx.end_turtle_with_area(&mut self.area);
        DrawStep::done()
    }
}

impl Shell {
    fn draw_strip(&mut self, cx: &mut Cx2d, rect: Rect) {
        // A raised guard turns the whole strip one solid colour no surface can set.
        self.draw_strip.color = if self.guard.is_some() { vec4(0.96, 0.62, 0.1, 1.0) } else { vec4(0.02, 0.027, 0.043, 1.0) };
        self.strip_rect = Rect { pos: rect.pos, size: dvec2(rect.size.x, self.strip_height) };
        self.draw_strip.draw_abs(cx, self.strip_rect);
        let front = if self.front.is_empty() { "-" } else { self.front.as_str() };
        let label = if self.guard.is_some() { "tb-shell   GUARD".to_string() } else { format!("tb-shell   {front}   {}", self.note) };
        self.draw_label.color = if self.guard.is_some() { vec4(0.0, 0.0, 0.0, 1.0) } else { vec4(0.6, 0.64, 0.7, 1.0) };
        self.draw_label.draw_abs(cx, rect.pos + dvec2(12.0, 8.0), &label);
        let mut x = rect.pos.x + rect.size.x - 12.0;
        // The loud mark's place is kept; its source (the sound tool's state) comes later.
        for (on, text, color) in [(self.marks.mic, "MIC", vec4(1.0, 0.35, 0.35, 1.0)), (self.marks.loud, "LOUD", vec4(1.0, 0.75, 0.2, 1.0))] {
            if on {
                x -= 8.0 * text.len() as f64 + 10.0;
                self.draw_mark.color = color;
                self.draw_mark.draw_abs(cx, dvec2(x, rect.pos.y + 8.0), text);
            }
        }
    }

    fn draw_guard(&mut self, cx: &mut Cx2d, rect: Rect, guard: &GuardView) {
        let top = rect.pos.y + self.strip_height;
        let (w, h) = (rect.size.x, rect.size.y - self.strip_height);
        self.draw_veil.draw_abs(cx, Rect { pos: dvec2(rect.pos.x, top), size: dvec2(w, h) });
        let mut y = top + 40.0;
        for line in wrap(&guard.text, ((w - 48.0) / 9.5) as usize) {
            self.draw_guard_text.draw_abs(cx, dvec2(rect.pos.x + 24.0, y), &line);
            y += 26.0;
        }
        self.draw_guard_small.draw_abs(cx, dvec2(rect.pos.x + 24.0, y + 8.0), &guard.who);
        let bw = w - 48.0;
        let (yes_y, no_y) = (top + h - 200.0, top + h - 110.0);
        let dim = if self.armed { 1.0 } else { 0.45 };
        self.draw_yes.color = vec4(0.11 * dim, 0.13 * dim, 0.19 * dim, 1.0);
        self.draw_no.color = self.draw_yes.color;
        self.yes_rect = Rect { pos: dvec2(rect.pos.x + 24.0, yes_y), size: dvec2(bw, 72.0) };
        self.no_rect = Rect { pos: dvec2(rect.pos.x + 24.0, no_y), size: dvec2(bw, 72.0) };
        self.draw_yes.draw_abs(cx, self.yes_rect);
        self.draw_no.draw_abs(cx, self.no_rect);
        self.draw_guard_text.draw_abs(cx, dvec2(rect.pos.x + 44.0, yes_y + 24.0), &guard.yes);
        self.draw_guard_text.draw_abs(cx, dvec2(rect.pos.x + 44.0, no_y + 24.0), &guard.no);
    }

    pub fn surface(&self, name: &str) -> Option<WidgetRef> {
        self.surfaces.iter().find(|(n, _)| n == name).map(|(_, w)| w.clone())
    }

    pub fn add(&mut self, cx: &mut Cx, name: &str) -> WidgetRef {
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

    pub fn remove(&mut self, cx: &mut Cx, name: &str) -> bool {
        let Some(at) = self.surfaces.iter().position(|(n, _)| n == name) else { return false };
        let (_, splash) = self.surfaces.remove(at);
        splash.set_text(cx, ""); // an empty body reclaims the isolate and its timers
        cx.widget_tree_mark_dirty(self.uid);
        cx.redraw_all();
        true
    }

    /// Raises (Some) or lowers (None) the guard. A new guard is not armed for
    /// half a second, so a tap meant for what was under it cannot answer it.
    pub fn set_guard(&mut self, cx: &mut Cx, guard: Option<GuardView>) {
        self.guard = guard;
        self.armed = false;
        self.pressed = None;
        self.answer = None;
        cx.stop_timer(self.arm_timer);
        self.guard_drawn_ms = None;
        self.raised_at = self.guard.as_ref().map(|_| std::time::Instant::now());
        if self.guard.is_some() {
            self.arm_timer = cx.start_timeout(0.5);
        }
        cx.redraw_all();
    }

    pub fn take_answer(&mut self) -> Option<bool> {
        self.answer.take()
    }

    pub fn take_go_home(&mut self) -> bool {
        std::mem::take(&mut self.go_home)
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

/// Greedy word wrap by character count (the guard's text is short).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in text.split_whitespace() {
        let line = lines.last_mut().unwrap();
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width.max(8) {
            lines.push(word.to_string());
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    lines
}
