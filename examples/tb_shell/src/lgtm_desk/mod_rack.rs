//! RACK module (`RECT_RACK`): saved takes, parent generation rewind, and song library
//! with paging so every tape on monolith1 is one tap away.

use super::module::{
    ControlHit, DeskModule, ModuleTouch, PanelDraw, PanelRect, TextAlign, TouchKind, RECT_RACK,
};
use super::wire::{json_escape, DeskCmds, DeskState};
use makepad_widgets::vec4;

const ROWS_PER_PAGE: usize = 4;
const CTRL_PAGE_PREV: u32 = 100;
const CTRL_PAGE_NEXT: u32 = 101;

#[derive(Default)]
pub struct RackModule {
    page: usize,
}

struct RackRow {
    kind: &'static str, // "parent" | "save" | "lib"
    id: String,
    label: String,
}

impl RackModule {
    fn rows(&self, st: &DeskState) -> Vec<RackRow> {
        let mut out = Vec::new();
        if st.song_gen > 0 || !st.parent_id.is_empty() {
            out.push(RackRow {
                kind: "parent",
                id: "__parent__".into(),
                label: format!("← Parent  g{}", (st.song_gen - 1).max(0)),
            });
        }
        for s in &st.saves {
            let lbl = if s.gen > 0 {
                format!("TAKE · {} · g{}", s.title, s.gen)
            } else {
                format!("TAKE · {}", s.title)
            };
            out.push(RackRow {
                kind: "save",
                id: s.id.clone(),
                label: lbl,
            });
        }
        for l in &st.library {
            let lbl = if l.gen > 0 {
                format!("{} · g{}", l.title, l.gen)
            } else {
                l.title.clone()
            };
            out.push(RackRow {
                kind: "lib",
                id: l.id.clone(),
                label: lbl,
            });
        }
        out
    }

    fn page_count(&self, n: usize) -> usize {
        if n <= ROWS_PER_PAGE {
            1
        } else {
            (n + ROWS_PER_PAGE - 1) / ROWS_PER_PAGE
        }
    }

    fn row_rect(&self, slot: usize) -> PanelRect {
        let r = RECT_RACK;
        PanelRect::new(r.x + 5.0, r.y + 13.0 + slot as f32 * 8.4, r.w - 10.0, 7.2)
    }

    fn prev_rect(&self) -> PanelRect {
        let r = RECT_RACK;
        PanelRect::new(r.right() - 34.0, r.y + 2.0, 11.5, 7.2)
    }

    fn next_rect(&self) -> PanelRect {
        let r = RECT_RACK;
        PanelRect::new(r.right() - 20.5, r.y + 2.0, 11.5, 7.2)
    }
}

impl DeskModule for RackModule {
    fn id(&self) -> &'static str { "rack" }
    fn title(&self) -> &'static str { "RACK" }
    fn rect(&self) -> PanelRect { RECT_RACK }
    fn needs_animation(&self, _st: &DeskState) -> bool { false }
    fn step(&mut self, _dt: f32, _st: &DeskState, _cmds: &mut DeskCmds) {}

