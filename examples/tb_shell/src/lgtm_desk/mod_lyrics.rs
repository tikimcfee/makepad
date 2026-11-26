//! LYRICS module (`RECT_LYRICS`): source & rewritten lyrics under the head, with per-band
//! colored changed-word chips, `[FIRM]` / `[LOOP]` status pills, and tap-to-loop / tap-to-seek.

use super::module::{
    parse_hex_color, ControlHit, DeskModule, ModuleTouch, PanelDraw, PanelRect, TextAlign,
    TouchKind, RECT_LYRICS,
};
use super::wire::{DeskCmds, DeskState};
use makepad_widgets::{vec4, Vec4f};
use std::time::Instant;

const CTRL_PREV_LINE: u32 = 1;
const CTRL_CUR_LINE: u32 = 2;
const CTRL_NEXT_LINE: u32 = 3;
const CTRL_LOOP_PILL: u32 = 4;

#[derive(Default)]
pub struct LyricsModule;

fn palette_color(idx: i32, st: &DeskState) -> (Vec4f, Vec4f) {
    // Returns (chip_bg, text_hi) matching lgtm.splash's 8-color band palette (-1 = settled neutral)
    let hex = if idx >= 0 {
        st.bands
            .get(idx as usize)
            .map(|b| b.color_hex.as_str())
            .unwrap_or("")
    } else {
        ""
    };
    if idx < 0 {
        return (vec4(0.12, 0.16, 0.23, 1.0), vec4(0.89, 0.91, 0.94, 1.0));
    }
    let fallback = match idx.rem_euclid(8) {
        0 => vec4(0.22, 0.74, 0.97, 1.0),
        1 => vec4(0.96, 0.62, 0.04, 1.0),
        2 => vec4(0.65, 0.55, 0.98, 1.0),
        3 => vec4(0.20, 0.83, 0.60, 1.0),
        4 => vec4(0.98, 0.44, 0.52, 1.0),
        5 => vec4(0.38, 0.65, 0.98, 1.0),
        6 => vec4(0.98, 0.75, 0.14, 1.0),
        _ => vec4(0.91, 0.47, 0.98, 1.0),
    };
    let c = parse_hex_color(hex, fallback);
    let bg = vec4(
        (c.x * 0.28).max(0.06),
        (c.y * 0.28).max(0.08),
        (c.z * 0.28).max(0.11),
        1.0,
    );
    let hi = vec4(
        (c.x * 0.65 + 0.35).min(1.0),
        (c.y * 0.65 + 0.35).min(1.0),
        (c.z * 0.65 + 0.35).min(1.0),
        1.0,
    );
    (bg, hi)
}

fn current_line_index(st: &DeskState, now: Instant) -> usize {
    let total = st.src_lines.len();
    if total == 0 {
        return 0;
    }
    let moving = st.playing || st.scrubbing;
    if (moving || st.line < 0) && !st.line_times_ms.is_empty() {
        if let Some(i) = st.line_index_at_ms(st.heard_pos_ms(now)) {
            return i.min(total - 1);
        }
    }
    if st.line >= 0 {
        (st.line as usize).min(total - 1)
    } else {
        0
    }
}

fn is_line_looped(st: &DeskState, idx: usize) -> bool {
    if !st.loop_active {
        return false;
    }
    if st.loop_line >= 0 {
        return st.loop_line as usize == idx;
    }
    if st.loop_a_ms >= 0.0 && st.loop_b_ms > st.loop_a_ms {
        if let Some(&t0) = st.line_times_ms.get(idx) {
            return t0 >= (st.loop_a_ms - 150.0) && t0 < (st.loop_b_ms - 50.0);
        }
    }
    false
}

fn chg_band_for(pairs: &[(usize, i32)], widx: usize) -> Option<i32> {
    pairs.iter().find(|(p, _)| *p == widx).map(|(_, b)| *b)
}

impl LyricsModule {
    fn prev_rect(&self) -> PanelRect {
        let r = RECT_LYRICS;
        PanelRect::new(r.x + 4.0, r.y + 12.0, r.w - 8.0, 7.5)
    }
    fn cur_rect(&self) -> PanelRect {
        let r = RECT_LYRICS;
        PanelRect::new(r.x + 4.0, r.y + 20.5, r.w - 8.0, 18.0)
    }
    fn next_rect(&self) -> PanelRect {
        let r = RECT_LYRICS;
        PanelRect::new(r.x + 4.0, r.y + 39.5, r.w - 8.0, 8.0)
    }
    fn loop_pill_rect(&self) -> PanelRect {
        let r = RECT_LYRICS;
        PanelRect::new(r.right() - 28.0, r.y + 22.0, 22.0, 6.4)
    }
}

