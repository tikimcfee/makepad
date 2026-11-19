//! Native GPU `TermdView` widget for `tb-shell` (`g3d-termd` Protocol 1.5).
//!
//! Connects over Unix stream socket to `termd-<ns>.sock`, subscribes (`mode: "raw"`,
//! `role: "own"`, `flow: "lossy"`, `history_rows: 800`), feeds `REPAINT` (`0x85`) and
//! streaming `OUTPUT` (`0x83`) bytes into Makepad's pure-Rust Ghostty VT screen
//! (`makepad_app_terminal::term`), and renders on PowerVR GE8320 in ~3–4 instanced GL
//! draw calls (`new_batch: false`: row-merged `DrawTermBg`, `DrawTermCursor`, pixel-snapped
//! `DrawTermBox` + `DrawText` `many_instances` glyph batch, and optional `DrawTermUnderline`).
//!
//! Enforces Ruling 1 / Proof 4 (`0` asleep wake-ups): when `panel_off` is true or no
//! visible Stage surface hosts `TermdView`, all active subscription sockets are shut down
//! immediately; waking or showing `terminal` re-subscribes and receives a fresh `REPAINT`.

use crate::termd_wire::{
    self, encode_frame, handshake_ns, j_get, j_obj, j_str, j_u64, key_fields, parse_key_spec,
    read_frame, socket_path, FRAME_ERROR, FRAME_EVENT, FRAME_INPUT, FRAME_OUTPUT, FRAME_REPAINT,
    FRAME_REPLY, FRAME_REQUEST,
};
use makepad_app_terminal::term::color::Rgb;
use makepad_app_terminal::term::modes::Mode;
use makepad_app_terminal::term::page::CellContent;
use makepad_app_terminal::term::screen::CursorStyle;
use makepad_app_terminal::term::stream::Stream as VtStream;
use makepad_app_terminal::term::style::{StyleColor, StyleFlags, Underline};
use makepad_app_terminal::term::terminal::Terminal as VtTerminal;
use makepad_micro_serde::{DeJson, JsonValue, SerJson};
use makepad_widgets::makepad_draw::shader::draw_text::PreparedGlyph;
use makepad_widgets::makepad_platform::event::TouchState;
use makepad_widgets::text::geom::Point;
use makepad_widgets::*;
use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static GLOBAL_PAUSED: AtomicBool = AtomicBool::new(false);
static NEXT_SUB_TOKEN: AtomicU64 = AtomicU64::new(1);

fn active_sub_socks() -> &'static Mutex<HashMap<u64, UnixStream>> {
    static MAP: std::sync::OnceLock<Mutex<HashMap<u64, UnixStream>>> = std::sync::OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Called by `tb-shell` whenever `visible` surfaces or `panel_off` changes.
/// Immediately shuts down all `TermdView` sockets when asleep or when no visible
/// surface uses `TermdView` (`0` background wake-ups).
pub fn sync_global_visibility(
    visible: &BTreeSet<String>,
    bodies: &HashMap<String, String>,
    panel_off: bool,
) {
    let any_visible_termd = !panel_off
        && visible.iter().any(|name| {
            name == "terminal"
                || bodies
                    .get(name)
                    .is_some_and(|b| b.contains("TermdView"))
        });
    let should_pause = !any_visible_termd;
    let was_paused = GLOBAL_PAUSED.swap(should_pause, Ordering::SeqCst);
    if should_pause && !was_paused {
        if let Ok(mut map) = active_sub_socks().lock() {
            for (_, sock) in map.drain() {
                let _ = sock.shutdown(Shutdown::Both);
            }
        }
    }
}

pub fn is_globally_paused() -> bool {
    GLOBAL_PAUSED.load(Ordering::SeqCst)
}

script_mod! {
    use mod.prelude.widgets_internal.*
    use mod.widgets.*

    set_type_default() do #(DrawTermdCellBg::script_shader(vm)) {
        ..mod.draw.DrawQuad
        draw_call_group: @tb_termd_cell_bg
        color: #x131722
        pixel: fn() {
            return vec4(self.color.rgb * self.color.a, self.color.a)
        }
    }

    set_type_default() do #(DrawTermdBox::script_shader(vm)) {
        ..mod.draw.DrawQuad
        draw_call_group: @tb_termd_box
        color: #xffffff
        arms: vec4(0.0, 0.0, 0.0, 0.0)
        center: vec2(0.0, 0.0)
        corner: vec2(0.0, 0.0)
        thickness: 1.0
        radius: 0.0
        pixel_size: 1.0
        pixel: fn() {
            let d = self.pos * self.rect_size - self.center
            let half = self.thickness * 0.5
            var distance = 10000.0
            if self.radius > 0.0 {
                let q = d * self.corner
                if q.x >= self.radius {
                    distance = abs(q.y) - half
                } else if q.y >= self.radius {
                    distance = abs(q.x) - half
                } else {
                    distance = abs(length(q - vec2(self.radius)) - self.radius) - half
                }
            } else {
                if self.arms.x + self.arms.y > 0.0 {
                    let left = if self.arms.x > 0.0 {-10000.0} else {-half}
                    let right = if self.arms.y > 0.0 {10000.0} else {half}
                    distance = max(abs(d.y) - half, max(left - d.x, d.x - right))
                }
                if self.arms.z + self.arms.w > 0.0 {
                    let top = if self.arms.z > 0.0 {-10000.0} else {-half}
                    let bottom = if self.arms.w > 0.0 {10000.0} else {half}
                    distance = min(distance, max(abs(d.x) - half, max(top - d.y, d.y - bottom)))
                }
            }
            let coverage = clamp(0.5 - distance / self.pixel_size, 0.0, 1.0)
            let alpha = self.color.a * coverage
            return vec4(self.color.rgb * alpha, alpha)
        }
    }

    set_type_default() do #(DrawTermdUnderline::script_shader(vm)) {
        ..mod.draw.DrawQuad
        draw_call_group: @tb_termd_underline
        color: #xc0caf5
        kind: 1.0
        on: fn(a: float) {
            return vec4(self.color.rgb * self.color.a * a, self.color.a * a)
        }
        pixel: fn() {
            if self.kind < 1.5 {
                return self.on(1.0)
            }
            if self.kind < 2.5 {
                if self.pos.y < 0.33 || self.pos.y > 0.66 {
                    return self.on(1.0)
                }
                return self.on(0.0)
            }
            if self.kind < 3.5 {
                let center = 0.5 + sin(self.pos.x * self.rect_size.x * 1.2) * 0.3
                if abs(self.pos.y - center) < 0.22 {
                    return self.on(1.0)
                }
                return self.on(0.0)
            }
            if self.kind < 4.5 {
                if modf(self.pos.x * self.rect_size.x, 3.0) < 1.5 {
                    return self.on(1.0)
                }
                return self.on(0.0)
            }
            if modf(self.pos.x * self.rect_size.x, 8.0) < 5.0 {
                return self.on(1.0)
            }
            return self.on(0.0)
        }
    }

    set_type_default() do #(DrawTermdCursor::script_shader(vm)) {
        ..mod.draw.DrawQuad
        draw_call_group: @tb_termd_cursor
        color: #x7aa2f7
        hollow: 0.0
        pixel: fn() {
            if self.hollow < 0.5 {
                return vec4(self.color.rgb * self.color.a, self.color.a)
            }
            let border = 1.0
            let bx = border / self.rect_size.x
            let by = border / self.rect_size.y
            if self.pos.x < bx || self.pos.x > 1.0 - bx || self.pos.y < by || self.pos.y > 1.0 - by {
                return vec4(self.color.rgb, 1.0)
            }
            return vec4(0.0, 0.0, 0.0, 0.0)
        }
    }

    mod.widgets.TermdViewBase = #(TermdView::register_widget(vm))
    mod.widgets.TermdView = set_type_default() do mod.widgets.TermdViewBase{
        width: Fill
        height: Fill
        ns: "default"
        term_id: ""
        role: "own"
        cols_preset: 48
        font_size: 12.0
        pad_x: 5.0
        pad_y: 4.0
        focused: true
        draw_bg +: {
            color: uniform(#x131722)
            border_radius: uniform(10.0)
            pixel: fn() {
                let p = self.pos * self.rect_size
                let sdf = Sdf2d.viewport(p)
                sdf.box(0.0, 0.0, self.rect_size.x, self.rect_size.y, self.border_radius)
                sdf.fill(self.color)
                return sdf.result
            }
        }
        draw_text +: {
            draw_call_group: @tb_termd_text
            text_style: TextStyle{
                font_family: FontFamily{
                    latin := FontMember{
                        res: crate_resource("makepad_widgets:resources/jetbrains_mono_variable.ttf")
                        asc: 0.0 desc: 0.0 weight: 400.0
                    }
                    icons := FontMember{
                        res: crate_resource("makepad_widgets:resources/fa-solid-900.ttf")
                        asc: 0.0 desc: 0.0
                    }
                    emoji := FontMember{
                        res: crate_resource("makepad_widgets:resources/NotoColorEmoji.ttf")
                        asc: 0.0 desc: 0.0
                    }
                    symbols := FontMember{
                        res: crate_resource("makepad_widgets:resources/Inter.ttf")
                        asc: 0.0 desc: 0.0
                    }
                }
                line_spacing: 1.05
            }
        }
        bold_text_style: TextStyle{
            font_family: FontFamily{
                latin := FontMember{
                    res: crate_resource("makepad_widgets:resources/jetbrains_mono_variable.ttf")
                    asc: 0.0 desc: 0.0 weight: 700.0
                }
                icons := FontMember{
                    res: crate_resource("makepad_widgets:resources/fa-solid-900.ttf")
                    asc: 0.0 desc: 0.0
                }
                emoji := FontMember{
                    res: crate_resource("makepad_widgets:resources/NotoColorEmoji.ttf")
                    asc: 0.0 desc: 0.0
                }
                symbols := FontMember{
                    res: crate_resource("makepad_widgets:resources/Inter.ttf")
                    asc: 0.0 desc: 0.0
                }
            }
            line_spacing: 1.05
        }
        draw_cell_bg +: {}
        draw_boxes +: {}
        draw_underline +: {}
        draw_cursor +: {}
    }
}

