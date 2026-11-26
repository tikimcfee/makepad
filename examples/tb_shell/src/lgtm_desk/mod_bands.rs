//! BANDS module (`RECT_BANDS`): soundboard channel strips for every prompt band (up to 12,
//! paged 8 per bank), with 3D vertical faders, step snapping, live/compiling/error LEDs,
//! changed-word counters, first words of the prompt, tap-name-to-edit, `+ ADD`, and custom `RM`.

use super::module::{
    parse_hex_color, ControlHit, DeskModule, ModuleTouch, PanelDraw, PanelRect, TextAlign,
    TouchKind, RECT_BANDS,
};
use super::wire::{json_escape, BandInfo, DeskCmds, DeskState};
use makepad_widgets::vec4;
use std::time::{Duration, Instant};

const MAX_VISIBLE_STRIPS: usize = 8;

// Control ID ranges:
//   0..15     -> fader drag for visible slot `i`
//   100..115  -> name / prompt tap to edit band `i`
//   200..215  -> remove custom band (`idx >= 8`)
//   250..265  -> reset built-in band prompt to default while editing (`idx < 8`)
//   300       -> bank prev
//   301       -> bank next
//   302       -> + Add prompt band
const CTRL_NAME_BASE: u32 = 100;
const CTRL_RM_BASE: u32 = 200;
const CTRL_DEF_BASE: u32 = 250;
const CTRL_BANK_PREV: u32 = 300;
const CTRL_BANK_NEXT: u32 = 301;
const CTRL_ADD_BAND: u32 = 302;

const ECHO_HOLD_MS: u64 = 600;

#[derive(Default)]
pub struct BandsModule {
    bank: usize,
    grabbed: Option<usize>, // absolute band index
    grab_start_y: f32,
    grab_start_val: f32,
    local_vals: Vec<f32>,
    last_sent_vals: Vec<f32>,
    hold_until: Vec<Option<Instant>>,
}

fn snap_band_val(b: &BandInfo, raw: f32) -> f32 {
    let clamped = raw.clamp(0.0, 1.0);
    let n = b.steps.len();
    if n > 1 {
        let idx = (clamped * (n - 1) as f32).round();
        (idx / (n - 1) as f32).clamp(0.0, 1.0)
    } else {
        (clamped * 100.0).round() / 100.0
    }
}

fn format_band_val(b: &BandInfo, val: f32) -> String {
    let n = b.steps.len();
    if n > 1 {
        let idx = ((val * (n - 1) as f32).round() as usize).min(n - 1);
        b.steps[idx].chars().take(7).collect()
    } else {
        format!("{}%", (val * 100.0).round() as i32)
    }
}

impl BandsModule {
    fn ensure_len(&mut self, total: usize, st: &DeskState) {
        while self.local_vals.len() < total {
            let v = st.bands.get(self.local_vals.len()).map(|b| b.value).unwrap_or(0.0);
            self.local_vals.push(v);
        }
        while self.last_sent_vals.len() < total {
            let v = st.bands.get(self.last_sent_vals.len()).map(|b| b.value).unwrap_or(-1.0);
            self.last_sent_vals.push(v);
        }
        if self.hold_until.len() < total {
            self.hold_until.resize(total, None);
        }
    }

    fn effective_val(&self, idx: usize, store_val: f32, now: Instant) -> f32 {
        if self.grabbed == Some(idx) {
            return self.local_vals.get(idx).copied().unwrap_or(store_val);
        }
        if let Some(Some(until)) = self.hold_until.get(idx) {
            if now < *until {
                let lv = self.local_vals.get(idx).copied().unwrap_or(store_val);
                if (store_val - lv).abs() > 0.015 {
                    return lv;
                }
            }
        }
        store_val
    }

    fn bank_count(&self, total: usize) -> usize {
        if total <= MAX_VISIBLE_STRIPS {
            1
        } else {
            (total + MAX_VISIBLE_STRIPS - 1) / MAX_VISIBLE_STRIPS
        }
    }