impl DeskModule for LyricsModule {
    fn id(&self) -> &'static str { "lyrics" }
    fn title(&self) -> &'static str { "LYRICS" }
    fn rect(&self) -> PanelRect { RECT_LYRICS }
    fn needs_animation(&self, _st: &DeskState) -> bool { false }
    fn step(&mut self, _dt: f32, _st: &DeskState, _cmds: &mut DeskCmds) {}

    fn draw(&self, p: &mut PanelDraw, st: &DeskState) {
        let r = RECT_LYRICS;
        let now = Instant::now();
        let total = st.src_lines.len();
        let cur = current_line_index(st, now);
        let cur_looped = total > 0 && is_line_looped(st, cur);
        let cur_settled = st.settled.get(cur).copied().unwrap_or(false);

        let pos_ms = st.heard_pos_ms(now);
        let cur_sec = st
            .sections
            .iter()
            .find(|s| pos_ms >= s.t0_ms && pos_ms < s.t1_ms);

        let right = if total == 0 {
            "waiting for lyrics".to_string()
        } else if st.loop_active {
            if st.transport_pass > 0 {
                format!("L {}/{} · LOOP #{}", cur + 1, total, st.transport_pass)
            } else {
                format!("L {}/{} · LOOP", cur + 1, total)
            }
        } else if let Some(sec) = cur_sec {
            format!("L {}/{} · {}", cur + 1, total, sec.state)
        } else if cur_settled {
            format!("L {}/{} · FIRM", cur + 1, total)
        } else {
            format!("L {}/{}", cur + 1, total)
        };
        let accent = if st.loop_active {
            vec4(0.24, 0.86, 0.98, 1.0)
        } else {
            vec4(0.38, 0.72, 0.98, 1.0)
        };
        p.module_frame(r, "LYRICS", &right, accent);

        if total == 0 {
            p.text(
                r.cx(),
                r.cy() + 5.0,
                4.0,
                vec4(0.62, 0.70, 0.82, 0.90),
                TextAlign::Center,
                "Pick a song in RACK or hold PTT",
            );
            return;
        }

        // Previous line
        if cur > 0 {
            let pr = self.prev_rect();
            let prev_txt = st.line_text_at(cur - 1);
            let prev_chg = !st.line_chg_at(cur - 1).is_empty();
            let col = if prev_chg {
                vec4(0.68, 0.78, 0.90, 0.90)
            } else {
                vec4(0.56, 0.65, 0.78, 0.88)
            };
            p.text_clip(pr.x + 3.0, pr.cy(), pr.w - 6.0, 3.6, col, TextAlign::Left, prev_txt);
        }

        // Active line card (tap to toggle line loop)
        let cr = self.cur_rect();
        let card_bg = if cur_looped {
            vec4(0.11, 0.21, 0.34, 1.0)
        } else {
            vec4(0.10, 0.15, 0.24, 1.0)
        };
        p.plate(cr, 0.68, 0.30, card_bg, 0.25, if cur_looped { 0.14 } else { 0.06 });

        let out_s = st.line_text_at(cur);
        let src_s = st.src_lines.get(cur).map(|s| s.as_str()).unwrap_or("");
        let chg_pairs = st.line_chg_at(cur);

        // Right-edge status pills inside active card ([LOOP] / [FIRM] / change count)
        let mut right_reserve = 6.0f32;
        if cur_looped || st.loop_active {
            let lr = self.loop_pill_rect();
            let lbl = if st.transport_pass > 0 {
                format!("LP#{}", st.transport_pass)
            } else {
                "LOOP".to_string()
            };
            p.pill(lr, vec4(0.12, 0.34, 0.56, 1.0), vec4(0.84, 0.96, 1.0, 1.0), &lbl);
            right_reserve = 28.0;
        } else if !chg_pairs.is_empty() {
            let lr = self.loop_pill_rect();
            p.pill(
                PanelRect::new(lr.x + 6.0, lr.y, 16.0, lr.h),
                vec4(0.14, 0.26, 0.42, 1.0),
                vec4(0.82, 0.94, 1.0, 1.0),
                &format!("{}w", chg_pairs.len()),
            );
            right_reserve = 22.0;
        }

        // Upper row of active card: word-by-word rewritten lyric with per-band colored chips
        let row_y = cr.y + 5.8;
        let max_x = cr.right() - right_reserve;
        if chg_pairs.is_empty() {
            p.text_clip(
                cr.x + 3.5,
                row_y,
                (max_x - cr.x - 4.0).max(16.0),
                5.4,
                vec4(0.98, 0.99, 1.0, 1.0),
                TextAlign::Left,
                out_s,
            );
        } else {
            let mut x = cr.x + 3.5;
            let sz = 5.0f32;
            let char_w = sz * 0.56;
            for (wi, word) in out_s.split_whitespace().enumerate() {
                let w_chars = word.chars().count() as f32;
                let band_opt = chg_band_for(chg_pairs, wi);
                let pad = if band_opt.is_some() { 3.2 } else { 0.8 };
                let w_u = (w_chars * char_w + pad).max(4.8);
                if x + w_u > max_x {
                    if x + 4.0 <= max_x {
                        p.text(x, row_y, sz, vec4(0.75, 0.82, 0.92, 0.9), TextAlign::Left, "…");
                    }
                    break;
                }
                if let Some(bidx) = band_opt {
                    let (bg, hi) = palette_color(bidx, st);
                    let chip = PanelRect::new(x, row_y - 3.6, w_u, 7.2);
                    p.plate(chip, 0.95, 0.35, bg, 0.30, 0.16);
                    p.text(chip.cx(), row_y, sz, hi, TextAlign::Center, word);
                } else {
                    p.text(x, row_y, sz, vec4(0.96, 0.98, 1.0, 1.0), TextAlign::Left, word);
                }
                x += w_u + 1.8;
            }
        }

        // Lower row of active card: original source line + FIRM tag
        let src_row_y = cr.y + 13.8;
        let src_col = if cur_settled {
            vec4(0.45, 0.92, 0.72, 0.94)
        } else {
            vec4(0.62, 0.72, 0.86, 0.90)
        };
        let src_lbl = if cur_settled {
            format!("{}  [FIRM]", src_s)
        } else {
            src_s.to_string()
        };
        p.text_clip(cr.x + 3.5, src_row_y, cr.w - 8.0, 3.5, src_col, TextAlign::Left, &src_lbl);

        // Next line
        if cur + 1 < total {
            let nr = self.next_rect();
            let next_txt = st.line_text_at(cur + 1);
            let next_chg = !st.line_chg_at(cur + 1).is_empty();
            let col = if next_chg {
                vec4(0.74, 0.84, 0.96, 0.92)
            } else {
                vec4(0.64, 0.72, 0.84, 0.90)
            };
            p.text_clip(nr.x + 3.0, nr.cy(), nr.w - 6.0, 3.6, col, TextAlign::Left, next_txt);
        }
    }

