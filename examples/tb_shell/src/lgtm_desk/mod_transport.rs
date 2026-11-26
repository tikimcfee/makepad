//! TRANSPORT module: 7 mechanical 3D keys (REW, PLAY, STOP, LOOP, REC, DUB, EJECT) lit from
//! authoritative `lgtm.transport` + `lgtm.loop` with immediate pending feedback until `lgtm.ack`.

use super::module::{fmt_time_ms, ControlHit, DeskModule, ModuleTouch, PanelDraw, PanelRect, TouchKind, RECT_TRANSPORT};
use super::wire::{DeskCmds, DeskState};
use makepad_widgets::{vec4, Vec4f};
use std::time::{Duration, Instant};

const KEYS: [&str; 7] = ["REW", "PLAY", "STOP", "LOOP", "REC", "DUB", "EJECT"];

pub struct TransportModule {
    key_travel: [f32; 7],
    key_target: [f32; 7],
    key_flash: [f32; 7],
    pending_key: i32,
    pending_name: String,
    pending_since: Option<Instant>,
    pending_pulse: f32,
    refused_why: String,
    refused_until: Option<Instant>,
    last_ack_seq: u64,
    last_tr_seq: u64,
    latch_hold_until: Option<Instant>,
    pressed_key: Option<usize>,
}

impl Default for TransportModule {
    fn default() -> Self {
        Self {
            key_travel: [0.0; 7],
            key_target: [0.0; 7],
            key_flash: [0.0; 7],
            pending_key: -1,
            pending_name: String::new(),
            pending_since: None,
            pending_pulse: 0.0,
            refused_why: String::new(),
            refused_until: None,
            last_ack_seq: 0,
            last_tr_seq: 0,
            latch_hold_until: None,
            pressed_key: None,
        }
    }
}

impl TransportModule {
    fn key_rect(&self, idx: usize) -> PanelRect {
        let r = RECT_TRANSPORT;
        let gap = 2.8;
        let pad_x = 5.0;
        let kw = (r.w - pad_x * 2.0 - gap * 6.0) / 7.0;
        PanelRect::new(r.x + pad_x + idx as f32 * (kw + gap), r.y + 13.5, kw, 24.5)
    }

    pub fn sync_from_state(&mut self, st: &DeskState) {
        let now = Instant::now();
        let mut seq_advanced = false;
        if st.ack_seq > self.last_ack_seq {
            self.last_ack_seq = st.ack_seq;
            seq_advanced = true;
            if st.ack_state == "refused" {
                self.refused_why = if st.ack_why.is_empty() {
                    format!("{} refused", st.ack_name)
                } else {
                    st.ack_why.clone()
                };
                self.refused_until = Some(now + Duration::from_secs(4));
            }
            if st.ack_state != "queued" {
                self.pending_key = -1;
                self.pending_since = None;
            }
        }
        if st.transport_seq > self.last_tr_seq {
            self.last_tr_seq = st.transport_seq;
            seq_advanced = true;
        }
        if seq_advanced {
            self.latch_hold_until = None;
        }
        let hold_active = self.latch_hold_until.map(|u| now < u).unwrap_or(false);
        // Latch PLAY and LOOP keys from authoritative state (unless held briefly after local tap)
        if self.pressed_key != Some(1) && !hold_active {
            self.key_target[1] = match st.transport_state.as_str() {
                "playing" | "seeking" => 0.85,
                "starting" => 0.45,
                _ => if !st.has_transport && st.playing { 0.85 } else { 0.0 },
            };
        }
        if self.pressed_key != Some(3) && !hold_active {
            self.key_target[3] = if st.loop_active { 0.78 } else { 0.0 };
        }
    }

    fn set_pending(&mut self, idx: usize, name: &str) {
        self.pending_key = idx as i32;
        self.pending_name = name.to_string();
        self.pending_since = Some(Instant::now());
        self.pending_pulse = 0.0;
        self.refused_why.clear();
        self.refused_until = None;
    }

