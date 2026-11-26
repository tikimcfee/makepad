#![allow(dead_code)]
//! Public module interface (`DeskModule`) and panel drawing helpers (`PanelDraw`) for One Desk.
//!
//! Any agent can add or replace a module (`TAPE`, `TRANSPORT`, `OUT`, `VOICE`, `BANDS`, `LYRICS`,
//! `RACK`, `LINK`) by implementing `DeskModule` in its own file without touching the orthographic
//! camera, shaders, pan/zoom/pinch engine, `< 7 mm` zoom guard, or label scaling.
//!
//! Coordinate system:
//! - Panel units are Y-down (`0..PANEL_W` across, `0..PANEL_H` down; `1 u ~ 1 mm` on a soundboard).
//! - All controls, rects, and touches delivered to modules are in panel units.

use super::mesh::mat4_trs_z;
use super::wire::{DeskCmds, DeskState, SectionInfo};
use makepad_widgets::{vec4, Mat4f, Vec4f};

pub const PANEL_W: f32 = 240.0;
pub const PANEL_H: f32 = 384.0;

// Shared rack grid rectangles (4 u outer margin, 4 u inter-module gutter)
pub const RECT_TAPE: PanelRect      = PanelRect { x: 4.0,   y: 4.0,   w: 232.0, h: 64.0 };
pub const RECT_TRANSPORT: PanelRect = PanelRect { x: 4.0,   y: 72.0,  w: 232.0, h: 42.0 };
pub const RECT_LYRICS: PanelRect    = PanelRect { x: 4.0,   y: 118.0, w: 232.0, h: 50.0 };
pub const RECT_BANDS: PanelRect     = PanelRect { x: 4.0,   y: 172.0, w: 232.0, h: 80.0 };
pub const RECT_OUT: PanelRect       = PanelRect { x: 4.0,   y: 256.0, w: 114.0, h: 72.0 };
pub const RECT_VOICE: PanelRect     = PanelRect { x: 122.0, y: 256.0, w: 114.0, h: 72.0 };
pub const RECT_RACK: PanelRect      = PanelRect { x: 4.0,   y: 332.0, w: 114.0, h: 48.0 };
pub const RECT_LINK: PanelRect      = PanelRect { x: 122.0, y: 332.0, w: 114.0, h: 48.0 };

#[derive(Clone, Copy, Debug, Default)]
pub struct PanelRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl PanelRect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
    pub fn cx(&self) -> f32 { self.x + self.w * 0.5 }
    pub fn cy(&self) -> f32 { self.y + self.h * 0.5 }
    pub fn right(&self) -> f32 { self.x + self.w }
    pub fn bottom(&self) -> f32 { self.y + self.h }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug)]
pub struct ControlHit {
    pub id: u32,
    /// Characteristic grab dimension in panel units (e.g. knob diameter, key min edge, fader width).
    /// When `min_grab_u * zoom < 40.0 px` (~7 mm on glass), a touch zooms to the module instead.
    pub min_grab_u: f32,
}

