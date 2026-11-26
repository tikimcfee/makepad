//! VOICE module: stepped SINGER & SCRATCH selectors, 2 banked detented 3D rotary knobs for active
//! voice params (with name above & value below), BANK pill (`P1-2/N`), and HEARD / FIT meters.

use super::module::{ControlHit, DeskModule, ModuleTouch, PanelDraw, PanelRect, TouchKind, RECT_VOICE};
use super::wire::{json_escape, DeskCmds, DeskState};
use makepad_widgets::vec4;
use std::time::{Duration, Instant};

const CTRL_SINGER: u32 = 1;
const CTRL_SCRATCH: u32 = 2;
const CTRL_BANK: u32 = 3;
const CTRL_KNOB_A: u32 = 10;
const CTRL_KNOB_B: u32 = 11;

pub struct VoiceModule {
    knob_val: [f32; 2],
    knob_disp: [f32; 2],
    param_bank: usize,
    grabbed_knob: Option<usize>,
    last_angle: f32,
    last_sent: Option<Instant>,
    last_voice_id: String,
}

impl Default for VoiceModule {
    fn default() -> Self {
        Self {
            knob_val: [0.33, 0.0],
            knob_disp: [0.33, 0.0],
            param_bank: 0,
            grabbed_knob: None,
            last_angle: 0.0,
            last_sent: None,
            last_voice_id: String::new(),
        }
    }
}

impl VoiceModule {
    fn singer_rect(&self) -> PanelRect {
        let r = RECT_VOICE;
        PanelRect::new(r.x + 5.0, r.y + 16.5, 48.0, 12.0)
    }

    fn scratch_rect(&self) -> PanelRect {
        let r = RECT_VOICE;
        PanelRect::new(r.x + 57.0, r.y + 16.5, 36.0, 12.0)
    }

    fn bank_rect(&self) -> PanelRect {
        let r = RECT_VOICE;
        PanelRect::new(r.x + 95.5, r.y + 16.5, 14.5, 12.0)
    }

    fn knob_geom(&self, slot: usize) -> (f32, f32, f32) {
        let r = RECT_VOICE;
        let cx = if slot == 0 { r.x + 31.0 } else { r.x + 83.0 };
        let cy = r.y + 45.0;
        (cx, cy, 7.6)
    }

    fn param_bank_count(&self, st: &DeskState) -> usize {
        let n = st.active_voice().map(|v| v.params.len()).unwrap_or(0);
        ((n + 1) / 2).max(1)
    }