    fn trigger(&mut self, idx: usize, st: &DeskState, cmds: &mut DeskCmds) {
        self.key_flash[idx] = 1.0;
        match idx {
            0 => {
                // REW -5s
                let pos = st.heard_pos_ms(Instant::now());
                let target = (pos - 5000.0).max(0.0).round() as i64;
                cmds.head_end(pos);
                cmds.cmd_pending("seek", &format!("{{\"ms\":{}}}", target), "seek");
                self.set_pending(0, "seek");
            }
            1 => {
                // Local-first latch feedback immediately on tap
                self.key_target[1] = 0.85;
                self.key_travel[1] = 0.85;
                self.latch_hold_until = Some(Instant::now() + Duration::from_millis(350));
                cmds.cmd_pending("play", "{}", "play");
                self.set_pending(1, "play");
            }
            2 => {
                self.key_target[1] = 0.0;
                self.key_travel[1] = 0.0;
                self.latch_hold_until = Some(Instant::now() + Duration::from_millis(350));
                let pos = st.heard_pos_ms(Instant::now());
                cmds.head_end(pos);
                cmds.cmd_pending("stop", "{}", "stop");
                self.set_pending(2, "stop");
            }
            3 => {
                self.latch_hold_until = Some(Instant::now() + Duration::from_millis(350));
                if st.loop_active {
                    self.key_target[3] = 0.0;
                    self.key_travel[3] = 0.0;
                    cmds.cmd_pending("loop", "{}", "loop");
                } else {
                    self.key_target[3] = 0.78;
                    self.key_travel[3] = 0.78;
                    let pos = st.heard_pos_ms(Instant::now());
                    let ln = st.line_index_at_ms(pos).unwrap_or(0);
                    cmds.cmd_pending("loop", &format!("{{\"line\":{}}}", ln), "loop");
                }
                self.set_pending(3, "loop");
            }
            4 => {
                cmds.cmd_pending("save", "{}", "save");
                self.set_pending(4, "save");
            }
            5 => {
                cmds.cmd_pending("again", "{}", "again");
                self.set_pending(5, "again");
            }
            6 => {
                // EJECT zooms straight to the RACK module on the desk!
                cmds.focus_module("rack");
            }
            _ => {}
        }
    }
}

impl DeskModule for TransportModule {
    fn id(&self) -> &'static str { "transport" }
    fn title(&self) -> &'static str { "TRANSPORT" }
    fn rect(&self) -> PanelRect { RECT_TRANSPORT }

    fn needs_animation(&self, st: &DeskState) -> bool {
        if self.pending_since.is_some()
            || st.transport_state == "starting"
            || st.transport_state == "stopping"
        {
            return true;
        }
        for i in 0..7 {
            if (self.key_travel[i] - self.key_target[i]).abs() > 0.005 || self.key_flash[i] > 0.01 {
                return true;
            }
        }
        false
    }

    fn step(&mut self, dt: f32, _st: &DeskState, _cmds: &mut DeskCmds) {
        let dt = dt.clamp(0.001, 0.05);
        let alpha = 1.0 - (-20.0 * dt).exp();
        for i in 0..7 {
            self.key_travel[i] += (self.key_target[i] - self.key_travel[i]) * alpha;
            if (self.key_travel[i] - self.key_target[i]).abs() < 0.004 {
                self.key_travel[i] = self.key_target[i];
            }
            self.key_flash[i] = (self.key_flash[i] - dt * 1.8).max(0.0);
        }
        if let Some(since) = self.pending_since {
            self.pending_pulse = (self.pending_pulse + dt * 7.5) % std::f32::consts::TAU;
            if since.elapsed() > Duration::from_secs(3) {
                self.pending_key = -1;
                self.pending_since = None;
            }
        }
        if let Some(until) = self.refused_until {
            if Instant::now() >= until {
                self.refused_why.clear();
                self.refused_until = None;
            }
        }
    }