impl ControlHit {
    pub const fn new(id: u32, min_grab_u: f32) -> Self {
        Self { id, min_grab_u }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchKind {
    Down,
    Move,
    Up,
    DoubleTap,
}

#[derive(Clone, Copy, Debug)]
pub struct ModuleTouch {
    pub kind: TouchKind,
    pub control_id: u32,
    pub x: f32,          // panel units
    pub y: f32,          // panel units
    pub dx: f32,         // delta panel units since previous event
    pub dy: f32,         // delta panel units since previous event
    pub time: f64,       // monotonic touch timestamp (seconds)
    pub px_per_u: f32,   // current zoom scale
}

pub trait DeskModule {
    fn id(&self) -> &'static str;
    fn title(&self) -> &'static str;
    fn rect(&self) -> PanelRect;
    fn needs_animation(&self, st: &DeskState) -> bool;
    fn step(&mut self, dt: f32, st: &DeskState, cmds: &mut DeskCmds);
    fn draw(&self, p: &mut PanelDraw, st: &DeskState);
    fn hit_control(&self, px: f32, py: f32, st: &DeskState) -> Option<ControlHit>;
    fn on_touch(&mut self, ev: ModuleTouch, st: &DeskState, cmds: &mut DeskCmds);
}

// ---------------------------------------------------------------------------
// Immediate-mode draw batch collected in panel units and flushed in 4 GPU draws
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct MeshInst {
    pub model: Mat4f,
    pub color: Vec4f,
    pub mat: Vec4f, // x: spec, y: emit, z: rim, w: unused
}

#[derive(Clone, Debug)]
pub struct TextInst {
    pub x: f32,
    pub y: f32,
    pub size_u: f32,
    pub color: Vec4f,
    pub align: TextAlign,
    pub is_title: bool, // module titles never fade out at whole-desk zoom
    pub text: String,
}

#[derive(Default)]
pub struct PanelDraw {
    pub boxes: Vec<MeshInst>,
    pub cyls: Vec<MeshInst>,
    pub jogs: Vec<MeshInst>,
    pub knobs: Vec<MeshInst>,
    pub texts: Vec<TextInst>,
    pub zoom_px_per_u: f32,
}

impl PanelDraw {
    pub fn clear(&mut self, zoom_px_per_u: f32) {
        self.boxes.clear();
        self.cyls.clear();
        self.jogs.clear();
        self.knobs.clear();
        self.texts.clear();
        self.zoom_px_per_u = zoom_px_per_u;
    }

    /// Raw 3D box positioned in panel coordinates (Y-down -> world Y-up `-cy`), centered on `rect`.
    pub fn plate(&mut self, rect: PanelRect, z: f32, depth: f32, color: Vec4f, spec: f32, emit: f32) {
        self.boxes.push(MeshInst {
            model: mat4_trs_z(rect.cx(), -rect.cy(), z, 0.0, rect.w * 0.5, rect.h * 0.5, depth * 0.5),
            color,
            mat: vec4(spec, emit, 0.35, 0.0),
        });
    }

    /// Standard dark-anodized module housing + header rail + always-readable title & right status.
    pub fn module_frame(&mut self, rect: PanelRect, title: &str, right_status: &str, accent: Vec4f) {
        // Outer raised bezel + recessed inner faceplate
        self.plate(rect, 0.25, 0.5, vec4(0.14, 0.17, 0.22, 1.0), 0.35, 0.0);
        self.plate(
            PanelRect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, rect.h - 2.0),
            0.40,
            0.35,
            vec4(0.085, 0.105, 0.145, 1.0),
            0.22,
            0.0,
        );
        // Top title strip + accent rule
        let hdr = PanelRect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, 9.5);
        self.plate(hdr, 0.55, 0.25, vec4(0.115, 0.145, 0.20, 1.0), 0.25, 0.0);
        self.plate(
            PanelRect::new(rect.x + 1.0, rect.y + 10.3, rect.w - 2.0, 0.7),
            0.68,
            0.2,
            accent,
            0.3,
            0.45,
        );
        // Mounting screws at corners
        for &(sx, sy) in &[
            (rect.x + 3.2, rect.y + 5.7),
            (rect.right() - 3.2, rect.y + 5.7),
        ] {
            self.cyls.push(MeshInst {
                model: mat4_trs_z(sx, -sy, 0.72, 0.0, 1.05, 1.05, 0.25),
                color: vec4(0.42, 0.47, 0.55, 1.0),
                mat: vec4(0.8, 0.0, 0.4, 0.0),
            });
        }
        self.title_text(rect.x + 6.5, rect.y + 5.7, 5.2, vec4(0.95, 0.97, 1.0, 1.0), TextAlign::Left, title);
        if !right_status.is_empty() {
            let title_w = title.chars().count() as f32 * 4.4;
            let avail_w = (rect.w - 17.5 - title_w).max(12.0);
            let max_chars = (avail_w / 2.95).floor().max(4.0) as usize;
            let clipped: String = if right_status.chars().count() > max_chars {
                let mut out: String = right_status.chars().take(max_chars.saturating_sub(1)).collect();
                out.push('…');
                out
            } else {
                right_status.to_string()
            };
            self.title_text(rect.right() - 7.5, rect.y + 5.7, 4.0, vec4(0.72, 0.80, 0.90, 0.96), TextAlign::Right, &clipped);
        }
    }

    pub fn text(&mut self, x: f32, y: f32, size_u: f32, color: Vec4f, align: TextAlign, s: &str) {
        if s.is_empty() { return; }
        self.texts.push(TextInst {
            x, y, size_u, color, align, is_title: false, text: s.to_string(),
        });
    }

