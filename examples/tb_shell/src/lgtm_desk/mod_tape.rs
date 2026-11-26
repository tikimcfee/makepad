//! TAPE module: full song strip (backing + vocal waveforms, section state tints, line ticks,
//! loop highlight, glowing playhead) + one flat machined 3D jog wheel.
//!
//! Dragging along the strip OR turning/flicking the jog wheel scrubs smoothly through the same
//! high-rate local `lgtm-head.sock` protocol (`grab`/`vel`/`release`/`{scrub_end:{}}`, falling
//! back to `<= 20 Hz` `lgtm_cmd` `scrub`/`scrub_end`), and turns the wheel strictly from heard
//! position moved along at the current signed rate.

use super::module::{fmt_time_ms, ControlHit, DeskModule, ModuleTouch, PanelDraw, PanelRect, TextAlign, TouchKind, RECT_TAPE};
use super::wire::{DeskCmds, DeskState};
use makepad_widgets::vec4;
use std::time::{Duration, Instant};

const CTRL_STRIP: u32 = 1;
const CTRL_JOG: u32 = 2;

pub struct TapeModule {
    pub pos_ms: f64,
    pub jog_angle: f32,
    pub scrub_active: bool,
    pub scrub_coasting: bool,
    pub scrub_ctrl: u32,
    pub scrub_omega: f32,
    pub scrub_rate: f32,

    // Dedicated sliding-window touch velocity history (never shared with anim timer)
    last_angle: f32,
    unwrapped: f32,
    hist: [(f64, f32); 8],
    hist_len: usize,
    hist_head: usize,
    last_move_at: Option<Instant>,
    grab_start_x: f32,
    dragged: bool,
}

impl Default for TapeModule {
    fn default() -> Self {
        Self {
            pos_ms: 0.0,
            jog_angle: 0.0,
            scrub_active: false,
            scrub_coasting: false,
            scrub_ctrl: 0,
            scrub_omega: 0.0,
            scrub_rate: 0.0,
            last_angle: 0.0,
            unwrapped: 0.0,
            hist: [(0.0, 0.0); 8],
            hist_len: 0,
            hist_head: 0,
            last_move_at: None,
            grab_start_x: 0.0,
            dragged: false,
        }
    }
}

impl TapeModule {
    fn strip_rect(&self) -> PanelRect {
        let r = RECT_TAPE;
        PanelRect::new(r.x + 5.0, r.y + 14.0, 168.0, 38.0)
    }

    fn jog_center_radius(&self) -> (f32, f32, f32) {
        let r = RECT_TAPE;
        (r.x + 204.0, r.y + 38.0, 16.5)
    }

    fn reset_hist(&mut self, t: f64, v0: f32) {
        self.unwrapped = v0;
        self.hist[0] = (t, v0);
        self.hist_len = 1;
        self.hist_head = 0;
    }

    fn push_hist(&mut self, t: f64, v: f32) -> f32 {
        self.hist_head = (self.hist_head + 1) % 8;
        self.hist[self.hist_head] = (t, v);
        if self.hist_len < 8 {
            self.hist_len += 1;
        }
        let mut best_old: Option<(f64, f32)> = None;
        for back in 1..self.hist_len {
            let idx = (self.hist_head + 8 - back) % 8;
            let (t_old, v_old) = self.hist[idx];
            let dt = t - t_old;
            if dt >= 0.008 {
                best_old = Some((t_old, v_old));
                if dt >= 0.045 {
                    break;
                }
            }
        }
        if let Some((t_old, v_old)) = best_old {
            let dt = (t - t_old).max(0.008) as f32;
            (v - v_old) / dt
        } else {
            0.0
        }
    }

    fn advance_visual_by_ms(&mut self, d_ms: f64) {
        let clamped = d_ms.clamp(-400.0, 400.0);
        // Clockwise rotation in world Y-up = negative angle delta
        let d_rad = (clamped / 1000.0) as f32 * 2.15;
        self.jog_angle -= d_rad;
    }

