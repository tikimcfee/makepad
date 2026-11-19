//! The window's one Rust widget: the 3-zone protected strip on top (`Home`,
//! `Stage` switcher / swipe tasks, `Wi-Fi · Bat% · HH:MM` + `MIC`/`LOUD`),
//! the general `1..N` multi-surface `Stage` (`stack` / `grid` / `deck`) under it,
//! and the root `GUARD` drawn in the final overlay tier gating all input.

use crate::jit_slot::JitRegistry;
use crate::stage::{SlotMode, Stage, StageMode};
use makepad_widgets::makepad_platform::event::TouchState;
use makepad_widgets::*;
use std::collections::HashMap;

#[derive(Script, ScriptHook, Debug)]
#[repr(C)]
pub struct DrawJitTex {
    #[deref]
    draw_super: DrawQuad,
}

script_mod! {
    use mod.prelude.widgets.*

    set_type_default() do #(DrawJitTex::script_shader(vm)){
        ..mod.draw.DrawQuad
        tex: texture_2d(float)
        pixel: fn() {
            return self.tex.sample(self.pos)
        }
    }

    mod.widgets.ShellBase = #(Shell::register_widget(vm))
    mod.widgets.Shell = set_type_default() do mod.widgets.ShellBase {
        width: Fill
        height: Fill
        strip_height: 32.0
        draw_strip +: { color: #x05070b }
        draw_pill +: { color: #x141b28 }
        draw_slot_bar +: { color: #x121824 }
        draw_dot +: { color: #x3ddc84 }
        draw_jit_tex +: {}
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

/// Plain two-button modal confirmation sheet (`tbui confirm` / `tbui guard`).
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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum StripMode {
    #[default]
    Classic,
    Split,
    Below,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SafeArea {
    pub active: bool,
    pub top: f64,
    pub bottom: f64,
    pub left: f64,
    pub right: f64,
    pub strip_mode: StripMode,
    pub strip_y: f64,
    pub strip_h: f64,
    pub strip_space_top: f64,
    pub strip_space_bot: f64,
    pub strip_pad_l: f64,
    pub strip_pad_r: f64,
    pub punch_x: f64,
    pub punch_w: f64,
    pub punch_h: f64,
    pub corner_r: f64,
}

impl Default for SafeArea {
    fn default() -> Self {
        Self::classic(32.0)
    }
}

impl SafeArea {
    pub fn classic(strip_h: f64) -> Self {
        let h = if strip_h > 0.0 { strip_h } else { 32.0 };
        Self {
            active: false,
            top: h,
            bottom: 0.0,
            left: 0.0,
            right: 0.0,
            strip_mode: StripMode::Classic,
            strip_y: 0.0,
            strip_h: h,
            strip_space_top: 0.0,
            strip_space_bot: 0.0,
            strip_pad_l: 6.0,
            strip_pad_r: 10.0,
            punch_x: 192.0,
            punch_w: 0.0,
            punch_h: 0.0,
            corner_r: 0.0,
        }
    }

    pub fn from_json(val: Option<&makepad_micro_serde::JsonValue>, fallback_strip_h: f64) -> Self {
        use makepad_micro_serde::JsonValue;
        let Some(JsonValue::Object(m)) = val else {
            return Self::classic(fallback_strip_h);
        };
        let num = |k: &str| -> Option<f64> {
            match m.get(k)? {
                JsonValue::F64(f) if f.is_finite() => Some(*f),
                JsonValue::I64(n) => Some(*n as f64),
                JsonValue::U64(n) => Some(*n as f64),
                JsonValue::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
                _ => None,
            }
        };

        let raw_top = num("top").map(|v| v.max(0.0));
        let bottom = num("bottom").unwrap_or(0.0).max(0.0);
        let left = num("left").unwrap_or(0.0).max(0.0);
        let right = num("right").unwrap_or(0.0).max(0.0);
        let punch_x = num("punch_x").unwrap_or(192.0).max(0.0);
        let punch_w = num("punch_w").unwrap_or(0.0).max(0.0);
        let punch_h = num("punch_h").unwrap_or(0.0).max(0.0);
        let corner_r = num("corner_r").unwrap_or(0.0).max(0.0);

        let strip_mode = match m.get("strip_mode").and_then(|v| v.as_str()) {
            Some("split") => StripMode::Split,
            Some("below") => StripMode::Below,
            Some(_) => StripMode::Classic,
            None => {
                if punch_w > 0.0 {
                    StripMode::Split
                } else {
                    StripMode::Classic
                }
            }
        };

        let strip_space_top = num("strip_space_top").unwrap_or(0.0).max(0.0);
        let strip_space_bot = num("strip_space_bot").unwrap_or(0.0).max(0.0);
        let base_h = if fallback_strip_h > 0.0 { fallback_strip_h } else { 32.0 };
        let default_h = match strip_mode {
            StripMode::Split => {
                if punch_h > 0.0 {
                    punch_h
                } else {
                    72.0
                }
            }
            StripMode::Below => 36.0,
            StripMode::Classic => base_h,
        };
        let strip_h = num("strip_h").unwrap_or(default_h).clamp(24.0, 180.0);
        let raw_strip_y = num("strip_y").map(|v| v.max(0.0));
        let strip_y = match strip_mode {
            StripMode::Below => match raw_strip_y {
                Some(y) if y >= punch_h => y,
                _ => punch_h + strip_space_top,
            },
            _ => raw_strip_y.unwrap_or(0.0),
        };
        let strip_pad_l = num("strip_pad_l")
            .unwrap_or(if corner_r > 0.0 { 24.0 } else { 6.0 })
            .max(0.0);
        let strip_pad_r = num("strip_pad_r")
            .unwrap_or(if corner_r > 0.0 { 24.0 } else { 10.0 })
            .max(0.0);

        let computed_top = strip_y + strip_h + strip_space_bot;
        let top = match strip_mode {
            StripMode::Split | StripMode::Below => computed_top.max(raw_top.unwrap_or(0.0)),
            StripMode::Classic => raw_top.unwrap_or(computed_top).max(strip_h),
        };

        Self {
            active: true,
            top,
            bottom,
            left,
            right,
            strip_mode,
            strip_y,
            strip_h,
            strip_space_top,
            strip_space_bot,
            strip_pad_l,
            strip_pad_r,
            punch_x,
            punch_w,
            punch_h,
            corner_r,
        }
    }
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
    #[live] draw_jit_tex: DrawJitTex,
    #[live] draw_label: DrawText,
    #[live] draw_sub: DrawText,
    #[live] draw_mark: DrawText,
    #[live] draw_veil: DrawColor,
    #[live] draw_yes: DrawColor,
    #[live] draw_no: DrawColor,
    #[live] draw_guard_text: DrawText,
    #[live] draw_guard_small: DrawText,
    #[live] top_list: DrawList2d,
    #[live] pub strip_height: f64,
    #[rust] area: Area,
    #[rust] pub surfaces: Vec<(String, WidgetRef)>,
    #[rust] pub jit: JitRegistry,
    #[rust] pub stage: Stage,
    #[rust] pub marks: Marks,
    #[rust] pub safe_area: SafeArea,
    #[rust] pub status_right: String,
    #[rust] guard: Option<GuardView>,
    #[rust] pressed: Option<bool>,
    #[rust] raised_at: Option<std::time::Instant>,
    #[rust] pub guard_drawn_ms: Option<f64>,
    #[rust] yes_rect: Rect,
    #[rust] no_rect: Rect,
    #[rust] answer: Option<bool>,
    #[rust] strip_rect: Rect,
    #[rust] bar_band_rect: Rect,
    #[rust] punch_rect: Rect,
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
        if self.guard.is_some() {
            let (down, _, up) = pointer_down_move_up(event);
            let button_at = |p: Vec2d| {
                [(true, self.yes_rect), (false, self.no_rect)]
                    .into_iter()
                    .find(|(_, r)| r.contains(p))
                    .map(|(yes, _)| yes)
            };
            if let Some(p) = down {
                self.pressed = button_at(p);
            }
            if let Some(p) = up {
                if self.pressed.is_some() && button_at(p) == self.pressed {
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
                            if self.punch_rect.size.x > 0.0 && self.punch_rect.contains(p) {
                                // Dead camera punch-out zone: ignore tap cleanly.
                            } else if self.home_rect.contains(p) {
                                self.actions.push(ShellAction::GoHome);
                            } else if self.bar_band_rect.contains(p) {
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
            if let Some(inst) = self.jit.slots.get(target) {
                let body_pos = self
                    .slot_hits
                    .iter()
                    .find(|h| &h.surface == target)
                    .map(|h| h.body_rect.pos)
                    .unwrap_or_default();
                let (down, mv, up) = pointer_down_move_up(event);
                if let Some(p) = down {
                    let lx = (p.x - body_pos.x).max(0.0);
                    let ly = (p.y - body_pos.y).max(0.0);
                    inst.send_cmd(format!("mouse k=down x={lx:.1} y={ly:.1}"));
                } else if let Some(p) = mv {
                    let lx = (p.x - body_pos.x).max(0.0);
                    let ly = (p.y - body_pos.y).max(0.0);
                    inst.send_cmd(format!("mouse k=move x={lx:.1} y={ly:.1}"));
                } else if let Some(p) = up {
                    let lx = (p.x - body_pos.x).max(0.0);
                    let ly = (p.y - body_pos.y).max(0.0);
                    inst.send_cmd(format!("mouse k=up x={lx:.1} y={ly:.1}"));
                } else if let Event::Scroll(e) = event {
                    let lx = (e.abs.x - body_pos.x).max(0.0);
                    let ly = (e.abs.y - body_pos.y).max(0.0);
                    inst.send_cmd(format!("mouse k=scroll x={lx:.1} y={ly:.1} dx={:.1} dy={:.1}", e.scroll.x, e.scroll.y));
                } else if let Event::TextInput(e) = event {
                    if !e.input.is_empty() && !e.input.contains(char::is_whitespace) {
                        inst.send_cmd(format!("text t={}", e.input));
                    }
                }
            } else if let Some((_, splash)) = self.surfaces.iter().find(|(n, _)| n == target) {
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
        let top_clear = self.top_clearance();
        let stage_top = win_rect.pos.y + top_clear;
        let stage_w = win_rect.size.x.max(1.0);
        let stage_h = (win_rect.size.y - top_clear).max(1.0);

        // Advance past the reserved top strip clearance, then open a clipped stage viewport.
        cx.walk_turtle(Walk::fixed(stage_w, top_clear));
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
        if self.jit.contains(name) {
            let walk_jit = match walk.height {
                Size::Fit { .. } => Walk::fixed(
                    match walk.width {
                        Size::Fixed(w) => w,
                        _ => 360.0,
                    },
                    240.0,
                ),
                _ => walk,
            };
            cx.begin_turtle(walk_jit, Layout { clip_x: true, clip_y: true, ..Layout::flow_down() });
            let rect = cx.turtle().rect();
            let (tex_opt, status) = if let Some(inst) = self.jit.slots.get_mut(name) {
                inst.set_slot_size(rect.size.x.round().max(64.0) as u32, rect.size.y.round().max(64.0) as u32);
                inst.poll_texture(cx.cx)
            } else {
                (None, String::new())
            };
            if let Some(tex) = tex_opt {
                self.draw_jit_tex.draw_vars.set_texture(0, &tex);
                self.draw_jit_tex.draw_abs(cx, rect);
            } else {
                self.draw_veil.draw_abs(cx, rect);
                let mut y = rect.pos.y + 24.0;
                for line in wrap(&status, ((rect.size.x - 32.0) / 7.2).max(12.0) as usize) {
                    self.draw_label.color = vec4(0.82, 0.88, 0.96, 1.0);
                    self.draw_label.draw_abs(cx, dvec2(rect.pos.x + 16.0, y), &line);
                    y += 18.0;
                }
            }
            let out = cx.end_turtle();
            return out.size.y;
        }

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

    pub fn top_clearance(&self) -> f64 {
        if self.safe_area.active {
            self.safe_area.top.max(24.0)
        } else {
            self.strip_height.max(24.0)
        }
    }

    pub fn set_safe_area(&mut self, cx: &mut Cx, next: SafeArea) -> bool {
        if self.safe_area == next {
            return false;
        }
        self.safe_area = next;
        cx.redraw_all();
        true
    }

    /// Draws `MIC` and `LOUD` truth marks right-aligned from `right_edge` leftward,
    /// never crossing left of `min_x`, and returns the leftmost X reached.
    fn draw_marks_right(&mut self, cx: &mut Cx2d, right_edge: f64, min_x: f64, cy: f64) -> f64 {
        let mut cur_x = right_edge;
        for (on, text, fg, bg) in [
            (
                self.marks.mic,
                "MIC",
                vec4(1.0, 0.40, 0.40, 1.0),
                vec4(0.30, 0.08, 0.11, 1.0),
            ),
            (
                self.marks.loud,
                "LOUD",
                vec4(1.0, 0.78, 0.22, 1.0),
                vec4(0.27, 0.17, 0.05, 1.0),
            ),
        ] {
            if !on {
                continue;
            }
            let (tw, th) = measure_line(&self.draw_mark, cx, text);
            let pill_w = (tw + 10.0).max(28.0);
            let pill_h = (th + 6.0).clamp(18.0, 22.0);
            if cur_x < right_edge {
                cur_x -= 5.0;
            }
            let px = (cur_x - pill_w).max(min_x);
            let pr = Rect {
                pos: dvec2(px, cy - pill_h * 0.5),
                size: dvec2(pill_w.min((cur_x - min_x).max(0.0)), pill_h),
            };
            if pr.size.x >= tw {
                self.draw_pill.color = bg;
                self.draw_pill.draw_abs(cx, pr);
                self.draw_mark.color = fg;
                self.draw_mark.draw_abs(
                    cx,
                    dvec2(pr.pos.x + (pr.size.x - tw) * 0.5, cy - th * 0.5),
                    text,
                );
                cur_x = px;
            }
        }
        cur_x
    }

    /// Draws passive `status_right` (`wifi · 94% · 22:41`) right-aligned to `right_edge`
    /// within `avail_w`, gracefully dropping leading segments before truncating so the clock
    /// and battery stay whole when horizontal space is tight. Returns leftmost X reached.
    fn draw_status_right(&mut self, cx: &mut Cx2d, right_edge: f64, avail_w: f64, cy: f64) -> f64 {
        if self.status_right.is_empty() || avail_w < 18.0 {
            return right_edge;
        }
        let parts: Vec<&str> = self.status_right.split(" · ").collect();
        let mut chosen: Option<(String, f64, f64)> = None;
        for start in 0..parts.len() {
            let cand = parts[start..].join(" · ");
            let (w, h) = measure_line(&self.draw_sub, cx, &cand);
            if w <= avail_w {
                chosen = Some((cand, w, h));
                break;
            }
        }
        let (txt, tw, th) = chosen.unwrap_or_else(|| {
            let last = parts.last().copied().unwrap_or(self.status_right.as_str());
            fit_line_to_width(&self.draw_sub, cx, last, avail_w)
        });
        if txt.is_empty() || tw <= 0.0 {
            return right_edge;
        }
        let x = (right_edge - tw).max(right_edge - avail_w);
        self.draw_sub.color = vec4(0.58, 0.64, 0.73, 1.0);
        self.draw_sub.draw_abs(cx, dvec2(x, cy - th * 0.5), &txt);
        x
    }

    fn draw_strip(&mut self, cx: &mut Cx2d, rect: Rect) {
        let top_clear = self.top_clearance();
        self.draw_strip.color = vec4(0.02, 0.027, 0.043, 1.0);
        self.strip_rect = Rect {
            pos: rect.pos,
            size: dvec2(rect.size.x, top_clear),
        };
        self.draw_strip.draw_abs(cx, self.strip_rect);

        let (mode, bar_y, bar_h, space_top, pad_l, pad_r) = if self.safe_area.active {
            let st = if self.safe_area.strip_mode == StripMode::Split {
                self.safe_area.strip_space_top
            } else {
                0.0
            };
            (
                self.safe_area.strip_mode,
                rect.pos.y + self.safe_area.strip_y,
                self.safe_area.strip_h,
                st,
                self.safe_area.strip_pad_l,
                self.safe_area.strip_pad_r,
            )
        } else {
            (
                StripMode::Classic,
                rect.pos.y,
                self.strip_height,
                0.0,
                6.0,
                10.0,
            )
        };

        self.bar_band_rect = Rect {
            pos: dvec2(rect.pos.x, bar_y),
            size: dvec2(rect.size.x, bar_h),
        };
        let band_top = bar_y + space_top;
        let band_h = (bar_h - space_top).max(20.0);

        let focus = if self.stage.focus.is_empty() {
            "-".to_string()
        } else {
            self.stage.focus.clone()
        };
        let extra = self.stage.extra_slot_count();

        if mode == StripMode::Split && self.safe_area.punch_w > 0.0 {
            let punch_l = (rect.pos.x + self.safe_area.punch_x - self.safe_area.punch_w * 0.5)
                .clamp(rect.pos.x, rect.pos.x + rect.size.x);
            let punch_r = (rect.pos.x + self.safe_area.punch_x + self.safe_area.punch_w * 0.5)
                .clamp(punch_l, rect.pos.x + rect.size.x);
            self.punch_rect = Rect {
                pos: dvec2(punch_l, rect.pos.y),
                size: dvec2(
                    punch_r - punch_l,
                    self.safe_area.punch_h.max((bar_y - rect.pos.y) + bar_h),
                ),
            };

            // 1. Left flank: `[pad_l .. punch_l - 4]` (`Home` + `< focus >`, plus 2nd-row note if active)
            let left_x = rect.pos.x + pad_l;
            let left_end = (punch_l - 4.0).max(left_x);
            let has_note_row = !self.note.is_empty() && band_h >= 46.0;
            let left_cy = if has_note_row {
                band_top + band_h * 0.36
            } else {
                band_top + band_h * 0.50
            };

            let (hw, hh) = measure_line(&self.draw_label, cx, "Home");
            let pill_w = (hw + 14.0).clamp(42.0, 54.0).min((left_end - left_x).max(0.0));
            let pill_h = 26.0_f64.min((band_h - 4.0).max(20.0));
            let pill_rect = Rect {
                pos: dvec2(left_x, left_cy - pill_h * 0.5),
                size: dvec2(pill_w, pill_h),
            };
            if pill_w >= hw {
                self.draw_pill.color = vec4(0.09, 0.12, 0.18, 1.0);
                self.draw_pill.draw_abs(cx, pill_rect);
                self.draw_label.color = vec4(0.84, 0.89, 0.95, 1.0);
                self.draw_label.draw_abs(
                    cx,
                    dvec2(pill_rect.pos.x + (pill_w - hw) * 0.5, left_cy - hh * 0.5),
                    "Home",
                );
            }
            let home_hit_w = ((pill_rect.pos.x - rect.pos.x) + pill_w + 4.0).min(punch_l - rect.pos.x);
            self.home_rect = Rect {
                pos: dvec2(rect.pos.x, bar_y),
                size: dvec2(home_hit_w.max(0.0), bar_h),
            };

            let focus_x = pill_rect.pos.x + pill_w + 6.0;
            let focus_avail = (left_end - focus_x).max(0.0);
            self.center_rect = Rect {
                pos: dvec2(self.home_rect.pos.x + self.home_rect.size.x, bar_y),
                size: dvec2(
                    (punch_l - (self.home_rect.pos.x + self.home_rect.size.x)).max(0.0),
                    bar_h,
                ),
            };

            if focus_avail >= 16.0 {
                let roomy = if extra > 0 {
                    format!("< {focus} +{extra} >")
                } else {
                    format!("< {focus} >")
                };
                let tight = if extra > 0 {
                    format!("<{focus}+{extra}>")
                } else {
                    format!("<{focus}>")
                };
                let mut candidates = Vec::with_capacity(3);
                if !has_note_row && !self.note.is_empty() {
                    candidates.push(format!("{tight} {}", self.note));
                }
                candidates.push(roomy);
                candidates.push(tight.clone());

                let mut picked: Option<(String, f64, f64)> = None;
                for c in &candidates {
                    let (w, h) = measure_line(&self.draw_label, cx, c);
                    if w <= focus_avail {
                        picked = Some((c.clone(), w, h));
                        break;
                    }
                }
                let (ftxt, fw, fh) = picked
                    .unwrap_or_else(|| fit_line_to_width(&self.draw_label, cx, &tight, focus_avail));
                if !ftxt.is_empty() && fw > 0.0 {
                    self.draw_label.color = vec4(0.72, 0.78, 0.86, 1.0);
                    self.draw_label
                        .draw_abs(cx, dvec2(focus_x, left_cy - fh * 0.5), &ftxt);
                }
            }

            if has_note_row {
                let note_cy = band_top + band_h * 0.72;
                let note_avail = (left_end - left_x).max(0.0);
                let (ntxt, nw, nh) = fit_line_to_width(&self.draw_sub, cx, &self.note, note_avail);
                if !ntxt.is_empty() && nw > 0.0 {
                    self.draw_sub.color = vec4(0.95, 0.76, 0.35, 1.0);
                    self.draw_sub
                        .draw_abs(cx, dvec2(left_x, note_cy - nh * 0.5), &ntxt);
                }
            }

            // 2. Right flank: `[punch_r + 4 .. W - pad_r]` (`MIC`/`LOUD` priority + `wifi · Bat% · HH:MM`)
            let right_edge = (rect.pos.x + rect.size.x - pad_r).max(punch_r);
            let right_start = (punch_r + 4.0).min(right_edge);
            let right_avail = (right_edge - right_start).max(0.0);
            let any_mark = self.marks.mic || self.marks.loud;
            if any_mark && band_h >= 46.0 {
                let mark_cy = band_top + band_h * 0.34;
                let stat_cy = band_top + band_h * 0.69;
                self.draw_marks_right(cx, right_edge, right_start, mark_cy);
                self.draw_status_right(cx, right_edge, right_avail, stat_cy);
            } else {
                let cy = band_top + band_h * 0.50;
                let marks_left = self.draw_marks_right(cx, right_edge, right_start, cy);
                let stat_edge = if any_mark {
                    (marks_left - 6.0).max(right_start)
                } else {
                    right_edge
                };
                let stat_avail = (stat_edge - right_start).max(0.0);
                self.draw_status_right(cx, stat_edge, stat_avail, cy);
            }
            return;
        }

        // Full-width strip (`Classic` or `Below` punch-out mode)
        self.punch_rect = Rect::default();
        let cy = band_top + band_h * 0.50;
        let left_x = rect.pos.x + pad_l;
        let right_edge = (rect.pos.x + rect.size.x - pad_r).max(left_x + 80.0);

        let (hw, hh) = measure_line(&self.draw_label, cx, "Home");
        let pill_w = (hw + 14.0).clamp(44.0, 54.0);
        let pill_h = (band_h - 8.0).clamp(20.0, 28.0);
        let pill_rect = Rect {
            pos: dvec2(left_x, cy - pill_h * 0.5),
            size: dvec2(pill_w, pill_h),
        };
        self.draw_pill.color = vec4(0.09, 0.12, 0.18, 1.0);
        self.draw_pill.draw_abs(cx, pill_rect);
        self.draw_label.color = vec4(0.84, 0.89, 0.95, 1.0);
        self.draw_label.draw_abs(
            cx,
            dvec2(pill_rect.pos.x + (pill_w - hw) * 0.5, cy - hh * 0.5),
            "Home",
        );
        let home_hit_w = (pill_rect.pos.x - rect.pos.x) + pill_w + 4.0;
        self.home_rect = Rect {
            pos: dvec2(rect.pos.x, bar_y),
            size: dvec2(home_hit_w, bar_h),
        };

        let focus_x = pill_rect.pos.x + pill_w + 6.0;
        let any_mark = self.marks.mic || self.marks.loud;
        let marks_left = self.draw_marks_right(cx, right_edge, focus_x + 36.0, cy);
        let stat_edge = if any_mark {
            (marks_left - 6.0).max(focus_x + 36.0)
        } else {
            right_edge
        };
        let stat_left = self.draw_status_right(cx, stat_edge, (stat_edge - (focus_x + 42.0)).max(0.0), cy);

        let center_right = (stat_left - 6.0).max(focus_x);
        self.center_rect = Rect {
            pos: dvec2(self.home_rect.pos.x + self.home_rect.size.x, bar_y),
            size: dvec2(
                (center_right - (self.home_rect.pos.x + self.home_rect.size.x)).max(36.0),
                bar_h,
            ),
        };

        let stage_badge = if extra > 0 {
            format!("< {focus} +{extra} >")
        } else {
            format!("< {focus} >")
        };
        let center_text = if self.note.is_empty() {
            stage_badge
        } else {
            format!("{stage_badge}  {}", self.note)
        };
        let (clipped, cw, ch) =
            fit_line_to_width(&self.draw_label, cx, &center_text, (center_right - focus_x).max(0.0));
        if !clipped.is_empty() && cw > 0.0 {
            self.draw_label.color = vec4(0.72, 0.78, 0.86, 1.0);
            self.draw_label
                .draw_abs(cx, dvec2(focus_x, cy - ch * 0.5), &clipped);
        }
    }

    fn draw_guard(&mut self, cx: &mut Cx2d, rect: Rect, guard: &GuardView) {
        let top_clear = self.top_clearance();
        let top = rect.pos.y + top_clear;
        let (w, h) = (rect.size.x, (rect.size.y - top_clear).max(1.0));
        self.draw_veil.draw_abs(cx, Rect { pos: dvec2(rect.pos.x, top), size: dvec2(w, h) });
        let mut y = top + 36.0;
        for line in wrap(&guard.text, ((w - 48.0) / 9.5) as usize) {
            self.draw_guard_text.draw_abs(cx, dvec2(rect.pos.x + 24.0, y), &line);
            y += 26.0;
        }
        if !guard.who.is_empty() {
            y += 8.0;
            for line in wrap(&guard.who, ((w - 48.0) / 7.8) as usize) {
                self.draw_guard_small.draw_abs(cx, dvec2(rect.pos.x + 24.0, y), &line);
                y += 18.0;
            }
        }
        let bw = w - 48.0;
        let (yes_y, no_y) = (top + h - 200.0, top + h - 110.0);
        self.draw_yes.color = vec4(0.14, 0.24, 0.38, 1.0);
        self.draw_no.color = vec4(0.11, 0.13, 0.19, 1.0);
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
        let removed_jit = self.jit.remove(name);
        if let Some(at) = self.surfaces.iter().position(|(n, _)| n == name) {
            let (_, splash) = self.surfaces.remove(at);
            self.measured_compact_h.remove(name);
            splash.set_text(cx, "");
            cx.widget_tree_mark_dirty(self.uid);
            cx.redraw_all();
            return true;
        }
        if removed_jit {
            self.measured_compact_h.remove(name);
            cx.redraw_all();
        }
        removed_jit
    }

    pub fn set_guard(&mut self, cx: &mut Cx, guard: Option<GuardView>) {
        self.guard = guard;
        self.pressed = None;
        self.answer = None;
        self.guard_drawn_ms = None;
        self.raised_at = self.guard.as_ref().map(|_| std::time::Instant::now());
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

fn measure_line(dt: &DrawText, cx: &mut Cx2d, text: &str) -> (f64, f64) {
    if text.is_empty() {
        return (0.0, 0.0);
    }
    let laid = dt.layout(cx, 0.0, 0.0, None, false, Align::default(), text);
    let Some(row) = laid.rows.first() else {
        return (0.0, 0.0);
    };
    let w = (row.width_in_lpxs * dt.font_scale) as f64;
    let h = ((row.ascender_in_lpxs - row.descender_in_lpxs) * dt.font_scale) as f64;
    (w, h)
}

fn fit_line_to_width(dt: &DrawText, cx: &mut Cx2d, text: &str, max_w: f64) -> (String, f64, f64) {
    if text.is_empty() || max_w <= 4.0 {
        return (String::new(), 0.0, 0.0);
    }
    let (w0, h0) = measure_line(dt, cx, text);
    if w0 <= max_w {
        return (text.to_string(), w0, h0);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    let mut best = (String::new(), 0.0, h0);
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let mut cand: String = chars[..mid].iter().collect();
        cand.push('…');
        let (cw, ch) = measure_line(dt, cx, &cand);
        if cw <= max_w {
            best = (cand, cw, ch);
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    best
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