    pub fn title_text(&mut self, x: f32, y: f32, size_u: f32, color: Vec4f, align: TextAlign, s: &str) {
        if s.is_empty() { return; }
        self.texts.push(TextInst {
            x, y, size_u, color, align, is_title: true, text: s.to_string(),
        });
    }

    /// Clipped text that truncates cleanly with `…` so it stays within `max_w_u` panel units.
    pub fn text_clip(&mut self, x: f32, y: f32, max_w_u: f32, size_u: f32, color: Vec4f, align: TextAlign, s: &str) {
        if s.is_empty() || max_w_u <= 1.0 { return; }
        let char_w = (size_u * 0.56).max(0.8);
        let max_chars = (max_w_u / char_w).floor().max(1.0) as usize;
        let clipped: String = if s.chars().count() > max_chars {
            if max_chars <= 1 {
                "…".to_string()
            } else {
                let mut out: String = s.chars().take(max_chars - 1).collect();
                out.push('…');
                out
            }
        } else {
            s.to_string()
        };
        self.text(x, y, size_u, color, align, &clipped);
    }

    /// Tactile 3D badge/chip pill with centered clipped text (for LYRICS chips, BANDS badges, RACK rows).
    pub fn pill(&mut self, rect: PanelRect, bg: Vec4f, fg: Vec4f, text: &str) {
        self.plate(rect, 0.95, 0.45, bg, 0.35, 0.14);
        let size_u = (rect.h * 0.54).clamp(2.8, 4.6);
        self.text_clip(rect.cx(), rect.cy(), (rect.w - 3.0).max(2.0), size_u, fg, TextAlign::Center, text);
    }

    /// Crisp circular status LED dot (for LINK lights, band states, cache indicators).
    pub fn dot(&mut self, cx: f32, cy: f32, r: f32, color: Vec4f, emit: f32) {
        self.cyls.push(MeshInst {
            model: mat4_trs_z(cx, -cy, 0.92, 0.0, r, r, 0.45),
            color,
            mat: vec4(0.4, emit.clamp(0.0, 1.0), 0.25, 0.0),
        });
    }

    /// Detented 3D rotary knob with base collar, illuminated detent pips, name above, value below.
    pub fn knob(
        &mut self,
        cx: f32,
        cy: f32,
        r: f32,
        norm: f32,
        detents: usize,
        active: bool,
        accent: Vec4f,
        label: &str,
        value: &str,
    ) {
        let norm = norm.clamp(0.0, 1.0);
        // Collar
        let collar_col = if active { accent } else { vec4(0.18, 0.22, 0.29, 1.0) };
        self.cyls.push(MeshInst {
            model: mat4_trs_z(cx, -cy, 0.72, 0.0, r * 1.12, r * 1.12, 0.5),
            color: collar_col,
            mat: vec4(0.45, if active { 0.40 } else { 0.0 }, 0.3, 0.0),
        });
        // Detent pips around 270 deg arc (7:30 -> 4:30 clockwise)
        let n = detents.clamp(2, 9);
        for di in 0..n {
            let f = if n > 1 { di as f32 / (n - 1) as f32 } else { 0.5 };
            let ang = (135.0 - f * 270.0).to_radians();
            // In world Y-up: pointer at +Y for ang=0, left-down at +135 deg
            let wx = cx - ang.sin() * (r * 1.32);
            let wy = -cy + ang.cos() * (r * 1.32);
            let lit = f <= norm + 0.04;
            let pip_col = if lit { accent } else { vec4(0.16, 0.20, 0.27, 1.0) };
            self.cyls.push(MeshInst {
                model: mat4_trs_z(wx, wy, 0.80, 0.0, r * 0.11, r * 0.11, 0.35),
                color: pip_col,
                mat: vec4(0.3, if lit { 0.8 } else { 0.0 }, 0.1, 0.0),
            });
        }
        // Knurled 3D cap with pointer bar
        let rot = (135.0 - norm * 270.0).to_radians();
        let cap_col = if active {
            vec4(0.42, 0.62, 0.80, 1.0)
        } else {
            vec4(0.28, 0.34, 0.44, 1.0)
        };
        self.knobs.push(MeshInst {
            model: mat4_trs_z(cx, -cy, 0.90, rot, r * 0.94, r * 0.94, r * 0.42),
            color: cap_col,
            mat: vec4(0.72, if active { 0.15 } else { 0.0 }, 0.6, 0.0),
        });
        // Luminous white indicator bar on top of the pointer ridge so angle reads at a glance
        let ptr_d = r * 0.43;
        let ptr_x = cx - rot.sin() * ptr_d;
        let ptr_y = -cy + rot.cos() * ptr_d;
        self.boxes.push(MeshInst {
            model: mat4_trs_z(ptr_x, ptr_y, 0.90 + r * 0.44, rot, r * 0.10, r * 0.32, r * 0.05),
            color: vec4(0.96, 0.98, 1.0, 1.0),
            mat: vec4(0.35, 0.70, 0.2, 0.0),
        });
        // Name above, value below
        self.text(cx, cy - r * 1.55 - 2.6, 3.6, vec4(0.68, 0.84, 0.98, 0.96), TextAlign::Center, label);
        self.text(cx, cy + r * 1.35 + 2.8, 3.8, vec4(0.95, 0.97, 1.0, 0.98), TextAlign::Center, value);
    }

