//! Live placeholders for the 4 modules owned by `lgtm-board` (`LYRICS`, `BANDS`, `RACK`, `LINK`).
//! Each implements `DeskModule` cleanly using `PanelDraw` helpers so `lgtm-board` can replace
//! them one file at a time (`mod_lyrics.rs`, `mod_bands.rs`, `mod_rack.rs`, `mod_link.rs`).

use super::module::{
    parse_hex_color, ControlHit, DeskModule, ModuleTouch, PanelDraw, PanelRect, TextAlign, TouchKind,
    RECT_BANDS, RECT_LINK, RECT_LYRICS, RECT_RACK,
};
use super::wire::{json_escape, DeskCmds, DeskState};
use makepad_widgets::vec4;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// LYRICS (placeholder until lgtm-board's mod_lyrics.rs lands)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct LyricsSlotModule;

impl DeskModule for LyricsSlotModule {
    fn id(&self) -> &'static str { "lyrics" }
    fn title(&self) -> &'static str { "LYRICS" }
    fn rect(&self) -> PanelRect { RECT_LYRICS }
    fn needs_animation(&self, _st: &DeskState) -> bool { false }
    fn step(&mut self, _dt: f32, _st: &DeskState, _cmds: &mut DeskCmds) {}

    fn draw(&self, p: &mut PanelDraw, st: &DeskState) {
        let r = RECT_LYRICS;
        let now = Instant::now();
        let cur = if st.line >= 0 {
            st.line as usize
        } else {
            st.line_index_at_ms(st.heard_pos_ms(now)).unwrap_or(0)
        };
        let total = st.src_lines.len();
        let right = if total > 0 { format!("line {} / {}", (cur + 1).min(total), total) } else { "waiting".into() };
        p.module_frame(r, "LYRICS", &right, vec4(0.38, 0.72, 0.98, 1.0));

        let rows = [
            (cur.checked_sub(1), r.y + 16.5, 3.4, vec4(0.54, 0.62, 0.74, 0.85), false),
            (Some(cur),          r.y + 26.0, 4.3, vec4(0.98, 0.99, 1.00, 1.00), true),
            (Some(cur + 1),      r.y + 42.5, 3.4, vec4(0.60, 0.68, 0.80, 0.88), false),
        ];
        for (opt_i, y, sz, col, is_cur) in rows {
            let Some(i) = opt_i else { continue; };
            if i >= total { continue; }
            if is_cur {
                p.plate(PanelRect::new(r.x + 4.0, r.y + 21.0, r.w - 8.0, 16.5), 0.70, 0.25, vec4(0.11, 0.16, 0.24, 1.0), 0.2, 0.08);
                let out_s = st.line_text_at(i);
                let src_s = st.src_lines.get(i).map(|s| s.as_str()).unwrap_or("");
                p.text_clip(r.x + 7.5, y, r.w - 30.0, sz, col, TextAlign::Left, out_s);
                if !src_s.is_empty() && src_s != out_s {
                    p.text_clip(r.x + 7.5, y + 7.2, r.w - 15.0, 3.2, vec4(0.60, 0.72, 0.86, 0.85), TextAlign::Left, src_s);
                }
                let chg_n = st.chg.get(i).map(|v| v.len()).unwrap_or(0);
                if chg_n > 0 {
                    p.pill(
                        PanelRect::new(r.right() - 22.0, r.y + 22.8, 16.0, 6.2),
                        vec4(0.18, 0.36, 0.56, 1.0),
                        vec4(0.92, 0.98, 1.0, 1.0),
                        &format!("{}Δ", chg_n),
                    );
                }
            } else {
                let txt = st.line_text_at(i);
                p.text_clip(r.x + 7.5, y, r.w - 15.0, sz, col, TextAlign::Left, txt);
            }
        }
    }

    fn hit_control(&self, _px: f32, _py: f32, _st: &DeskState) -> Option<ControlHit> { None }
    fn on_touch(&mut self, _ev: ModuleTouch, _st: &DeskState, _cmds: &mut DeskCmds) {}
}