    fn draw(&self, p: &mut PanelDraw, st: &DeskState) {
        let r = RECT_RACK;
        let rows = self.rows(st);
        let pages = self.page_count(rows.len());
        let pg = self.page.min(pages.saturating_sub(1));

        let hdr_right = if pages > 1 {
            ""
        } else if st.song_gen > 0 {
            // Show current generation badge when single page
            "tapes"
        } else {
            "tapes"
        };
        let count_lbl = if pages > 1 {
            format!("{}/{}", pg + 1, pages)
        } else {
            format!("{}s · {}L", st.saves.len(), st.library.len())
        };
        p.module_frame(r, "RACK", if pages > 1 { hdr_right } else { &count_lbl }, vec4(0.92, 0.56, 0.24, 1.0));

        if pages > 1 {
            p.title_text(
                r.right() - 37.5,
                r.y + 5.7,
                4.0,
                vec4(0.75, 0.82, 0.92, 0.94),
                TextAlign::Right,
                &count_lbl,
            );
            let pr = self.prev_rect();
            p.plate(pr, 0.95, 0.45, vec4(0.16, 0.21, 0.30, 1.0), 0.35, 0.12);
            p.title_text(pr.cx(), pr.cy(), 4.2, vec4(0.92, 0.96, 1.0, 1.0), TextAlign::Center, "<");
            let nr = self.next_rect();
            p.plate(nr, 0.95, 0.45, vec4(0.16, 0.21, 0.30, 1.0), 0.35, 0.12);
            p.title_text(nr.cx(), nr.cy(), 4.2, vec4(0.92, 0.96, 1.0, 1.0), TextAlign::Center, ">");
        }

        if rows.is_empty() {
            p.text_clip(
                r.cx(),
                r.cy() + 5.0,
                r.w - 12.0,
                3.6,
                vec4(0.62, 0.70, 0.82, 0.88),
                TextAlign::Center,
                &st.song_title,
            );
            return;
        }

        let start = pg * ROWS_PER_PAGE;
        for (slot, row) in rows.iter().skip(start).take(ROWS_PER_PAGE).enumerate() {
            let rr = self.row_rect(slot);
            let is_active = row.id == st.song_id && row.kind == "lib";
            let (bg, fg, dot_col) = if is_active {
                (
                    vec4(0.16, 0.33, 0.54, 1.0),
                    vec4(0.98, 1.0, 1.0, 1.0),
                    vec4(0.26, 0.92, 0.58, 1.0),
                )
            } else if row.kind == "parent" {
                (
                    vec4(0.24, 0.18, 0.12, 1.0),
                    vec4(0.98, 0.86, 0.62, 1.0),
                    vec4(0.96, 0.72, 0.28, 1.0),
                )
            } else if row.kind == "save" {
                (
                    vec4(0.14, 0.20, 0.30, 1.0),
                    vec4(0.86, 0.94, 1.0, 1.0),
                    vec4(0.94, 0.54, 0.24, 1.0),
                )
            } else {
                (
                    vec4(0.12, 0.15, 0.22, 1.0),
                    vec4(0.84, 0.90, 0.96, 1.0),
                    vec4(0.36, 0.58, 0.82, 1.0),
                )
            };
            p.plate(rr, 0.92, 0.40, bg, 0.32, if is_active { 0.16 } else { 0.04 });
            p.dot(rr.x + 3.6, rr.cy(), 1.3, dot_col, if is_active { 0.9 } else { 0.35 });
            p.text_clip(
                rr.x + 7.2,
                rr.cy(),
                rr.w - 10.0,
                4.8,
                fg,
                TextAlign::Left,
                &row.label,
            );
        }
    }

    fn hit_control(&self, px: f32, py: f32, st: &DeskState) -> Option<ControlHit> {
        let rows = self.rows(st);
        let pages = self.page_count(rows.len());
        if pages > 1 {
            if self.prev_rect().contains(px, py) {
                return Some(ControlHit::new(CTRL_PAGE_PREV, 15.0));
            }
            if self.next_rect().contains(px, py) {
                return Some(ControlHit::new(CTRL_PAGE_NEXT, 15.0));
            }
        }
        let pg = self.page.min(pages.saturating_sub(1));
        let start = pg * ROWS_PER_PAGE;
        let vis = rows.len().saturating_sub(start).min(ROWS_PER_PAGE);
        for slot in 0..vis {
            if self.row_rect(slot).contains(px, py) {
                return Some(ControlHit::new(slot as u32, 16.0));
            }
        }
        None
    }

    fn on_touch(&mut self, ev: ModuleTouch, st: &DeskState, cmds: &mut DeskCmds) {
        if ev.kind != TouchKind::Up {
            return;
        }
        let rows = self.rows(st);
        let pages = self.page_count(rows.len());
        match ev.control_id {
            CTRL_PAGE_PREV => {
                self.page = if self.page == 0 { pages.saturating_sub(1) } else { self.page - 1 };
                cmds.request_redraw = true;
                return;
            }
            CTRL_PAGE_NEXT => {
                self.page = (self.page + 1) % pages.max(1);
                cmds.request_redraw = true;
                return;
            }
            _ => {}
        }
        let pg = self.page.min(pages.saturating_sub(1));
        let idx = pg * ROWS_PER_PAGE + ev.control_id as usize;
        let Some(row) = rows.get(idx) else { return; };
        match row.kind {
            "parent" => cmds.cmd_pending("back", "{}", "back"),
            "save" => cmds.cmd_pending("load", &format!("{{\"id\":\"{}\"}}", json_escape(&row.id)), "load"),
            _ => cmds.cmd_pending("pick", &format!("{{\"q\":\"{}\"}}", json_escape(&row.id)), "pick"),
        }
    }
}