    /// Weighted 3D vertical fader: recessed track, LED fill, rest ticks, 3D cap, name above, value below.
    pub fn fader(
        &mut self,
        cx: f32,
        cy: f32,
        w: f32,
        h: f32,
        norm: f32,
        rest: f32,
        accent: Vec4f,
        active: bool,
        label: &str,
        value: &str,
    ) {
        let norm = norm.clamp(0.0, 1.0);
        let rest = rest.clamp(0.0, 1.0);
        let top = cy - h * 0.5;
        let bot = cy + h * 0.5;
        // Track bezel + dark slot
        self.plate(PanelRect::new(cx - w * 0.24, top - 1.2, w * 0.48, h + 2.4), 0.60, 0.4, vec4(0.15, 0.19, 0.26, 1.0), 0.35, 0.0);
        self.plate(PanelRect::new(cx - w * 0.09, top, w * 0.18, h), 0.72, 0.3, vec4(0.03, 0.04, 0.06, 1.0), 0.1, 0.0);
        // Glowing fill from bottom up to cap
        let fill_h = (norm * h).max(0.8);
        self.plate(PanelRect::new(cx - w * 0.065, bot - fill_h, w * 0.13, fill_h), 0.88, 0.25, accent, 0.3, 0.65);
        // Rest ticks left & right
        let ry = bot - rest * h;
        self.plate(PanelRect::new(cx - w * 0.46, ry - 0.35, w * 0.16, 0.7), 0.85, 0.25, vec4(0.55, 0.62, 0.72, 1.0), 0.4, 0.1);
        self.plate(PanelRect::new(cx + w * 0.30, ry - 0.35, w * 0.16, 0.7), 0.85, 0.25, vec4(0.55, 0.62, 0.72, 1.0), 0.4, 0.1);
        // Raised 3D fader cap + glowing center grip stripe
        let cap_y = bot - norm * h;
        let cap_h = (h * 0.20).clamp(5.2, 8.5);
        let cap_w = (w * 0.88).clamp(9.0, 18.0);
        let cap_col = if active { vec4(0.42, 0.54, 0.70, 1.0) } else { vec4(0.28, 0.34, 0.43, 1.0) };
        self.plate(PanelRect::new(cx - cap_w * 0.5, cap_y - cap_h * 0.5, cap_w, cap_h), 1.9, 1.8, cap_col, 0.78, if active { 0.14 } else { 0.0 });
        self.plate(PanelRect::new(cx - cap_w * 0.42, cap_y - 0.65, cap_w * 0.84, 1.3), 2.85, 0.3, accent, 0.4, if active { 0.95 } else { 0.55 });
        // Name above, value below
        self.text(cx, top - 4.2, 3.6, vec4(0.95, 0.78, 0.48, 0.96), TextAlign::Center, label);
        self.text(cx, bot + 4.4, 3.6, vec4(0.94, 0.97, 1.0, 0.98), TextAlign::Center, value);
    }

