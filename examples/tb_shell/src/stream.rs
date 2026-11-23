//! Streams: values that change many times a second (a voice wave, a level)
//! and do not belong in the store. A client pushes floats to a named stream
//! (`{"op":"push","stream":"mic","values":[...]}` on a kept connection); every
//! `StreamView{stream: "mic"}` in any surface writes them into its one drawn
//! quad's instance values in place. No script runs, no store write, no
//! widget is rebuilt and no draw list is recorded again: the pass is only
//! repainted.
//!
//! The look is ours (the bars shader below), tried here; bodies only place
//! the view and choose its colours, so it passes the shader gate.

use makepad_widgets::*;
use std::cell::RefCell;
use std::collections::HashMap;

/// Values a view holds: 8 vec4 instance slots. Instance values reach the pixel
/// shader as vertex outputs, and the GE8320 allows 64 components in all: 16
/// slots (76 with the quad's own) failed to link there. More values would go
/// through a texture.
pub const MAX_VALUES: usize = 32;

thread_local! {
    /// stream name -> (version, values). UI thread only, like the isolates.
    /// stream name -> (version, values, ring: appended to, so drawn as a full ring of MAX_VALUES)
    static STREAMS: RefCell<HashMap<String, (u64, Vec<f32>, bool)>> = RefCell::new(HashMap::new());
}

/// Stores the newest values of a stream; the views pick them up on the next event.
/// `append`: the values are added to the stream's own ring of the last MAX_VALUES (a
/// client sends one new value a time), and the stream is drawn as that full ring, newest at
/// the right, slots not filled yet at zero; otherwise they replace it. `clear` empties it
/// first (a new take starts).
pub fn push(name: &str, values: Vec<f32>, append: bool, clear: bool) {
    STREAMS.with(|s| {
        let mut s = s.borrow_mut();
        let e = s.entry(name.to_string()).or_insert((0, Vec::new(), false));
        e.0 += 1;
        if clear {
            e.1.clear();
        }
        e.2 |= append;
        if append {
            e.1.extend(values);
            let over = e.1.len().saturating_sub(MAX_VALUES);
            e.1.drain(..over);
        } else {
            e.1 = values;
        }
    });
}

fn current(name: &str, seen: u64) -> Option<(u64, Vec<f32>, bool)> {
    STREAMS.with(|s| s.borrow().get(name).filter(|(v, _, _)| *v != seen).cloned())
}