    fn visible_slice(&self, total: usize) -> (usize, usize) {
        let banks = self.bank_count(total);
        let b = self.bank.min(banks.saturating_sub(1));
        let start = b * MAX_VISIBLE_STRIPS;
        let end = (start + MAX_VISIBLE_STRIPS).min(total);
        (start, end)
    }

    fn strip_geom(&self, slot: usize, vis_count: usize) -> (f32, f32, f32, f32) {
        let r = RECT_BANDS;
        let n = vis_count.clamp(1, MAX_VISIBLE_STRIPS);
        let col_w = (r.w - 8.0) / n as f32;
        let cx = r.x + 4.0 + col_w * (slot as f32 + 0.5);
        let cy = r.y + 46.5;
        let fw = (col_w - 3.6).clamp(15.0, 24.0);
        let fh = 34.0;
        (cx, cy, fw, fh)
    }

    fn name_rect(&self, slot: usize, vis_count: usize) -> PanelRect {
        let (cx, cy, w, h) = self.strip_geom(slot, vis_count);
        PanelRect::new(cx - w * 0.54, cy - h * 0.5 - 16.5, w * 1.08, 14.5)
    }

    fn prompt_rect(&self, slot: usize, vis_count: usize) -> PanelRect {
        let r = RECT_BANDS;
        let (cx, _, w, _) = self.strip_geom(slot, vis_count);
        PanelRect::new(cx - w * 0.54, r.bottom() - 8.5, w * 1.08, 7.5)
    }

    fn rm_rect(&self, slot: usize, vis_count: usize) -> PanelRect {
        let (cx, cy, w, h) = self.strip_geom(slot, vis_count);
        PanelRect::new(cx + w * 0.06, cy - h * 0.5 - 16.0, w * 0.48, 5.8)
    }

    fn add_btn_rect(&self) -> PanelRect {
        let r = RECT_BANDS;
        PanelRect::new(r.right() - 28.0, r.y + 2.0, 20.5, 7.2)
    }

    fn bank_prev_rect(&self) -> PanelRect {
        let r = RECT_BANDS;
        PanelRect::new(r.right() - 58.0, r.y + 2.0, 12.0, 7.2)
    }

    fn bank_next_rect(&self) -> PanelRect {
        let r = RECT_BANDS;
        PanelRect::new(r.right() - 44.0, r.y + 2.0, 12.0, 7.2)
    }
}

impl DeskModule for BandsModule {
    fn id(&self) -> &'static str { "bands" }
    fn title(&self) -> &'static str { "BANDS" }
    fn rect(&self) -> PanelRect { RECT_BANDS }
    fn needs_animation(&self, _st: &DeskState) -> bool { false }
    fn step(&mut self, _dt: f32, _st: &DeskState, _cmds: &mut DeskCmds) {}