    /// Mechanical 3D keycap with press travel, top LED strip, and sharp centered label.
    pub fn key(
        &mut self,
        rect: PanelRect,
        travel: f32,
        led_col: Vec4f,
        led_on: f32,
        body_col: Vec4f,
        label: &str,
    ) {
        let t = travel.clamp(0.0, 1.0);
        let depth = 2.0 - t * 1.1;
        let z = 0.65 + depth * 0.5;
        // Recessed socket well
        self.plate(PanelRect::new(rect.x - 0.6, rect.y - 0.6, rect.w + 1.2, rect.h + 1.2), 0.55, 0.3, vec4(0.03, 0.04, 0.07, 1.0), 0.1, 0.0);
        // Keycap body
        self.plate(rect, z, depth, body_col, 0.55, t * 0.12);
        // Top LED indicator bar
        let led_r = PanelRect::new(rect.x + rect.w * 0.22, rect.y + 2.4, rect.w * 0.56, 2.0);
        self.plate(led_r, z + depth * 0.5 + 0.15, 0.3, led_col, 0.3, led_on.clamp(0.0, 1.0));
        // Label centered in lower key face (5.8 u -> ~7.5 px at whole-desk zoom so keys always read)
        let txt_col = if led_on > 0.6 { vec4(0.98, 1.0, 0.98, 1.0) } else { vec4(0.86, 0.91, 0.97, 0.96) };
        self.text_clip(rect.cx(), rect.y + rect.h * 0.65, (rect.w - 2.0).max(4.0), 5.8, txt_col, TextAlign::Center, label);
    }

    /// Stepped selector strip with name above, value inside, and step pips along bottom.
    pub fn selector(
        &mut self,
        rect: PanelRect,
        label: &str,
        value: &str,
        idx: usize,
        count: usize,
        active: bool,
        accent: Vec4f,
    ) {
        self.text(rect.cx(), rect.y - 2.8, 3.4, vec4(0.65, 0.82, 0.98, 0.95), TextAlign::Center, label);
        let bg = if active { vec4(0.18, 0.28, 0.42, 1.0) } else { vec4(0.13, 0.17, 0.24, 1.0) };
        self.plate(rect, 1.15, 1.0, bg, 0.45, if active { 0.12 } else { 0.0 });
        self.text(rect.cx(), rect.y + rect.h * 0.42, 3.9, vec4(0.96, 0.98, 1.0, 1.0), TextAlign::Center, value);
        let n = count.clamp(1, 8);
        let gap = 1.6;
        let pip_w = ((rect.w - 6.0 - gap * (n.saturating_sub(1)) as f32) / n as f32).clamp(2.0, 8.0);
        let total_w = pip_w * n as f32 + gap * (n.saturating_sub(1)) as f32;
        let x0 = rect.cx() - total_w * 0.5;
        for i in 0..n {
            let lit = i == idx.min(n - 1);
            self.plate(
                PanelRect::new(x0 + i as f32 * (pip_w + gap), rect.bottom() - 3.0, pip_w, 1.6),
                1.72,
                0.25,
                if lit { accent } else { vec4(0.22, 0.26, 0.34, 1.0) },
                0.25,
                if lit { 0.8 } else { 0.0 },
            );
        }
    }

    /// Recessed 3D bar meter (vertical or horizontal) with optional label & value.
    pub fn meter(
        &mut self,
        rect: PanelRect,
        level: f32,
        color: Vec4f,
        vertical: bool,
        label: &str,
        value: &str,
    ) {
        let lvl = level.clamp(0.0, 1.0);
        self.plate(rect, 0.62, 0.35, vec4(0.04, 0.06, 0.09, 1.0), 0.15, 0.0);
        if lvl > 0.005 {
            let fill = if vertical {
                let fh = (rect.h - 1.0) * lvl;
                PanelRect::new(rect.x + 0.5, rect.bottom() - 0.5 - fh, (rect.w - 1.0).max(0.8), fh.max(0.6))
            } else {
                let fw = (rect.w - 1.0) * lvl;
                PanelRect::new(rect.x + 0.5, rect.y + 0.5, fw.max(0.6), (rect.h - 1.0).max(0.8))
            };
            self.plate(fill, 0.85, 0.25, color, 0.3, 0.55 + lvl * 0.35);
        }
        if !label.is_empty() {
            if vertical {
                self.text(rect.cx(), rect.y - 3.4, 3.2, vec4(0.72, 0.80, 0.90, 0.92), TextAlign::Center, label);
            } else {
                self.text(rect.x, rect.y - 3.0, 3.2, vec4(0.72, 0.80, 0.90, 0.92), TextAlign::Left, label);
            }
        }
        if !value.is_empty() {
            if vertical {
                self.text(rect.cx(), rect.bottom() + 3.8, 3.2, vec4(0.92, 0.95, 1.0, 0.95), TextAlign::Center, value);
            } else {
                self.text(rect.right(), rect.y - 3.0, 3.2, vec4(0.92, 0.95, 1.0, 0.95), TextAlign::Right, value);
            }
        }
    }

