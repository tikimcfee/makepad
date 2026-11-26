//! The flat desk (lgtm SPEC day two, "One desk"), baked from lgtm-deck3d's JIT app
//! (phone: makepad-jit/edits/apps/lgtm_desk; notes ~/lgtm/deck3d/DESK-PORTING.md). Its files are
//! taken as they are; what the bake adds is here: the desk wakes the UI only while on screen, and it
//! reads the shell's push streams (lgtm_head, lgtm_pos, lgtm_orig, lgtm_new) on the UI thread. Its
//! WireHub still talks to the store over the shell's own socket, as the JIT app did.

pub mod mesh;
mod mod_bands;
mod mod_link;
mod mod_lyrics;
mod mod_out;
mod mod_rack;
mod mod_tape;
mod mod_transport;
mod mod_voice;
pub mod module;
pub mod panel;
pub mod wire;

use makepad_widgets::*;
use panel::{DeskViewport, DrawDeskMesh};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The surface the desk is baked into (the old perspective deck's).
pub const DESK_SURFACE: &str = "deck";

static DESK_VISIBLE: AtomicBool = AtomicBool::new(false);
static DESK_SHOWN_EPOCH: AtomicU64 = AtomicU64::new(0);

pub fn desk_visible() -> bool {
    DESK_VISIBLE.load(Ordering::SeqCst)
}

pub fn shown_epoch() -> u64 {
    DESK_SHOWN_EPOCH.load(Ordering::SeqCst)
}

/// The shell calls this whenever its visible set changes (as for TermdView and the old deck).
pub fn sync_visibility(visible: &BTreeSet<String>, bodies: &HashMap<String, String>, panel_off: bool) {
    let vis = !panel_off
        && visible.iter().any(|name| name == DESK_SURFACE || bodies.get(name).is_some_and(|b| b.contains("LgtmDesk")));
    let was = DESK_VISIBLE.swap(vis, Ordering::SeqCst);
    if vis && !was {
        DESK_SHOWN_EPOCH.fetch_add(1, Ordering::SeqCst);
        SignalToUI::set_ui_signal();
    }
}

/// New wire state wakes the UI only while the desk is on screen.
pub fn wake_ui_if_visible() {
    if desk_visible() {
        SignalToUI::set_ui_signal();
    }
}

/// The shell's push streams, read on the UI thread: the local head's [pos_ms, rate, dur_ms]
/// first, then the engine's position fraction (ignored while the head is fresh) and the levels.
pub fn take_streams(seen: &mut [u64; 4]) -> bool {
    let hub = wire::wire_hub();
    let now = Instant::now();
    let mut moved = false;
    let mut tape_moved = false;
    let mut out_moved = false;
    let head = crate::stream::latest("lgtm_head", seen[3]);
    let pos = crate::stream::latest("lgtm_pos", seen[0]);
    let orig = crate::stream::latest("lgtm_orig", seen[1]);
    let new = crate::stream::latest("lgtm_new", seen[2]);
    if head.is_none() && pos.is_none() && orig.is_none() && new.is_none() {
        return false;
    }
    let Ok(mut st) = hub.state.lock() else { return false };
    if let Some((v, values)) = head {
        seen[3] = v;
        if values.len() >= 2 {
            st.head_pos_ms = values[0] as f64;
            st.head_rate = values[1] as f64;
            st.head_at = Some(now);
            if values.len() >= 3 && values[2] as f64 > 100.0 {
                st.dur_ms = values[2] as f64;
            }
            moved = true;
            tape_moved = true;
        }
    }
    let head_fresh = st.head_is_fresh(now);
    if let Some((v, values)) = pos {
        seen[0] = v;
        if let (false, Some(f)) = (head_fresh, values.last()) {
            let clamped = (*f as f64).clamp(0.0, 1.0) * st.dur_ms.max(1.0);
            let changed = (clamped - st.pos_anchor_ms).abs() > 0.05;
            let stale = now.duration_since(st.last_stream_at) > Duration::from_millis(80);
            if changed || !st.has_stream_pos || stale {
                st.pos_anchor_ms = clamped;
                st.pos_anchor_at = now;
                st.has_stream_pos = true;
            }
            st.last_stream_at = now;
            moved = true;
            tape_moved = true;
        }
    }
    if let Some((v, values)) = orig {
        seen[1] = v;
        if let Some(f) = values.last() {
            st.level_orig = f.clamp(0.0, 1.0);
            moved = true;
            out_moved = true;
        }
    }
    if let Some((v, values)) = new {
        seen[2] = v;
        if let Some(f) = values.last() {
            st.level_new = f.clamp(0.0, 1.0);
            moved = true;
            out_moved = true;
        }
    }
    // the desk copies by domain (rev_tape: head and position; rev_out: the meters), then rev
    if tape_moved {
        st.rev_tape += 1;
    }
    if out_moved {
        st.rev_out += 1;
    }
    if moved {
        st.rev += 1;
    }
    moved
}


script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*
    use mod.math.*
    use mod.shader.*
    use mod.draw
    use mod.geom

    // Flat orthographic 1-light shader: pure LDR baked-feel studio console lighting, 0 perspective
    mod.draw.DrawDeskMesh = mod.std.set_type_default() do #(DrawDeskMesh::script_shader(vm)){
        alpha_blend: false
        backface_culling: true

        vertex_pos: vertex_position(vec4f)
        fb0: fragment_output(0, vec4f)

        draw_call: uniform_buffer(draw.DrawCallUniforms)
        draw_pass: uniform_buffer(draw.DrawPassUniforms)
        draw_list: uniform_buffer(draw.DrawListUniforms)
        geom: vertex_buffer(geom.IcoVertex, geom.IcoGeom)

        // Single fixed top-left front studio lamp (orthographic camera looks along -Z)
        light_dir: uniform(vec3(-0.30, 0.52, 0.80))

        world_normal: varying(vec3f)

        vertex: fn() {
            let world = self.model * self.geom.pos
            self.world_normal = (self.model * self.geom.normal).xyz
            let view_pos = self.draw_pass.camera_view * world
            self.vertex_pos = self.draw_pass.camera_projection * view_pos
        }

        pixel: fn() {
            let normal = normalize(self.world_normal)
            let to_light = normalize(self.light_dir)
            let to_cam = vec3(0.0, 0.0, 1.0)

            let ambient = 0.34
            let diff = max(dot(normal, to_light), 0.0) * 0.66
            let half_dir = normalize(to_light + to_cam)
            let spec_pow = mix(12.0, 52.0, clamp(self.mat.x, 0.0, 1.0))
            let spec_amp = mix(0.05, 0.44, clamp(self.mat.x, 0.0, 1.0))
            let spec = pow(max(dot(normal, half_dir), 0.0), spec_pow) * spec_amp

            let warm_lamp = vec3(1.0, 0.98, 0.94)
            let lit = self.color.xyz * (ambient + diff * warm_lamp) + vec3(spec, spec, spec)
            let emit = self.color.xyz * clamp(self.mat.y, 0.0, 1.0) * 1.15
            return vec4(lit + emit, 1.0)
        }

        fragment: fn() {
            self.fb0 = self.pixel()
        }
    }

    mod.widgets.DeskViewport = set_type_default() do #(DeskViewport::register_widget(vm)){
        width: Fill
        height: Fill
        draw_image +: {
            scene_texture: texture_2d(float)
            pixel: fn() {
                return self.scene_texture.sample(self.pos)
            }
        }
        draw_txt +: {
            text_style: theme.font_regular{ font_size: 10.0 }
            color: #xe2e8f0
        }
    }

    mod.widgets.LgtmDesk = mod.widgets.DeskViewport{}
}

/// Splash isolates (view files) see `LgtmDesk` with the other widgets.
pub fn isolate_mod(vm: &mut ScriptVm) {
    self::script_mod(vm);
    script_eval!(vm, {
        mod.prelude.widgets = {
            ..mod.prelude.widgets,
            LgtmDesk: mod.widgets.LgtmDesk,
        }
    });
}