    fn draw(&self, p: &mut PanelDraw, st: &DeskState) {
        let r = RECT_BANDS;
        let total = st.bands.len();
        let (start, end) = self.visible_slice(total);
        let vis = end.saturating_sub(start);
        let active_words: i32 = st.bands.iter().map(|b| st.band_fx_count(b).max(0)).sum();

        let right = if total == 0 {
            "waiting".to_string()
        } else if total > MAX_VISIBLE_STRIPS {
            format!("{}-{}/{}", start + 1, end, total)
        } else if active_words > 0 {
            format!("{} ch · {}w", total, active_words)
        } else {
            format!("{} ch", total)
        };
        // Leave space on right of header when +ADD / bank buttons are drawn
        let right_txt = if total < 12 || total > MAX_VISIBLE_STRIPS { "" } else { &right };
        p.module_frame(r, "BANDS", right_txt, vec4(0.24, 0.78, 0.98, 1.0));

        // Header status + paging + "+ ADD" button
        let status_x = if total > MAX_VISIBLE_STRIPS {
            r.right() - 62.0
        } else if total < 12 {
            r.right() - 32.0
        } else {
            r.right() - 6.5
        };
        if total < 12 || total > MAX_VISIBLE_STRIPS {
            p.title_text(
                status_x,
                r.y + 5.7,
                4.2,
                vec4(0.72, 0.80, 0.90, 0.95),
                TextAlign::Right,
                &right,
            );
        }
        if total > MAX_VISIBLE_STRIPS {
            let pr = self.bank_prev_rect();
            p.plate(pr, 0.95, 0.45, vec4(0.14, 0.20, 0.30, 1.0), 0.35, 0.12);
            p.title_text(pr.cx(), pr.cy(), 4.2, vec4(0.92, 0.96, 1.0, 1.0), TextAlign::Center, "<");
            let nr = self.bank_next_rect();
            p.plate(nr, 0.95, 0.45, vec4(0.14, 0.20, 0.30, 1.0), 0.35, 0.12);
            p.title_text(nr.cx(), nr.cy(), 4.2, vec4(0.92, 0.96, 1.0, 1.0), TextAlign::Center, ">");
        }
        if total < 12 {
            let adding = st.edit_id == "__add__";
            let ar = self.add_btn_rect();
            let abg = if adding {
                vec4(0.18, 0.42, 0.62, 1.0)
            } else {
                vec4(0.14, 0.22, 0.34, 1.0)
            };
            p.plate(ar, 0.95, 0.45, abg, 0.35, 0.14);
            p.title_text(ar.cx(), ar.cy(), 4.0, vec4(0.78, 0.92, 1.0, 1.0), TextAlign::Center, "+ADD");
        }

        if total == 0 {
            p.text(
                r.cx(),
                r.cy() + 5.0,
                4.0,
                vec4(0.60, 0.68, 0.78, 0.88),
                TextAlign::Center,
                "Waiting for prompt bands…",
            );
            return;
        }

        let now = Instant::now();
        for slot in 0..vis {
            let idx = start + slot;
            let b = &st.bands[idx];
            let (cx, cy, w, h) = self.strip_geom(slot, vis);
            let is_grabbed = self.grabbed == Some(idx);
            let val = self.effective_val(idx, b.value, now);
            let accent = parse_hex_color(&b.color_hex, vec4(0.24, 0.78, 0.98, 1.0));
            let is_editing = !st.edit_id.is_empty() && st.edit_id == b.id;

            // Subtle channel strip housing plate (highlights when editing this band's prompt)
            let strip_bg = if is_editing {
                vec4(0.13, 0.20, 0.32, 1.0)
            } else {
                vec4(0.095, 0.12, 0.17, 1.0)
            };
            p.plate(
                PanelRect::new(cx - w * 0.54, r.y + 12.0, w * 1.08, r.h - 14.0),
                0.50,
                0.22,
                strip_bg,
                0.20,
                if is_editing { 0.14 } else { 0.0 },
            );

            // Top band state LED + fx word count badge (or compiling n_done/n_total)
            let top_y = cy - h * 0.5 - 12.8;
            let (led_col, led_emit) = match b.state.as_str() {
                "compiling" => (vec4(0.96, 0.72, 0.20, 1.0), 0.95),
                "error" => (vec4(0.98, 0.32, 0.36, 1.0), 0.95),
                _ => (vec4(0.24, 0.88, 0.52, 1.0), 0.55),
            };
            p.dot(cx - w * 0.38, top_y, 1.35, led_col, led_emit);

            if idx >= 8 {
                p.pill(
                    self.rm_rect(slot, vis),
                    vec4(0.32, 0.12, 0.16, 1.0),
                    vec4(0.98, 0.72, 0.74, 1.0),
                    "RM",
                );
            } else if is_editing {
                p.pill(
                    self.rm_rect(slot, vis),
                    vec4(0.28, 0.21, 0.08, 1.0),
                    vec4(0.99, 0.90, 0.54, 1.0),
                    "DEF",
                );
            } else if b.state == "compiling" && b.n_total > 0 {
                p.text_clip(
                    cx + 1.5,
                    top_y,
                    w * 0.78,
                    3.2,
                    vec4(0.98, 0.88, 0.50, 0.96),
                    TextAlign::Center,
                    &format!("{}/{}", b.n_done, b.n_total),
                );
            } else if st.band_fx_count(b) > 0 {
                let fx_cnt = st.band_fx_count(b);
                let badge_bg = vec4(accent.x * 0.28, accent.y * 0.28, accent.z * 0.28, 1.0);
                p.pill(
                    PanelRect::new(cx - 2.0, top_y - 2.8, w * 0.52, 5.6),
                    badge_bg,
                    vec4(0.94, 0.98, 1.0, 1.0),
                    &format!("{}w", fx_cnt),
                );
            } else {
                p.text(
                    cx + 3.0,
                    top_y,
                    3.2,
                    vec4(0.48, 0.56, 0.68, 0.88),
                    TextAlign::Center,
                    "0",
                );
            }

            // 3D fader with band name above (tappable for prompt edit!) and live value/step below
            let lbl_src = if !b.id.is_empty() { b.id.as_str() } else { b.name.as_str() };
            let lbl: String = lbl_src.to_uppercase().chars().take(8).collect();
            let val_s = format_band_val(b, val);
            p.fader(cx, cy, w, h, val, b.rest, accent, is_grabbed, &lbl, &val_s);

            // First words of prompt (or error text) clipped along the bottom of the strip
            let foot = if b.state == "error" && !b.err.is_empty() {
                format!("! {}", b.err)
            } else if !b.prompt.is_empty() {
                b.prompt.clone()
            } else {
                b.does.clone()
            };
            let foot_col = if b.state == "error" {
                vec4(0.98, 0.62, 0.65, 0.95)
            } else if is_editing {
                vec4(0.96, 0.98, 1.0, 0.98)
            } else {
                vec4(0.68, 0.78, 0.90, 0.92)
            };
            p.text_clip(cx, r.bottom() - 4.4, w * 1.04, 3.3, foot_col, TextAlign::Center, &foot);
        }
    }