    /// Flat machined 3D jog wheel with rotating amber index dot, name above, value below.
    pub fn jog_wheel(
        &mut self,
        cx: f32,
        cy: f32,
        r: f32,
        angle: f32,
        active: bool,
        label: &str,
        value: &str,
    ) {
        // Recessed circular well
        self.cyls.push(MeshInst {
            model: mat4_trs_z(cx, -cy, 0.55, 0.0, r * 1.08, r * 1.08, 0.35),
            color: vec4(0.04, 0.055, 0.08, 1.0),
            mat: vec4(0.2, 0.0, 0.1, 0.0),
        });
        // Machined 6-spoke wheel
        let col = if active { vec4(0.98, 0.72, 0.34, 1.0) } else { vec4(0.82, 0.86, 0.92, 1.0) };
        self.jogs.push(MeshInst {
            model: mat4_trs_z(cx, -cy, 0.75, angle, r, r, 4.5),
            color: col,
            mat: vec4(0.82, if active { 0.20 } else { 0.0 }, 0.7, 0.0),
        });
        // Center brass spindle
        self.cyls.push(MeshInst {
            model: mat4_trs_z(cx, -cy, 1.80, 0.0, r * 0.18, r * 0.18, 1.6),
            color: vec4(0.85, 0.67, 0.32, 1.0),
            mat: vec4(0.85, 0.05, 0.7, 0.0),
        });
        // Rotating index dot on spoke 0
        let dx = cx + angle.cos() * (r * 0.64);
        let dy = -cy + angle.sin() * (r * 0.64);
        self.cyls.push(MeshInst {
            model: mat4_trs_z(dx, dy, 2.25, 0.0, r * 0.12, r * 0.12, 0.6),
            color: vec4(0.98, 0.52, 0.16, 1.0),
            mat: vec4(0.45, 0.65, 0.2, 0.0),
        });
        self.text(cx, cy - r - 3.6, 3.5, vec4(0.75, 0.85, 0.96, 0.95), TextAlign::Center, label);
        self.text(cx, cy + r + 3.6, 3.6, vec4(0.95, 0.97, 1.0, 0.98), TextAlign::Center, value);
    }