    fn draw(&self, p: &mut PanelDraw, st: &DeskState) {
        let now = Instant::now();
        let status = if !self.refused_why.is_empty() {
            format!("refused: {}", self.refused_why)
        } else if self.pending_since.is_some() {
            format!("pending {}…", self.pending_name)
        } else if st.scrubbing || (st.head_is_fresh(now) && st.head_rate.abs() > 0.05 && !st.playing) {
            format!("scrub {:+.1}x · {}", st.effective_rate(now), fmt_time_ms(st.heard_pos_ms(now)))
        } else {
            let st_name = if st.has_transport { st.transport_state.as_str() } else { st.status.as_str() };
            format!("{} · {}", st_name, fmt_time_ms(st.heard_pos_ms(now)))
        };
        p.module_frame(RECT_TRANSPORT, "TRANSPORT", &status, vec4(0.22, 0.88, 0.56, 1.0));

        let body_cols: [Vec4f; 7] = [
            vec4(0.19, 0.24, 0.33, 1.0), // REW
            vec4(0.13, 0.38, 0.30, 1.0), // PLAY
            vec4(0.22, 0.26, 0.34, 1.0), // STOP
            vec4(0.14, 0.30, 0.38, 1.0), // LOOP
            vec4(0.46, 0.16, 0.20, 1.0), // REC
            vec4(0.32, 0.21, 0.46, 1.0), // DUB
            vec4(0.26, 0.30, 0.38, 1.0), // EJECT
        ];
        let led_cols: [Vec4f; 7] = [
            vec4(0.40, 0.78, 0.98, 1.0),
            vec4(0.22, 0.96, 0.58, 1.0),
            vec4(0.96, 0.76, 0.30, 1.0),
            vec4(0.22, 0.94, 1.00, 1.0),
            vec4(1.00, 0.26, 0.32, 1.0),
            vec4(0.78, 0.50, 1.00, 1.0),
            vec4(0.98, 0.65, 0.22, 1.0),
        ];
        let pulse = 0.55 + 0.45 * self.pending_pulse.sin();

        for i in 0..7 {
            let kr = self.key_rect(i);
            let is_pending = self.pending_since.is_some() && self.pending_key == i as i32;
            let is_starting = i == 1 && st.transport_state == "starting";
            let is_stopping = i == 2 && st.transport_state == "stopping";
            let (led_col, led_on) = if is_pending || is_starting || is_stopping {
                (vec4(1.0, 0.88, 0.35, 1.0), pulse)
            } else {
                let lit = self.key_flash[i].max(if i == 1 && st.playing {
                    1.0
                } else if i == 3 && st.loop_active {
                    0.95
                } else {
                    0.08
                });
                (led_cols[i], lit)
            };
            p.key(kr, self.key_travel[i], led_col, led_on, body_cols[i], KEYS[i]);
        }
    }

    fn hit_control(&self, px: f32, py: f32, _st: &DeskState) -> Option<ControlHit> {
        for i in 0..7 {
            let kr = self.key_rect(i);
            if kr.contains(px, py) {
                // Report 26 u so TRANSPORT keys remain playable right from whole-desk zoom
                return Some(ControlHit::new(i as u32, 26.0));
            }
        }
        None
    }

    fn on_touch(&mut self, ev: ModuleTouch, st: &DeskState, cmds: &mut DeskCmds) {
        let idx = ev.control_id as usize;
        if idx >= 7 { return; }
        match ev.kind {
            TouchKind::Down => {
                self.pressed_key = Some(idx);
                // Local-first: depress key cap immediately on touch frame
                self.key_travel[idx] = 1.0;
                self.key_target[idx] = 1.0;
                cmds.request_redraw = true;
            }
            TouchKind::Up => {
                self.pressed_key = None;
                if idx != 1 && idx != 3 {
                    self.key_target[idx] = 0.0;
                }
                if self.key_rect(idx).contains(ev.x, ev.y) {
                    self.trigger(idx, st, cmds);
                } else {
                    self.sync_from_state(st);
                    cmds.request_redraw = true;
                }
            }
            _ => {}
        }
    }
}