    fn hit_control(&self, px: f32, py: f32, st: &DeskState) -> Option<ControlHit> {
        if st.src_lines.is_empty() {
            return None;
        }
        if st.loop_active && self.loop_pill_rect().contains(px, py) {
            return Some(ControlHit::new(CTRL_LOOP_PILL, 16.0));
        }
        if self.cur_rect().contains(px, py) {
            return Some(ControlHit::new(CTRL_CUR_LINE, 18.0));
        }
        if self.prev_rect().contains(px, py) {
            return Some(ControlHit::new(CTRL_PREV_LINE, 16.0));
        }
        if self.next_rect().contains(px, py) {
            return Some(ControlHit::new(CTRL_NEXT_LINE, 16.0));
        }
        None
    }

    fn on_touch(&mut self, ev: ModuleTouch, st: &DeskState, cmds: &mut DeskCmds) {
        if ev.kind != TouchKind::Up {
            return;
        }
        let total = st.src_lines.len();
        if total == 0 {
            return;
        }
        let cur = current_line_index(st, Instant::now());
        match ev.control_id {
            CTRL_LOOP_PILL => {
                cmds.cmd_pending("loop", "{}", "loop");
            }
            CTRL_CUR_LINE => {
                if is_line_looped(st, cur) {
                    cmds.cmd_pending("loop", "{}", "loop");
                } else {
                    cmds.cmd_pending("loop", &format!("{{\"line\":{}}}", cur), "loop");
                }
            }
            CTRL_PREV_LINE => {
                if cur > 0 {
                    if let Some(&ms) = st.line_times_ms.get(cur - 1) {
                        if ms >= 0.0 {
                            cmds.cmd_pending("seek", &format!("{{\"ms\":{:.0}}}", ms), "seek");
                        }
                    }
                }
            }
            CTRL_NEXT_LINE => {
                if cur + 1 < total {
                    if let Some(&ms) = st.line_times_ms.get(cur + 1) {
                        if ms >= 0.0 {
                            cmds.cmd_pending("seek", &format!("{{\"ms\":{:.0}}}", ms), "seek");
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