    /// Tape strip: backing & voice waveforms, section state tints, line ticks, loop band, glowing head.
    pub fn strip(
        &mut self,
        rect: PanelRect,
        back_wave: &[f32],
        voc_wave: &[f32],
        sections: &[SectionInfo],
        dur_ms: f64,
        line_times_ms: &[f64],
        head_frac: f32,
        loop_range: Option<(f32, f32)>,
    ) {
        let dur = dur_ms.max(1.0);
        // Dark tape track bed
        self.plate(rect, 0.62, 0.35, vec4(0.04, 0.055, 0.085, 1.0), 0.18, 0.0);

        // Section tint bands (scratch = slate/amber, singing = violet pulse, sung = emerald/teal)
        if !sections.is_empty() {
            for sec in sections {
                let f0 = (sec.t0_ms / dur).clamp(0.0, 1.0) as f32;
                let f1 = ((sec.t1_ms / dur).clamp(0.0, 1.0) as f32).max(f0);
                if f1 <= f0 + 0.002 { continue; }
                let (col, emit) = match sec.state.as_str() {
                    "sung" => (vec4(0.10, 0.34, 0.28, 1.0), 0.18),
                    "singing" => (vec4(0.36, 0.22, 0.56, 1.0), 0.35),
                    _ => (vec4(0.24, 0.19, 0.12, 1.0), 0.12),
                };
                let sx = rect.x + f0 * rect.w;
                let sw = ((f1 - f0) * rect.w - 0.5).max(0.8);
                self.plate(PanelRect::new(sx, rect.y + 0.6, sw, 4.2), 0.82, 0.2, col, 0.2, emit);
                if !sec.label.is_empty() && sw > 12.0 {
                    self.text(sx + 2.0, rect.y + 2.7, 2.8, vec4(0.90, 0.94, 1.0, 0.90), TextAlign::Left, &sec.label);
                }
            }
        }

        // Loop band highlight
        if let Some((a, b)) = loop_range {
            let la = a.clamp(0.0, 1.0);
            let lb = b.clamp(la, 1.0);
            if lb > la + 0.001 {
                self.plate(
                    PanelRect::new(rect.x + la * rect.w, rect.y + 5.0, (lb - la) * rect.w, rect.h - 5.6),
                    0.78,
                    0.18,
                    vec4(0.12, 0.34, 0.44, 1.0),
                    0.2,
                    0.28,
                );
            }
        }

        // Waveform bars (budget <= 96 bars total across strip so instanced count stays ultra-low)
        let bins = 72usize;
        let bar_step = (rect.w - 2.0) / bins as f32;
        let bar_w = (bar_step * 0.70).max(0.8);
        let mid_y = rect.y + 5.0 + (rect.h - 6.0) * 0.52;
        let max_h = (rect.h - 8.5) * 0.44;
        for bi in 0..bins {
            let bx = rect.x + 1.0 + bi as f32 * bar_step;
            let b_amp = sample_bin(back_wave, bi, bins);
            let v_amp = sample_bin(voc_wave, bi, bins);
            if b_amp > 0.02 {
                let bh = (b_amp * max_h * 1.6).clamp(1.0, max_h * 1.9);
                self.plate(
                    PanelRect::new(bx, mid_y - bh * 0.5, bar_w, bh),
                    0.95,
                    0.2,
                    vec4(0.32, 0.40, 0.52, 1.0),
                    0.2,
                    0.12,
                );
            }
            if v_amp > 0.02 {
                let vh = (v_amp * max_h * 1.45).clamp(1.0, max_h * 1.75);
                self.plate(
                    PanelRect::new(bx, mid_y - vh * 0.5, bar_w * 0.72, vh),
                    1.12,
                    0.22,
                    vec4(0.24, 0.80, 0.98, 1.0),
                    0.3,
                    0.42,
                );
            }
        }

        // Subtle line tick marks along bottom edge (sampled up to ~40 visible ticks)
        if !line_times_ms.is_empty() {
            let stride = (line_times_ms.len() / 40).max(1);
            for (i, &t0) in line_times_ms.iter().enumerate() {
                if i % stride != 0 || t0 < 0.0 { continue; }
                let fx = (t0 / dur).clamp(0.0, 1.0) as f32;
                self.plate(
                    PanelRect::new(rect.x + fx * rect.w, rect.bottom() - 2.6, 0.55, 2.2),
                    1.20,
                    0.2,
                    vec4(0.60, 0.68, 0.80, 1.0),
                    0.2,
                    0.25,
                );
            }
        }

        // Glowing vertical tape head cursor
        let hx = rect.x + head_frac.clamp(0.0, 1.0) * rect.w;
        self.plate(
            PanelRect::new(hx - 0.85, rect.y + 0.4, 1.7, rect.h - 0.8),
            1.65,
            0.65,
            vec4(0.98, 0.42, 0.24, 1.0),
            0.5,
            0.95,
        );
    }
}

fn sample_bin(arr: &[f32], bin: usize, total: usize) -> f32 {
    if arr.is_empty() {
        return 0.0;
    }
    let idx = (bin * arr.len()) / total.max(1);
    arr.get(idx).copied().unwrap_or(0.0)
}

pub fn fmt_time_ms(ms: f64) -> String {
    let s = ms.max(0.0) / 1000.0;
    let m = (s / 60.0).floor() as i32;
    let rem = s - (m as f64) * 60.0;
    format!("{:02}:{:04.1}", m, rem)
}

pub fn parse_hex_color(hex: &str, fallback: Vec4f) -> Vec4f {
    let h = hex.trim().trim_start_matches('#');
    if h.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&h[0..2], 16),
            u8::from_str_radix(&h[2..4], 16),
            u8::from_str_radix(&h[4..6], 16),
        ) {
            return vec4(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0);
        }
    }
    fallback
}
