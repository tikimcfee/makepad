//! Orthographic One Desk viewport (`DeskViewport`):
//! - Flat orthographic camera straight at the panel, 1 fixed light, 1 child 3D pass (4 GPU draws)
//! - Single-finger pan on bare panel, two-finger pan & pinch zoom, double-tap fit module / whole desk
//! - `< 7 mm` (~40 logical px) auto-zoom guard: tapping a tiny control zooms to its module instead
//! - Sharp real text rendered in exact panel transform; small labels fade out when unreadable while
//!   module titles always stay crisp
//! - Zero frames when nothing moves + built-in frame-cost measurement (whole-desk vs 1-module zoom)

use super::mesh::{build_box_mesh, build_cyl_mesh, build_jog_mesh, build_knob_mesh, mat4_ortho_world};
use super::mod_bands::BandsModule;
use super::mod_link::LinkModule;
use super::mod_lyrics::LyricsModule;
use super::mod_out::OutModule;
use super::mod_rack::RackModule;
use super::mod_tape::TapeModule;
use super::mod_transport::TransportModule;
use super::mod_voice::VoiceModule;
use super::module::{DeskModule, ModuleTouch, PanelDraw, PanelRect, TextAlign, TouchKind, PANEL_H, PANEL_W};
use super::wire::{
    json_escape, send_close_prompt_edit, send_head_json, send_head_out, send_head_vel,
    send_lgtm_cmd, send_lgtm_mix_batch, send_lgtm_out, send_open_prompt_edit, wire_hub,
    DeskCmd, DeskCmds, DeskState, DIRTY_BANDS, DIRTY_MIX_VALS, DIRTY_OUT, DIRTY_TAPE,
    DIRTY_TRANSPORT, DIRTY_VOICE,
};
use makepad_widgets::event::{TouchState, TouchUpdateEvent};
use makepad_widgets::*;
use std::time::{Duration, Instant};

const PASS_SCALE: f64 = 0.80;
/// ~7 mm on a 384x854 (~68 mm wide) phone glass = ~40 logical px.
const MIN_GRAB_PX: f32 = 40.0;

fn snap_font_size(fs: f32) -> f32 {
    const STEPS: [f32; 8] = [7.5, 9.0, 10.5, 12.0, 14.0, 16.0, 18.0, 20.0];
    let mut best = STEPS[0];
    let mut best_d = (fs - best).abs();
    for &s in &STEPS[1..] {
        let d = (fs - s).abs();
        if d < best_d {
            best = s;
            best_d = d;
        }
    }
    best
}

#[derive(Script, ScriptHook)]
#[repr(C)]
pub struct DrawDeskMesh {
    #[deref]
    draw_vars: DrawVars,
    #[live]
    model: Mat4f,
    #[live]
    color: Vec4f,
    #[live]
    mat: Vec4f,
}

