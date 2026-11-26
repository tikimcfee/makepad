//! OUT module: 4 weighted 3D output faders (LEVEL, BALANCE, TONE, DUCK -> `lgtm_out`) + 2 live
//! 3D VU meters (ORIG, NEW). Double-tap any fader to reset to rest.

use super::module::{ControlHit, DeskModule, ModuleTouch, PanelDraw, PanelRect, TouchKind, RECT_OUT};
use super::wire::{DeskCmds, DeskState};
use makepad_widgets::{vec4, Vec4f};
use std::time::{Duration, Instant};

const NAMES: [&str; 4] = ["LEVEL", "BAL", "TONE", "DUCK"];
const REST: [f32; 4] = [0.75, 0.50, 0.50, 0.62];

pub struct OutModule {
    val: [f32; 4],
    disp: [f32; 4],
    vel: [f32; 4],
    grabbed: Option<usize>,
    grab_start_y: f32,
    grab_start_val: f32,
    last_time: f64,
    last_out_rev: u64,
    local_hold_until: [Option<Instant>; 4],
}

impl Default for OutModule {
    fn default() -> Self {
        Self {
            val: REST,
            disp: REST,
            vel: [0.0; 4],
            grabbed: None,
            grab_start_y: 0.0,
            grab_start_val: 0.0,
            last_time: 0.0,
            last_out_rev: 0,
            local_hold_until: [None; 4],
        }
    }
}

impl OutModule {
    fn fader_geom(&self, idx: usize) -> (f32, f32, f32, f32) {
        let r = RECT_OUT;
        // 4 vertical faders across left 84 u, leaving right 26 u for dual VU meters
        let cx = r.x + 13.0 + idx as f32 * 20.5;
        let cy = r.y + 42.5;
        (cx, cy, 16.0, 35.0)
    }

    fn fader_hit_rect(&self, idx: usize) -> PanelRect {
        let (cx, cy, w, h) = self.fader_geom(idx);
        PanelRect::new(cx - w * 0.55, cy - h * 0.58, w * 1.10, h * 1.16)
    }

    pub fn sync_from_state(&mut self, st: &DeskState) {
        if st.out_rev == self.last_out_rev {
            return;
        }
        self.last_out_rev = st.out_rev;
        let now = Instant::now();
        let incoming = [st.out_level, st.out_balance, st.out_tone, st.out_duck];
        for i in 0..4 {
            let v = incoming[i].clamp(0.0, 1.0);
            if self.grabbed == Some(i) || self.vel[i].abs() >= 0.01 {
                continue;
            }
            if let Some(until) = self.local_hold_until[i] {
                if (v - self.val[i]).abs() <= 0.008 || now >= until {
                    self.local_hold_until[i] = None;
                } else {
                    // Stale store echo right after release: keep local-first cap position
                    continue;
                }
            }
            self.val[i] = v;
        }
    }

    fn emit_out(&self, cmds: &mut DeskCmds) {
        // DeskViewport coalesces OutPatch to <=1/frame, sends instant head datagrams, and throttles store <=5Hz
        cmds.out_patch(self.val[0], self.val[1], self.val[2], self.val[3]);
    }

    fn format_val(&self, idx: usize) -> String {
        let v = self.val[idx];
        match idx {
            0 => format!("{:+.1}dB", (v - 0.75) * 24.0),
            1 => format!("{}%", (v * 100.0).round() as i32),
            2 => format!("{:+}", ((v - 0.5) * 200.0).round() as i32),
            3 => format!("-{:.1}dB", v * 12.0),
            _ => String::new(),
        }
    }
}

impl DeskModule for OutModule {
    fn id(&self) -> &'static str { "out" }
    fn title(&self) -> &'static str { "OUT" }
    fn rect(&self) -> PanelRect { RECT_OUT }

    fn needs_animation(&self, st: &DeskState) -> bool {
        if st.playing || st.scrubbing {
            return true;
        }
        for i in 0..4 {
            if (self.disp[i] - self.val[i]).abs() > 0.003 || self.vel[i].abs() > 0.01 {
                return true;
            }
        }
        false
    }