#[derive(Script, ScriptHook)]
#[repr(C)]
struct DrawTermdCellBg {
    #[deref]
    draw_super: DrawQuad,
    #[live]
    color: Vec4f,
}

#[derive(Script, ScriptHook)]
#[repr(C)]
struct DrawTermdBox {
    #[deref]
    draw_super: DrawQuad,
    #[live]
    color: Vec4f,
    #[live]
    arms: Vec4f,
    #[live]
    center: Vec2f,
    #[live]
    corner: Vec2f,
    #[live]
    thickness: f32,
    #[live]
    radius: f32,
    #[live]
    pixel_size: f32,
}

#[derive(Script, ScriptHook)]
#[repr(C)]
struct DrawTermdUnderline {
    #[deref]
    draw_super: DrawQuad,
    #[live]
    color: Vec4f,
    #[live]
    kind: f32,
}

#[derive(Script, ScriptHook)]
#[repr(C)]
struct DrawTermdCursor {
    #[deref]
    draw_super: DrawQuad,
    #[live]
    color: Vec4f,
    #[live]
    hollow: f32,
}

#[derive(Clone, Copy)]
struct CachedGlyph {
    glyph: PreparedGlyph,
    font_size_in_lpxs: f32,
    x_offset_in_lpxs: f32,
    y_offset_in_lpxs: f32,
}

fn termd_theme() -> ([Rgb; 16], Rgb, Rgb, Rgb) {
    let base16 = [
        Rgb::new(0x13, 0x17, 0x22), // 0: bg black
        Rgb::new(0xf7, 0x76, 0x8e), // 1: red
        Rgb::new(0x9e, 0xce, 0x6a), // 2: green
        Rgb::new(0xe0, 0xaf, 0x68), // 3: yellow
        Rgb::new(0x7a, 0xa2, 0xf7), // 4: blue
        Rgb::new(0xbb, 0x9a, 0xf7), // 5: magenta
        Rgb::new(0x2a, 0xc3, 0xde), // 6: cyan
        Rgb::new(0xa9, 0xb1, 0xd6), // 7: white
        Rgb::new(0x41, 0x48, 0x68), // 8: bright black
        Rgb::new(0xff, 0x7a, 0x93), // 9: bright red
        Rgb::new(0xb9, 0xf2, 0x7c), // 10: bright green
        Rgb::new(0xff, 0x9e, 0x64), // 11: bright yellow
        Rgb::new(0x7d, 0xa6, 0xff), // 12: bright blue
        Rgb::new(0xbb, 0x9a, 0xf7), // 13: bright magenta
        Rgb::new(0x0d, 0xb9, 0xd7), // 14: bright cyan
        Rgb::new(0xc0, 0xca, 0xf5), // 15: bright white
    ];
    (
        base16,
        Rgb::new(0xc0, 0xca, 0xf5),
        Rgb::new(0x13, 0x17, 0x22),
        Rgb::new(0x7a, 0xa2, 0xf7),
    )
}

fn make_themed_vt(cols: usize, rows: usize) -> VtTerminal {
    let mut vt = VtTerminal::new(cols.max(2), rows.max(2));
    let (base16, fg, bg, cur) = termd_theme();
    vt.cursor_color = Some(cur);
    vt.base_cursor = Some(cur);
    vt.set_theme(&base16, fg, bg);
    vt
}

enum SubMsg {
    Subscribed {
        cols: u16,
        rows: u16,
        is_owner: bool,
    },
    Repaint {
        cols: u16,
        rows: u16,
        #[allow(dead_code)]
        flags: u32,
        bytes: Vec<u8>,
    },
    Output(Vec<u8>),
    Size {
        cols: u16,
        rows: u16,
    },
    Role {
        is_owner: bool,
    },
    Exited,
    Disconnected,
}

enum OutgoingCmd {
    Resize { cols: u16, rows: u16 },
    Key {
        spec: String,
        ctrl: bool,
        alt: bool,
        shift: bool,
    },
    RawInput(Vec<u8>),
}

struct SubHandle {
    ns: String,
    id: String,
    role_req: String,
    stop: Arc<AtomicBool>,
    token: u64,
    rx: Receiver<SubMsg>,
    cmd_tx: Sender<OutgoingCmd>,
    is_owner: bool,
    last_sent_grid: (u16, u16),
    alive: bool,
}

