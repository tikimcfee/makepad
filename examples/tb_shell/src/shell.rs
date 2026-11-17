//! The window's one Rust widget: the 3-zone protected strip on top (`Home`,
//! `Stage` switcher / swipe tasks, `Wi-Fi · Bat% · HH:MM` + `MIC`/`LOUD`),
//! the general `1..N` multi-surface `Stage` (`stack` / `grid` / `deck`) under it,
//! and the root `GUARD` drawn in the final overlay tier gating all input.

use crate::stage::{SlotMode, Stage, StageMode};
use makepad_widgets::makepad_platform::event::TouchState;
use makepad_widgets::*;
use std::collections::HashMap;

script_mod! {
    use mod.prelude.widgets.*

    mod.widgets.ShellBase = #(Shell::register_widget(vm))
    mod.widgets.Shell = set_type_default() do mod.widgets.ShellBase {
        width: Fill
        height: Fill
        strip_height: 32.0
        draw_strip +: { color: #x05070b }
        draw_pill +: { color: #x141b28 }
        draw_slot_bar +: { color: #x121824 }
        draw_dot +: { color: #x3ddc84 }
        draw_label +: {
            color: #xd6e2f0
            text_style: theme.font_regular{ font_size: 10.5 }
        }
        draw_sub +: {
            color: #x8b95a5
            text_style: theme.font_regular{ font_size: 9.5 }
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
    /// Root's test switch is on: injected input may answer a guard.
    pub test_taps: bool,
}

/// Who may answer a guard. Only a person's input from the device, except under
/// root's test switch.
#[derive(Default)]
pub struct GuardInput {
    pub remote_on: bool,
    pub test_taps: bool,
    last_seq: u64,
}

/// UI actions initiated by strip taps/swipes or per-slot mini-headers.
#[derive(Clone, Debug)]
pub enum ShellAction {
    GoHome,
    ToggleStageView,
    CycleTask(i32),
    FocusSlot(String),
    ToggleHeader(String),
    CycleSlotMode(String),
    RemoveSlot(String),
}

#[derive(Clone)]
struct SlotHit {
    surface: String,
    header_rect: Rect,
    body_rect: Rect,
    btn_collapse: Rect,
    btn_mode: Rect,
    btn_close: Rect,
}

#[derive(Clone)]
struct DeckTabHit {
    surface: String,
    tab_rect: Rect,
    close_rect: Rect,
}

#[derive(Clone)]
enum DragOrigin {
    Strip(Vec2d),
    Header { surface: String, start: Vec2d, start_scroll: f64, dragged: bool },
}

#[derive(Script, ScriptHook, WidgetRef, WidgetRegister)]
pub struct Shell {
    #[source] source: ScriptObjectRef,
    #[uid] uid: WidgetUid,
    #[walk] walk: Walk,
    #[live] draw_strip: DrawColor,
    #[live] draw_pill: DrawColor,
    #[live] draw_slot_bar: DrawColor,
    #[live] draw_dot: DrawColor,
    #[live] draw_label: DrawText,
    #[live] draw_sub: DrawText,
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
    #[rust] pub stage: Stage,
    #[rust] pub marks: Marks,
    #[rust] pub status_right: String,
    #[rust] pub guard_input: GuardInput,
    #[rust] guard: Option<GuardView>,
    #[rust] armed: bool,
    #[rust] arm_timer: Timer,
    #[rust] pressed: Option<bool>,
    #[rust] raised_at: Option<std::time::Instant>,
    #[rust] pub guard_drawn_ms: Option<f64>,
    #[rust] yes_rect: Rect,
    #[rust] no_rect: Rect,
    #[rust] answer: Option<bool>,
    #[rust] strip_rect: Rect,
    #[rust] home_rect: Rect,
    #[rust] center_rect: Rect,
    #[rust] pub note: String,
    #[rust] actions: Vec<ShellAction>,
    #[rust] slot_hits: Vec<SlotHit>,
    #[rust] deck_hits: Vec<DeckTabHit>,
    #[rust] drag_origin: Option<DragOrigin>,
    #[rust] active_body_surface: Option<String>,
    #[rust] pub stage_scroll_y: f64,
    #[rust] max_stage_scroll: f64,
    #[rust] measured_compact_h: HashMap<String, f64>,
    /// `(surface_name, cpu_draw_ms)` for every drawn slot in the latest frame.
    #[rust] pub last_draw_ms: Vec<(String, f64)>,
    /// `(surface_name, slot_w, slot_h)` from the latest draw frame.
    #[rust] pub last_slot_sizes: Vec<(String, f64, f64)>,
}

fn is_input(event: &Event) -> bool {
    !matches!(event, Event::Signal | Event::Timer(_) | Event::NextFrame(_) | Event::Startup | Event::Draw(_))
}

fn pointer_down_move_up(event: &Event) -> (Option<Vec2d>, Option<Vec2d>, Option<Vec2d>) {
    match event {
        Event::MouseDown(e) if e.button == MouseButton::PRIMARY => (Some(e.abs), None, None),
        Event::MouseMove(e) => (None, Some(e.abs), None),
        Event::MouseUp(e) if e.button == MouseButton::PRIMARY => (None, None, Some(e.abs)),
        Event::TouchUpdate(e) => {
            let start = e.touches.iter().find(|t| t.state == TouchState::Start).map(|t| t.abs);
            let mv = e.touches.iter().find(|t| t.state == TouchState::Move).map(|t| t.abs);
            let stop = e.touches.iter().find(|t| t.state == TouchState::Stop).map(|t| t.abs);
            (start, mv, stop)
        }
        _ => (None, None, None),
    }
}

fn event_abs(event: &Event) -> Option<Vec2d> {
    match event {
        Event::MouseDown(e) => Some(e.abs),
        Event::MouseMove(e) => Some(e.abs),
        Event::MouseUp(e) => Some(e.abs),
        Event::Scroll(e) => Some(e.abs),
        Event::LongPress(e) => Some(e.abs),
        Event::TouchUpdate(e) => e.touches.first().map(|t| t.abs),
        _ => None,
    }
}

impl Widget for Shell {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let seq = makepad_widgets::makepad_platform::makepad_hal::a11y::user_seq();
        let native = !self.guard_input.remote_on || seq != self.guard_input.last_seq;
        self.guard_input.last_seq = seq;
        let trusted = native || self.guard_input.test_taps;

        if self.guard.is_some() {
            if self.arm_timer.is_event(event).is_some() {
                self.armed = true;
                cx.redraw_all();
            }
            let (down, _, up) = pointer_down_move_up(event);
            if !trusted && (down.is_some() || up.is_some()) {
                log!("tb-shell: guard: injected input ignored");
                return;
            }
            let button_at = |p: Vec2d| {
                [(true, self.yes_rect), (false, self.no_rect)]
                    .into_iter()
                    .find(|(_, r)| r.contains(p))
                    .map(|(yes, _)| yes)
            };
            if let Some(p) = down {
                self.pressed = if self.armed { button_at(p) } else { None };
            }
            if let Some(p) = up {
                if self.armed && self.pressed.is_some() && button_at(p) == self.pressed {
                    self.answer = self.pressed;
                }
                self.pressed = None;
            }
        }

        if self.guard.is_none() {
            let (down, mv, up) = pointer_down_move_up(event);
            let mut chrome_consumed = false;

            if let Some(p) = down {
                if self.strip_rect.contains(p) {
                    self.drag_origin = Some(DragOrigin::Strip(p));
                    self.active_body_surface = None;
                    chrome_consumed = true;
                } else if let Some(th) = self.deck_hits.iter().find(|t| t.tab_rect.contains(p)).cloned() {
                    if th.close_rect.size.x > 0.0 && th.close_rect.contains(p) {
                        self.actions.push(ShellAction::RemoveSlot(th.surface));
                    } else {
                        self.actions.push(ShellAction::FocusSlot(th.surface));
                    }
                    self.active_body_surface = None;
                    chrome_consumed = true;
                } else if let Some(sh) = self.slot_hits.iter().find(|h| h.header_rect.size.y > 0.0 && h.header_rect.contains(p)).cloned() {
                    self.drag_origin = Some(DragOrigin::Header {
                        surface: sh.surface,
                        start: p,
                        start_scroll: self.stage_scroll_y,
                        dragged: false,
                    });
                    self.active_body_surface = None;
                    chrome_consumed = true;
                } else if let Some(sh) = self.slot_hits.iter().find(|h| h.body_rect.size.y > 0.0 && h.body_rect.contains(p)).cloned() {
                    if self.stage.focus != sh.surface {
                        self.actions.push(ShellAction::FocusSlot(sh.surface.clone()));
                    }
                    self.active_body_surface = Some(sh.surface);
                } else {
                    self.active_body_surface = None;
                }
            }

            if let Some(p) = mv {
                if let Some(DragOrigin::Header { start, start_scroll, ref mut dragged, .. }) = self.drag_origin {
                    let dy = p.y - start.y;
                    if dy.abs() > 8.0 && self.max_stage_scroll > 0.0 {
                        *dragged = true;
                        let next_scroll = (start_scroll - dy).clamp(0.0, self.max_stage_scroll);
                        if (next_scroll - self.stage_scroll_y).abs() > 0.5 {
                            self.stage_scroll_y = next_scroll;
                            cx.redraw_all();
                        }
                    }
                    chrome_consumed = true;
                } else if matches!(self.drag_origin, Some(DragOrigin::Strip(_))) {
                    chrome_consumed = true;
                }
            }

            if let Some(p) = up {
                match self.drag_origin.take() {
                    Some(DragOrigin::Strip(start)) => {
                        let dx = p.x - start.x;
                        let dy = p.y - start.y;
                        if dx.abs() >= 36.0 && dx.abs() > dy.abs() {
                            self.actions.push(ShellAction::CycleTask(if dx < 0.0 { 1 } else { -1 }));
                        } else if self.strip_rect.contains(p) && dx.abs() < 24.0 {
                            if self.home_rect.contains(p) {
                                self.actions.push(ShellAction::GoHome);
                            } else {
                                self.actions.push(ShellAction::ToggleStageView);
                            }
                        }
                        chrome_consumed = true;
                    }
                    Some(DragOrigin::Header { surface, dragged, .. }) => {
                        if !dragged {
                            if let Some(sh) = self.slot_hits.iter().find(|h| h.surface == surface) {
                                if sh.btn_close.contains(p) {
                                    self.actions.push(ShellAction::RemoveSlot(surface));
                                } else if sh.btn_mode.contains(p) {
                                    self.actions.push(ShellAction::CycleSlotMode(surface));
                                } else if sh.btn_collapse.contains(p) {
                                    self.actions.push(ShellAction::ToggleHeader(surface));
                                } else if sh.header_rect.contains(p) {
                                    self.actions.push(ShellAction::FocusSlot(surface));
                                }
                            }
                        }
                        chrome_consumed = true;
                    }
                    None => {}
                }
            }

            if chrome_consumed && is_input(event) {
                return;
            }
        }

        let gated = self.guard.is_some() && is_input(event);
        if !is_input(event) {
            for (_, splash) in &self.surfaces {
                splash.handle_event(cx, event, scope);
            }
            return;
        }
        if gated {
            return;
        }

        // Route input strictly to the target visible slot.
        let target_surface = if let Some(abs) = event_abs(event) {
            if matches!(event, Event::MouseMove(_) | Event::MouseUp(_) | Event::TouchUpdate(_))
                && self.active_body_surface.is_some()
            {
                self.active_body_surface.clone()
            } else {
                self.slot_hits
                    .iter()
                    .find(|h| h.body_rect.size.y > 0.0 && h.body_rect.contains(abs))
                    .map(|h| h.surface.clone())
            }
        } else {
            Some(self.stage.focus.clone())
        };

        if matches!(event, Event::MouseUp(_)) {
            self.active_body_surface = None;
        } else if let Event::TouchUpdate(e) = event {
            if e.touches.iter().all(|t| matches!(t.state, TouchState::Stop)) {
                self.active_body_surface = None;
            }
        }

        if let Some(ref target) = target_surface {
            if let Some((_, splash)) = self.surfaces.iter().find(|(n, _)| n == target) {
                splash.handle_event(cx, event, scope);
            }
        } else if let Event::Scroll(e) = event {
            if self.max_stage_scroll > 0.0 {
                let next_scroll = (self.stage_scroll_y + e.scroll.y).clamp(0.0, self.max_stage_scroll);
                if (next_scroll - self.stage_scroll_y).abs() > 0.5 {
                    self.stage_scroll_y = next_scroll;
                    cx.redraw_all();
                }
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.slot_hits.clear();
        self.deck_hits.clear();
        self.last_draw_ms.clear();
        self.last_slot_sizes.clear();

        cx.begin_turtle(walk, Layout::flow_down());
        let win_rect = cx.turtle().rect();
        let stage_top = win_rect.pos.y + self.strip_height;
        let stage_w = win_rect.size.x.max(1.0);
        let stage_h = (win_rect.size.y - self.strip_height).max(1.0);

        // Advance past the reserved top strip height, then open a clipped stage viewport.
        cx.walk_turtle(Walk::fixed(stage_w, self.strip_height));
        cx.begin_turtle(
            Walk::fixed(stage_w, stage_h),
            Layout { clip_x: true, clip_y: true, ..Layout::flow_down() },
        );

        match self.stage.mode {
            StageMode::Deck => self.draw_deck(cx, scope, dvec2(win_rect.pos.x, stage_top), stage_w, stage_h),
            StageMode::Grid if self.stage.slots.len() > 1 => {
                self.draw_grid(cx, scope, dvec2(win_rect.pos.x, stage_top), stage_w, stage_h);
            }
            _ => self.draw_stack(cx, scope, dvec2(win_rect.pos.x, stage_top), stage_w, stage_h),
        }

        cx.end_turtle();

        self.top_list.begin_overlay_last(cx);
        if let Some(guard) = self.guard.clone() {
            self.draw_guard(cx, win_rect, &guard);
            if let Some(at) = self.raised_at.take() {
                let ms = at.elapsed().as_secs_f64() * 1000.0;
                self.guard_drawn_ms = Some(ms);
                log!("tb-shell: guard drawn {ms:.1} ms after it was raised");
            }
        }
        self.draw_strip(cx, win_rect);
        self.top_list.end(cx);
        cx.end_turtle_with_area(&mut self.area);
        DrawStep::done()
    }
}

impl Shell {
    fn draw_deck(&mut self, cx: &mut Cx2d, scope: &mut Scope, origin: Vec2d, w: f64, h: f64) {
        self.max_stage_scroll = 0.0;
        self.stage_scroll_y = 0.0;
        let tab_h = if self.stage.slots.len() > 1 { 28.0 } else { 0.0 };
        if tab_h > 0.0 {
            self.draw_slot_bar.color = vec4(0.06, 0.08, 0.12, 1.0);
            self.draw_slot_bar.draw_abs(cx, Rect { pos: origin, size: dvec2(w, tab_h) });
            let n = self.stage.slots.len().max(1);
            let tab_w = ((w - 8.0) / n as f64).clamp(64.0, 140.0);
            let mut tx = origin.x + 4.0;
            for s in self.stage.slots.clone() {
                if tx + 32.0 > origin.x + w {
                    break;
                }
                let actual_w = tab_w.min(origin.x + w - tx - 2.0);
                let tr = Rect { pos: dvec2(tx, origin.y + 3.0), size: dvec2(actual_w - 3.0, tab_h - 6.0) };
                let focused = s.surface == self.stage.focus;
                self.draw_pill.color = if focused { vec4(0.16, 0.28, 0.45, 1.0) } else { vec4(0.10, 0.13, 0.19, 1.0) };
                self.draw_pill.draw_abs(cx, tr);
                self.draw_label.color = if focused { vec4(0.95, 0.97, 1.0, 1.0) } else { vec4(0.65, 0.70, 0.78, 1.0) };
                let max_ch = ((actual_w - 24.0) / 6.8).max(3.0) as usize;
                let title: String = s.surface.chars().take(max_ch).collect();
                self.draw_label.draw_abs(cx, dvec2(tr.pos.x + 7.0, tr.pos.y + 4.0), &title);
                let close_r = Rect { pos: dvec2(tr.pos.x + tr.size.x - 18.0, tr.pos.y), size: dvec2(18.0, tr.size.y) };
                self.draw_sub.color = vec4(0.62, 0.67, 0.75, 1.0);
                self.draw_sub.draw_abs(cx, dvec2(close_r.pos.x + 4.0, tr.pos.y + 5.0), "x");
                self.deck_hits.push(DeckTabHit { surface: s.surface, tab_rect: tr, close_rect: close_r });
                tx += actual_w;
            }
            cx.walk_turtle(Walk::fixed(w, tab_h));
        }

        let active = self
            .stage
            .slots
            .iter()
            .find(|s| s.surface == self.stage.focus)
            .or_else(|| self.stage.slots.first())
            .cloned();
        let Some(slot) = active else { return };
        let body_h = (h - tab_h).max(1.0);
        let body_rect = Rect { pos: dvec2(origin.x, origin.y + tab_h), size: dvec2(w, body_h) };
        self.draw_clipped_slot_body(cx, scope, &slot.surface, Walk::fixed(w, body_h));
        self.last_slot_sizes.push((slot.surface.clone(), w, body_h));
        self.slot_hits.push(SlotHit {
            surface: slot.surface,
            header_rect: Rect::default(),
            body_rect,
            btn_collapse: Rect::default(),
            btn_mode: Rect::default(),
            btn_close: Rect::default(),
        });
    }

    fn draw_stack(&mut self, cx: &mut Cx2d, scope: &mut Scope, origin: Vec2d, w: f64, h: f64) {
        let slots = self.stage.slots.clone();
        if slots.is_empty() {
            self.max_stage_scroll = 0.0;
            return;
        }
        // Fast path: 1 unpinned Fill slot -> zero mini-header overhead.
        if slots.len() == 1 && slots[0].mode == SlotMode::Fill && !slots[0].pinned {
            self.max_stage_scroll = 0.0;
            self.stage_scroll_y = 0.0;
            let s = &slots[0];
            let body_rect = Rect { pos: origin, size: dvec2(w, h) };
            self.draw_clipped_slot_body(cx, scope, &s.surface, Walk::fixed(w, h));
            self.last_slot_sizes.push((s.surface.clone(), w, h));
            self.slot_hits.push(SlotHit {
                surface: s.surface.clone(),
                header_rect: Rect::default(),
                body_rect,
                btn_collapse: Rect::default(),
                btn_mode: Rect::default(),
                btn_close: Rect::default(),
            });
            return;
        }

        // Precompute header + compact/fixed heights so Fill slots share remaining viewport cleanly.
        let mut fixed_sum = 0.0;
        let mut fill_weight_sum = 0.0;
        for s in &slots {
            let hdr_h = if s.mode == SlotMode::Header { 26.0 } else { 22.0 };
            fixed_sum += hdr_h;
            match s.mode {
                SlotMode::Header => {}
                SlotMode::Compact => {
                    let est = s.px.or_else(|| self.measured_compact_h.get(&s.surface).copied()).unwrap_or(168.0);
                    fixed_sum += est;
                }
                SlotMode::Fill => {
                    fill_weight_sum += s.weight.max(0.1);
                }
            }
        }
        let avail_fill = (h - fixed_sum).max(0.0);

        // Estimate total content height for outer stage scroll clamping.
        let mut est_total = 0.0;
        for s in &slots {
            est_total += if s.mode == SlotMode::Header { 26.0 } else { 22.0 };
            match s.mode {
                SlotMode::Header => {}
                SlotMode::Compact => {
                    est_total += s.px.or_else(|| self.measured_compact_h.get(&s.surface).copied()).unwrap_or(168.0);
                }
                SlotMode::Fill => {
                    let share = if fill_weight_sum > 0.0 { avail_fill * (s.weight.max(0.1) / fill_weight_sum) } else { 140.0 };
                    est_total += share.max(120.0);
                }
            }
        }
        self.max_stage_scroll = (est_total - h).max(0.0);
        self.stage_scroll_y = self.stage_scroll_y.clamp(0.0, self.max_stage_scroll);

        cx.begin_turtle(
            Walk::fixed(w, h),
            Layout { scroll: dvec2(0.0, self.stage_scroll_y), clip_x: true, clip_y: true, ..Layout::flow_down() },
        );

        let mut cur_y = origin.y - self.stage_scroll_y;
        let has_fill = fill_weight_sum > 0.0;

        for s in &slots {
            let hdr_h = if s.mode == SlotMode::Header { 26.0 } else { 22.0 };
            let hdr_rect = Rect { pos: dvec2(origin.x, cur_y), size: dvec2(w, hdr_h) };
            let (btn_collapse, btn_mode, btn_close) = self.draw_slot_header(cx, hdr_rect, s);
            cx.walk_turtle(Walk::fixed(w, hdr_h));
            cur_y += hdr_h;

            match s.mode {
                SlotMode::Header => {
                    self.slot_hits.push(SlotHit {
                        surface: s.surface.clone(),
                        header_rect: hdr_rect,
                        body_rect: Rect::default(),
                        btn_collapse,
                        btn_mode,
                        btn_close,
                    });
                }
                SlotMode::Compact => {
                    let max_compact = (h * 0.62).clamp(180.0, 420.0);
                    let walk = match s.px {
                        Some(px) => Walk::fixed(w, px.min(max_compact)),
                        None => Walk {
                            width: Size::fill(),
                            height: Size::Fit {
                                min: Some(FitBound::Abs(44.0)),
                                max: Some(FitBound::Abs(max_compact)),
                            },
                            ..Walk::default()
                        },
                    };
                    let used_h = self.draw_clipped_slot_body(cx, scope, &s.surface, walk);
                    if s.px.is_none() && used_h > 1.0 {
                        let prev = self.measured_compact_h.insert(s.surface.clone(), used_h);
                        if has_fill && prev.map_or(true, |p| (p - used_h).abs() > 1.5) {
                            cx.redraw_all();
                        }
                    }
                    let body_rect = Rect { pos: dvec2(origin.x, cur_y), size: dvec2(w, used_h) };
                    cur_y += used_h;
                    self.last_slot_sizes.push((s.surface.clone(), w, used_h));
                    self.slot_hits.push(SlotHit {
                        surface: s.surface.clone(),
                        header_rect: hdr_rect,
                        body_rect,
                        btn_collapse,
                        btn_mode,
                        btn_close,
                    });
                }
                SlotMode::Fill => {
                    let share = avail_fill * (s.weight.max(0.1) / fill_weight_sum.max(0.1));
                    let body_h = share.max(120.0);
                    self.draw_clipped_slot_body(cx, scope, &s.surface, Walk::fixed(w, body_h));
                    let body_rect = Rect { pos: dvec2(origin.x, cur_y), size: dvec2(w, body_h) };
                    cur_y += body_h;
                    self.last_slot_sizes.push((s.surface.clone(), w, body_h));
                    self.slot_hits.push(SlotHit {
                        surface: s.surface.clone(),
                        header_rect: hdr_rect,
                        body_rect,
                        btn_collapse,
                        btn_mode,
                        btn_close,
                    });
                }
            }
        }

        cx.end_turtle();
    }

    fn draw_grid(&mut self, cx: &mut Cx2d, scope: &mut Scope, origin: Vec2d, w: f64, h: f64) {
        let slots: Vec<_> = self.stage.slots.iter().filter(|s| s.mode != SlotMode::Header).cloned().collect();
        if slots.is_empty() {
            self.max_stage_scroll = 0.0;
            return;
        }
        let rows = (slots.len() + 1) / 2;
        let gap = 4.0;
        let col_w = ((w - gap) * 0.5).max(80.0);
        let row_h = ((h - gap * (rows.saturating_sub(1) as f64)) / rows as f64).max(148.0);
        let total_h = rows as f64 * row_h + rows.saturating_sub(1) as f64 * gap;
        self.max_stage_scroll = (total_h - h).max(0.0);
        self.stage_scroll_y = self.stage_scroll_y.clamp(0.0, self.max_stage_scroll);

        cx.begin_turtle(
            Walk::fixed(w, h),
            Layout { scroll: dvec2(0.0, self.stage_scroll_y), clip_x: true, clip_y: true, ..Layout::flow_down() },
        );

        for r in 0..rows {
            if r > 0 {
                cx.walk_turtle(Walk::fixed(w, gap));
            }
            cx.begin_turtle(Walk::fixed(w, row_h), Layout::flow_right());
            for c in 0..2 {
                let idx = r * 2 + c;
                if idx >= slots.len() {
                    break;
                }
                if c > 0 {
                    cx.walk_turtle(Walk::fixed(gap, row_h));
                }
                let s = &slots[idx];
                let cell_w = if idx == slots.len() - 1 && c == 0 { w } else { col_w };
                let cell_x = origin.x + if c == 1 { col_w + gap } else { 0.0 };
                let cell_y = origin.y - self.stage_scroll_y + r as f64 * (row_h + gap);

                cx.begin_turtle(Walk::fixed(cell_w, row_h), Layout::flow_down());
                let hdr_h = 20.0;
                let hdr_rect = Rect { pos: dvec2(cell_x, cell_y), size: dvec2(cell_w, hdr_h) };
                let (btn_collapse, btn_mode, btn_close) = self.draw_slot_header(cx, hdr_rect, s);
                cx.walk_turtle(Walk::fixed(cell_w, hdr_h));

                let body_h = (row_h - hdr_h).max(40.0);
                self.draw_clipped_slot_body(cx, scope, &s.surface, Walk::fixed(cell_w, body_h));
                let body_rect = Rect { pos: dvec2(cell_x, cell_y + hdr_h), size: dvec2(cell_w, body_h) };
                self.last_slot_sizes.push((s.surface.clone(), cell_w, body_h));
                self.slot_hits.push(SlotHit {
                    surface: s.surface.clone(),
                    header_rect: hdr_rect,
                    body_rect,
                    btn_collapse,
                    btn_mode,
                    btn_close,
                });
                cx.end_turtle();
            }
            cx.end_turtle();
        }
        cx.end_turtle();
    }

    fn draw_clipped_slot_body(&mut self, cx: &mut Cx2d, scope: &mut Scope, name: &str, walk: Walk) -> f64 {
        let Some(splash) = self.surface(name) else {
            cx.walk_turtle(walk);
            return 0.0;
        };
        cx.begin_turtle(walk, Layout { clip_x: true, clip_y: true, ..Layout::flow_down() });
        let inner_walk = match walk.height {
            Size::Fit { min, max } => Walk { width: Size::fill(), height: Size::Fit { min, max }, ..Walk::default() },
            _ => Walk::fill(),
        };
        let t0 = std::time::Instant::now();
        splash.draw_walk_all(cx, scope, inner_walk);
        let dt_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let rect = cx.end_turtle();
        self.last_draw_ms.push((name.to_string(), dt_ms));
        rect.size.y
    }

    fn draw_slot_header(&mut self, cx: &mut Cx2d, r: Rect, slot: &crate::stage::StageSlot) -> (Rect, Rect, Rect) {
        let focused = slot.surface == self.stage.focus;
        self.draw_slot_bar.color = if focused { vec4(0.10, 0.15, 0.23, 1.0) } else { vec4(0.07, 0.09, 0.13, 1.0) };
        self.draw_slot_bar.draw_abs(cx, r);

        self.draw_dot.color = if focused {
            vec4(0.24, 0.86, 0.52, 1.0)
        } else if slot.pinned {
            vec4(0.37, 0.83, 1.0, 1.0)
        } else {
            vec4(0.35, 0.40, 0.48, 1.0)
        };
        self.draw_dot.draw_abs(cx, Rect { pos: dvec2(r.pos.x + 8.0, r.pos.y + (r.size.y - 6.0) * 0.5), size: dvec2(6.0, 6.0) });

        let tag = if slot.pinned { " · pin" } else if slot.mode == SlotMode::Header { " (folded)" } else { "" };
        let title = format!("{}{tag}", slot.surface);
        self.draw_sub.color = if focused { vec4(0.88, 0.92, 0.97, 1.0) } else { vec4(0.62, 0.67, 0.75, 1.0) };
        self.draw_sub.draw_abs(cx, dvec2(r.pos.x + 20.0, r.pos.y + (r.size.y - 12.0) * 0.5), &title);

        let bw = 26.0;
        let close_r = Rect { pos: dvec2(r.pos.x + r.size.x - bw, r.pos.y), size: dvec2(bw, r.size.y) };
        let mode_r = Rect { pos: dvec2(close_r.pos.x - bw, r.pos.y), size: dvec2(bw, r.size.y) };
        let col_r = Rect { pos: dvec2(mode_r.pos.x - bw, r.pos.y), size: dvec2(bw, r.size.y) };

        self.draw_sub.color = vec4(0.66, 0.72, 0.80, 1.0);
        let col_sym = if slot.mode == SlotMode::Header { "+" } else { "-" };
        let mode_sym = if slot.mode == SlotMode::Compact { "[]" } else { "<>" };
        let ty = r.pos.y + (r.size.y - 12.0) * 0.5;
        self.draw_sub.draw_abs(cx, dvec2(col_r.pos.x + 9.0, ty), col_sym);
        self.draw_sub.draw_abs(cx, dvec2(mode_r.pos.x + 6.0, ty), mode_sym);
        self.draw_sub.draw_abs(cx, dvec2(close_r.pos.x + 9.0, ty), "x");

        (col_r, mode_r, close_r)
    }

    fn draw_strip(&mut self, cx: &mut Cx2d, rect: Rect) {
        self.draw_strip.color = if self.guard.is_some() { vec4(0.96, 0.62, 0.1, 1.0) } else { vec4(0.02, 0.027, 0.043, 1.0) };
        self.strip_rect = Rect { pos: rect.pos, size: dvec2(rect.size.x, self.strip_height) };
        self.draw_strip.draw_abs(cx, self.strip_rect);

        let mut right_x = rect.pos.x + rect.size.x - 10.0;
        for (on, text, color) in [
            (self.marks.mic, "MIC", vec4(1.0, 0.35, 0.35, 1.0)),
            (self.marks.loud, "LOUD", vec4(1.0, 0.75, 0.2, 1.0)),
            (self.marks.test_taps, "TEST", vec4(1.0, 0.2, 1.0, 1.0)),
        ] {
            if on {
                right_x -= 7.8 * text.len() as f64 + 8.0;
                self.draw_mark.color = color;
                self.draw_mark.draw_abs(cx, dvec2(right_x, rect.pos.y + 9.0), text);
            }
        }

        if self.guard.is_some() {
            self.home_rect = Rect::default();
            self.center_rect = Rect::default();
            self.draw_label.color = vec4(0.0, 0.0, 0.0, 1.0);
            self.draw_label.draw_abs(cx, rect.pos + dvec2(12.0, 9.0), "tb-shell   GUARD");
            return;
        }

        // Left zone: `[ Home ]`
        self.home_rect = Rect { pos: rect.pos, size: dvec2(58.0, self.strip_height) };
        self.draw_pill.color = vec4(0.09, 0.12, 0.18, 1.0);
        self.draw_pill.draw_abs(cx, Rect { pos: dvec2(rect.pos.x + 6.0, rect.pos.y + 4.0), size: dvec2(48.0, self.strip_height - 8.0) });
        self.draw_label.color = vec4(0.84, 0.89, 0.95, 1.0);
        self.draw_label.draw_abs(cx, dvec2(rect.pos.x + 13.0, rect.pos.y + 9.0), "Home");

        // Passive right status (`Wi-Fi · Bat% · HH:MM`)
        if !self.status_right.is_empty() {
            let sw = self.status_right.chars().count() as f64 * 5.9 + 8.0;
            right_x -= sw;
            self.draw_sub.color = vec4(0.56, 0.62, 0.70, 1.0);
            self.draw_sub.draw_abs(cx, dvec2(right_x, rect.pos.y + 10.0), &self.status_right);
        }

        // Center zone: `‹ focus (+K) ›` + optional shell note
        self.center_rect = Rect {
            pos: dvec2(rect.pos.x + 58.0, rect.pos.y),
            size: dvec2((right_x - (rect.pos.x + 58.0)).max(40.0), self.strip_height),
        };
        let focus = if self.stage.focus.is_empty() { "-" } else { self.stage.focus.as_str() };
        let extra = self.stage.extra_slot_count();
        let stage_badge = if extra > 0 { format!("< {focus} +{extra} >") } else { format!("< {focus} >") };
        let center_text = if self.note.is_empty() { stage_badge } else { format!("{stage_badge}  {}", self.note) };
        let max_chars = ((self.center_rect.size.x - 10.0) / 6.3).max(6.0) as usize;
        let clipped_text: String = center_text.chars().take(max_chars).collect();
        self.draw_label.color = vec4(0.72, 0.78, 0.86, 1.0);
        self.draw_label.draw_abs(cx, dvec2(self.center_rect.pos.x + 6.0, rect.pos.y + 9.0), &clipped_text);
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
        y += 8.0;
        for line in wrap(&guard.who, ((w - 48.0) / 7.8) as usize) {
            self.draw_guard_small.draw_abs(cx, dvec2(rect.pos.x + 24.0, y), &line);
            y += 18.0;
        }
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
        self.measured_compact_h.remove(name);
        splash.set_text(cx, "");
        cx.widget_tree_mark_dirty(self.uid);
        cx.redraw_all();
        true
    }

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

    pub fn take_actions(&mut self) -> Vec<ShellAction> {
        std::mem::take(&mut self.actions)
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