    fn step(&mut self, dt: f32, _st: &DeskState, cmds: &mut DeskCmds) {
        let dt = dt.clamp(0.001, 0.05);
        for i in 0..4 {
            if self.grabbed == Some(i) {
                self.disp[i] = self.val[i];
                continue;
            }
            if self.vel[i].abs() > 0.01 {
                self.val[i] = (self.val[i] + self.vel[i] * dt).clamp(0.0, 1.0);
                self.disp[i] = self.val[i];
                self.vel[i] *= (-8.5 * dt).exp();
                if self.vel[i].abs() <= 0.01 {
                    self.vel[i] = 0.0;
                    self.local_hold_until[i] = Some(Instant::now() + Duration::from_millis(450));
                }
                self.emit_out(cmds);
            } else {
                let diff = self.val[i] - self.disp[i];
                self.disp[i] += diff * (1.0 - (-18.0 * dt).exp());
                if diff.abs() < 0.003 {
                    self.disp[i] = self.val[i];
                }
            }
        }
    }

    fn draw(&self, p: &mut PanelDraw, st: &DeskState) {
        let r = RECT_OUT;
        p.module_frame(r, "OUT", &format!("DUCK -{:0.1}dB", self.val[3] * 12.0), vec4(0.98, 0.62, 0.20, 1.0));

        let accents: [Vec4f; 4] = [
            vec4(0.98, 0.62, 0.20, 1.0), // LEVEL
            vec4(0.24, 0.80, 0.98, 1.0), // BAL
            vec4(0.74, 0.54, 0.98, 1.0), // TONE
            vec4(0.24, 0.90, 0.60, 1.0), // DUCK
        ];
        for i in 0..4 {
            let (cx, cy, w, h) = self.fader_geom(i);
            let txt = self.format_val(i);
            p.fader(cx, cy, w, h, self.disp[i], REST[i], accents[i], self.grabbed == Some(i), NAMES[i], &txt);
        }

        // Two live 3D VU meters on right edge
        let lvl_o = st.level_orig.max(if st.playing { 0.06 } else { 0.01 });
        let lvl_n = st.level_new.max(if st.playing { 0.06 } else { 0.01 });
        p.meter(PanelRect::new(r.x + 89.0, r.y + 24.0, 8.5, 37.0), lvl_o, vec4(0.96, 0.68, 0.22, 1.0), true, "ORG", "");
        p.meter(PanelRect::new(r.x + 100.5, r.y + 24.0, 8.5, 37.0), lvl_n, vec4(0.22, 0.80, 0.98, 1.0), true, "NEW", "");
    }

    fn hit_control(&self, px: f32, py: f32, _st: &DeskState) -> Option<ControlHit> {
        for i in 0..4 {
            if self.fader_hit_rect(i).contains(px, py) {
                // 16 u width -> ~25 px at whole desk (< 40 px / 7 mm) -> auto-zooms to OUT!
                return Some(ControlHit::new(i as u32, 16.0));
            }
        }
        None
    }

    fn on_touch(&mut self, ev: ModuleTouch, _st: &DeskState, cmds: &mut DeskCmds) {
        let idx = ev.control_id as usize;
        if idx >= 4 { return; }
        match ev.kind {
            TouchKind::Down => {
                self.grabbed = Some(idx);
                self.vel[idx] = 0.0;
                self.disp[idx] = self.val[idx];
                self.grab_start_y = ev.y;
                self.grab_start_val = self.val[idx];
                self.last_time = ev.time;
                cmds.request_redraw = true;
            }
            TouchKind::Move => {
                let (_, _, _, h) = self.fader_geom(idx);
                let prev = self.val[idx];
                let dy = self.grab_start_y - ev.y; // up = positive
                let next = (self.grab_start_val + dy / h.max(10.0)).clamp(0.0, 1.0);
                // Local-first: cap tracks finger immediately on this touch event with zero store/spring lag
                self.val[idx] = next;
                self.disp[idx] = next;
                let dt = (ev.time - self.last_time).clamp(0.008, 0.08) as f32;
                self.last_time = ev.time;
                let inst_v = ((next - prev) / dt).clamp(-4.5, 4.5);
                self.vel[idx] = self.vel[idx] * 0.45 + inst_v * 0.55;
                self.emit_out(cmds);
            }
            TouchKind::Up => {
                self.grabbed = None;
                self.local_hold_until[idx] = Some(Instant::now() + Duration::from_millis(450));
                if self.vel[idx].abs() < 0.25 {
                    self.vel[idx] = 0.0;
                    self.emit_out(cmds);
                }
                cmds.request_redraw = true;
            }
            TouchKind::DoubleTap => {
                self.grabbed = None;
                self.vel[idx] = 0.0;
                self.val[idx] = REST[idx];
                self.disp[idx] = REST[idx];
                self.local_hold_until[idx] = Some(Instant::now() + Duration::from_millis(450));
                self.emit_out(cmds);
            }
        }
    }
}