impl Drop for SubHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut map) = active_sub_socks().lock() {
            if let Some(sock) = map.remove(&self.token) {
                let _ = sock.shutdown(Shutdown::Both);
            }
        }
    }
}

fn spawn_subscription(
    ns: String,
    id: String,
    role_req: String,
    init_cols: u16,
    init_rows: u16,
) -> SubHandle {
    let (msg_tx, msg_rx) = channel::<SubMsg>();
    let (cmd_tx, cmd_rx) = channel::<OutgoingCmd>();
    let stop = Arc::new(AtomicBool::new(false));
    let token = NEXT_SUB_TOKEN.fetch_add(1, Ordering::Relaxed);

    let stop_bg = Arc::clone(&stop);
    let ns_bg = ns.clone();
    let id_bg = id.clone();
    let role_bg = role_req.clone();

    let _ = std::thread::Builder::new()
        .name("tb-termd-sub".into())
        .spawn(move || {
            sub_worker(
                token,
                ns_bg,
                id_bg,
                role_bg,
                init_cols,
                init_rows,
                stop_bg,
                msg_tx,
                cmd_rx,
            );
        });

    let is_owner = role_req == "own";
    SubHandle {
        ns,
        id,
        role_req,
        stop,
        token,
        rx: msg_rx,
        cmd_tx,
        is_owner,
        last_sent_grid: (init_cols, init_rows),
        alive: true,
    }
}

fn sub_worker(
    token: u64,
    ns: String,
    id: String,
    role_req: String,
    init_cols: u16,
    init_rows: u16,
    stop: Arc<AtomicBool>,
    msg_tx: Sender<SubMsg>,
    cmd_rx: Receiver<OutgoingCmd>,
) {
    let shared_writer: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
    let shared_handle = Arc::new(AtomicU32::new(0));
    let has_key_feat = Arc::new(AtomicBool::new(true));
    let next_req = Arc::new(AtomicU64::new(10));

    // Dedicated writer helper so resize/key commands from UI never wait on socket reads.
    let writer_ref = Arc::clone(&shared_writer);
    let handle_ref = Arc::clone(&shared_handle);
    let key_feat_ref = Arc::clone(&has_key_feat);
    let req_ref = Arc::clone(&next_req);
    let stop_w = Arc::clone(&stop);
    let _ = std::thread::Builder::new()
        .name("tb-termd-w".into())
        .spawn(move || {
            while let Ok(cmd) = cmd_rx.recv() {
                if stop_w.load(Ordering::SeqCst) {
                    break;
                }
                let h = handle_ref.load(Ordering::Acquire);
                if h == 0 {
                    continue;
                }
                let bytes = match cmd {
                    OutgoingCmd::Resize { cols, rows } => {
                        let r = req_ref.fetch_add(1, Ordering::Relaxed);
                        let body = j_obj(vec![
                            ("req", JsonValue::U64(r)),
                            ("op", JsonValue::String("resize".into())),
                            ("handle", JsonValue::U64(h as u64)),
                            ("cols", JsonValue::U64(cols as u64)),
                            ("rows", JsonValue::U64(rows as u64)),
                        ])
                        .serialize_json();
                        encode_frame(FRAME_REQUEST, 0, body.as_bytes())
                    }
                    OutgoingCmd::Key {
                        spec,
                        ctrl,
                        alt,
                        shift,
                    } => {
                        let Ok(pk) = parse_key_spec(&spec, ctrl, alt, shift) else {
                            continue;
                        };
                        if key_feat_ref.load(Ordering::Relaxed) {
                            let r = req_ref.fetch_add(1, Ordering::Relaxed);
                            let mut fields = key_fields(h, &pk);
                            fields.push(("req", JsonValue::U64(r)));
                            fields.push(("op", JsonValue::String("key".into())));
                            let body = j_obj(fields).serialize_json();
                            encode_frame(FRAME_REQUEST, 0, body.as_bytes())
                        } else {
                            encode_frame(FRAME_INPUT, h, &pk.fallback_bytes)
                        }
                    }
                    OutgoingCmd::RawInput(raw) => encode_frame(FRAME_INPUT, h, &raw),
                };
                if let Ok(mut guard) = writer_ref.lock() {
                    if let Some(ref mut w) = *guard {
                        let _ = w.write_all(&bytes);
                    }
                }
            }
        });

    let cur_cols = init_cols.max(2);
    let cur_rows = init_rows.max(2);

    while !stop.load(Ordering::SeqCst) && !is_globally_paused() {
        let (mut writer, mut reader, welcome) = match handshake_ns(&ns, "tb-termd-view/0.1") {
            Ok(ok) => ok,
            Err(_) => {
                let _ = msg_tx.send(SubMsg::Disconnected);
                SignalToUI::set_ui_signal();
                break;
            }
        };
        has_key_feat.store(welcome.has("key"), Ordering::Relaxed);

        if let Ok(ctrl_sock) = writer.try_clone() {
            if let Ok(mut map) = active_sub_socks().lock() {
                map.insert(token, ctrl_sock);
            }
        }
        if stop.load(Ordering::SeqCst) || is_globally_paused() {
            let _ = writer.shutdown(Shutdown::Both);
            break;
        }

        let mut sub_fields = vec![
            ("req", JsonValue::U64(1)),
            ("op", JsonValue::String("subscribe".into())),
            ("id", JsonValue::String(id.clone())),
            ("mode", JsonValue::String("raw".into())),
            ("role", JsonValue::String(role_req.clone())),
            ("flow", JsonValue::String("lossy".into())),
            ("history_rows", JsonValue::U64(800)),
        ];
        if role_req == "own" {
            sub_fields.push(("cols", JsonValue::U64(cur_cols as u64)));
            sub_fields.push(("rows", JsonValue::U64(cur_rows as u64)));
        }
        let sub_json = j_obj(sub_fields).serialize_json();
        if writer
            .write_all(&encode_frame(FRAME_REQUEST, 0, sub_json.as_bytes()))
            .is_err()
        {
            break;
        }

        if let Ok(w_clone) = writer.try_clone() {
            let _ = w_clone.set_write_timeout(Some(Duration::from_millis(100)));
            if let Ok(mut guard) = shared_writer.lock() {
                *guard = Some(w_clone);
            }
        }

        let mut saw_restart = false;
        let mut terminal_closed = false;

        loop {
            if stop.load(Ordering::SeqCst) || is_globally_paused() {
                break;
            }
            let frame = match read_frame(&mut reader, welcome.max_frame) {
                Ok(f) => f,
                Err(_) => break,
            };
            match frame.ftype {
                FRAME_REPLY => {
                    let Ok(s) = std::str::from_utf8(&frame.payload) else {
                        continue;
                    };
                    let Ok(val) = JsonValue::deserialize_json(s) else {
                        continue;
                    };
                    if j_u64(j_get(&val, "req")) == Some(1) {
                        if termd_wire::j_bool(j_get(&val, "ok")) == Some(true) {
                            let h = j_u64(j_get(&val, "handle")).unwrap_or(0) as u32;
                            shared_handle.store(h, Ordering::Release);
                            let cols = j_u64(j_get(&val, "cols")).unwrap_or(cur_cols as u64) as u16;
                            let rows = j_u64(j_get(&val, "rows")).unwrap_or(cur_rows as u64) as u16;
                            let role = j_str(j_get(&val, "role")).unwrap_or("");
                            let _ = msg_tx.send(SubMsg::Subscribed {
                                cols,
                                rows,
                                is_owner: role == "own",
                            });
                            SignalToUI::set_ui_signal();
                        } else {
                            terminal_closed = true;
                            let _ = msg_tx.send(SubMsg::Disconnected);
                            SignalToUI::set_ui_signal();
                            break;
                        }
                    }
                }
                FRAME_REPAINT if frame.payload.len() >= 16 => {
                    let cols = u16::from_le_bytes([frame.payload[8], frame.payload[9]]);
                    let rows = u16::from_le_bytes([frame.payload[10], frame.payload[11]]);
                    let flags = u32::from_le_bytes([
                        frame.payload[12],
                        frame.payload[13],
                        frame.payload[14],
                        frame.payload[15],
                    ]);
                    let bytes = frame.payload[16..].to_vec();
                    let _ = msg_tx.send(SubMsg::Repaint {
                        cols,
                        rows,
                        flags,
                        bytes,
                    });
                    SignalToUI::set_ui_signal();
                }
                FRAME_OUTPUT if frame.payload.len() >= 8 => {
                    let bytes = frame.payload[8..].to_vec();
                    if !bytes.is_empty() {
                        let _ = msg_tx.send(SubMsg::Output(bytes));
                        SignalToUI::set_ui_signal();
                    }
                }
                FRAME_EVENT => {
                    let Ok(s) = std::str::from_utf8(&frame.payload) else {
                        continue;
                    };
                    let Ok(val) = JsonValue::deserialize_json(s) else {
                        continue;
                    };
                    match j_str(j_get(&val, "ev")) {
                        Some("size") => {
                            let cols = j_u64(j_get(&val, "cols")).unwrap_or(80) as u16;
                            let rows = j_u64(j_get(&val, "rows")).unwrap_or(24) as u16;
                            let _ = msg_tx.send(SubMsg::Size { cols, rows });
                            SignalToUI::set_ui_signal();
                        }
                        Some("role") => {
                            let is_owner = j_str(j_get(&val, "role")) == Some("own");
                            let _ = msg_tx.send(SubMsg::Role { is_owner });
                            SignalToUI::set_ui_signal();
                        }
                        Some("exit") => {
                            let _ = msg_tx.send(SubMsg::Exited);
                            SignalToUI::set_ui_signal();
                        }
                        Some("closed") => {
                            terminal_closed = true;
                            let _ = msg_tx.send(SubMsg::Exited);
                            SignalToUI::set_ui_signal();
                            break;
                        }
                        _ => {}
                    }
                }
                FRAME_ERROR => {
                    if let Ok(s) = std::str::from_utf8(&frame.payload) {
                        if let Ok(val) = JsonValue::deserialize_json(s) {
                            let err_obj = j_get(&val, "error").unwrap_or(&val);
                            if j_str(j_get(err_obj, "code")) == Some("restart") {
                                saw_restart = true;
                            }
                        }
                    }
                    break;
                }
                _ => {}
            }
        }

        shared_handle.store(0, Ordering::Release);
        if let Ok(mut guard) = shared_writer.lock() {
            *guard = None;
        }
        if let Ok(mut map) = active_sub_socks().lock() {
            map.remove(&token);
        }

        if stop.load(Ordering::SeqCst) || is_globally_paused() || terminal_closed {
            break;
        }
        if saw_restart {
            std::thread::sleep(Duration::from_millis(80));
            continue;
        }
        std::thread::sleep(Duration::from_millis(150));
        if !socket_path(&ns).exists() {
            let _ = msg_tx.send(SubMsg::Disconnected);
            SignalToUI::set_ui_signal();
            break;
        }
    }

    if let Ok(mut map) = active_sub_socks().lock() {
        map.remove(&token);
    }
}