impl DrawDeskMesh {
    fn draw(&mut self, cx: &mut CxDraw, mesh: &Geometry) {
        self.draw_vars.geometry_id = Some(mesh.geometry_id());
        if self.draw_vars.can_instance() {
            let new_area = cx.add_instance(&mut self.draw_vars);
            self.draw_vars.area = cx.update_area_refs(self.draw_vars.area, new_area);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum GrabMode {
    #[default]
    None,
    BarePan,
    Control { mod_idx: usize, control_id: u32 },
    Pinch,
}

#[derive(Clone, Copy, Default)]
struct ActiveTouch {
    uid: u64,
    abs: Vec2d,
}

#[derive(Script, Widget)]
pub struct DeskViewport {
    #[uid]
    uid: WidgetUid,
    #[walk]
    walk: Walk,
    #[redraw]
    #[rust]
    area: Area,
    #[live]
    draw_mesh: DrawDeskMesh,
    #[live]
    draw_image: DrawQuad,
    #[live]
    draw_txt: DrawText,
    #[new]
    pass: DrawPass,
    #[new]
    draw_list: DrawList,
    #[new]
    color_texture: Texture,
    #[new]
    depth_texture: Texture,

    // 4 shared procedural meshes
    #[rust]
    mesh_box: Option<Geometry>,
    #[rust]
    mesh_cyl: Option<Geometry>,
    #[rust]
    mesh_jog: Option<Geometry>,
    #[rust]
    mesh_knob: Option<Geometry>,

    // Orthographic camera (panel units -> screen px: screen = rect.pos + (panel - pan) * zoom)
    #[rust]
    pan_x: f32,
    #[rust]
    pan_y: f32,
    #[rust]
    zoom: f32,
    #[rust]
    tgt_pan_x: f32,
    #[rust]
    tgt_pan_y: f32,
    #[rust]
    tgt_zoom: f32,
    #[rust]
    cam_initialized: bool,
    #[rust]
    fitted_module: Option<usize>,
    #[rust]
    last_rect: Rect,

    // Touch & gesture state
    #[rust]
    grab: GrabMode,
    #[rust]
    last_abs: Vec2d,
    #[rust]
    last_panel: (f32, f32),
    #[rust]
    last_tap_time: f64,
    #[rust]
    last_tap_abs: Vec2d,
    #[rust]
    last_ctrl_tap_time: f64,
    #[rust]
    last_ctrl_tap: Option<(usize, u32)>,
    #[rust]
    active_touches: Vec<ActiveTouch>,
    #[rust]
    pinch_last_mid: Vec2d,
    #[rust]
    pinch_last_dist: f64,

    // Modules (all 8 full-parity One Desk modules)
    #[rust]
    tape: TapeModule,
    #[rust]
    transport: TransportModule,
    #[rust]
    out_mod: OutModule,
    #[rust]
    voice: VoiceModule,
    #[rust]
    lyrics: LyricsModule,
    #[rust]
    bands: BandsModule,
    #[rust]
    rack: RackModule,
    #[rust]
    link: LinkModule,

    // Reusable batch buffer & wire state
    #[rust]
    batch: PanelDraw,
    #[rust]
    state: DeskState,
    #[rust]
    last_wire_rev: u64,
    // the bake: the shell's push streams seen, the show it last woke for, on screen last event
    #[rust]
    stream_seen: [u64; 4],
    #[rust]
    seen_epoch: u64,
    #[rust]
    was_visible: bool,
    #[rust]
    last_scrub_cmd_at: Option<Instant>,
    #[rust]
    last_scrub_cmd_rate: f32,

    // Per-frame outbound coalescer & change-only deduper (<=1 write per control per frame, 0 if unchanged)
    #[rust]
    pending_out: Option<[f32; 4]>,
    #[rust]
    last_sent_head_out: Option<[i32; 4]>,
    #[rust]
    last_sent_store_out: Option<[i32; 4]>,
    #[rust]
    last_store_out_at: Option<Instant>,
    #[rust]
    pending_mix: Vec<(String, f32)>,
    #[rust]
    last_sent_mix: Vec<(String, i32)>,
    #[rust]
    pending_voice_param: Vec<(String, String, String)>,
    #[rust]
    last_sent_voice_param: Vec<(String, String, String)>,

    // Live 1.0 s rolling frame pacing meter (zero-alloc fixed capacity ring -> DeskState.perf_fps/perf_max_gap_ms)
    #[rust]
    perf_ring: Vec<(Instant, f32)>,
    #[rust]
    perf_head: usize,
    #[rust]
    perf_last_frame: Option<Instant>,
    #[rust]
    perf_was_active: bool,

    // Animation pacing & zero-idle timer
    #[rust]
    anim_timer: Timer,
    #[rust]
    timer_running: bool,
    #[rust]
    last_anim_time: f64,
    #[rust]
    last_draw_at: Option<Instant>,

    // Frame cost metrics (microseconds CPU draw_walk: whole-desk vs single-module)
    #[rust]
    whole_frames: u64,
    #[rust]
    whole_us_sum: u64,
    #[rust]
    whole_us_max: u64,
    #[rust]
    mod_frames: u64,
    #[rust]
    mod_us_sum: u64,
    #[rust]
    mod_us_max: u64,
}

impl ScriptHook for DeskViewport {
    fn on_after_new(&mut self, vm: &mut ScriptVm) {
        self.zoom = 1.58;
        self.tgt_zoom = 1.58;
        self.perf_ring = Vec::with_capacity(96);
        let hub = wire_hub();
        if let Ok(st) = hub.state.lock() {
            let _ = self.state.sync_changed_from(&st);
            self.last_wire_rev = st.rev;
        }
        self.tape.sync_static_anchor(&self.state);
        self.transport.sync_from_state(&self.state);
        self.out_mod.sync_from_state(&self.state);
        self.voice.sync_from_state(&self.state);

        vm.with_cx_mut(|cx| {
            self.mesh_box = Some(build_box_mesh(cx));
            self.mesh_cyl = Some(build_cyl_mesh(cx));
            self.mesh_jog = Some(build_jog_mesh(cx));
            self.mesh_knob = Some(build_knob_mesh(cx));

            self.color_texture = Texture::new_with_format(
                cx,
                TextureFormat::RenderBGRAu8 { size: TextureSize::Auto, initial: true },
            );
            self.depth_texture = Texture::new_with_format(
                cx,
                TextureFormat::DepthD32 { size: TextureSize::Auto, initial: true },
            );
            let bg = vec4(0.035, 0.048, 0.072, 1.0);
            self.pass.set_color_texture(cx, &self.color_texture, DrawPassClearColor::ClearWith(bg));
            self.pass.set_depth_texture(cx, &self.depth_texture, DrawPassClearDepth::ClearWith(1.0));
            self.pass.set_keep_camera_matrix(cx, true);
            self.pass.set_dpi_factor(cx, PASS_SCALE);
        });
    }
}

impl DeskViewport {
    fn module_count(&self) -> usize { 8 }

    fn module_ref(&self, idx: usize) -> &dyn DeskModule {
        match idx {
            0 => &self.tape,
            1 => &self.transport,
            2 => &self.lyrics,
            3 => &self.bands,
            4 => &self.out_mod,
            5 => &self.voice,
            6 => &self.rack,
            _ => &self.link,
        }
    }

    fn whole_desk_fit_params(&self, rect: Rect) -> (f32, f32, f32) {
        let rw = rect.size.x.max(10.0) as f32;
        let rh = rect.size.y.max(10.0) as f32;
        let z = (rw / (PANEL_W + 4.0)).min(rh / (PANEL_H + 4.0)).clamp(0.5, 4.0);
        let px = PANEL_W * 0.5 - (rw * 0.5) / z;
        let py = PANEL_H * 0.5 - (rh * 0.5) / z;
        (px, py, z)
    }

    fn set_fit_whole_desk(&mut self, instant: bool) {
        let (px, py, z) = self.whole_desk_fit_params(self.last_rect);
        self.tgt_pan_x = px;
        self.tgt_pan_y = py;
        self.tgt_zoom = z;
        self.fitted_module = None;
        if instant {
            self.pan_x = px;
            self.pan_y = py;
            self.zoom = z;
        }
    }

    fn set_fit_module(&mut self, mod_idx: usize, focus_x: Option<f32>) {
        let mr = self.module_ref(mod_idx).rect();
        let rw = self.last_rect.size.x.max(10.0) as f32;
        let rh = self.last_rect.size.y.max(10.0) as f32;
        let (_, _, min_z) = self.whole_desk_fit_params(self.last_rect);
        let pad = 6.0;
        let fit_z = (rw / (mr.w + pad * 2.0)).min(rh / (mr.h + pad * 2.0));
        // Full-width modules (TAPE, TRANSPORT, LYRICS, BANDS) zoom to a genuine working close-up
        // (>= 1.80x whole-desk so every fader/chip exceeds the 7 mm grab threshold), centered on tap X.
        let z = if mr.w > 180.0 {
            fit_z.max(min_z * 1.80).clamp(min_z * 1.50, 5.5)
        } else {
            fit_z.clamp(min_z * 1.15, 5.5)
        };
        let view_w = rw / z;
        let cx = if mr.w > 180.0 {
            let fx = focus_x.unwrap_or(mr.cx());
            let lo = mr.x - 4.0 + view_w * 0.5;
            let hi = mr.right() + 4.0 - view_w * 0.5;
            if hi > lo { fx.clamp(lo, hi) } else { mr.cx() }
        } else {
            mr.cx()
        };
        self.tgt_zoom = z;
        self.tgt_pan_x = cx - view_w * 0.5;
        self.tgt_pan_y = mr.cy() - (rh * 0.5) / z;
        self.fitted_module = Some(mod_idx);
    }

    fn clamp_pan(&mut self) {
        let rw = self.last_rect.size.x.max(10.0) as f32;
        let rh = self.last_rect.size.y.max(10.0) as f32;
        let view_w = rw / self.zoom.max(0.2);
        let view_h = rh / self.zoom.max(0.2);
        let margin_x = (view_w * 0.35).min(PANEL_W * 0.45);
        let margin_y = (view_h * 0.35).min(PANEL_H * 0.45);
        self.pan_x = self.pan_x.clamp(-margin_x, (PANEL_W - view_w + margin_x).max(-margin_x));
        self.pan_y = self.pan_y.clamp(-margin_y, (PANEL_H - view_h + margin_y).max(-margin_y));
    }

    fn screen_to_panel(&self, abs: Vec2d) -> (f32, f32) {
        let sx = (abs.x - self.last_rect.pos.x) as f32;
        let sy = (abs.y - self.last_rect.pos.y) as f32;
        (self.pan_x + sx / self.zoom.max(0.2), self.pan_y + sy / self.zoom.max(0.2))
    }

    fn module_at(&self, px: f32, py: f32) -> Option<usize> {
        for i in 0..self.module_count() {
            if self.module_ref(i).rect().contains(px, py) {
                return Some(i);
            }
        }
        None
    }

    fn camera_animating(&self) -> bool {
        (self.pan_x - self.tgt_pan_x).abs() > 0.08
            || (self.pan_y - self.tgt_pan_y).abs() > 0.08
            || (self.zoom - self.tgt_zoom).abs() > 0.004
    }

    fn needs_animation(&self) -> bool {
        if self.camera_animating() {
            return true;
        }
        for i in 0..self.module_count() {
            if self.module_ref(i).needs_animation(&self.state) {
                return true;
            }
        }
        false
    }

    fn ensure_anim_or_redraw(&mut self, cx: &mut Cx) {
        self.area.redraw(cx);
        if self.needs_animation() {
            if !self.timer_running {
                // Even 30 fps (exact 2x divisor of 60 Hz display; strictly 0 fps when idle)
                self.anim_timer = cx.start_interval(1.0 / 30.0);
                self.timer_running = true;
                self.last_anim_time = 0.0;
            }
        } else if self.timer_running {
            cx.stop_timer(self.anim_timer);
            self.anim_timer = Timer::default();
            self.timer_running = false;
        }
    }

    fn redraw_throttled(&mut self, cx: &mut Cx) {
        let now = Instant::now();
        if let Some(last) = self.last_draw_at {
            if now.duration_since(last) < Duration::from_millis(32) {
                return;
            }
        }
        self.last_draw_at = Some(now);
        self.area.redraw(cx);
    }

    fn call_module_touch(&mut self, mod_idx: usize, ev: ModuleTouch, cmds: &mut DeskCmds) {
        match mod_idx {
            0 => self.tape.on_touch(ev, &self.state, cmds),
            1 => self.transport.on_touch(ev, &self.state, cmds),
            2 => self.lyrics.on_touch(ev, &self.state, cmds),
            3 => self.bands.on_touch(ev, &self.state, cmds),
            4 => self.out_mod.on_touch(ev, &self.state, cmds),
            5 => self.voice.on_touch(ev, &self.state, cmds),
            6 => self.rack.on_touch(ev, &self.state, cmds),
            _ => self.link.on_touch(ev, &self.state, cmds),
        }
    }

    fn quant_out(v: [f32; 4]) -> [i32; 4] {
        [
            (v[0].clamp(0.0, 1.0) * 1000.0).round() as i32,
            (v[1].clamp(0.0, 1.0) * 1000.0).round() as i32,
            (v[2].clamp(0.0, 1.0) * 1000.0).round() as i32,
            (v[3].clamp(0.0, 1.0) * 120.0).round() as i32, // 0.1 dB resolution matching wire
        ]
    }

    fn flush_pending_writes(&mut self, force_store_out: bool) {
        // 1. OUT: instant lgtm-head.sock datagram (<=1/frame, 0 if unchanged) + throttled store sync (<=5Hz & on release)
        if let Some(out_v) = self.pending_out.take() {
            let q = Self::quant_out(out_v);
            let head_ok = if self.last_sent_head_out != Some(q) {
                let ok = send_head_out(out_v[0], out_v[1], out_v[2], out_v[3], self.state.out_presence_db);
                if ok {
                    self.last_sent_head_out = Some(q);
                }
                ok
            } else {
                true
            };
            if self.last_sent_store_out != Some(q) {
                let now = Instant::now();
                let store_due = force_store_out
                    || !head_ok
                    || self
                        .last_store_out_at
                        .map(|t| now.duration_since(t) >= Duration::from_millis(200))
                        .unwrap_or(true);
                if store_due {
                    self.last_sent_store_out = Some(q);
                    self.last_store_out_at = Some(now);
                    send_lgtm_out(out_v[0], out_v[1], out_v[2], out_v[3], self.state.out_presence_db);
                } else {
                    // Keep pending until 200ms store window or FingerUp flushes it
                    self.pending_out = Some(out_v);
                }
            }
        }

        // 2. BANDS mix: coalesce all changed faders into at most ONE batched store write per frame (0 if unchanged)
        if !self.pending_mix.is_empty() {
            let mut changed: Vec<(String, f32)> = Vec::new();
            for (band_id, val) in self.pending_mix.drain(..) {
                let q = (val.clamp(0.0, 1.0) * 1000.0).round() as i32;
                let same = self
                    .last_sent_mix
                    .iter()
                    .find(|(k, _)| k == &band_id)
                    .map(|(_, prev)| *prev == q)
                    .unwrap_or(false);
                if !same {
                    if let Some(entry) = self.last_sent_mix.iter_mut().find(|(k, _)| k == &band_id) {
                        entry.1 = q;
                    } else {
                        self.last_sent_mix.push((band_id.clone(), q));
                    }
                    changed.push((band_id, val));
                }
            }
            if !changed.is_empty() {
                send_lgtm_mix_batch(&changed);
            }
        }

        // 3. VOICE params: at most 1 command per changed param per frame (0 when unchanged/same step)
        if !self.pending_voice_param.is_empty() {
            for (vid, pid, raw) in self.pending_voice_param.drain(..) {
                let same = self
                    .last_sent_voice_param
                    .iter()
                    .find(|(v, p, _)| v == &vid && p == &pid)
                    .map(|(_, _, prev)| prev == &raw)
                    .unwrap_or(false);
                if !same {
                    if let Some(entry) = self
                        .last_sent_voice_param
                        .iter_mut()
                        .find(|(v, p, _)| v == &vid && p == &pid)
                    {
                        entry.2 = raw.clone();
                    } else {
                        self.last_sent_voice_param.push((vid.clone(), pid.clone(), raw.clone()));
                    }
                    let _ = send_lgtm_cmd(
                        "voice_param",
                        &format!(
                            "{{\"voice\":\"{}\",\"id\":\"{}\",\"value\":{}}}",
                            json_escape(&vid),
                            json_escape(&pid),
                            raw
                        ),
                    );
                }
            }
        }
    }

    fn dispatch_cmds(&mut self, cx: &mut Cx, cmds: DeskCmds) {
        let mut focus_id: Option<&'static str> = None;
        for cmd in cmds.items {
            match cmd {
                DeskCmd::LgtmCmd { name, data_json, .. } => {
                    let _ = send_lgtm_cmd(&name, &data_json);
                }
                DeskCmd::OutPatch { level, balance, tone, duck } => {
                    self.pending_out = Some([level, balance, tone, duck]);
                }
                DeskCmd::MixPatch { band_id, value } => {
                    if let Some(ex) = self.pending_mix.iter_mut().find(|(k, _)| k == &band_id) {
                        ex.1 = value;
                    } else {
                        self.pending_mix.push((band_id, value));
                    }
                }
                DeskCmd::VoiceParam { voice_id, param_id, raw_value } => {
                    if let Some(ex) = self
                        .pending_voice_param
                        .iter_mut()
                        .find(|(v, p, _)| v == &voice_id && p == &param_id)
                    {
                        ex.2 = raw_value;
                    } else {
                        self.pending_voice_param.push((voice_id, param_id, raw_value));
                    }
                }
                DeskCmd::HeadVel { op, omega, rate, held, coast } => {
                    if send_head_vel(op, omega, rate, held, coast) {
                        if op == "grab" && !self.state.head_runs {
                            let _ = send_lgtm_cmd("scrub", "{\"rate\":0.0}");
                        }
                    } else {
                        // Remote monolith1 fallback <= 20 Hz
                        let q = if rate.abs() < 0.05 { 0.0 } else { (rate.clamp(-4.0, 4.0) * 100.0).round() / 100.0 };
                        let now = Instant::now();
                        let force = op == "grab";
                        let due = force
                            || self.last_scrub_cmd_at.map(|t| now.duration_since(t) >= Duration::from_millis(50)).unwrap_or(true);
                        let changed = force || (q == 0.0 && self.last_scrub_cmd_rate != 0.0) || (q - self.last_scrub_cmd_rate).abs() >= 0.02;
                        if due && changed {
                            self.last_scrub_cmd_at = Some(now);
                            self.last_scrub_cmd_rate = q;
                            let payload = if q == 0.0 { "{\"rate\":0.0}".into() } else { format!("{{\"rate\":{:.2}}}", q) };
                            let _ = send_lgtm_cmd("scrub", &payload);
                        }
                    }
                }
                DeskCmd::HeadEnd { end_ms } => {
                    self.last_scrub_cmd_at = None;
                    self.last_scrub_cmd_rate = 0.0;
                    if send_head_json("{\"scrub_end\":true}\n") {
                        if !self.state.head_runs {
                            let _ = send_lgtm_cmd("scrub_end", &format!("{{\"pos_ms\":{}}}", end_ms.round() as i64));
                        }
                    } else {
                        let _ = send_lgtm_cmd("scrub_end", "{}");
                    }
                }
                DeskCmd::OpenPromptEdit { band_id, name, prompt } => {
                    send_open_prompt_edit(&band_id, &name, &prompt);
                }
                DeskCmd::ClosePromptEdit => {
                    send_close_prompt_edit();
                }
                DeskCmd::Reconnect => {
                    let _ = send_lgtm_cmd("reconnect", "{}");
                }
                DeskCmd::FocusModule { module_id } => {
                    focus_id = Some(module_id);
                }
            }
        }
        if let Some(mid) = focus_id {
            for i in 0..self.module_count() {
                if self.module_ref(i).id() == mid {
                    self.set_fit_module(i, None);
                    break;
                }
            }
        }
        if cmds.request_redraw || focus_id.is_some() {
            self.ensure_anim_or_redraw(cx);
        }
    }

    pub fn sync_from_wire(&mut self, cx: &mut Cx) {
        let hub = wire_hub();
        let dirty = {
            let st = hub.state.lock().unwrap();
            if st.rev == self.last_wire_rev {
                return;
            }
            self.last_wire_rev = st.rev;
            self.state.sync_changed_from(&st)
        };
        if dirty == 0 {
            return;
        }
        // Only recompute the specific module(s) whose store keys actually changed
        if (dirty & DIRTY_TAPE) != 0 {
            self.tape.sync_static_anchor(&self.state);
        }
        if (dirty & DIRTY_TRANSPORT) != 0 {
            self.transport.sync_from_state(&self.state);
        }
        if (dirty & DIRTY_OUT) != 0 {
            self.out_mod.sync_from_state(&self.state);
            if self.pending_out.is_none() {
                let q = Self::quant_out([
                    self.state.out_level,
                    self.state.out_balance,
                    self.state.out_tone,
                    self.state.out_duck,
                ]);
                self.last_sent_store_out = Some(q);
            }
        }
        if (dirty & (DIRTY_BANDS | DIRTY_MIX_VALS)) != 0 {
            for b in &self.state.bands {
                if self.pending_mix.iter().any(|(k, _)| k == &b.id) {
                    continue;
                }
                let q = (b.value.clamp(0.0, 1.0) * 1000.0).round() as i32;
                if let Some(entry) = self.last_sent_mix.iter_mut().find(|(k, _)| k == &b.id) {
                    entry.1 = q;
                } else {
                    self.last_sent_mix.push((b.id.clone(), q));
                }
            }
        }
        if (dirty & DIRTY_VOICE) != 0 {
            self.voice.sync_from_state(&self.state);
        }

        if self.needs_animation() {
            if !self.timer_running {
                self.ensure_anim_or_redraw(cx);
            }
        } else {
            if self.timer_running {
                cx.stop_timer(self.anim_timer);
                self.anim_timer = Timer::default();
                self.timer_running = false;
            }
            self.redraw_throttled(cx);
        }
    }

    fn step_animation(&mut self, cx: &mut Cx, dt: f32) {
        let dt = dt.clamp(0.001, 0.05);

        // Smooth orthographic camera spring (double-tap fit / <7mm auto-zoom)
        if self.camera_animating() {
            let a = 1.0 - (-16.0 * dt).exp();
            self.pan_x += (self.tgt_pan_x - self.pan_x) * a;
            self.pan_y += (self.tgt_pan_y - self.pan_y) * a;
            self.zoom += (self.tgt_zoom - self.zoom) * a;
            if !self.camera_animating() {
                self.pan_x = self.tgt_pan_x;
                self.pan_y = self.tgt_pan_y;
                self.zoom = self.tgt_zoom;
            }
        }

        let mut cmds = DeskCmds::default();
        self.tape.step(dt, &self.state, &mut cmds);
        self.transport.step(dt, &self.state, &mut cmds);
        self.out_mod.step(dt, &self.state, &mut cmds);
        self.voice.step(dt, &self.state, &mut cmds);
        self.lyrics.step(dt, &self.state, &mut cmds);
        self.bands.step(dt, &self.state, &mut cmds);
        self.rack.step(dt, &self.state, &mut cmds);
        self.link.step(dt, &self.state, &mut cmds);
        self.dispatch_cmds(cx, cmds);

        self.area.redraw(cx);
        if !self.needs_animation() && self.timer_running {
            cx.stop_timer(self.anim_timer);
            self.anim_timer = Timer::default();
            self.timer_running = false;
        }
    }

    fn handle_touch_update(&mut self, cx: &mut Cx, te: &TouchUpdateEvent) {
        // Maintain active multi-touch set for smooth two-finger pan & pinch zoom
        for tp in &te.touches {
            match tp.state {
                TouchState::Start => {
                    if let Some(ex) = self.active_touches.iter_mut().find(|x| x.uid == tp.uid) {
                        ex.abs = tp.abs;
                    } else {
                        self.active_touches.push(ActiveTouch { uid: tp.uid, abs: tp.abs });
                    }
                }
                TouchState::Move => {
                    if let Some(ex) = self.active_touches.iter_mut().find(|x| x.uid == tp.uid) {
                        ex.abs = tp.abs;
                    } else {
                        self.active_touches.push(ActiveTouch { uid: tp.uid, abs: tp.abs });
                    }
                }
                TouchState::Stop => {
                    self.active_touches.retain(|x| x.uid != tp.uid);
                }
                _ => {}
            }
        }

        if self.active_touches.len() >= 2 {
            let a = self.active_touches[0].abs;
            let b = self.active_touches[1].abs;
            let mid = dvec2((a.x + b.x) * 0.5, (a.y + b.y) * 0.5);
            let dist = ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt().max(12.0);
            if self.grab != GrabMode::Pinch {
                // Cancel any in-progress control scrub if a second finger lands for pinch
                if matches!(self.grab, GrabMode::Control { mod_idx: 0, .. }) {
                    let mut cmds = DeskCmds::default();
                    self.tape.end_scrub_if_active(&self.state, &mut cmds);
                    self.dispatch_cmds(cx, cmds);
                }
                self.grab = GrabMode::Pinch;
                self.pinch_last_mid = mid;
                self.pinch_last_dist = dist;
                self.fitted_module = None;
            } else {
                let (_, _, min_z) = self.whole_desk_fit_params(self.last_rect);
                let panel_under_old = self.screen_to_panel(self.pinch_last_mid);
                let scale = (dist / self.pinch_last_dist.max(12.0)) as f32;
                let new_zoom = (self.zoom * scale).clamp(min_z * 0.85, 6.0);
                self.zoom = new_zoom;
                // Keep panel_under_old pinned beneath the new pinch midpoint (simultaneous pan + zoom!)
                let new_sx = (mid.x - self.last_rect.pos.x) as f32;
                let new_sy = (mid.y - self.last_rect.pos.y) as f32;
                self.pan_x = panel_under_old.0 - new_sx / new_zoom;
                self.pan_y = panel_under_old.1 - new_sy / new_zoom;
                self.clamp_pan();
                self.tgt_pan_x = self.pan_x;
                self.tgt_pan_y = self.pan_y;
                self.tgt_zoom = self.zoom;
                self.pinch_last_mid = mid;
                self.pinch_last_dist = dist;
                self.area.redraw(cx);
            }
        } else if self.grab == GrabMode::Pinch {
            self.grab = GrabMode::None;
        }
    }

    fn draw_all_meshes(&mut self, cx: &mut Cx2d) {
        if let Some(mesh) = self.mesh_box.as_ref() {
            for inst in &self.batch.boxes {
                self.draw_mesh.model = inst.model;
                self.draw_mesh.color = inst.color;
                self.draw_mesh.mat = inst.mat;
                self.draw_mesh.draw(cx, mesh);
            }
        }
        if let Some(mesh) = self.mesh_cyl.as_ref() {
            for inst in &self.batch.cyls {
                self.draw_mesh.model = inst.model;
                self.draw_mesh.color = inst.color;
                self.draw_mesh.mat = inst.mat;
                self.draw_mesh.draw(cx, mesh);
            }
        }
        if let Some(mesh) = self.mesh_jog.as_ref() {
            for inst in &self.batch.jogs {
                self.draw_mesh.model = inst.model;
                self.draw_mesh.color = inst.color;
                self.draw_mesh.mat = inst.mat;
                self.draw_mesh.draw(cx, mesh);
            }
        }
        if let Some(mesh) = self.mesh_knob.as_ref() {
            for inst in &self.batch.knobs {
                self.draw_mesh.model = inst.model;
                self.draw_mesh.color = inst.color;
                self.draw_mesh.mat = inst.mat;
                self.draw_mesh.draw(cx, mesh);
            }
        }
    }

    fn draw_all_texts(&mut self, cx: &mut Cx2d, rect: Rect) {
        let zoom = self.zoom;
        for t in &self.batch.texts {
            let sx = rect.pos.x + (t.x - self.pan_x) as f64 * zoom as f64;
            let sy = rect.pos.y + (t.y - self.pan_y) as f64 * zoom as f64;
            if sx < rect.pos.x - 160.0
                || sx > rect.pos.x + rect.size.x + 160.0
                || sy < rect.pos.y - 30.0
                || sy > rect.pos.y + rect.size.y + 30.0
            {
                continue;
            }
            let raw_fs = t.size_u * zoom * 0.82;
            let (target_fs, alpha) = if t.is_title {
                let min_px = if t.size_u >= 5.0 { 9.0 } else { 7.5 };
                (raw_fs.clamp(min_px, 16.0), 1.0)
            } else {
                if raw_fs < 5.4 {
                    continue;
                }
                let a = ((raw_fs - 5.4) / 2.4).clamp(0.0, 1.0);
                (raw_fs.min(20.0), a)
            };
            // Snap to 8 discrete hinted sizes so pinch/zoom never thrashes DrawText's glyph atlas
            // or layout cache with hundreds of distinct fractional font sizes.
            let fs = snap_font_size(target_fs);
            self.draw_txt.text_style.font_size = fs;
            self.draw_txt.color = vec4(t.color.x, t.color.y, t.color.z, t.color.w * alpha);
            let char_mult = match t.align {
                TextAlign::Left => 0.62,
                TextAlign::Center => 0.64,
                TextAlign::Right => 0.68,
            };
            let est_w = t.text.chars().count() as f64 * (fs as f64 * char_mult);
            let draw_x = match t.align {
                TextAlign::Left => sx,
                TextAlign::Center => sx - est_w * 0.5,
                TextAlign::Right => sx - est_w,
            };
            let draw_y = sy - (fs as f64 * 0.68);
            self.draw_txt.draw_abs(cx, dvec2(draw_x, draw_y), &t.text);
        }
    }
    fn record_frame_pacing(&mut self, t_start: Instant) {
        let active_now = self.timer_running || self.grab != GrabMode::None || self.camera_animating();
        let gap_ms = if self.perf_was_active {
            self.perf_last_frame
                .map(|prev| {
                    let d = t_start.duration_since(prev).as_secs_f32() * 1000.0;
                    if d < 1500.0 { d } else { 0.0 }
                })
                .unwrap_or(0.0)
        } else {
            0.0
        };
        if self.perf_ring.len() < 96 {
            self.perf_ring.push((t_start, gap_ms));
        } else {
            self.perf_ring[self.perf_head] = (t_start, gap_ms);
            self.perf_head = (self.perf_head + 1) % 96;
        }
        self.perf_last_frame = Some(t_start);
        self.perf_was_active = active_now;

        let window = Duration::from_secs(1);
        let mut count = 0u32;
        let mut oldest = t_start;
        let mut max_gap = 0.0f32;
        for &(ts, g) in &self.perf_ring {
            if t_start.duration_since(ts) <= window {
                count += 1;
                if ts < oldest {
                    oldest = ts;
                }
                if g > max_gap {
                    max_gap = g;
                }
            }
        }
        let fps = if !active_now && count <= 2 {
            0.0
        } else if count >= 3 {
            let span = t_start.duration_since(oldest).as_secs_f32();
            if span >= 0.12 && span < 0.85 {
                ((count - 1) as f32 / span).min(60.0)
            } else {
                (count as f32).min(60.0)
            }
        } else {
            count as f32
        };
        self.state.perf_fps = fps;
        self.state.perf_max_gap_ms = max_gap;
    }
}

impl Widget for DeskViewport {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let t_start = Instant::now();
        let rect = cx.walk_turtle(walk);
        if rect.size.x < 4.0 || rect.size.y < 4.0 {
            return DrawStep::done();
        }
        self.last_rect = rect;
        if !self.cam_initialized {
            self.cam_initialized = true;
            self.set_fit_whole_desk(true);
        }
        self.record_frame_pacing(t_start);

        // Orthographic viewport bounds in panel units (for exact camera + per-module frustum culling)
        let view_w = rect.size.x as f32 / self.zoom.max(0.2);
        let view_h = rect.size.y as f32 / self.zoom.max(0.2);
        let vis_x0 = self.pan_x - 10.0;
        let vis_x1 = self.pan_x + view_w + 10.0;
        let vis_y0 = self.pan_y - 10.0;
        let vis_y1 = self.pan_y + view_h + 10.0;
        let in_view = |mr: PanelRect| -> bool {
            mr.right() >= vis_x0 && mr.x <= vis_x1 && mr.bottom() >= vis_y0 && mr.y <= vis_y1
        };

        // Collect only on-screen modules' 3D instances & panel-space labels
        self.batch.clear(self.zoom);
        // Dark outer soundboard chassis backing plate
        self.batch.plate(PanelRect::new(0.0, 0.0, PANEL_W, PANEL_H), -0.35, 0.6, vec4(0.055, 0.072, 0.102, 1.0), 0.15, 0.0);

        if in_view(self.tape.rect())      { self.tape.draw(&mut self.batch, &self.state); }
        if in_view(self.transport.rect()) { self.transport.draw(&mut self.batch, &self.state); }
        if in_view(self.lyrics.rect())    { self.lyrics.draw(&mut self.batch, &self.state); }
        if in_view(self.bands.rect())     { self.bands.draw(&mut self.batch, &self.state); }
        if in_view(self.out_mod.rect())   { self.out_mod.draw(&mut self.batch, &self.state); }
        if in_view(self.voice.rect())     { self.voice.draw(&mut self.batch, &self.state); }
        if in_view(self.rack.rect())      { self.rack.draw(&mut self.batch, &self.state); }
        if in_view(self.link.rect())      { self.link.draw(&mut self.batch, &self.state); }

        // Orthographic projection straight at the panel (one pass, 4 instanced draws)
        let view = Mat4f::identity();
        let proj = mat4_ortho_world(
            self.pan_x,
            self.pan_x + view_w,
            -(self.pan_y + view_h),
            -self.pan_y,
            -30.0,
            30.0,
        );
        self.pass.set_camera(cx, view, proj);

        cx.make_child_pass(&self.pass);
        cx.begin_pass(&self.pass, None);
        self.draw_list.begin_always(cx);
        self.draw_all_meshes(cx);
        self.draw_list.end(cx);
        cx.end_pass(&self.pass);

        self.draw_image.draw_vars.set_texture(0, &self.color_texture);
        self.draw_image.draw_abs(cx, rect);
        self.area = self.draw_image.area();
        cx.set_pass_area(&self.pass, self.area);
        self.pass.set_dpi_factor(cx, PASS_SCALE);

        // Sharp real text in exact panel transform
        self.draw_all_texts(cx, rect);

        // Flush coalesced outgoing control writes at most once per rendered frame (0 if unchanged)
        self.flush_pending_writes(false);

        // Record steady-state CPU frame cost (skipping frame 1 cold JIT compile)
        let us = t_start.elapsed().as_micros() as u64;
        self.state.perf_draw_ms = us as f32 / 1000.0;
        let (_, _, fit_z) = self.whole_desk_fit_params(rect);
        let is_zoomed = self.zoom > fit_z * 1.35;
        if is_zoomed {
            self.mod_frames += 1;
            if self.mod_frames > 1 {
                self.mod_us_sum += us;
                if us > self.mod_us_max { self.mod_us_max = us; }
            }
            if matches!(self.mod_frames, 2 | 5 | 15 | 30) || (self.mod_frames > 30 && self.mod_frames % 30 == 0) {
                let n = (self.mod_frames - 1).max(1);
                let avg = self.mod_us_sum as f64 / n as f64 / 1000.0;
                println!(
                    "desk_frame[module]: frames={} warm_avg={:.2}ms warm_max={:.2}ms last={:.2}ms inst={} texts={} zoom={:.2}",
                    self.mod_frames,
                    avg,
                    self.mod_us_max as f64 / 1000.0,
                    us as f64 / 1000.0,
                    self.batch.boxes.len() + self.batch.cyls.len() + self.batch.jogs.len() + self.batch.knobs.len(),
                    self.batch.texts.len(),
                    self.zoom,
                );
            }
        } else {
            self.whole_frames += 1;
            if self.whole_frames > 1 {
                self.whole_us_sum += us;
                if us > self.whole_us_max { self.whole_us_max = us; }
            }
            if matches!(self.whole_frames, 2 | 5 | 15 | 30) || (self.whole_frames > 30 && self.whole_frames % 30 == 0) {
                let n = (self.whole_frames - 1).max(1);
                let avg = self.whole_us_sum as f64 / n as f64 / 1000.0;
                println!(
                    "desk_frame[whole]: frames={} warm_avg={:.2}ms warm_max={:.2}ms last={:.2}ms inst={} texts={} zoom={:.2}",
                    self.whole_frames,
                    avg,
                    self.whole_us_max as f64 / 1000.0,
                    us as f64 / 1000.0,
                    self.batch.boxes.len() + self.batch.cyls.len() + self.batch.jogs.len() + self.batch.knobs.len(),
                    self.batch.texts.len(),
                    self.zoom,
                );
            }
        }

        DrawStep::done()
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, _scope: &mut Scope) {
        // the bake: off screen the desk sleeps (no timer, no syncs); a scrub in hand ends
        if !super::desk_visible() {
            if self.was_visible {
                self.was_visible = false;
                let mut cmds = DeskCmds::default();
                let st = self.state.clone();
                self.tape.end_scrub_if_active(&st, &mut cmds);
                self.dispatch_cmds(cx, cmds);
                if self.timer_running {
                    cx.stop_timer(self.anim_timer);
                    self.anim_timer = Timer::default();
                    self.timer_running = false;
                }
            }
            return;
        }
        if let Event::Signal = event {
            let epoch = super::shown_epoch();
            if epoch != self.seen_epoch || !self.was_visible {
                // shown again: take the whole wire state and draw
                self.seen_epoch = epoch;
                self.last_wire_rev = u64::MAX;
                self.area.redraw(cx);
            }
            self.was_visible = true;
            super::take_streams(&mut self.stream_seen);
            self.sync_from_wire(cx);
        }
        if let Event::TouchUpdate(te) = event {
            self.handle_touch_update(cx, te);
        }
        if let Some(te) = self.anim_timer.is_event(event) {
            let now_t = te.time.unwrap_or(self.last_anim_time + (1.0 / 30.0));
            let dt = if self.last_anim_time > 0.0 {
                (now_t - self.last_anim_time) as f32
            } else {
                1.0 / 30.0
            };
            self.last_anim_time = now_t;
            self.step_animation(cx, dt);
        }

        if self.grab == GrabMode::Pinch {
            return;
        }

        match event.hits(cx, self.area) {
            Hit::FingerDown(fe) => {
                self.last_abs = fe.abs;
                let (px, py) = self.screen_to_panel(fe.abs);
                self.last_panel = (px, py);

                // Check if touch landed on a module control
                if let Some(mi) = self.module_at(px, py) {
                    let hit = self.module_ref(mi).hit_control(px, py, &self.state);
                    if let Some(ch) = hit {
                        let screen_grab_px = ch.min_grab_u * self.zoom;
                        if screen_grab_px < MIN_GRAB_PX {
                            // Control too small (< 7 mm on glass): zoom smoothly to its module instead of moving it!
                            self.grab = GrabMode::None;
                            self.set_fit_module(mi, Some(px));
                            self.ensure_anim_or_redraw(cx);
                            return;
                        }
                        // Check double-tap on this control (e.g. reset fader/knob to rest)
                        let is_ctrl_dtap = self.last_ctrl_tap == Some((mi, ch.id))
                            && (fe.time - self.last_ctrl_tap_time) < 0.34;
                        if is_ctrl_dtap {
                            self.last_ctrl_tap = None;
                            self.grab = GrabMode::None;
                            let mut cmds = DeskCmds::default();
                            let zoom = self.zoom;
                            self.call_module_touch(
                                mi,
                                ModuleTouch {
                                    kind: TouchKind::DoubleTap,
                                    control_id: ch.id,
                                    x: px, y: py, dx: 0.0, dy: 0.0,
                                    time: fe.time, px_per_u: zoom,
                                },
                                &mut cmds,
                            );
                            self.dispatch_cmds(cx, cmds);
                            self.flush_pending_writes(true);
                            return;
                        }
                        self.last_ctrl_tap = Some((mi, ch.id));
                        self.last_ctrl_tap_time = fe.time;
                        self.grab = GrabMode::Control { mod_idx: mi, control_id: ch.id };
                        let mut cmds = DeskCmds::default();
                        let zoom = self.zoom;
                        self.call_module_touch(
                            mi,
                            ModuleTouch {
                                kind: TouchKind::Down,
                                control_id: ch.id,
                                x: px, y: py, dx: 0.0, dy: 0.0,
                                time: fe.time, px_per_u: zoom,
                            },
                            &mut cmds,
                        );
                        self.dispatch_cmds(cx, cmds);
                        return;
                    }
                }

                // Bare panel or module title bar: check double-tap fit toggle
                let dist_prev = ((fe.abs.x - self.last_tap_abs.x).powi(2) + (fe.abs.y - self.last_tap_abs.y).powi(2)).sqrt();
                if (fe.time - self.last_tap_time) < 0.34 && dist_prev < 28.0 {
                    self.last_tap_time = 0.0;
                    self.grab = GrabMode::None;
                    if let Some(mi) = self.module_at(px, py) {
                        if self.fitted_module == Some(mi) {
                            self.set_fit_whole_desk(false);
                        } else {
                            self.set_fit_module(mi, Some(px));
                        }
                    } else {
                        self.set_fit_whole_desk(false);
                    }
                    self.ensure_anim_or_redraw(cx);
                    return;
                }
                self.last_tap_time = fe.time;
                self.last_tap_abs = fe.abs;
                self.grab = GrabMode::BarePan;
            }
            Hit::FingerMove(fe) => {
                let (px, py) = self.screen_to_panel(fe.abs);
                let dx_u = px - self.last_panel.0;
                let dy_u = py - self.last_panel.1;
                match self.grab {
                    GrabMode::BarePan => {
                        let d = fe.abs - self.last_abs;
                        self.pan_x -= d.x as f32 / self.zoom.max(0.2);
                        self.pan_y -= d.y as f32 / self.zoom.max(0.2);
                        self.clamp_pan();
                        self.tgt_pan_x = self.pan_x;
                        self.tgt_pan_y = self.pan_y;
                        self.tgt_zoom = self.zoom;
                        self.fitted_module = None;
                        self.area.redraw(cx);
                    }
                    GrabMode::Control { mod_idx, control_id } => {
                        let mut cmds = DeskCmds::default();
                        let zoom = self.zoom;
                        self.call_module_touch(
                            mod_idx,
                            ModuleTouch {
                                kind: TouchKind::Move,
                                control_id,
                                x: px, y: py, dx: dx_u, dy: dy_u,
                                time: fe.time, px_per_u: zoom,
                            },
                            &mut cmds,
                        );
                        self.dispatch_cmds(cx, cmds);
                    }
                    _ => {}
                }
                self.last_abs = fe.abs;
                self.last_panel = (px, py);
            }
            Hit::FingerUp(fe) => {
                let (px, py) = self.screen_to_panel(fe.abs);
                if let GrabMode::Control { mod_idx, control_id } = self.grab {
                    let mut cmds = DeskCmds::default();
                    let zoom = self.zoom;
                    self.call_module_touch(
                        mod_idx,
                        ModuleTouch {
                            kind: TouchKind::Up,
                            control_id,
                            x: px, y: py, dx: 0.0, dy: 0.0,
                            time: fe.time, px_per_u: zoom,
                        },
                        &mut cmds,
                    );
                    self.dispatch_cmds(cx, cmds);
                    self.flush_pending_writes(true);
                }
                self.grab = GrabMode::None;
            }
            Hit::FingerScroll(fe) if fe.scroll.y != 0.0 => {
                let (_, _, min_z) = self.whole_desk_fit_params(self.last_rect);
                let panel_under = self.screen_to_panel(fe.abs);
                let step = if fe.scroll.y > 0.0 { 1.12 } else { 1.0 / 1.12 };
                let new_z = (self.zoom * step).clamp(min_z * 0.85, 6.0);
                self.zoom = new_z;
                let sx = (fe.abs.x - self.last_rect.pos.x) as f32;
                let sy = (fe.abs.y - self.last_rect.pos.y) as f32;
                self.pan_x = panel_under.0 - sx / new_z;
                self.pan_y = panel_under.1 - sy / new_z;
                self.clamp_pan();
                self.tgt_pan_x = self.pan_x;
                self.tgt_pan_y = self.pan_y;
                self.tgt_zoom = self.zoom;
                self.fitted_module = None;
                self.area.redraw(cx);
            }
            _ => {}
        }
    }
}