    fn singer_choices(&self, st: &DeskState) -> Vec<Option<String>> {
        // First choice is always None ("none" -> scratch, sends {"id":null}).
        // Prefer canonical singer order (yingmusic, soulx, acestep, vevo2) filtered from lgtm.singers,
        // skipping `voice-*` speech wrappers which live in the SCRATCH selector beside it.
        const PREFERRED: [&str; 4] = ["yingmusic", "soulx", "acestep", "vevo2"];
        let mut out: Vec<Option<String>> = vec![None];
        let raw: Vec<&str> = st
            .singers
            .iter()
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty() && !s.starts_with("voice-"))
            .collect();
        if raw.is_empty() {
            for &id in &PREFERRED {
                out.push(Some(id.to_string()));
            }
        } else {
            for &pref in &PREFERRED {
                if raw.contains(&pref) {
                    out.push(Some(pref.to_string()));
                }
            }
            for &s in &raw {
                if !PREFERRED.contains(&s) && !out.iter().any(|x| x.as_deref() == Some(s)) {
                    out.push(Some(s.to_string()));
                }
            }
        }
        if !st.singer_id.is_empty()
            && !out.iter().any(|x| x.as_deref() == Some(st.singer_id.as_str()))
        {
            out.push(Some(st.singer_id.clone()));
        }
        out
    }

    fn bank_label(&self, st: &DeskState) -> String {
        let n = st.active_voice().map(|v| v.params.len()).unwrap_or(0);
        if n <= 2 {
            format!("P1-{}", n.max(1))
        } else {
            let a = self.param_bank * 2 + 1;
            let b = (a + 1).min(n);
            format!("{}-{}", a, b)
        }
    }

    pub fn sync_from_state(&mut self, st: &DeskState) {
        if st.voice_id != self.last_voice_id {
            self.last_voice_id = st.voice_id.clone();
            self.param_bank = 0;
        }
        let banks = self.param_bank_count(st);
        if self.param_bank >= banks {
            self.param_bank = 0;
        }
        let base = self.param_bank * 2;
        if let Some(v) = st.active_voice() {
            for slot in 0..2 {
                if self.grabbed_knob == Some(slot) {
                    continue;
                }
                if let Some(p) = v.params.get(base + slot) {
                    self.knob_val[slot] = if !p.steps.is_empty() {
                        if p.steps.len() > 1 { p.step_idx as f32 / (p.steps.len() - 1) as f32 } else { 0.5 }
                    } else {
                        let span = (p.max - p.min).max(1e-4);
                        ((p.value - p.min) / span).clamp(0.0, 1.0)
                    };
                } else {
                    self.knob_val[slot] = 0.0;
                }
            }
        }
    }

    fn commit_knob(&mut self, slot: usize, st: &DeskState, cmds: &mut DeskCmds, force: bool) {
        let now = Instant::now();
        if !force {
            if let Some(last) = self.last_sent {
                if now.duration_since(last) < Duration::from_millis(55) {
                    return;
                }
            }
        }
        self.last_sent = Some(now);
        let pi = self.param_bank * 2 + slot;
        let Some(v) = st.active_voice() else { return; };
        let Some(p) = v.params.get(pi) else { return; };
        let vesc = json_escape(&v.id);
        let pid = json_escape(&p.id);
        if !p.steps.is_empty() {
            let n = p.steps.len();
            let si = if n > 1 {
                ((self.knob_val[slot] * (n - 1) as f32).round() as usize).min(n - 1)
            } else {
                0
            };
            self.knob_val[slot] = if n > 1 { si as f32 / (n - 1) as f32 } else { 0.5 };
            let raw = if p.step_is_num {
                p.steps[si].clone()
            } else {
                format!("\"{}\"", json_escape(&p.steps[si]))
            };
            cmds.cmd("voice_param", &format!("{{\"voice\":\"{}\",\"id\":\"{}\",\"value\":{}}}", vesc, pid, raw));
        } else {
            let real_v = p.min + self.knob_val[slot].clamp(0.0, 1.0) * (p.max - p.min);
            cmds.cmd("voice_param", &format!("{{\"voice\":\"{}\",\"id\":\"{}\",\"value\":{:.3}}}", vesc, pid, real_v));
        }
    }

    fn knob_readout(&self, slot: usize, st: &DeskState) -> (String, String, usize) {
        let pi = self.param_bank * 2 + slot;
        if let Some(v) = st.active_voice() {
            if let Some(p) = v.params.get(pi) {
                if !p.steps.is_empty() {
                    let n = p.steps.len();
                    let si = if n > 1 {
                        ((self.knob_val[slot] * (n - 1) as f32).round() as usize).min(n - 1)
                    } else {
                        0
                    };
                    let val: String = p.steps.get(si).map(|s| s.as_str()).unwrap_or("-").chars().take(10).collect();
                    return (p.name.clone(), val, n.clamp(2, 9));
                } else {
                    let real_v = p.min + self.knob_val[slot] * (p.max - p.min);
                    return (p.name.clone(), format!("{:.2}", real_v), 7);
                }
            }
        }
        (format!("P{}", pi + 1), "-".into(), 5)
    }
}

impl DeskModule for VoiceModule {
    fn id(&self) -> &'static str { "voice" }
    fn title(&self) -> &'static str { "VOICE" }
    fn rect(&self) -> PanelRect { RECT_VOICE }

    fn needs_animation(&self, _st: &DeskState) -> bool {
        for i in 0..2 {
            if (self.knob_disp[i] - self.knob_val[i]).abs() > 0.003 {
                return true;
            }
        }
        false
    }

    fn step(&mut self, dt: f32, _st: &DeskState, _cmds: &mut DeskCmds) {
        let alpha = 1.0 - (-24.0 * dt.clamp(0.001, 0.05)).exp();
        for i in 0..2 {
            self.knob_disp[i] += (self.knob_val[i] - self.knob_disp[i]) * alpha;
            if (self.knob_disp[i] - self.knob_val[i]).abs() < 0.003 {
                self.knob_disp[i] = self.knob_val[i];
            }
        }
    }