#[derive(Clone, Copy)]
struct BgRun {
    x: f64,
    y: f64,
    w: f64,
    color: Vec4f,
}

#[derive(Clone, Copy)]
struct GlyphDraw {
    x: f64,
    y: f64,
    ch: char,
    color: Vec4f,
    bold: bool,
    columns: u8,
}

#[derive(Clone, Copy)]
struct DecoDraw {
    x: f64,
    y: f64,
    w: f64,
    color: Vec4f,
    kind: f32,
    strike: bool,
}

#[derive(Script, WidgetRef, WidgetRegister)]
pub struct TermdView {
    #[source]
    source: ScriptObjectRef,
    #[uid]
    uid: WidgetUid,
    #[walk]
    walk: Walk,

    #[live]
    ns: ArcStringMut,
    #[live]
    term_id: ArcStringMut,
    #[live]
    role: ArcStringMut,
    #[live(48usize)]
    cols_preset: usize,
    #[live(12.0)]
    font_size: f64,
    #[live(5.0)]
    pad_x: f64,
    #[live(4.0)]
    pad_y: f64,
    #[live(true)]
    focused: bool,

    #[live]
    draw_bg: DrawQuad,
    #[live]
    draw_text: DrawText,
    #[live]
    bold_text_style: TextStyle,
    #[live]
    draw_cell_bg: DrawTermdCellBg,
    #[live]
    draw_boxes: DrawTermdBox,
    #[live]
    draw_underline: DrawTermdUnderline,
    #[live]
    draw_cursor: DrawTermdCursor,

    #[rust]
    area: Area,
    #[rust]
    rect: Rect,
    #[rust]
    effective_font_size: f64,
    #[rust]
    unit_adv_per_pt: f64,
    #[rust]
    cell_w: f64,
    #[rust]
    cell_h: f64,
    #[rust]
    cell_baseline: f64,
    #[rust]
    glyph_cache: HashMap<(char, bool, u8), Option<CachedGlyph>>,
    #[rust]
    glyph_cache_key: (u64, u64, u64, u64),

    #[rust]
    vt: Option<VtTerminal>,
    #[rust]
    vt_stream: VtStream,
    #[rust]
    exited: bool,
    #[rust]
    sub: Option<SubHandle>,

    #[rust]
    view_offset: usize,
    #[rust]
    drag_last_y: Option<f64>,
    #[rust]
    scroll_px_rem: f64,
}

impl ScriptHook for TermdView {}

impl WidgetNode for TermdView {
    fn widget_uid(&self) -> WidgetUid {
        self.uid
    }

    fn walk(&mut self, _cx: &mut Cx) -> Walk {
        self.walk
    }

    fn area(&self) -> Area {
        self.area
    }

    fn redraw(&mut self, cx: &mut Cx) {
        self.draw_bg.redraw(cx);
    }
}