// ---------------------------------------------------------------------------
// BANDS (interactive vertical channel strips placeholder for lgtm-board)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct BandsSlotModule {
    grabbed: Option<usize>,
    grab_start_y: f32,
    grab_start_val: f32,
    local_vals: Vec<f32>,
    last_sent: Option<Instant>,
}

impl BandsSlotModule {
    fn strip_geom(&self, idx: usize, count: usize) -> (f32, f32, f32, f32) {
        let r = RECT_BANDS;
        let n = count.clamp(1, 8);
        let col_w = (r.w - 8.0) / n as f32;
        let cx = r.x + 4.0 + col_w * (idx as f32 + 0.5);
        let cy = r.y + 45.5;
        (cx, cy, (col_w - 4.0).clamp(14.0, 24.0), 40.0)
    }

    fn name_rect(&self, idx: usize, count: usize) -> PanelRect {
        let (cx, cy, w, h) = self.strip_geom(idx, count);
        PanelRect::new(cx - w * 0.52, cy - h * 0.5 - 10.5, w * 1.04, 8.5)
    }
}

impl DeskModule for BandsSlotModule {
    fn id(&self) -> &'static str { "bands" }
    fn title(&self) -> &'static str { "BANDS" }
    fn rect(&self) -> PanelRect { RECT_BANDS }
    fn needs_animation(&self, _st: &DeskState) -> bool { false }
    fn step(&mut self, _dt: f32, _st: &DeskState, _cmds: &mut DeskCmds) {}

    fn draw(&self, p: &mut PanelDraw, st: &DeskState) {
        let r = RECT_BANDS;
        let n = st.bands.len();
        p.module_frame(r, "BANDS", &format!("{} ch", n), vec4(0.24, 0.78, 0.98, 1.0));
        if n == 0 {
            p.text(r.cx(), r.cy() + 4.0, 4.0, vec4(0.60, 0.68, 0.78, 0.85), TextAlign::Center, "Waiting for band definitions…");
            return;
        }
        let show = n.min(8);
        for i in 0..show {
            let b = &st.bands[i];
            let (cx, cy, w, h) = self.strip_geom(i, show);
            let val = if self.grabbed == Some(i) {
                self.local_vals.get(i).copied().unwrap_or(b.value)
            } else {
                b.value
            };
            let accent = parse_hex_color(&b.color_hex, vec4(0.24, 0.78, 0.98, 1.0));
            let val_s = if !b.steps.is_empty() {
                let si = ((val * (b.steps.len() - 1) as f32).round() as usize).min(b.steps.len() - 1);
                b.steps[si].chars().take(7).collect()
            } else {
                format!("{}%", (val * 100.0).round() as i32)
            };
            let lbl: String = b.id.to_uppercase().chars().take(8).collect();
            p.fader(cx, cy, w, h, val, b.rest, accent, self.grabbed == Some(i), &lbl, &val_s);
            if !b.prompt.is_empty() {
                p.text_clip(cx, r.bottom() - 4.0, w + 2.0, 2.7, vec4(0.62, 0.72, 0.84, 0.88), TextAlign::Center, &b.prompt);
            }
        }
    }

    fn hit_control(&self, px: f32, py: f32, st: &DeskState) -> Option<ControlHit> {
        let show = st.bands.len().min(8);
        for i in 0..show {
            if self.name_rect(i, show).contains(px, py) {
                return Some(ControlHit::new(100 + i as u32, 16.0));
            }
            let (cx, cy, w, h) = self.strip_geom(i, show);
            let frect = PanelRect::new(cx - w * 0.55, cy - h * 0.55, w * 1.10, h * 1.10);
            if frect.contains(px, py) {
                return Some(ControlHit::new(i as u32, w));
            }
        }
        None
    }

