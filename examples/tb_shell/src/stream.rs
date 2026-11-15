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
    static STREAMS: RefCell<HashMap<String, (u64, Vec<f32>)>> = RefCell::new(HashMap::new());
}

/// Stores the newest values of a stream; the views pick them up on the next event.
/// `append`: the values are added to the stream's own ring of the last MAX_VALUES (a
/// client sends one new value a time); otherwise they replace it.
pub fn push(name: &str, values: Vec<f32>, append: bool) {
    STREAMS.with(|s| {
        let mut s = s.borrow_mut();
        let e = s.entry(name.to_string()).or_insert((0, Vec::new()));
        e.0 += 1;
        if append {
            e.1.extend(values);
            let over = e.1.len().saturating_sub(MAX_VALUES);
            e.1.drain(..over);
        } else {
            e.1 = values;
        }
    });
}

fn current(name: &str, seen: u64) -> Option<(u64, Vec<f32>)> {
    STREAMS.with(|s| s.borrow().get(name).filter(|(v, _)| *v != seen).cloned())
}

script_mod! {
    use mod.prelude.widgets_internal.*

    set_type_default() do #(DrawStreamBars::script_shader(vm)){
        ..mod.draw.DrawQuad
        color_bg: #x0b0e14
        color_bar: #x5fd4ff
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
            let n = max(self.count, 1.0)
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

    fn set_values(&mut self, values: &[f32]) {
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
    #[rust] seen: u64,
}

impl Widget for StreamView {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, _scope: &mut Scope) {
        // A push arrives as a signal; the shell has stored it before the event reaches here.
        if let Event::Signal = event {
            if let Some((version, values)) = current(self.stream.as_ref(), self.seen) {
                self.seen = version;
                self.draw_bg.set_values(&values);
                self.draw_bg.write_in_place(cx);
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        if let Some((version, values)) = current(self.stream.as_ref(), u64::MAX) {
            self.seen = version;
            self.draw_bg.set_values(&values);
        }
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