impl TermdView {
    fn drain_sub(&mut self) -> bool {
        let Some(sub) = self.sub.as_mut() else {
            return false;
        };
        let mut dirty = false;
        while let Ok(msg) = sub.rx.try_recv() {
            match msg {
                SubMsg::Subscribed {
                    cols,
                    rows,
                    is_owner,
                } => {
                    sub.is_owner = is_owner;
                    sub.last_sent_grid = (cols, rows);
                    if let Some(vt) = self.vt.as_mut() {
                        vt.resize(cols as usize, rows as usize);
                        vt.outbound.clear();
                        vt.events.clear();
                    } else {
                        self.vt = Some(make_themed_vt(cols as usize, rows as usize));
                    }
                    dirty = true;
                }
                SubMsg::Repaint {
                    cols,
                    rows,
                    flags: _,
                    bytes,
                } => {
                    let mut fresh = make_themed_vt(cols as usize, rows as usize);
                    self.vt_stream = VtStream::new();
                    self.vt_stream.process(&bytes, &mut fresh);
                    fresh.outbound.clear();
                    fresh.events.clear();
                    let max_sb = fresh.screen().scrollback.len();
                    self.view_offset = self.view_offset.min(max_sb);
                    self.vt = Some(fresh);
                    self.exited = false;
                    dirty = true;
                }
                SubMsg::Output(bytes) => {
                    let vt = self
                        .vt
                        .get_or_insert_with(|| make_themed_vt(48, 24));
                    self.vt_stream.process(&bytes, vt);
                    vt.outbound.clear();
                    vt.events.clear();
                    let max_sb = vt.screen().scrollback.len();
                    self.view_offset = self.view_offset.min(max_sb);
                    dirty = true;
                }
                SubMsg::Size { cols, rows } => {
                    if let Some(vt) = self.vt.as_mut() {
                        vt.resize(cols as usize, rows as usize);
                        vt.outbound.clear();
                        vt.events.clear();
                        dirty = true;
                    }
                }
                SubMsg::Role { is_owner } => {
                    sub.is_owner = is_owner;
                }
                SubMsg::Exited => {
                    self.exited = true;
                    dirty = true;
                }
                SubMsg::Disconnected => {
                    sub.alive = false;
                    dirty = true;
                }
            }
        }
        dirty
    }

    fn refresh_metrics(&mut self, cx: &mut Cx2d) {
        if self.unit_adv_per_pt <= 0.0 {
            let saved = self.draw_text.text_style.font_size;
            self.draw_text.text_style.font_size = 10.0;
            if let Some(run) = self.draw_text.prepare_single_line_run(cx, "M") {
                if let Some(g) = run.glyphs.first() {
                    if g.advance_in_lpxs > 0.0 {
                        self.unit_adv_per_pt = (g.advance_in_lpxs as f64) / 10.0;
                    }
                }
            }
            self.draw_text.text_style.font_size = saved;
            if self.unit_adv_per_pt <= 0.0 {
                self.unit_adv_per_pt = 0.60;
            }
        }

        let avail_w = (self.rect.size.x - self.pad_x * 2.0).max(48.0);
        let font_size = if self.cols_preset > 0 {
            let target_cell_w = avail_w / (self.cols_preset as f64);
            (target_cell_w / self.unit_adv_per_pt).clamp(5.5, 22.0)
        } else {
            self.font_size.clamp(6.0, 24.0)
        };
        self.effective_font_size = font_size;
        self.draw_text.text_style.font_size = font_size as f32;

        let epoch = cx.fonts.borrow().rasterizer().borrow().epoch();
        let families = cx.fonts.borrow().font_family_generation();
        let key = (
            font_size.to_bits(),
            cx.current_dpi_factor().to_bits(),
            epoch,
            families,
        );
        if key != self.glyph_cache_key {
            self.glyph_cache.clear();
            self.glyph_cache_key = key;
        }

        if let Some(run) = self.draw_text.prepare_single_line_run(cx, "M") {
            if let Some(g) = run.glyphs.first() {
                self.cell_w = g.advance_in_lpxs as f64;
                let glyph_h = (run.ascender_in_lpxs - run.descender_in_lpxs) as f64;
                self.cell_h = glyph_h * self.draw_text.text_style.line_spacing as f64;
                self.cell_baseline = (self.cell_h - glyph_h) * 0.5 + run.ascender_in_lpxs as f64;
            }
        }
        if self.cell_w <= 0.0 {
            self.cell_w = font_size * 0.60;
        }
        if self.cell_h <= 0.0 {
            self.cell_h = font_size * 1.32;
        }
    }

    fn desired_grid(&self) -> (u16, u16) {
        let avail_w = (self.rect.size.x - self.pad_x * 2.0).max(16.0);
        let avail_h = (self.rect.size.y - self.pad_y * 2.0).max(16.0);
        let cols = if self.cols_preset > 0 {
            self.cols_preset.clamp(12, 300) as u16
        } else {
            ((avail_w / self.cell_w).floor().max(8.0) as usize).min(300) as u16
        };
        let rows = ((avail_h / self.cell_h).floor().max(4.0) as usize).min(200) as u16;
        (cols, rows)
    }

    fn sync_subscription(&mut self, want_cols: u16, want_rows: u16) {
        if is_globally_paused() {
            if self.sub.is_some() {
                self.sub = None;
            }
            return;
        }

        let ns = if self.ns.as_ref().trim().is_empty() {
            "default"
        } else {
            self.ns.as_ref().trim()
        };
        let id = self.term_id.as_ref().trim();
        let role = if self.role.as_ref().trim().is_empty() {
            "own"
        } else {
            self.role.as_ref().trim()
        };

        if id.is_empty() {
            self.sub = None;
            self.vt = None;
            return;
        }

        let needs_new = match &self.sub {
            None => true,
            Some(s) => !s.alive || s.ns != ns || s.id != id || s.role_req != role,
        };
        if needs_new {
            self.view_offset = 0;
            self.exited = false;
            self.vt = Some(make_themed_vt(want_cols as usize, want_rows as usize));
            self.vt_stream = VtStream::new();
            self.sub = Some(spawn_subscription(
                ns.to_string(),
                id.to_string(),
                role.to_string(),
                want_cols,
                want_rows,
            ));
            return;
        }

        if let Some(sub) = self.sub.as_mut() {
            if sub.is_owner && sub.last_sent_grid != (want_cols, want_rows) {
                sub.last_sent_grid = (want_cols, want_rows);
                let _ = sub.cmd_tx.send(OutgoingCmd::Resize {
                    cols: want_cols,
                    rows: want_rows,
                });
            }
        }
    }

    fn cached_glyph(
        &mut self,
        cx: &mut Cx2d,
        ch: char,
        bold: bool,
        columns: u8,
    ) -> Option<CachedGlyph> {
        if let Some(hit) = self.glyph_cache.get(&(ch, bold, columns)) {
            return *hit;
        }
        let mut buf = [0u8; 4];
        let text: &str = ch.encode_utf8(&mut buf);
        let style = self.draw_text.text_style.clone();
        if bold {
            self.draw_text.text_style.font_family = self.bold_text_style.font_family.clone();
            self.draw_text.text_style.font_size = self.effective_font_size as f32;
        }
        let prepared = self.draw_text.prepare_single_line_run(cx, text);
        self.draw_text.text_style = style;

        let cached = prepared.and_then(|run| {
            run.glyphs.first().map(|g| {
                let available = self.cell_w as f32 * columns.max(1) as f32;
                let advance = run.width_in_lpxs;
                let fit = if advance > available * 1.05 {
                    available / advance
                } else {
                    1.0
                };
                let font_size = g.font_size_in_lpxs;
                let center_y = g.glyph.center_y_in_ems() * font_size;
                CachedGlyph {
                    glyph: g.glyph,
                    font_size_in_lpxs: font_size * fit,
                    x_offset_in_lpxs: (g.pen_x_in_lpxs + g.offset_x_in_lpxs) * fit,
                    y_offset_in_lpxs: center_y * (1.0 - fit),
                }
            })
        });
        if cached.is_some_and(|g| !g.glyph.is_pending()) {
            self.glyph_cache.insert((ch, bold, columns), cached);
        }
        cached
    }