script_mod! {
    use mod.prelude.widgets_internal.*

    set_type_default() do #(DrawStreamBars::script_shader(vm)){
        ..mod.draw.DrawQuad
        color_bg: #x0b0e14
        color_track: #x1c2230
        color_bar: #x5fd4ff
        // glow: a soft radial wash from the bottom centre, as bright as the energy, cool to warm
        // uniforms, not instances: the GE8320's 64 varying components are nearly spent
        color_glow: uniform(#x2f7fd0)
        color_warm: uniform(#x8a6cf0)
        soft: 0.0
        border_radius: 0.0
        pick: fn(i) {
            let q = floor(i / 4.0)
            let r = i - q * 4.0
            let lane = vec4(1.0 - min(abs(r), 1.0), 1.0 - min(abs(r - 1.0), 1.0), 1.0 - min(abs(r - 2.0), 1.0), 1.0 - min(abs(r - 3.0), 1.0))
            let mut v = self.v0
            if q > 0.5 { v = self.v1 }
            if q > 1.5 { v = self.v2 }
            if q > 2.5 { v = self.v3 }
            if q > 3.5 { v = self.v4 }
            if q > 4.5 { v = self.v5 }
            if q > 5.5 { v = self.v6 }
            if q > 6.5 { v = self.v7 }
            return dot(v, lane)
        }
        pixel: fn() {
            let sdf = Sdf2d.viewport(self.pos * self.rect_size)
            // meter: one value, as a length from the left over a dim track
            if self.meter > 0.5 {
                let on = step(self.pos.x, clamp(self.v0.x, 0.0, 1.0))
                let col = mix(self.color_track, self.color_bar, on)
                if self.border_radius > 0.5 {
                    sdf.box(0.0, 0.0, self.rect_size.x, self.rect_size.y, self.border_radius)
                    sdf.fill(col)
                    return sdf.result
                }
                return col
            }
            let n = max(self.count, 1.0)
            if self.soft > 0.5 {
                let cell = self.pos.x * n
                let idx = floor(cell)
                let h0 = clamp(self.pick(idx), 0.0, 1.0)
                let h1 = clamp(self.pick(min(idx + 1.0, n - 1.0)), 0.0, 1.0)
                let h = mix(h0, h1, smoothstep(0.0, 1.0, fract(cell))) * 0.92 + 0.05
                let from_bot = 1.0 - self.pos.y
                let wave = (1.0 - smoothstep(h - 0.35, h + 0.08, from_bot)) * (0.45 + 0.55 * (1.0 - from_bot))
                let rib = 0.82 + 0.18 * smoothstep(0.08, 0.35, fract(cell))
                let mut bg = self.color_bg
                let mut lift = 1.0
                if self.energy > -0.5 {
                    // the level's envelope (Rust, attack 30 ms / release 250 ms), never draw_pass.time
                    let e = clamp(self.energy, 0.0, 1.0)
                    let d = (self.pos - vec2(0.5, 1.1)) * vec2(1.0, 1.6)
                    let falloff = 1.0 - smoothstep(0.0, 1.0, length(d))
                    let glow_col = mix(self.color_glow, self.color_warm, e * e)
                    bg = mix(self.color_bg, glow_col, clamp((0.15 + 0.85 * e) * falloff, 0.0, 1.0))
                    lift = 0.6 + 0.4 * e
                }
                let col = mix(bg, self.color_bar, clamp(wave * rib * lift, 0.0, 1.0))
                if self.border_radius > 0.5 {
                    sdf.box(0.0, 0.0, self.rect_size.x, self.rect_size.y, self.border_radius)
                    sdf.fill(col)
                    return sdf.result
                }
                return col
            }
            let h = clamp(self.pick(floor(self.pos.x * n)), 0.0, 1.0)
            let y = abs(self.pos.y - 0.5) * 2.0
            let gap = step(0.15, fract(self.pos.x * n))
            let on = (1.0 - smoothstep(h - 0.04, h, y)) * gap
            return mix(self.color_bg, self.color_bar, on)
        }
    }

    mod.widgets.StreamViewBase = #(StreamView::register_widget(vm))
    mod.widgets.StreamView = set_type_default() do mod.widgets.StreamViewBase {
        width: Fill
        height: 90
    }
}

#[derive(Script, ScriptHook, Debug)]
#[repr(C)]
pub struct DrawStreamBars {
    #[deref]
    draw_super: DrawQuad,
    #[live] count: f32,
    #[live] meter: f32,
    #[live] soft: f32,
    #[live] border_radius: f32,
    /// The glow's envelope 0..1; -1 = no glow (one instance float: the varying budget).
    #[live] energy: f32,
    #[live] v0: Vec4f, #[live] v1: Vec4f, #[live] v2: Vec4f, #[live] v3: Vec4f,
    #[live] v4: Vec4f, #[live] v5: Vec4f, #[live] v6: Vec4f, #[live] v7: Vec4f,
}

const SLOTS: [LiveId; 8] = [
    live_id!(v0), live_id!(v1), live_id!(v2), live_id!(v3), live_id!(v4), live_id!(v5), live_id!(v6), live_id!(v7),
];

impl DrawStreamBars {
    fn slots_mut(&mut self) -> [&mut Vec4f; 8] {
        [&mut self.v0, &mut self.v1, &mut self.v2, &mut self.v3, &mut self.v4, &mut self.v5, &mut self.v6, &mut self.v7]
    }

    fn set_values(&mut self, values: &[f32], ring: bool) {
        // A ring always has all its slots, so a bar's width never changes; newest at the right.
        let padded: Vec<f32>;
        let values = if ring {
            padded = std::iter::repeat(0.0).take(MAX_VALUES.saturating_sub(values.len())).chain(values.iter().copied()).collect();
            &padded[..]
        } else {
            values
        };
        let n = values.len().min(MAX_VALUES);
        self.count = n as f32;
        for (i, slot) in self.slots_mut().into_iter().enumerate() {
            let at = |k: usize| values.get(i * 4 + k).copied().unwrap_or(0.0);
            *slot = vec4(at(0), at(1), at(2), at(3));
        }
    }

    /// Writes the values into the already drawn quad and repaints its pass.
    fn write_in_place(&mut self, cx: &mut Cx) {
        let count = self.count;
        self.draw_vars.set_instance_on_area(cx, live_id!(count), &[count]);
        for i in 0..8 {
            let v = *self.slots_mut()[i];
            self.draw_vars.set_instance_on_area(cx, SLOTS[i], &[v.x, v.y, v.z, v.w]);
        }
    }
}

#[derive(Script, ScriptHook, Widget)]
pub struct StreamView {
    #[source] source: ScriptObjectRef,
    #[uid] uid: WidgetUid,
    #[walk] walk: Walk,
    #[redraw] #[live] draw_bg: DrawStreamBars,
    #[live] stream: ArcStringMut,
    /// One value drawn as a horizontal meter (a level), not as bars.
    #[live] meter: bool,
    /// `soft` mode: the background glows with this stream's level (its first value), through
    /// an attack/release envelope, under the wave.
    #[live] energy_stream: ArcStringMut,
    #[live] glow: bool,
    #[rust] seen: u64,
    #[rust] seen_energy: u64,
    #[rust] energy: f32,
    #[rust] energy_target: f32,
    #[rust] energy_at: Option<std::time::Instant>,
    #[rust] energy_frame: NextFrame,
}

const ATTACK_MS: f32 = 30.0;
const RELEASE_MS: f32 = 250.0;

impl StreamView {
    /// Moves the envelope towards the level by the time since its last step; returns
    /// whether it is still moving (then it asks for another frame, else none: at rest a
    /// silent mic means no wake-ups).
    fn step_energy(&mut self, cx: &mut Cx) -> bool {
        let now = std::time::Instant::now();
        let dt = self.energy_at.map_or(16.0, |at| now.duration_since(at).as_secs_f32() * 1000.0).min(100.0);
        self.energy_at = Some(now);
        let tau = if self.energy_target > self.energy { ATTACK_MS } else { RELEASE_MS };
        self.energy += (self.energy_target - self.energy) * (1.0 - (-dt / tau).exp());
        let moving = (self.energy - self.energy_target).abs() > 0.004;
        if !moving {
            self.energy = self.energy_target;
        }
        self.draw_bg.energy = if self.glow { self.energy } else { -1.0 };
        self.draw_bg.draw_vars.set_instance_on_area(cx, live_id!(energy), &[self.draw_bg.energy]);
        if moving {
            self.energy_frame = cx.new_next_frame();
        } else {
            self.energy_at = None;
        }
        moving
    }
}

impl Widget for StreamView {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, _scope: &mut Scope) {
        // A push arrives as a signal; the shell has stored it before the event reaches here.
        if let Event::Signal = event {
            if let Some((version, values, ring)) = current(self.stream.as_ref(), self.seen) {
                self.seen = version;
                self.draw_bg.set_values(&values, ring);
                self.draw_bg.write_in_place(cx);
            }
            if self.glow && !self.energy_stream.as_ref().is_empty() {
                if let Some((version, values, _)) = current(self.energy_stream.as_ref(), self.seen_energy) {
                    self.seen_energy = version;
                    self.energy_target = values.last().copied().unwrap_or(0.0).clamp(0.0, 1.0);
                    if self.energy_at.is_none() {
                        self.step_energy(cx);
                    }
                }
            }
        }
        if self.energy_frame.is_event(event).is_some() {
            self.step_energy(cx);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        if let Some((version, values, ring)) = current(self.stream.as_ref(), u64::MAX) {
            self.seen = version;
            self.draw_bg.set_values(&values, ring);
        }
        self.draw_bg.meter = if self.meter { 1.0 } else { 0.0 };
        self.draw_bg.energy = if self.glow { self.energy } else { -1.0 };
        self.draw_bg.draw_walk(cx, walk);
        DrawStep::done()
    }
}

/// Installed into every isolate: the widget, then the isolate's widget
/// prelude again (it is a copy taken before host mods run).
pub fn isolate_mod(vm: &mut ScriptVm) {
    self::script_mod(vm);
    script_eval!(vm, {
        mod.prelude.widgets = {
            ..mod.prelude.widgets,
            StreamView: mod.widgets.StreamView,
        }
    });
}