    pub fn sync_static_anchor(&mut self, st: &DeskState) {
        let now = Instant::now();
        let dur = st.dur_ms.max(1.0);
        if !self.scrub_active && !st.scrubbing && !st.playing && !st.head_is_fresh(now) {
            let live_ms = st.pos_anchor_ms.clamp(0.0, dur);
            let d = live_ms - self.pos_ms;
            if d.abs() > 0.1 {
                self.pos_ms = live_ms;
                self.advance_visual_by_ms(d);
            }
        } else {
            let heard = st.heard_pos_ms(now);
            let err = heard - self.pos_ms;
            if err.abs() > 350.0 {
                self.pos_ms = heard;
                self.advance_visual_by_ms(err.clamp(-180.0, 180.0));
            }
        }
    }

    pub fn end_scrub_if_active(&mut self, st: &DeskState, cmds: &mut DeskCmds) {
        if self.scrub_active || self.scrub_coasting || st.scrubbing {
            let now = Instant::now();
            let end_ms = if st.head_is_fresh(now) { st.heard_pos_ms(now) } else { self.pos_ms };
            self.scrub_active = false;
            self.scrub_coasting = false;
            self.scrub_ctrl = 0;
            self.scrub_omega = 0.0;
            self.scrub_rate = 0.0;
            self.last_move_at = None;
            cmds.head_end(end_ms);
        }
    }
}

impl DeskModule for TapeModule {
    fn id(&self) -> &'static str { "tape" }
    fn title(&self) -> &'static str { "TAPE" }
    fn rect(&self) -> PanelRect { RECT_TAPE }

    fn needs_animation(&self, st: &DeskState) -> bool {
        let now = Instant::now();
        self.scrub_active || self.scrub_coasting || st.playing || st.scrubbing || st.head_is_fresh(now)
    }

    fn step(&mut self, dt: f32, st: &DeskState, cmds: &mut DeskCmds) {
        let dt = dt.clamp(0.001, 0.05);
        let dur = st.dur_ms.max(1.0);

        // 1. Hold-still brake & flick-coast flywheel decay
        if self.scrub_active {
            if !self.scrub_coasting {
                let idle_ms = self.last_move_at.map(|t| t.elapsed().as_millis()).unwrap_or(999);
                if idle_ms >= 55 {
                    let decay = (-18.0 * dt).exp();
                    self.scrub_omega *= decay;
                    self.scrub_rate *= decay;
                    if self.scrub_rate.abs() < 0.05 {
                        self.scrub_omega = 0.0;
                        self.scrub_rate = 0.0;
                    }
                }
                cmds.head_vel("vel", self.scrub_omega, self.scrub_rate, true, false);
            } else {
                let decay = (-2.8 * dt).exp();
                self.scrub_omega *= decay;
                self.scrub_rate *= decay;
                let hit_end = (self.scrub_rate < 0.0 && self.pos_ms <= 1.0)
                    || (self.scrub_rate > 0.0 && self.pos_ms >= dur - 1.0);
                if hit_end || self.scrub_rate.abs() < 0.05 {
                    self.end_scrub_if_active(st, cmds);
                } else {
                    cmds.head_vel("vel", self.scrub_omega, self.scrub_rate, false, true);
                }
            }
        }

        // 2. Smooth heard-position dead-reckoning & wheel turning (zero jump)
        let now = Instant::now();
        let head_fresh = st.head_is_fresh(now);
        if head_fresh || st.scrubbing || st.playing {
            let heard = st.heard_pos_ms(now);
            let err = heard - self.pos_ms;
            if err.abs() > 350.0 {
                self.pos_ms = heard;
                self.advance_visual_by_ms(err.clamp(-180.0, 180.0));
            } else {
                let rate = st.effective_rate(now);
                let step_ms = rate * (dt as f64) * 1000.0;
                let gain = if head_fresh { 28.0 } else { 15.0 };
                let corr = (heard - (self.pos_ms + step_ms)) * (1.0 - (-gain * dt as f64).exp());
                let next = (self.pos_ms + step_ms + corr).clamp(0.0, dur);
                let d_ms = next - self.pos_ms;
                self.pos_ms = next;
                self.advance_visual_by_ms(d_ms);
            }
        } else if self.scrub_active {
            let step_ms = (self.scrub_rate as f64) * (dt as f64) * 1000.0;
            let next = (self.pos_ms + step_ms).clamp(0.0, dur);
            let d_ms = next - self.pos_ms;
            self.pos_ms = next;
            self.advance_visual_by_ms(d_ms);
        }
    }