    fn draw_box_glyph(&mut self, cx: &mut Cx2d, ch: char, x: f64, y: f64, color: Vec4f) -> bool {
        let (arms, heavy, rounded) = match ch {
            '─' => (3, false, false),
            '━' => (3, true, false),
            '│' => (12, false, false),
            '┃' => (12, true, false),
            '┌' => (10, false, false),
            '┏' => (10, true, false),
            '╭' => (10, false, true),
            '┐' => (9, false, false),
            '┓' => (9, true, false),
            '╮' => (9, false, true),
            '└' => (6, false, false),
            '┗' => (6, true, false),
            '╰' => (6, false, true),
            '┘' => (5, false, false),
            '┛' => (5, true, false),
            '╯' => (5, false, true),
            '├' => (14, false, false),
            '┣' => (14, true, false),
            '┤' => (13, false, false),
            '┫' => (13, true, false),
            '┬' => (11, false, false),
            '┳' => (11, true, false),
            '┴' => (7, false, false),
            '┻' => (7, true, false),
            '┼' => (15, false, false),
            '╋' => (15, true, false),
            _ => return false,
        };
        let dpi = cx.current_dpi_factor().max(0.1);
        let pixels = ((self.effective_font_size * 0.07 * dpi).round().max(1.0))
            * if heavy { 2.0 } else { 1.0 };
        let thickness = pixels / dpi;
        let snap = |val: f64| ((val * dpi - pixels * 0.5).round() + pixels * 0.5) / dpi;
        let (mx, my) = (snap(x + self.cell_w * 0.5), snap(y + self.cell_h * 0.5));
        self.draw_boxes.color = color;
        self.draw_boxes.arms = vec4(
            (arms & 1 != 0) as u8 as f32,
            (arms & 2 != 0) as u8 as f32,
            (arms & 4 != 0) as u8 as f32,
            (arms & 8 != 0) as u8 as f32,
        );
        self.draw_boxes.center = vec2((mx - x) as f32, (my - y) as f32);
        self.draw_boxes.corner = vec2(
            if arms & 2 != 0 { 1.0 } else { -1.0 },
            if arms & 8 != 0 { 1.0 } else { -1.0 },
        );
        self.draw_boxes.thickness = thickness as f32;
        self.draw_boxes.radius = if rounded {
            (self.cell_w.min(self.cell_h) * 0.35).max(thickness) as f32
        } else {
            0.0
        };
        self.draw_boxes.pixel_size = (1.0 / dpi) as f32;
        self.draw_boxes.draw_abs(
            cx,
            Rect {
                pos: dvec2(x, y),
                size: dvec2(self.cell_w, self.cell_h),
            },
        );
        true
    }

    fn rgb_to_vec4(rgb: Rgb, alpha: f32) -> Vec4f {
        vec4(
            rgb.r as f32 / 255.0,
            rgb.g as f32 / 255.0,
            rgb.b as f32 / 255.0,
            alpha,
        )
    }

    fn resolve_colors(
        vt: &VtTerminal,
        style: &makepad_app_terminal::term::style::Style,
        global_inverse: bool,
    ) -> (Option<Vec4f>, Option<Vec4f>) {
        let mut fg_color = style.fg_color;
        if style.flags.has(StyleFlags::BOLD) {
            if let StyleColor::Palette(i) = fg_color {
                if i < 8 {
                    fg_color = StyleColor::Palette(i + 8);
                }
            }
        }
        let mut fg = fg_color.resolve(&vt.palette, vt.default_fg);
        let mut bg = style.bg_color.resolve_opt(&vt.palette);
        if style.flags.has(StyleFlags::INVERSE) != global_inverse {
            let old_fg = fg;
            fg = bg.unwrap_or(vt.default_bg);
            bg = Some(old_fg);
        }
        if style.flags.has(StyleFlags::INVISIBLE) {
            return (None, bg.map(|b| Self::rgb_to_vec4(b, 1.0)));
        }
        let alpha = if style.flags.has(StyleFlags::FAINT) {
            0.55
        } else {
            1.0
        };
        (
            Some(Self::rgb_to_vec4(fg, alpha)),
            bg.map(|b| Self::rgb_to_vec4(b, 1.0)),
        )
    }

    fn scroll_by_pixels(&mut self, cx: &mut Cx, dy: f64) {
        let Some(vt) = self.vt.as_ref() else {
            return;
        };
        if self.cell_h <= 1.0 {
            return;
        }
        let max_sb = vt.screen().scrollback.len();
        if max_sb == 0 {
            return;
        }
        // Dragging finger DOWN (`dy > 0`) scrolls UP into older scrollback (`view_offset` increases).
        self.scroll_px_rem += dy;
        let steps = (self.scroll_px_rem / self.cell_h).trunc() as isize;
        if steps != 0 {
            self.scroll_px_rem -= (steps as f64) * self.cell_h;
            let next = (self.view_offset as isize + steps).clamp(0, max_sb as isize) as usize;
            if next != self.view_offset {
                self.view_offset = next;
                self.redraw(cx);
            }
        }
    }
}

impl Widget for TermdView {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, _scope: &mut Scope) {
        if let Event::Signal = event {
            if is_globally_paused() {
                if self.sub.is_some() {
                    self.sub = None;
                }
                return;
            }
            if self.drain_sub() {
                self.redraw(cx);
            }
        }

        // Direct touch / mouse scrollback drag + tap-to-focus + snap-to-live badge tap.
        match event {
            Event::TouchUpdate(tu) => {
                if let Some(t) = tu.touches.first() {
                    if self.rect.contains(t.abs) || self.drag_last_y.is_some() {
                        match t.state {
                            TouchState::Start => {
                                if self.view_offset > 0
                                    && t.abs.x >= self.rect.pos.x + self.rect.size.x - 72.0
                                    && t.abs.y >= self.rect.pos.y + self.rect.size.y - 38.0
                                {
                                    self.view_offset = 0;
                                    self.scroll_px_rem = 0.0;
                                    self.redraw(cx);
                                } else {
                                    self.drag_last_y = Some(t.abs.y);
                                    self.scroll_px_rem = 0.0;
                                    cx.set_key_focus(self.area);
                                }
                            }
                            TouchState::Move => {
                                if let Some(prev_y) = self.drag_last_y {
                                    let dy = t.abs.y - prev_y;
                                    self.drag_last_y = Some(t.abs.y);
                                    self.scroll_by_pixels(cx, dy);
                                }
                            }
                            TouchState::Stop | TouchState::Cancel => {
                                self.drag_last_y = None;
                                self.scroll_px_rem = 0.0;
                            }
                            _ => {}
                        }
                    }
                }
            }
            Event::Scroll(se) if self.rect.contains(se.abs) => {
                self.scroll_by_pixels(cx, -se.scroll.y);
                se.handled_y.set(true);
            }
            _ => {}
        }

