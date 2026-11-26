//! LINK module (`RECT_LINK`): network link & Stage-2 head LEDs, engine subsystem health,
//! backing & voice cache fill bars, active singing/ack status, and manual reconnect trigger.

use super::module::{
    ControlHit, DeskModule, ModuleTouch, PanelDraw, PanelRect, TextAlign, TouchKind, RECT_LINK,
};
use super::wire::{DeskCmds, DeskState};
use makepad_widgets::vec4;

const CTRL_RECONNECT: u32 = 1;

#[derive(Default)]
pub struct LinkModule;

impl LinkModule {
    fn sync_rect(&self) -> PanelRect {
        let r = RECT_LINK;
        PanelRect::new(r.right() - 27.0, r.y + 13.0, 22.0, 8.2)
    }
}

impl DeskModule for LinkModule {
    fn id(&self) -> &'static str { "link" }
    fn title(&self) -> &'static str { "LINK" }
    fn rect(&self) -> PanelRect { RECT_LINK }
    fn needs_animation(&self, _st: &DeskState) -> bool { false }
    fn step(&mut self, _dt: f32, _st: &DeskState, _cmds: &mut DeskCmds) {}

    fn draw(&self, p: &mut PanelDraw, st: &DeskState) {
        let r = RECT_LINK;
        let head_active = st.head_stage >= 2 && st.head_runs && !st.head_live;
        let eff_link = if head_active && (st.playing || st.scrubbing) && st.link == "live" {
            "sound"
        } else {
            st.link.as_str()
        };
        let hdr_right = if head_active {
            format!("{} · s{}", eff_link, st.head_stage)
        } else {
            eff_link.to_string()
        };
        p.module_frame(r, "LINK", &hdr_right, vec4(0.24, 0.88, 0.62, 1.0));

        // Row 1: NET light + HEAD light + ENG parts light + SYNC button
        let row1_y = r.y + 17.2;
        let link_col = match eff_link {
            "sound" => vec4(0.22, 0.96, 0.56, 1.0),
            "live" => vec4(0.26, 0.80, 0.98, 1.0),
            "connecting" => vec4(0.98, 0.76, 0.24, 1.0),
            _ => vec4(0.88, 0.28, 0.32, 1.0),
        };
        p.dot(r.x + 8.0, row1_y, 2.0, link_col, 0.85);
        p.text(r.x + 12.0, row1_y, 4.7, vec4(0.90, 0.95, 1.0, 0.96), TextAlign::Left, "NET");

        let (head_col, head_emit) = if st.head_underruns > 0 {
            (vec4(0.98, 0.32, 0.34, 1.0), 0.95)
        } else if head_active && st.head_cache_backing_frac >= 0.99 {
            (vec4(0.24, 0.94, 0.60, 1.0), 0.85)
        } else if st.head_cache_backing_frac > 0.0 {
            (vec4(0.96, 0.74, 0.24, 1.0), 0.70)
        } else {
            (vec4(0.42, 0.50, 0.60, 1.0), 0.15)
        };
        p.dot(r.x + 33.5, row1_y, 2.0, head_col, head_emit);
        let head_lbl = if st.head_underruns > 0 {
            format!("u{}", st.head_underruns)
        } else if head_active {
            "HD".to_string()
        } else {
            "LV".to_string()
        };
        p.text(r.x + 37.5, row1_y, 4.7, vec4(0.90, 0.95, 1.0, 0.96), TextAlign::Left, &head_lbl);

        // Engine parts light
        let (eng_col, eng_emit, eng_lbl) = if st.parts_total > 0 {
            let all_ok = st.parts_done >= st.parts_total;
            (
                if all_ok { vec4(0.24, 0.92, 0.58, 1.0) } else { vec4(0.96, 0.74, 0.24, 1.0) },
                if all_ok { 0.75 } else { 0.85 },
                format!("{}/{}", st.parts_done, st.parts_total),
            )
        } else {
            (
                if st.status == "error" { vec4(0.96, 0.30, 0.34, 1.0) } else { vec4(0.36, 0.64, 0.94, 1.0) },
                0.55,
                "ENG".to_string(),
            )
        };
        p.dot(r.x + 56.0, row1_y, 2.0, eng_col, eng_emit);
        p.text(r.x + 60.0, row1_y, 4.7, vec4(0.86, 0.92, 0.98, 0.94), TextAlign::Left, &eng_lbl);

        let sr = self.sync_rect();
        p.plate(sr, 0.95, 0.45, vec4(0.15, 0.22, 0.34, 1.0), 0.35, 0.14);
        p.title_text(sr.cx(), sr.cy(), 4.2, vec4(0.88, 0.95, 1.0, 1.0), TextAlign::Center, "SYNC");

        // Row 2: Backing cache fill (left) + Voice lines cache fill (right)
        let bar_y = r.y + 28.5;
        let half_w = (r.w - 16.0) * 0.5;
        let bk_frac = if st.head_cache_backing_frac > 0.0 {
            st.head_cache_backing_frac.clamp(0.0, 1.0)
        } else if st.head_cache_backing {
            1.0
        } else {
            0.0
        };
        let bk_val = format!("{}%", (bk_frac * 100.0).round() as i32);
        p.meter(
            PanelRect::new(r.x + 6.0, bar_y, half_w, 4.8),
            bk_frac,
            vec4(0.24, 0.86, 0.64, 1.0),
            false,
            "BACK",
            &bk_val,
        );

        let total_lines = st.src_lines.len().max(1) as f32;
        let ln_frac = (st.head_cache_lines.max(0) as f32 / total_lines).clamp(0.0, 1.0);
        let ln_val = format!("{}/{}", st.head_cache_lines.max(0), st.src_lines.len());
        p.meter(
            PanelRect::new(r.x + 10.0 + half_w, bar_y, half_w, 4.8),
            ln_frac,
            vec4(0.24, 0.80, 0.98, 1.0),
            false,
            "VOX",
            &ln_val,
        );

        // Row 3: Left = active singing / ack / note; Right = live FPS + worst frame gap meter
        let singing_sec = st.sections.iter().find(|s| s.state == "singing");
        let (foot_txt, foot_col) = if let Some(sec) = singing_sec {
            let nm = if sec.label.is_empty() {
                format!("sec {}", sec.idx + 1)
            } else {
                sec.label.clone()
            };
            (format!("SING · {}", nm), vec4(0.84, 0.68, 1.0, 0.98))
        } else if st.singer_state == "singing" || st.singer_state == "loading" {
            let nm = if st.singer_id.is_empty() { "singer" } else { st.singer_id.as_str() };
            (format!("{} {}", nm, st.singer_state), vec4(0.84, 0.68, 1.0, 0.98))
        } else if st.ack_state == "refused" && !st.ack_why.is_empty() {
            (format!("REFUSED · {}", st.ack_why), vec4(0.98, 0.58, 0.60, 0.98))
        } else if st.ack_state == "queued" && !st.ack_name.is_empty() {
            (format!("QUEUED · {}", st.ack_name), vec4(0.98, 0.86, 0.50, 0.96))
        } else if !st.note.is_empty() {
            (st.note.clone(), vec4(0.76, 0.84, 0.94, 0.92))
        } else if !st.song_artist.is_empty() {
            (
                format!("{} · {}", st.transport_state, st.song_artist),
                vec4(0.72, 0.80, 0.90, 0.90),
            )
        } else {
            (st.transport_state.clone(), vec4(0.72, 0.80, 0.90, 0.90))
        };

        let (perf_txt, perf_col) = if st.perf_fps < 0.5 && st.perf_max_gap_ms < 0.5 {
            ("0f · idle".to_string(), vec4(0.54, 0.64, 0.76, 0.92))
        } else {
            let gap = st.perf_max_gap_ms.max(0.0);
            let col = if gap <= 36.0 {
                vec4(0.30, 0.94, 0.60, 1.0)
            } else if gap <= 60.0 {
                vec4(0.98, 0.78, 0.28, 1.0)
            } else {
                vec4(0.98, 0.36, 0.38, 1.0)
            };
            (format!("{:.0}f · {:.0}ms", st.perf_fps.max(0.0).round(), gap.round()), col)
        };
        let foot_y = r.bottom() - 4.8;
        p.text_clip(r.x + 6.0, foot_y, r.w - 48.0, 4.7, foot_col, TextAlign::Left, &foot_txt);
        p.text(r.right() - 6.0, foot_y, 4.6, perf_col, TextAlign::Right, &perf_txt);
    }

    fn hit_control(&self, px: f32, py: f32, _st: &DeskState) -> Option<ControlHit> {
        if self.sync_rect().contains(px, py) {
            return Some(ControlHit::new(CTRL_RECONNECT, 16.0));
        }
        None
    }

    fn on_touch(&mut self, ev: ModuleTouch, _st: &DeskState, cmds: &mut DeskCmds) {
        if ev.kind == TouchKind::Up && ev.control_id == CTRL_RECONNECT {
            cmds.emit_reconnect();
        }
    }
}