    fn on_touch(&mut self, ev: ModuleTouch, st: &DeskState, cmds: &mut DeskCmds) {
        if ev.control_id >= 100 {
            let idx = (ev.control_id - 100) as usize;
            if ev.kind == TouchKind::Up {
                if let Some(b) = st.bands.get(idx) {
                    cmds.open_prompt_edit(&b.id, &b.name, &b.prompt);
                }
            }
            return;
        }
        let idx = ev.control_id as usize;
        let Some(b) = st.bands.get(idx) else { return; };
        if self.local_vals.len() < st.bands.len() {
            self.local_vals.resize(st.bands.len(), 0.0);
        }
        match ev.kind {
            TouchKind::Down => {
                self.grabbed = Some(idx);
                self.grab_start_y = ev.y;
                self.grab_start_val = b.value;
                self.local_vals[idx] = b.value;
                cmds.request_redraw = true;
            }
            TouchKind::Move => {
                let (_, _, _, h) = self.strip_geom(idx, st.bands.len().min(8));
                let next = (self.grab_start_val + (self.grab_start_y - ev.y) / h.max(10.0)).clamp(0.0, 1.0);
                self.local_vals[idx] = next;
                let now = Instant::now();
                let due = self.last_sent.map(|t| now.duration_since(t) >= Duration::from_millis(40)).unwrap_or(true);
                if due {
                    self.last_sent = Some(now);
                    cmds.mix_patch(&b.id, next);
                } else {
                    cmds.request_redraw = true;
                }
            }
            TouchKind::Up => {
                let v = self.local_vals.get(idx).copied().unwrap_or(b.value);
                self.grabbed = None;
                cmds.mix_patch(&b.id, v);
            }
            TouchKind::DoubleTap => {
                self.grabbed = None;
                self.local_vals[idx] = b.rest;
                cmds.mix_patch(&b.id, b.rest);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RACK (placeholder for lgtm-board's mod_rack.rs)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct RackSlotModule;

impl RackSlotModule {
    fn items(&self, st: &DeskState) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        if st.song_gen > 0 || !st.parent_id.is_empty() {
            out.push(("parent".into(), "__parent__".into(), format!("BACK (g{})", (st.song_gen - 1).max(0))));
        }
        for s in &st.saves {
            let lbl = if s.title.contains("(gen ") || s.title.contains("(g") {
                s.title.clone()
            } else {
                format!("{} (g{})", s.title, s.gen)
            };
            out.push(("save".into(), s.id.clone(), lbl));
        }
        for l in &st.library {
            let lbl = if l.title.contains("(gen ") || l.title.contains("(g") {
                l.title.clone()
            } else {
                format!("{} (g{})", l.title, l.gen)
            };
            out.push(("lib".into(), l.id.clone(), lbl));
        }
        out
    }

    fn row_rect(&self, idx: usize) -> PanelRect {
        let r = RECT_RACK;
        PanelRect::new(r.x + 5.0, r.y + 13.5 + idx as f32 * 8.2, r.w - 10.0, 7.0)
    }
}

impl DeskModule for RackSlotModule {
    fn id(&self) -> &'static str { "rack" }
    fn title(&self) -> &'static str { "RACK" }
    fn rect(&self) -> PanelRect { RECT_RACK }
    fn needs_animation(&self, _st: &DeskState) -> bool { false }
    fn step(&mut self, _dt: f32, _st: &DeskState, _cmds: &mut DeskCmds) {}

    fn draw(&self, p: &mut PanelDraw, st: &DeskState) {
        let r = RECT_RACK;
        let items = self.items(st);
        p.module_frame(r, "RACK", &format!("{} tapes", items.len()), vec4(0.92, 0.56, 0.24, 1.0));
        for (i, (kind, id, title)) in items.iter().take(4).enumerate() {
            let rr = self.row_rect(i);
            let active = id == &st.song_id;
            let bg = if active {
                vec4(0.18, 0.34, 0.54, 1.0)
            } else if kind == "parent" {
                vec4(0.24, 0.18, 0.14, 1.0)
            } else {
                vec4(0.13, 0.17, 0.24, 1.0)
            };
            p.pill(rr, bg, vec4(0.94, 0.97, 1.0, 1.0), title);
        }
    }

    fn hit_control(&self, px: f32, py: f32, st: &DeskState) -> Option<ControlHit> {
        let n = self.items(st).len().min(4);
        for i in 0..n {
            if self.row_rect(i).contains(px, py) {
                return Some(ControlHit::new(i as u32, 12.0));
            }
        }
        None
    }

    fn on_touch(&mut self, ev: ModuleTouch, st: &DeskState, cmds: &mut DeskCmds) {
        if ev.kind != TouchKind::Up { return; }
        let items = self.items(st);
        let Some((kind, id, _)) = items.get(ev.control_id as usize) else { return; };
        match kind.as_str() {
            "parent" => cmds.cmd_pending("back", "{}", "back"),
            "save" => cmds.cmd_pending("load", &format!("{{\"id\":\"{}\"}}", json_escape(id)), "load"),
            _ => cmds.cmd_pending("pick", &format!("{{\"q\":\"{}\"}}", json_escape(id)), "pick"),
        }
    }
}

// ---------------------------------------------------------------------------
// LINK (placeholder for lgtm-board's mod_link.rs)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct LinkSlotModule;

impl LinkSlotModule {
    fn reconn_rect(&self) -> PanelRect {
        let r = RECT_LINK;
        PanelRect::new(r.right() - 32.0, r.y + 14.0, 27.0, 9.0)
    }
}

impl DeskModule for LinkSlotModule {
    fn id(&self) -> &'static str { "link" }
    fn title(&self) -> &'static str { "LINK" }
    fn rect(&self) -> PanelRect { RECT_LINK }
    fn needs_animation(&self, _st: &DeskState) -> bool { false }
    fn step(&mut self, _dt: f32, _st: &DeskState, _cmds: &mut DeskCmds) {}

    fn draw(&self, p: &mut PanelDraw, st: &DeskState) {
        let r = RECT_LINK;
        p.module_frame(r, "LINK", &st.link, vec4(0.24, 0.88, 0.62, 1.0));

        // Link & Head LEDs
        let link_col = match st.link.as_str() {
            "sound" => vec4(0.22, 0.96, 0.56, 1.0),
            "live" => vec4(0.26, 0.80, 0.98, 1.0),
            "connecting" => vec4(0.98, 0.76, 0.24, 1.0),
            _ => vec4(0.85, 0.26, 0.30, 1.0),
        };
        p.dot(r.x + 9.0, r.y + 18.5, 2.3, link_col, 0.85);
        p.text(r.x + 14.0, r.y + 18.5, 3.6, vec4(0.90, 0.95, 1.0, 0.96), TextAlign::Left, &format!("NET {}", st.link));

        let head_col = if st.head_cache_backing { vec4(0.24, 0.94, 0.60, 1.0) } else { vec4(0.45, 0.52, 0.62, 1.0) };
        p.dot(r.x + 48.0, r.y + 18.5, 2.3, head_col, if st.head_cache_backing { 0.8 } else { 0.1 });
        p.text(r.x + 53.0, r.y + 18.5, 3.6, vec4(0.90, 0.95, 1.0, 0.96), TextAlign::Left, &format!("HEAD u={}", st.head_underruns));

        p.pill(self.reconn_rect(), vec4(0.16, 0.22, 0.32, 1.0), vec4(0.85, 0.92, 1.0, 1.0), "SYNC");

        // Cache fill bar
        let total = st.src_lines.len().max(1) as f32;
        let fill = (st.head_cache_lines as f32 / total).clamp(0.0, 1.0);
        let cache_lbl = format!("{}/{} lines", st.head_cache_lines, st.src_lines.len());
        p.meter(PanelRect::new(r.x + 6.0, r.y + 30.0, r.w - 12.0, 5.0), fill, vec4(0.24, 0.84, 0.96, 1.0), false, "CACHE", &cache_lbl);

        // Current engine note / active render status
        let note = if st.note.is_empty() { st.status.as_str() } else { st.note.as_str() };
        p.text_clip(r.x + 6.0, r.bottom() - 5.0, r.w - 12.0, 3.3, vec4(0.76, 0.84, 0.94, 0.92), TextAlign::Left, note);
    }

    fn hit_control(&self, px: f32, py: f32, _st: &DeskState) -> Option<ControlHit> {
        if self.reconn_rect().contains(px, py) {
            return Some(ControlHit::new(1, 14.0));
        }
        None
    }

    fn on_touch(&mut self, ev: ModuleTouch, _st: &DeskState, cmds: &mut DeskCmds) {
        if ev.kind == TouchKind::Up && ev.control_id == 1 {
            cmds.emit_reconnect();
        }
    }
}