        match event.hits(cx, self.area) {
            Hit::FingerDown(fe) if fe.is_primary_hit() => {
                cx.set_key_focus(self.area);
                if self.view_offset > 0
                    && fe.abs.x >= self.rect.pos.x + self.rect.size.x - 72.0
                    && fe.abs.y >= self.rect.pos.y + self.rect.size.y - 38.0
                {
                    self.view_offset = 0;
                    self.scroll_px_rem = 0.0;
                    self.redraw(cx);
                } else {
                    self.drag_last_y = Some(fe.abs.y);
                    self.scroll_px_rem = 0.0;
                }
            }
            Hit::FingerMove(fe) => {
                if let Some(prev_y) = self.drag_last_y {
                    let dy = fe.abs.y - prev_y;
                    self.drag_last_y = Some(fe.abs.y);
                    self.scroll_by_pixels(cx, dy);
                }
            }
            Hit::FingerUp(_) => {
                self.drag_last_y = None;
                self.scroll_px_rem = 0.0;
            }
            Hit::KeyDown(ke) => {
                let spec_opt = match ke.key_code {
                    KeyCode::ReturnKey | KeyCode::NumpadEnter => Some("Enter"),
                    KeyCode::Escape => Some("Escape"),
                    KeyCode::Tab => Some("Tab"),
                    KeyCode::Backspace => Some("Backspace"),
                    KeyCode::Delete => Some("Delete"),
                    KeyCode::ArrowUp => Some("ArrowUp"),
                    KeyCode::ArrowDown => Some("ArrowDown"),
                    KeyCode::ArrowLeft => Some("ArrowLeft"),
                    KeyCode::ArrowRight => Some("ArrowRight"),
                    KeyCode::Home => Some("Home"),
                    KeyCode::End => Some("End"),
                    KeyCode::PageUp => Some("PageUp"),
                    KeyCode::PageDown => Some("PageDown"),
                    KeyCode::KeyC if ke.modifiers.control => Some("c"),
                    KeyCode::KeyD if ke.modifiers.control => Some("d"),
                    KeyCode::KeyZ if ke.modifiers.control => Some("z"),
                    KeyCode::KeyL if ke.modifiers.control => Some("l"),
                    KeyCode::KeyU if ke.modifiers.control => Some("u"),
                    KeyCode::KeyW if ke.modifiers.control => Some("w"),
                    KeyCode::KeyA if ke.modifiers.control => Some("a"),
                    KeyCode::KeyE if ke.modifiers.control => Some("e"),
                    _ => None,
                };
                if let Some(spec) = spec_opt {
                    self.view_offset = 0;
                    if let Some(sub) = self.sub.as_ref() {
                        let _ = sub.cmd_tx.send(OutgoingCmd::Key {
                            spec: spec.into(),
                            ctrl: ke.modifiers.control,
                            alt: ke.modifiers.alt,
                            shift: ke.modifiers.shift,
                        });
                    }
                }
            }
            Hit::TextInput(ti) if !ti.input.is_empty() => {
                self.view_offset = 0;
                if let Some(sub) = self.sub.as_ref() {
                    let _ = sub
                        .cmd_tx
                        .send(OutgoingCmd::RawInput(ti.input.as_bytes().to_vec()));
                }
            }
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let _ = self.drain_sub();

        let rect = cx.walk_turtle(walk);
        self.rect = rect;
        self.refresh_metrics(cx);

        let (want_cols, want_rows) = self.desired_grid();
        self.sync_subscription(want_cols, want_rows);
        let _ = self.drain_sub();

        let default_bg = self
            .vt
            .as_ref()
            .map_or(Rgb::new(0x13, 0x17, 0x22), |v| v.default_bg);
        let bg_vec = Self::rgb_to_vec4(default_bg, 1.0);
        self.draw_bg
            .draw_vars
            .set_uniform(cx, live_id!(color), &[bg_vec.x, bg_vec.y, bg_vec.z, bg_vec.w]);
        self.draw_bg.draw_abs(cx, rect);
        self.area = self.draw_bg.area();

        let Some(vt_ref) = self.vt.as_ref() else {
            return DrawStep::done();
        };

        let cell_w = self.cell_w;
        let cell_h = self.cell_h;
        let origin_x = rect.pos.x + self.pad_x;
        let origin_y = rect.pos.y + self.pad_y;

        let screen = vt_ref.screen();
        let cols = screen.cols.min(want_cols as usize + 8);
        let rows = screen.rows.min(want_rows as usize + 2);
        let total = screen.total_rows();
        let top_virtual = screen.scrollback.len().saturating_sub(self.view_offset);
        let global_inverse = vt_ref.modes.get(Mode::ReverseColors);
        let cursor_visible = vt_ref.modes.get(Mode::CursorVisible);
        let cursor_style = vt_ref.cursor_style;
        let cursor_color = vt_ref.cursor_color.unwrap_or(vt_ref.default_fg);

        let mut bg_runs: Vec<BgRun> = Vec::new();
        let mut glyphs: Vec<GlyphDraw> = Vec::with_capacity(rows * cols / 3);
        let mut decos: Vec<DecoDraw> = Vec::new();

        for vis_row in 0..rows {
            let virt = top_virtual + vis_row;
            if virt >= total {
                break;
            }
            let Some(row) = screen.row_virtual(virt) else {
                continue;
            };
            let y = origin_y + vis_row as f64 * cell_h;
            let mut run: Option<(usize, usize, Vec4f)> = None;

            for col in 0..cols {
                let cell = row.cell(col);
                let (fg, bg) = match cell {
                    Some(c) => Self::resolve_colors(vt_ref, &c.style, global_inverse),
                    None => (None, None),
                };

                match (&mut run, bg) {
                    (Some((_, end, color)), Some(c)) if *color == c && *end == col => {
                        *end = col + 1;
                    }
                    (prev, next) => {
                        if let Some((start, end, color)) = prev.take() {
                            bg_runs.push(BgRun {
                                x: origin_x + start as f64 * cell_w,
                                y,
                                w: (end - start) as f64 * cell_w,
                                color,
                            });
                        }
                        if let Some(c) = next {
                            *prev = Some((col, col + 1, c));
                        }
                    }
                }

                let Some(cell) = cell else { continue };
                let Some(fg) = fg else { continue };

                let underline = cell.style.flags.underline();
                let strike = cell.style.flags.has(StyleFlags::STRIKETHROUGH);
                if underline != Underline::None || strike {
                    let ul_color = cell
                        .style
                        .underline_color
                        .resolve_opt(&vt_ref.palette)
                        .map(|c| Self::rgb_to_vec4(c, 1.0))
                        .unwrap_or(fg);
                    let width = cell.content.width().max(1) as f64 * cell_w;
                    if underline != Underline::None {
                        decos.push(DecoDraw {
                            x: origin_x + col as f64 * cell_w,
                            y,
                            w: width,
                            color: ul_color,
                            kind: underline as u8 as f32,
                            strike: false,
                        });
                    }
                    if strike {
                        decos.push(DecoDraw {
                            x: origin_x + col as f64 * cell_w,
                            y,
                            w: width,
                            color: fg,
                            kind: 1.0,
                            strike: true,
                        });
                    }
                }

                match &cell.content {
                    CellContent::Char(c) | CellContent::WideChar(c) => {
                        if *c != ' ' {
                            glyphs.push(GlyphDraw {
                                x: origin_x + col as f64 * cell_w,
                                y,
                                ch: *c,
                                color: fg,
                                bold: cell.style.flags.has(StyleFlags::BOLD),
                                columns: cell.content.width(),
                            });
                        }
                    }
                    CellContent::Cluster(cluster) => {
                        for c in &cluster.cps {
                            glyphs.push(GlyphDraw {
                                x: origin_x + col as f64 * cell_w,
                                y,
                                ch: *c,
                                color: fg,
                                bold: cell.style.flags.has(StyleFlags::BOLD),
                                columns: cell.content.width(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            if let Some((start, end, color)) = run.take() {
                bg_runs.push(BgRun {
                    x: origin_x + start as f64 * cell_w,
                    y,
                    w: (end - start) as f64 * cell_w,
                    color,
                });
            }
        }

        let cursor = if self.view_offset == 0 && cursor_visible && !self.exited && cols > 0 {
            Some((screen.cursor.x.min(cols - 1), screen.cursor.y.min(rows.saturating_sub(1))))
        } else {
            None
        };

        // Layer 1: merged cell background runs + scrollback indicator (1 instanced draw call).
        self.draw_cell_bg.new_draw_call(cx);
        for r in &bg_runs {
            self.draw_cell_bg.color = r.color;
            self.draw_cell_bg.draw_abs(
                cx,
                Rect {
                    pos: dvec2(r.x, r.y),
                    size: dvec2(r.w, cell_h),
                },
            );
        }
        if self.view_offset > 0 {
            let sb = self
                .vt
                .as_ref()
                .map_or(1, |v| v.screen().scrollback.len().max(1));
            let frac = self.view_offset as f64 / sb as f64;
            let h = (rect.size.y * 0.18).max(20.0);
            let track = (rect.size.y - h).max(0.0);
            let y = rect.pos.y + track * (1.0 - frac);
            self.draw_cell_bg.color = vec4(0.48, 0.64, 0.97, 0.45);
            self.draw_cell_bg.draw_abs(
                cx,
                Rect {
                    pos: dvec2(rect.pos.x + rect.size.x - 4.0, y),
                    size: dvec2(3.0, h),
                },
            );
            // Snap-to-live corner badge background
            self.draw_cell_bg.color = vec4(0.14, 0.24, 0.40, 0.92);
            self.draw_cell_bg.draw_abs(
                cx,
                Rect {
                    pos: dvec2(rect.pos.x + rect.size.x - 64.0, rect.pos.y + rect.size.y - 30.0),
                    size: dvec2(58.0, 24.0),
                },
            );
        }

        // Layer 2: cursor.
        let has_focus = self.focused || cx.has_key_focus(self.area);
        if let Some((ccol, crow)) = cursor {
            let x = origin_x + ccol as f64 * cell_w;
            let y = origin_y + crow as f64 * cell_h;
            let cvec = Self::rgb_to_vec4(cursor_color, 1.0);
            self.draw_cursor.new_draw_call(cx);
            self.draw_cursor.color = cvec;
            self.draw_cursor.hollow = if has_focus { 0.0 } else { 1.0 };
            let crect = match cursor_style {
                CursorStyle::BlinkingBar | CursorStyle::SteadyBar => {
                    self.draw_cursor.hollow = 0.0;
                    Rect {
                        pos: dvec2(x, y),
                        size: dvec2((cell_w * 0.16).max(1.5), cell_h),
                    }
                }
                CursorStyle::BlinkingUnderline | CursorStyle::SteadyUnderline => {
                    self.draw_cursor.hollow = 0.0;
                    Rect {
                        pos: dvec2(x, y + cell_h - (cell_h * 0.14).max(2.0)),
                        size: dvec2(cell_w, (cell_h * 0.14).max(2.0)),
                    }
                }
                _ => Rect {
                    pos: dvec2(x, y),
                    size: dvec2(cell_w, cell_h),
                },
            };
            self.draw_cursor.draw_abs(cx, crect);
        }

        // Layer 3: box-drawing + batched glyph atlas instances.
        self.draw_boxes.new_draw_call(cx);
        self.draw_text.new_draw_call(cx);
        self.draw_text.begin_many_instances(cx);
        let baseline = self.cell_baseline;
        for g in &glyphs {
            let mut color = g.color;
            if let Some((ccol, crow)) = cursor {
                let gx = ((g.x - origin_x) / cell_w).round() as usize;
                let gy = ((g.y - origin_y) / cell_h).round() as usize;
                if gx == ccol
                    && gy == crow
                    && has_focus
                    && matches!(
                        cursor_style,
                        CursorStyle::Default
                            | CursorStyle::BlinkingBlock
                            | CursorStyle::SteadyBlock
                    )
                {
                    color = Self::rgb_to_vec4(default_bg, 1.0);
                }
            }
            if self.draw_box_glyph(cx, g.ch, g.x, g.y, color) {
                continue;
            }
            if let Some(glyph) = self.cached_glyph(cx, g.ch, g.bold, g.columns) {
                let pt = Point::new(
                    (g.x + glyph.x_offset_in_lpxs as f64) as f32,
                    (g.y + baseline) as f32 + glyph.y_offset_in_lpxs,
                );
                self.draw_text.draw_prepared_glyph_abs(
                    cx,
                    pt,
                    glyph.font_size_in_lpxs,
                    glyph.glyph,
                    color,
                );
            }
        }
        if self.view_offset > 0 {
            let badge_txt = "v Live";
            for (i, ch) in badge_txt.chars().enumerate() {
                if ch == ' ' {
                    continue;
                }
                if let Some(glyph) = self.cached_glyph(cx, ch, true, 1) {
                    let bx = rect.pos.x + rect.size.x - 56.0 + (i as f64) * cell_w.max(6.5);
                    let by = rect.pos.y + rect.size.y - 29.0 + baseline;
                    self.draw_text.draw_prepared_glyph_abs(
                        cx,
                        Point::new(
                            (bx + glyph.x_offset_in_lpxs as f64) as f32,
                            by as f32 + glyph.y_offset_in_lpxs,
                        ),
                        glyph.font_size_in_lpxs,
                        glyph.glyph,
                        vec4(0.88, 0.94, 1.0, 1.0),
                    );
                }
            }
        }
        self.draw_text.end_many_instances(cx);

        // Layer 4: underlines / strikethroughs (only when non-empty).
        if !decos.is_empty() {
            self.draw_underline.new_draw_call(cx);
            for d in &decos {
                self.draw_underline.color = d.color;
                self.draw_underline.kind = d.kind;
                let (dy, dh) = if d.strike {
                    (d.y + cell_h * 0.5 - 1.0, 1.5)
                } else if d.kind >= 1.5 && d.kind < 3.5 {
                    (d.y + cell_h - 4.0, 4.0)
                } else {
                    (d.y + cell_h - 2.0, 1.5)
                };
                self.draw_underline.draw_abs(
                    cx,
                    Rect {
                        pos: dvec2(d.x, dy),
                        size: dvec2(d.w, dh),
                    },
                );
            }
        }

        DrawStep::done()
    }
}

/// Installed into every Splash isolate (`TermdView`).
pub fn isolate_mod(vm: &mut ScriptVm) {
    self::script_mod(vm);
    script_eval!(vm, {
        mod.prelude.widgets = {
            ..mod.prelude.widgets,
            TermdView: mod.widgets.TermdView,
        }
    });
}