    fn draw(&self, p: &mut PanelDraw, st: &DeskState) {
        let r = RECT_TAPE;
        let gen_tag = if st.song_gen > 0 { format!("g{}", st.song_gen) } else { "orig".into() };
        let short_title: String = if st.song_title.chars().count() > 16 {
            let mut s: String = st.song_title.chars().take(15).collect();
            s.push('…');
            s
        } else {
            st.song_title.clone()
        };
        let status = format!("{} [{}] {}/{}", short_title, gen_tag, fmt_time_ms(self.pos_ms), fmt_time_ms(st.dur_ms));
        p.module_frame(r, "TAPE", &status, vec4(0.96, 0.56, 0.20, 1.0));

        // Loop fractional range
        let dur = st.dur_ms.max(1.0);
        let loop_range = if st.loop_active {
            if st.loop_a_ms >= 0.0 && st.loop_b_ms > st.loop_a_ms {
                Some(((st.loop_a_ms / dur) as f32, (st.loop_b_ms / dur) as f32))
            } else if st.loop_line >= 0 {
                let idx = st.loop_line as usize;
                let t0 = st.line_times_ms.get(idx).copied().unwrap_or(0.0).max(0.0);
                let t1 = st.line_times_ms.get(idx + 1).copied().unwrap_or((t0 + 4000.0).min(dur));
                Some(((t0 / dur) as f32, (t1 / dur) as f32))
            } else {
                None
            }
        } else {
            None
        };

        let srect = self.strip_rect();
        let head_frac = (self.pos_ms / dur).clamp(0.0, 1.0) as f32;
        p.strip(srect, &st.wave_back, &st.wave_src, &st.sections, dur, &st.line_times_ms, head_frac, loop_range);

        // Current lyric preview directly beneath strip
        let preview = st
            .line_index_at_ms(self.pos_ms)
            .map(|i| st.line_text_at(i))
            .filter(|s| !s.is_empty())
            .unwrap_or("Drag tape strip or turn jog wheel to scrub");
        p.text_clip(srect.x + 2.0, srect.bottom() + 6.0, srect.w - 4.0, 3.8, vec4(0.84, 0.90, 0.98, 0.95), TextAlign::Left, preview);

        // Flat machined 3D jog wheel
        let (jx, jy, jr) = self.jog_center_radius();
        let now = Instant::now();
        let jog_val = if self.scrub_active || st.scrubbing {
            let rate = if st.head_is_fresh(now) { st.head_rate as f32 } else { self.scrub_rate };
            if rate.abs() < 0.05 { "HOLD".to_string() } else { format!("{:+.1}x", rate) }
        } else if st.playing {
            format!("{:.2}x", st.pos_rate)
        } else {
            fmt_time_ms(self.pos_ms)
        };
        let jog_lbl = if self.scrub_active || st.scrubbing { "SCRUB" } else { "JOG" };
        p.jog_wheel(jx, jy, jr, self.jog_angle, self.scrub_active, jog_lbl, &jog_val);
    }

    fn hit_control(&self, px: f32, py: f32, _st: &DeskState) -> Option<ControlHit> {
        let srect = self.strip_rect();
        if srect.contains(px, py) {
            return Some(ControlHit::new(CTRL_STRIP, srect.h)); // 38 u -> playable right from whole desk
        }
        let (jx, jy, jr) = self.jog_center_radius();
        let d = ((px - jx).powi(2) + (py - jy).powi(2)).sqrt();
        if d <= jr * 1.25 {
            return Some(ControlHit::new(CTRL_JOG, jr * 2.0)); // 33 u -> ~53 px whole desk, playable immediately!
        }
        None
    }