    fn hit_control(&self, px: f32, py: f32, st: &DeskState) -> Option<ControlHit> {
        let total = st.bands.len();
        if total < 12 && self.add_btn_rect().contains(px, py) {
            return Some(ControlHit::new(CTRL_ADD_BAND, 16.0));
        }
        if total > MAX_VISIBLE_STRIPS {
            if self.bank_prev_rect().contains(px, py) {
                return Some(ControlHit::new(CTRL_BANK_PREV, 16.0));
            }
            if self.bank_next_rect().contains(px, py) {
                return Some(ControlHit::new(CTRL_BANK_NEXT, 16.0));
            }
        }
        let (start, end) = self.visible_slice(total);
        let vis = end.saturating_sub(start);
        for slot in 0..vis {
            let idx = start + slot;
            if idx >= 8 && self.rm_rect(slot, vis).contains(px, py) {
                return Some(ControlHit::new(CTRL_RM_BASE + slot as u32, 16.0));
            }
            if idx < 8
                && !st.edit_id.is_empty()
                && st.bands.get(idx).map(|b| b.id == st.edit_id).unwrap_or(false)
                && self.rm_rect(slot, vis).contains(px, py)
            {
                return Some(ControlHit::new(CTRL_DEF_BASE + slot as u32, 16.0));
            }
            if self.name_rect(slot, vis).contains(px, py) || self.prompt_rect(slot, vis).contains(px, py) {
                return Some(ControlHit::new(CTRL_NAME_BASE + slot as u32, 16.0));
            }
            let (cx, cy, w, h) = self.strip_geom(slot, vis);
            let frect = PanelRect::new(cx - w * 0.54, cy - h * 0.52, w * 1.08, h * 1.15);
            if frect.contains(px, py) {
                return Some(ControlHit::new(slot as u32, w));
            }
        }
        None
    }