    fn draw(&self, p: &mut PanelDraw, st: &DeskState) {
        let r = RECT_VOICE;
        let hdr_status: String = if !st.singer_why.is_empty()
            && matches!(st.singer_state.as_str(), "error" | "refused" | "failed")
        {
            format!("! {}", st.singer_why)
        } else if !st.singer_id.is_empty() {
            if st.singer_state.is_empty() || st.singer_state == "off" {
                "ready".to_string()
            } else {
                st.singer_state.clone()
            }
        } else if !st.singer_state.is_empty() && st.singer_state != "off" {
            st.singer_state.clone()
        } else {
            "scratch".to_string()
        };
        let hdr_accent = match st.singer_state.as_str() {
            "error" | "refused" | "failed" => vec4(0.96, 0.34, 0.36, 1.0),
            "singing" | "loading" => vec4(0.76, 0.48, 0.98, 1.0),
            _ => vec4(0.24, 0.80, 0.98, 1.0),
        };
        p.module_frame(r, "VOICE", &hdr_status, hdr_accent);

        // Stepped SINGER selector (`lgtm.singers` / `lgtm.singer`: none, yingmusic, soulx, acestep, vevo2)
        let choices = self.singer_choices(st);
        let cur_opt = if st.singer_id.is_empty() { None } else { Some(st.singer_id.as_str()) };
        let s_idx = choices.iter().position(|c| c.as_deref() == cur_opt).unwrap_or(0);
        let s_name = cur_opt.unwrap_or("none");
        let s_active = !st.singer_id.is_empty();
        let s_accent = match st.singer_state.as_str() {
            "error" | "refused" | "failed" => vec4(0.96, 0.34, 0.36, 1.0),
            "singing" | "loading" => vec4(0.80, 0.52, 1.0, 1.0),
            _ if s_active => vec4(0.24, 0.92, 0.66, 1.0),
            _ => vec4(0.24, 0.82, 0.98, 1.0),
        };
        p.selector(
            self.singer_rect(),
            "SINGER",
            s_name,
            s_idx,
            choices.len().max(1),
            s_active,
            s_accent,
        );

        // Stepped SCRATCH selector (`lgtm.voices` / `lgtm.voice`) + BANK pill for its 2 param knobs
        let v_idx = st.voices.iter().position(|v| v.id == st.voice_id).unwrap_or(0);
        let v_name = st.active_voice().map(|v| v.id.as_str()).unwrap_or(&st.voice_id);
        p.selector(
            self.scratch_rect(),
            "SCRATCH",
            v_name,
            v_idx,
            st.voices.len().max(1),
            false,
            vec4(0.96, 0.68, 0.24, 1.0),
        );

        let bank_txt = self.bank_label(st);
        p.pill(self.bank_rect(), vec4(0.16, 0.22, 0.32, 1.0), vec4(0.86, 0.94, 1.0, 1.0), &bank_txt);

        // Two banked detented rotary knobs
        for slot in 0..2 {
            let (cx, cy, kr) = self.knob_geom(slot);
            let (lbl, val, detents) = self.knob_readout(slot, st);
            p.knob(cx, cy, kr, self.knob_disp[slot], detents, self.grabbed_knob == Some(slot), vec4(0.26, 0.84, 0.98, 1.0), &lbl, &val);
        }

        // Words-heard & fit meters along bottom
        let fit = st.active_voice().map(|v| &v.fit);
        let (heard_lvl, heard_txt) = match fit {
            Some(f) if f.has_fit && f.words_heard >= 0.0 => (f.words_heard.clamp(0.0, 1.0), format!("{}%", (f.words_heard * 100.0).round() as i32)),
            _ => (0.0, "--".into()),
        };
        let (fit_lvl, fit_txt) = match fit {
            Some(f) if f.has_fit && f.onset_ms >= 0.0 => ((1.0 - f.onset_ms / 150.0).clamp(0.0, 1.0), format!("{}ms", f.onset_ms.round() as i32)),
            _ => (0.0, "--".into()),
        };
        p.meter(PanelRect::new(r.x + 6.0, r.bottom() - 6.0, 48.0, 3.8), heard_lvl, vec4(0.22, 0.92, 0.56, 1.0), false, "HEARD", &heard_txt);
        p.meter(PanelRect::new(r.x + 60.0, r.bottom() - 6.0, 48.0, 3.8), fit_lvl, vec4(0.24, 0.80, 0.98, 1.0), false, "FIT", &fit_txt);
    }