    fn on_touch(&mut self, ev: ModuleTouch, st: &DeskState, cmds: &mut DeskCmds) {
        let (jx, jy, _jr) = self.jog_center_radius();
        match ev.kind {
            TouchKind::Down => {
                self.scrub_active = true;
                self.scrub_coasting = false;
                self.scrub_ctrl = ev.control_id;
                self.scrub_omega = 0.0;
                self.scrub_rate = 0.0;
                self.last_move_at = Some(Instant::now());
                self.grab_start_x = ev.x;
                self.dragged = false;
                if ev.control_id == CTRL_JOG {
                    // Convert panel Y-down to world Y-up angle so clockwise = negative d_ang
                    self.last_angle = (-(ev.y - jy)).atan2(ev.x - jx);
                    self.reset_hist(ev.time, 0.0);
                } else {
                    self.reset_hist(ev.time, ev.x);
                }
                cmds.head_vel("grab", 0.0, 0.0, true, false);
            }
            TouchKind::Move => {
                if !self.scrub_active {
                    return;
                }
                if ev.control_id == CTRL_JOG {
                    let dx = ev.x - jx;
                    let dy_up = -(ev.y - jy);
                    let dist = (dx * dx + dy_up * dy_up).sqrt();
                    let ang = dy_up.atan2(dx);
                    let mut d_ang = ang - self.last_angle;
                    while d_ang > std::f32::consts::PI { d_ang -= std::f32::consts::TAU; }
                    while d_ang < -std::f32::consts::PI { d_ang += std::f32::consts::TAU; }
                    self.last_angle = ang;

                    // Near-spindle damping eliminates atan2 spikes near center
                    let hub_gain = ((dist - 2.6) / 3.0).clamp(0.0, 1.0);
                    self.unwrapped += d_ang * hub_gain;
                    let omega_ccw = self.push_hist(ev.time, self.unwrapped);
                    // Clockwise = negative omega_ccw = positive forward rate
                    let omega_cw = (-omega_ccw).clamp(-24.0, 24.0);
                    self.scrub_omega = omega_cw;
                    let inst_rate = (omega_cw / 2.15).clamp(-4.0, 4.0);
                    self.scrub_rate = (self.scrub_rate * 0.25 + inst_rate * 0.75).clamp(-4.0, 4.0);
                    if self.scrub_rate.abs() < 0.05 { self.scrub_rate = 0.0; }
                    self.dragged = true;
                } else {
                    // Strip horizontal scrub: drag right = forward (+rate), drag left = reverse (-rate)
                    if (ev.x - self.grab_start_x).abs() > 1.5 {
                        self.dragged = true;
                    }
                    let vx_u_per_s = self.push_hist(ev.time, ev.x);
                    // 22 panel units/s along the strip = 1.0x normal tape speed
                    let inst_rate = (vx_u_per_s / 22.0).clamp(-4.0, 4.0);
                    self.scrub_rate = (self.scrub_rate * 0.25 + inst_rate * 0.75).clamp(-4.0, 4.0);
                    if self.scrub_rate.abs() < 0.05 { self.scrub_rate = 0.0; }
                    self.scrub_omega = self.scrub_rate * 2.15;
                }
                self.last_move_at = Some(Instant::now());
                cmds.head_vel("vel", self.scrub_omega, self.scrub_rate, true, false);
            }
            TouchKind::Up => {
                // Single tap on strip (without dragging) jumps directly to that song position
                if ev.control_id == CTRL_STRIP && !self.dragged {
                    let srect = self.strip_rect();
                    let frac = ((ev.x - srect.x) / srect.w.max(1.0)).clamp(0.0, 1.0) as f64;
                    let target_ms = (frac * st.dur_ms.max(1.0)).round() as i64;
                    self.end_scrub_if_active(st, cmds);
                    self.pos_ms = target_ms as f64;
                    cmds.cmd_pending("seek", &format!("{{\"ms\":{}}}", target_ms), "seek");
                    return;
                }
                let stale = self.last_move_at.map(|t| t.elapsed() > Duration::from_millis(120)).unwrap_or(true);
                if stale {
                    self.scrub_omega = 0.0;
                    self.scrub_rate = 0.0;
                }
                if self.scrub_rate.abs() < 0.35 {
                    self.end_scrub_if_active(st, cmds);
                } else {
                    self.scrub_coasting = true;
                    cmds.head_vel("release", self.scrub_omega, self.scrub_rate, false, true);
                }
            }
            TouchKind::DoubleTap => {}
        }
    }
}