    fn on_touch(&mut self, ev: ModuleTouch, st: &DeskState, cmds: &mut DeskCmds) {
        let total = st.bands.len();
        match ev.control_id {
            CTRL_ADD_BAND => {
                if ev.kind == TouchKind::Up {
                    if st.edit_id == "__add__" {
                        cmds.close_prompt_edit();
                    } else {
                        cmds.open_prompt_edit("__add__", "New band", "");
                    }
                }
                return;
            }
            CTRL_BANK_PREV => {
                if ev.kind == TouchKind::Up {
                    let banks = self.bank_count(total);
                    self.bank = if self.bank == 0 { banks.saturating_sub(1) } else { self.bank - 1 };
                    cmds.request_redraw = true;
                }
                return;
            }
            CTRL_BANK_NEXT => {
                if ev.kind == TouchKind::Up {
                    let banks = self.bank_count(total);
                    self.bank = (self.bank + 1) % banks.max(1);
                    cmds.request_redraw = true;
                }
                return;
            }
            _ => {}
        }

        let (start, end) = self.visible_slice(total);
        let vis = end.saturating_sub(start);

        if ev.control_id >= CTRL_DEF_BASE {
            if ev.kind == TouchKind::Up {
                let idx = start + (ev.control_id - CTRL_DEF_BASE) as usize;
                if let Some(b) = st.bands.get(idx) {
                    cmds.cmd_pending("band_reset", &format!("{{\"id\":\"{}\"}}", json_escape(&b.id)), "band_reset");
                    cmds.close_prompt_edit();
                }
            }
            return;
        }

        if ev.control_id >= CTRL_RM_BASE {
            if ev.kind == TouchKind::Up {
                let idx = start + (ev.control_id - CTRL_RM_BASE) as usize;
                if let Some(b) = st.bands.get(idx) {
                    cmds.cmd_pending("band_rm", &format!("{{\"id\":\"{}\"}}", json_escape(&b.id)), "band_rm");
                }
            }
            return;
        }

        if ev.control_id >= CTRL_NAME_BASE {
            if ev.kind == TouchKind::Up {
                let idx = start + (ev.control_id - CTRL_NAME_BASE) as usize;
                if let Some(b) = st.bands.get(idx) {
                    if st.edit_id == b.id {
                        cmds.close_prompt_edit();
                    } else {
                        let nm = if b.name.is_empty() { b.id.as_str() } else { b.name.as_str() };
                        cmds.open_prompt_edit(&b.id, nm, &b.prompt);
                    }
                }
            }
            return;
        }

        let slot = ev.control_id as usize;
        let idx = start + slot;
        let Some(b) = st.bands.get(idx) else { return; };
        self.ensure_len(total, st);

        match ev.kind {
            TouchKind::Down => {
                let now = Instant::now();
                let start_v = self.effective_val(idx, b.value, now);
                self.grabbed = Some(idx);
                self.grab_start_y = ev.y;
                self.grab_start_val = start_v;
                self.local_vals[idx] = start_v;
                self.last_sent_vals[idx] = start_v;
                self.hold_until[idx] = None;
                cmds.request_redraw = true;
            }
            TouchKind::Move => {
                let (_, _, _, h) = self.strip_geom(slot, vis);
                let raw = self.grab_start_val + (self.grab_start_y - ev.y) / h.max(10.0);
                let snapped = snap_band_val(b, raw);
                if (snapped - self.local_vals[idx]).abs() >= 0.001 {
                    self.local_vals[idx] = snapped;
                    cmds.request_redraw = true;
                }
                if (snapped - self.last_sent_vals[idx]).abs() >= 0.005 {
                    self.last_sent_vals[idx] = snapped;
                    cmds.mix_patch(&b.id, snapped);
                }
            }
            TouchKind::Up => {
                let v = snap_band_val(b, self.local_vals.get(idx).copied().unwrap_or(b.value));
                self.grabbed = None;
                self.local_vals[idx] = v;
                self.hold_until[idx] = Some(Instant::now() + Duration::from_millis(ECHO_HOLD_MS));
                if (v - self.last_sent_vals[idx]).abs() >= 0.005 {
                    self.last_sent_vals[idx] = v;
                    cmds.mix_patch(&b.id, v);
                } else {
                    cmds.request_redraw = true;
                }
            }
            TouchKind::DoubleTap => {
                self.grabbed = None;
                let v = snap_band_val(b, b.rest);
                self.local_vals[idx] = v;
                self.last_sent_vals[idx] = v;
                self.hold_until[idx] = Some(Instant::now() + Duration::from_millis(ECHO_HOLD_MS));
                cmds.mix_patch(&b.id, v);
            }
        }
    }
}