    fn hit_control(&self, px: f32, py: f32, _st: &DeskState) -> Option<ControlHit> {
        // Controls are ~15.5 u (< 40 px at whole desk -> auto-zoom into VOICE; >= 45 px once zoomed in!)
        if self.singer_rect().contains(px, py) {
            return Some(ControlHit::new(CTRL_SINGER, 15.5));
        }
        if self.scratch_rect().contains(px, py) {
            return Some(ControlHit::new(CTRL_SCRATCH, 15.5));
        }
        if self.bank_rect().contains(px, py) {
            return Some(ControlHit::new(CTRL_BANK, 15.5));
        }
        for slot in 0..2 {
            let (cx, cy, kr) = self.knob_geom(slot);
            if ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() <= kr * 1.65 {
                return Some(ControlHit::new(if slot == 0 { CTRL_KNOB_A } else { CTRL_KNOB_B }, kr * 2.1));
            }
        }
        None
    }

    fn on_touch(&mut self, ev: ModuleTouch, st: &DeskState, cmds: &mut DeskCmds) {
        match ev.control_id {
            CTRL_SINGER => {
                if ev.kind == TouchKind::Up {
                    let choices = self.singer_choices(st);
                    let cur_opt = if st.singer_id.is_empty() { None } else { Some(st.singer_id.as_str()) };
                    let cur = choices.iter().position(|c| c.as_deref() == cur_opt).unwrap_or(0);
                    let next = &choices[(cur + 1) % choices.len().max(1)];
                    match next {
                        None => cmds.cmd_pending("singer", "{\"id\":null}", "singer"),
                        Some(id) => cmds.cmd_pending("singer", &format!("{{\"id\":\"{}\"}}", json_escape(id)), "singer"),
                    }
                }
            }
            CTRL_SCRATCH => {
                if ev.kind == TouchKind::Up && !st.voices.is_empty() {
                    let cur = st.voices.iter().position(|v| v.id == st.voice_id).unwrap_or(0);
                    let next = (cur + 1) % st.voices.len();
                    let vid = &st.voices[next].id;
                    self.param_bank = 0;
                    cmds.cmd_pending("voice", &format!("{{\"id\":\"{}\"}}", json_escape(vid)), "voice");
                }
            }
            CTRL_BANK => {
                if ev.kind == TouchKind::Up {
                    let banks = self.param_bank_count(st);
                    self.param_bank = (self.param_bank + 1) % banks;
                    self.sync_from_state(st);
                    cmds.request_redraw = true;
                }
            }
            CTRL_KNOB_A | CTRL_KNOB_B => {
                let slot = if ev.control_id == CTRL_KNOB_A { 0 } else { 1 };
                let (cx, cy, _kr) = self.knob_geom(slot);
                match ev.kind {
                    TouchKind::Down => {
                        self.grabbed_knob = Some(slot);
                        self.last_angle = (-(ev.y - cy)).atan2(ev.x - cx);
                        cmds.request_redraw = true;
                    }
                    TouchKind::Move => {
                        let dx = ev.x - cx;
                        let dy_up = -(ev.y - cy);
                        let dist = (dx * dx + dy_up * dy_up).sqrt();
                        let ang = dy_up.atan2(dx);
                        let delta = if dist >= 4.2 {
                            let mut d_ang = ang - self.last_angle;
                            while d_ang > std::f32::consts::PI { d_ang -= std::f32::consts::TAU; }
                            while d_ang < -std::f32::consts::PI { d_ang += std::f32::consts::TAU; }
                            -d_ang / 270.0_f32.to_radians()
                        } else {
                            -ev.dy / 22.0
                        };
                        self.last_angle = ang;
                        self.knob_val[slot] = (self.knob_val[slot] + delta).clamp(0.0, 1.0);
                        self.commit_knob(slot, st, cmds, false);
                    }
                    TouchKind::Up => {
                        self.grabbed_knob = None;
                        self.commit_knob(slot, st, cmds, true);
                    }
                    TouchKind::DoubleTap => {
                        self.grabbed_knob = None;
                        let pi = self.param_bank * 2 + slot;
                        if let Some(v) = st.active_voice() {
                            if let Some(p) = v.params.get(pi) {
                                self.knob_val[slot] = if !p.steps.is_empty() {
                                    if p.steps.len() > 1 { p.rest_step_idx as f32 / (p.steps.len() - 1) as f32 } else { 0.0 }
                                } else {
                                    let span = (p.max - p.min).max(1e-4);
                                    ((p.rest - p.min) / span).clamp(0.0, 1.0)
                                };
                            }
                        }
                        self.commit_knob(slot, st, cmds, true);
                    }
                }
            }
            _ => {}
        }
    }
}
