//! tb-screen-power: lightweight Wayland screen-power & idle daemon for `phoc` / `wlroots`.
//!
//! Replaces `phosh` + `gnome-session` + `gsd-*` (`167 MB` RSS) with a `< 1 MB` single-thread
//! `poll()` daemon speaking pure Wayland wire protocol over `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`:
//!   - Binds `wl_output`, `wl_seat`, `zwlr_output_power_manager_v1`, and `org_kde_kwin_idle`.
//!   - Watches `/dev/input/event0` (`TB_SCREEN_INPUT`) for short `KEY_POWER` (`116`, `< 800 ms`)
//!     presses to toggle screen power (`ON <-> OFF`).
//!   - Reads `/userdata/tinybot/etc/screen-timeout` (`TB_SCREEN_TIMEOUT_FILE`, seconds, default
//!     `60`, `0` disables; live-reloaded via `inotify`) and configures `org_kde_kwin_idle_timeout`.
//!   - Serves `on`, `off`, `toggle`, and `status` on `$XDG_RUNTIME_DIR/tb-screen.sock` (mode 0600)
//!     for `tbui screen <on|off|toggle|status>`.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputEvent {
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

extern "C" {
    fn poll(fds: *mut PollFd, nfds: usize, timeout: i32) -> i32;
    fn inotify_init1(flags: i32) -> i32;
    fn inotify_add_watch(fd: i32, path: *const std::ffi::c_char, mask: u32) -> i32;
}

const POLLIN: i16 = 0x0001;
const POLLERR: i16 = 0x0008;
const POLLHUP: i16 = 0x0010;
const O_NONBLOCK: i32 = 0o4000;
const O_CLOEXEC: i32 = 0o2000000;

const EV_KEY: u16 = 1;
const KEY_POWER: u16 = 116;

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn resolve_wayland_socket() -> Result<PathBuf, String> {
    let xdg = env_opt("XDG_RUNTIME_DIR").unwrap_or_else(|| "/run/user/8000".into());
    if let Some(wd) = env_opt("WAYLAND_DISPLAY") {
        let p = Path::new(&wd);
        return Ok(if p.is_absolute() { p.to_path_buf() } else { PathBuf::from(&xdg).join(wd) });
    }
    let mut cands: Vec<PathBuf> = std::fs::read_dir(&xdg)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
        })
        .collect();
    cands.sort();
    cands.into_iter().next().ok_or_else(|| format!("no wayland-* socket in {xdg}"))
}

fn resolve_screen_sock() -> String {
    if let Some(p) = env_opt("TB_SCREEN_SOCK") {
        return p;
    }
    let xdg = env_opt("XDG_RUNTIME_DIR").unwrap_or_else(|| "/run/user/8000".into());
    format!("{xdg}/tb-screen.sock")
}

fn read_timeout_secs(path: &str) -> u32 {
    match std::fs::read_to_string(path) {
        Ok(s) => s.trim().parse::<u32>().unwrap_or(60),
        Err(_) => 60,
    }
}

/// Encodes a Wayland string (`u32 len_including_nul` + bytes + `\0` + 4-byte padding).
fn push_wl_str(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let len_with_nul = (bytes.len() + 1) as u32;
    buf.extend_from_slice(&len_with_nul.to_ne_bytes());
    buf.extend_from_slice(bytes);
    buf.push(0);
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
}

fn parse_wl_str(payload: &[u8], offset: usize) -> Option<(String, usize)> {
    if offset + 4 > payload.len() {
        return None;
    }
    let len = u32::from_ne_bytes(payload[offset..offset + 4].try_into().ok()?) as usize;
    let padded = (len + 3) & !3;
    let start = offset + 4;
    if len == 0 || start + padded > payload.len() {
        return Some((String::new(), start + padded.min(payload.len().saturating_sub(start))));
    }
    let raw = &payload[start..start + len - 1];
    Some((String::from_utf8_lossy(raw).into_owned(), start + padded))
}

struct WaylandConn {
    sock: UnixStream,
    rx_buf: Vec<u8>,
    next_id: u32,
    registry_id: u32,
    init_sync_id: u32,
    synced: bool,
    outputs: Vec<(u32, u32)>,          // (global_name, wl_output_id)
    power_objs: Vec<(u32, u32)>,       // (wl_output_id, zwlr_output_power_v1_id)
    seat_id: Option<u32>,
    power_mgr_id: Option<u32>,
    idle_mgr_id: Option<u32>,
    idle_timeout_id: Option<u32>,
    configured_timeout_s: Option<u32>,
    compositor_power_on: Option<bool>,
}

impl WaylandConn {
    fn connect() -> Result<Self, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (path, sock) = loop {
            match resolve_wayland_socket().and_then(|p| match UnixStream::connect(&p) {
                Ok(s) => Ok((p, s)),
                Err(e) => Err(format!("{}: {e}", p.display())),
            }) {
                Ok(v) => break v,
                Err(e) if Instant::now() < deadline => {
                    let _ = e;
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(e) => return Err(format!("connect Wayland: {e}")),
            }
        };
        sock.set_nonblocking(true).map_err(|e| format!("nonblock: {e}"))?;
        eprintln!("tb-screen-power: connected to {}", path.display());
        let mut me = Self {
            sock,
            rx_buf: Vec::with_capacity(4096),
            next_id: 2,
            registry_id: 0,
            init_sync_id: 0,
            synced: false,
            outputs: Vec::new(),
            power_objs: Vec::new(),
            seat_id: None,
            power_mgr_id: None,
            idle_mgr_id: None,
            idle_timeout_id: None,
            configured_timeout_s: None,
            compositor_power_on: None,
        };
        me.registry_id = me.alloc_id();
        me.send_words(1, 1, &[me.registry_id])?; // wl_display.get_registry
        me.init_sync_id = me.alloc_id();
        me.send_words(1, 0, &[me.init_sync_id])?; // wl_display.sync
        Ok(me)
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send_raw(&mut self, obj_id: u32, opcode: u16, payload: &[u8]) -> Result<(), String> {
        let total = 8 + payload.len();
        let mut msg = Vec::with_capacity(total);
        msg.extend_from_slice(&obj_id.to_ne_bytes());
        let hdr = ((total as u32) << 16) | (opcode as u32);
        msg.extend_from_slice(&hdr.to_ne_bytes());
        msg.extend_from_slice(payload);
        self.sock.write_all(&msg).map_err(|e| format!("wayland write: {e}"))
    }

    fn send_words(&mut self, obj_id: u32, opcode: u16, args: &[u32]) -> Result<(), String> {
        let mut payload = Vec::with_capacity(args.len() * 4);
        for w in args {
            payload.extend_from_slice(&w.to_ne_bytes());
        }
        self.send_raw(obj_id, opcode, &payload)
    }

    /// `wl_registry::bind(name: uint, iface: string, version: uint, new_id: uint)`
    fn bind_global(&mut self, global_name: u32, iface: &str, version: u32) -> Result<u32, String> {
        let id = self.alloc_id();
        let mut payload = Vec::with_capacity(24 + iface.len());
        payload.extend_from_slice(&global_name.to_ne_bytes());
        push_wl_str(&mut payload, iface);
        payload.extend_from_slice(&version.to_ne_bytes());
        payload.extend_from_slice(&id.to_ne_bytes());
        self.send_raw(self.registry_id, 0, &payload)?;
        Ok(id)
    }

    fn ensure_output_powers(&mut self) -> Result<(), String> {
        let Some(mgr) = self.power_mgr_id else { return Ok(()) };
        let outs: Vec<u32> = self.outputs.iter().map(|(_, oid)| *oid).collect();
        for oid in outs {
            if self.power_objs.iter().any(|(o, _)| *o == oid) {
                continue;
            }
            let pid = self.alloc_id();
            // zwlr_output_power_manager_v1::get_output_power(id: new_id, output: object) -> opcode 0
            self.send_words(mgr, 0, &[pid, oid])?;
            self.power_objs.push((oid, pid));
        }
        Ok(())
    }

    fn apply_idle_timeout(&mut self, timeout_secs: u32) -> Result<(), String> {
        if !self.synced || self.configured_timeout_s == Some(timeout_secs) {
            return Ok(());
        }
        let (Some(idle_mgr), Some(seat)) = (self.idle_mgr_id, self.seat_id) else {
            return Ok(());
        };
        if let Some(old_id) = self.idle_timeout_id.take() {
            // org_kde_kwin_idle_timeout::release() -> opcode 0
            let _ = self.send_words(old_id, 0, &[]);
        }
        if timeout_secs > 0 {
            let tid = self.alloc_id();
            let ms = timeout_secs.saturating_mul(1000);
            // org_kde_kwin_idle::get_idle_timeout(id: new_id, seat: object, timeout: uint) -> opcode 0
            self.send_words(idle_mgr, 0, &[tid, seat, ms])?;
            self.idle_timeout_id = Some(tid);
            eprintln!("tb-screen-power: armed kwin_idle timeout = {timeout_secs} s");
        } else {
            eprintln!("tb-screen-power: idle blanking disabled (timeout = 0)");
        }
        self.configured_timeout_s = Some(timeout_secs);
        Ok(())
    }

    fn simulate_activity(&mut self) {
        if let Some(tid) = self.idle_timeout_id {
            // org_kde_kwin_idle_timeout::simulate_user_activity() -> opcode 1
            let _ = self.send_words(tid, 1, &[]);
        }
    }

    fn set_power(&mut self, on: bool) -> Result<(), String> {
        self.ensure_output_powers()?;
        if self.power_objs.is_empty() {
            return Err("zwlr_output_power_manager_v1 not bound or no wl_output".into());
        }
        let mode: u32 = if on { 1 } else { 0 };
        let ids: Vec<u32> = self.power_objs.iter().map(|(_, pid)| *pid).collect();
        for pid in ids {
            // zwlr_output_power_v1::set_mode(mode: uint) -> opcode 0
            self.send_words(pid, 0, &[mode])?;
        }
        if on {
            self.simulate_activity();
        }
        self.compositor_power_on = Some(on);
        Ok(())
    }

    /// Reads available Wayland events; returns `(eof, idle_triggered, resumed_triggered)`.
    fn pump_events(&mut self, timeout_secs: u32) -> Result<(bool, bool, bool), String> {
        let mut tmp = [0u8; 4096];
        let mut eof = false;
        loop {
            match self.sock.read(&mut tmp) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => self.rx_buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => break,
                Err(e) => return Err(format!("wayland read: {e}")),
            }
        }

        let mut saw_idle = false;
        let mut saw_resumed = false;
        let mut pos = 0usize;
        while pos + 8 <= self.rx_buf.len() {
            let obj_id = u32::from_ne_bytes(self.rx_buf[pos..pos + 4].try_into().unwrap());
            let word1 = u32::from_ne_bytes(self.rx_buf[pos + 4..pos + 8].try_into().unwrap());
            let size = (word1 >> 16) as usize;
            let opcode = (word1 & 0xffff) as u16;
            if size < 8 {
                return Err(format!("invalid wayland event size {size}"));
            }
            if pos + size > self.rx_buf.len() {
                break;
            }
            let payload = self.rx_buf[pos + 8..pos + size].to_vec();
            pos += size;

            if obj_id == 1 && opcode == 0 && payload.len() >= 12 {
                // wl_display::error(object_id, code, message)
                let bad_obj = u32::from_ne_bytes(payload[0..4].try_into().unwrap());
                let code = u32::from_ne_bytes(payload[4..8].try_into().unwrap());
                let msg = parse_wl_str(&payload, 8).map(|(s, _)| s).unwrap_or_default();
                return Err(format!("wl_display error on obj {bad_obj} (code {code}): {msg}"));
            } else if obj_id == self.init_sync_id && opcode == 0 {
                self.synced = true;
                self.ensure_output_powers()?;
                self.apply_idle_timeout(timeout_secs)?;
            } else if obj_id == self.registry_id && opcode == 0 && payload.len() >= 12 {
                // wl_registry::global(name, interface, version)
                let gname = u32::from_ne_bytes(payload[0..4].try_into().unwrap());
                if let Some((iface, next_off)) = parse_wl_str(&payload, 4) {
                    if next_off + 4 <= payload.len() {
                        let _ver = u32::from_ne_bytes(payload[next_off..next_off + 4].try_into().unwrap());
                        match iface.as_str() {
                            "wl_output" => {
                                let oid = self.bind_global(gname, "wl_output", 1)?;
                                self.outputs.push((gname, oid));
                                if self.synced {
                                    self.ensure_output_powers()?;
                                }
                            }
                            "wl_seat" if self.seat_id.is_none() => {
                                self.seat_id = Some(self.bind_global(gname, "wl_seat", 1)?);
                            }
                            "zwlr_output_power_manager_v1" if self.power_mgr_id.is_none() => {
                                self.power_mgr_id = Some(self.bind_global(gname, "zwlr_output_power_manager_v1", 1)?);
                            }
                            "org_kde_kwin_idle" if self.idle_mgr_id.is_none() => {
                                self.idle_mgr_id = Some(self.bind_global(gname, "org_kde_kwin_idle", 1)?);
                            }
                            _ => {}
                        }
                    }
                }
            } else if obj_id == self.registry_id && opcode == 1 && payload.len() >= 4 {
                // wl_registry::global_remove(name)
                let gname = u32::from_ne_bytes(payload[0..4].try_into().unwrap());
                if let Some(idx) = self.outputs.iter().position(|(n, _)| *n == gname) {
                    let (_, oid) = self.outputs.remove(idx);
                    if let Some(pidx) = self.power_objs.iter().position(|(o, _)| *o == oid) {
                        let (_, pid) = self.power_objs.remove(pidx);
                        let _ = self.send_words(pid, 1, &[]); // destroy
                    }
                }
            } else if Some(obj_id) == self.idle_timeout_id {
                if opcode == 0 {
                    saw_idle = true;
                } else if opcode == 1 {
                    saw_resumed = true;
                }
            } else if let Some(pidx) = self.power_objs.iter().position(|(_, pid)| *pid == obj_id) {
                if opcode == 0 && payload.len() >= 4 {
                    // zwlr_output_power_v1::mode(uint)
                    let m = u32::from_ne_bytes(payload[0..4].try_into().unwrap());
                    self.compositor_power_on = Some(m != 0);
                } else if opcode == 1 {
                    // zwlr_output_power_v1::failed() — another client currently owns it or output gone
                    let (_, pid) = self.power_objs.remove(pidx);
                    let _ = self.send_words(pid, 1, &[]); // destroy, will re-request on next set_power
                    eprintln!("tb-screen-power: zwlr_output_power_v1 failed (exclusive holder present); will retry on demand");
                }
            }
        }
        if pos > 0 {
            self.rx_buf.drain(..pos);
        }
        Ok((eof, saw_idle, saw_resumed))
    }
}

fn read_panel_state(panel_file: &str, fallback: Option<bool>) -> bool {
    if let Ok(s) = std::fs::read_to_string(panel_file) {
        let t = s.trim();
        if !t.is_empty() {
            return t != "off";
        }
    }
    fallback.unwrap_or(true)
}

struct InputWatch {
    file: File,
    buf: [u8; 24 * 16],
    carry: usize,
}

fn open_input_raw(path: &str, quiet_err: bool) -> Option<File> {
    match OpenOptions::new().read(true).custom_flags(O_NONBLOCK).open(path) {
        Ok(f) => Some(f),
        Err(e) => {
            if !quiet_err {
                eprintln!("tb-screen-power: cannot open {path}: {e}");
            }
            None
        }
    }
}

fn has_key_power_bit(fd: RawFd) -> bool {
    extern "C" {
        fn ioctl(fd: i32, request: usize, ...) -> i32;
    }
    // _IOC(_IOC_READ=2, 'E'=0x45, 0x20 + EV_KEY(1) = 0x21, 96)
    const EVIOCGBIT_EV_KEY_96: usize = (2usize << 30) | (96usize << 16) | (0x45usize << 8) | 0x21usize;
    let mut bits = [0u8; 96];
    let rc = unsafe { ioctl(fd, EVIOCGBIT_EV_KEY_96, bits.as_mut_ptr()) };
    let byte_idx = (KEY_POWER as usize) / 8;
    let bit_mask = 1u8 << ((KEY_POWER as usize) % 8);
    rc > byte_idx as i32 && (bits[byte_idx] & bit_mask) != 0
}

/// Auto-discovers `/dev/input/event*` devices advertising `KEY_POWER` (`116`, e.g. `mtk-pmic-keys`
/// on `event1`), or opens explicit comma-separated path(s) from `TB_SCREEN_INPUT`.
fn discover_power_inputs(override_env: Option<&str>) -> Vec<InputWatch> {
    let mut out = Vec::new();
    if let Some(spec) = override_env {
        for p in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(file) = open_input_raw(p, false) {
                eprintln!("tb-screen-power: watching {p} for KEY_POWER");
                out.push(InputWatch { file, buf: [0u8; 24 * 16], carry: 0 });
            }
        }
        return out;
    }
    let mut cands: Vec<PathBuf> = std::fs::read_dir("/dev/input")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("event")))
        .collect();
    cands.sort();
    for p in &cands {
        let Some(ps) = p.to_str() else { continue };
        if let Some(file) = open_input_raw(ps, true) {
            if has_key_power_bit(file.as_raw_fd()) {
                eprintln!("tb-screen-power: watching {ps} for KEY_POWER");
                out.push(InputWatch { file, buf: [0u8; 24 * 16], carry: 0 });
            }
        }
    }
    if out.is_empty() {
        for fallback in ["/dev/input/event1", "/dev/input/event0"] {
            if let Some(file) = open_input_raw(fallback, true) {
                eprintln!("tb-screen-power: watching fallback {fallback} for KEY_POWER");
                out.push(InputWatch { file, buf: [0u8; 24 * 16], carry: 0 });
            }
        }
        if out.is_empty() {
            eprintln!("tb-screen-power: no readable /dev/input/event* with KEY_POWER (KEY_POWER watch inactive)");
        }
    }
    out
}

fn open_timeout_inotify(timeout_file: &str) -> Option<(File, String)> {
    const IN_CLOSE_WRITE: u32 = 0x008;
    const IN_MOVED_TO: u32 = 0x080;
    const IN_CREATE: u32 = 0x100;
    const IN_DELETE: u32 = 0x200;
    let p = Path::new(timeout_file);
    let dir = p.parent()?.to_str()?;
    let fname = p.file_name()?.to_str()?.to_string();
    if !Path::new(dir).is_dir() {
        return None;
    }
    let fd = unsafe { inotify_init1(O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let c = CString::new(dir).ok()?;
    if unsafe { inotify_add_watch(fd, c.as_ptr(), IN_CLOSE_WRITE | IN_MOVED_TO | IN_CREATE | IN_DELETE) } < 0 {
        let _ = unsafe { File::from_raw_fd(fd) };
        return None;
    }
    let f = unsafe { File::from_raw_fd(fd) };
    let _ = unsafe { libc_set_nonblock(f.as_raw_fd()) };
    Some((f, fname))
}

unsafe fn libc_set_nonblock(fd: RawFd) -> i32 {
    extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    const F_GETFL: i32 = 3;
    const F_SETFL: i32 = 4;
    let flags = fcntl(fd, F_GETFL, 0);
    if flags < 0 { -1 } else { fcntl(fd, F_SETFL, flags | O_NONBLOCK) }
}

fn parse_client_op(raw: &str) -> &str {
    let t = raw.trim();
    if t.contains("\"on\"") || t == "on" {
        "on"
    } else if t.contains("\"off\"") || t == "off" {
        "off"
    } else if t.contains("\"toggle\"") || t == "toggle" {
        "toggle"
    } else if t.contains("\"status\"") || t == "status" {
        "status"
    } else {
        ""
    }
}

fn main() {
    let panel_file = env_opt("TB_SHELL_PANEL_FILE").unwrap_or_else(|| "/sys/kernel/tinybot_panel/state".into());
    let input_override = env_opt("TB_SCREEN_INPUT");
    let timeout_file = env_opt("TB_SCREEN_TIMEOUT_FILE").unwrap_or_else(|| "/userdata/tinybot/etc/screen-timeout".into());
    let sock_path = resolve_screen_sock();

    let mut wl = match WaylandConn::connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tb-screen-power: {e}");
            std::process::exit(1);
        }
    };

    let _ = std::fs::remove_file(&sock_path);
    let listener = match UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tb-screen-power: bind {sock_path}: {e}");
            std::process::exit(1);
        }
    };
    let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600));
    let _ = listener.set_nonblocking(true);
    eprintln!("tb-screen-power: listening on {sock_path}");

    let mut inputs = discover_power_inputs(input_override.as_deref());
    let mut timeout_watch = open_timeout_inotify(&timeout_file);
    let mut timeout_secs = read_timeout_secs(&timeout_file);

    // Pump any already-buffered initial events
    if let Err(e) = wl.pump_events(timeout_secs) {
        eprintln!("tb-screen-power: {e}");
        std::process::exit(1);
    }

    // Captures `(pressed_at, was_on_at_key_down)` on KEY_POWER value==1 so compositor-side
    // `kwin_idle::resumed` wakes during key hold never invert key-up's toggle direction.
    let mut power_down: Option<(Instant, bool)> = None;

    loop {
        let mut fds = Vec::with_capacity(3 + inputs.len());
        fds.push(PollFd { fd: wl.sock.as_raw_fd(), events: POLLIN, revents: 0 });
        fds.push(PollFd { fd: listener.as_raw_fd(), events: POLLIN, revents: 0 });
        let inputs_base = fds.len();
        for iw in &inputs {
            fds.push(PollFd { fd: iw.file.as_raw_fd(), events: POLLIN, revents: 0 });
        }
        let watch_idx = timeout_watch.as_ref().map(|(f, _)| {
            let idx = fds.len();
            fds.push(PollFd { fd: f.as_raw_fd(), events: POLLIN, revents: 0 });
            idx
        });

        let rc = unsafe { poll(fds.as_mut_ptr(), fds.len(), -1) };
        if rc < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(4) {
                continue;
            }
            break;
        }

        // 1. `/dev/input/event*` (`mtk-pmic-keys`) KEY_POWER short press (< 800 ms):
        //    processed BEFORE `wl.pump_events` so `was_on` at key-down (`value == 1`) is
        //    sampled before any simultaneous Wayland mode/resumed event in the same poll turn.
        let mut i = inputs.len();
        while i > 0 {
            i -= 1;
            let rev = fds[inputs_base + i].revents;
            if rev & (POLLIN | POLLERR | POLLHUP) == 0 {
                continue;
            }
            let mut drop_input = false;
            let iw = &mut inputs[i];
            loop {
                match iw.file.read(&mut iw.buf[iw.carry..]) {
                    Ok(0) => {
                        drop_input = true;
                        break;
                    }
                    Ok(n) => {
                        let total = iw.carry + n;
                        let ev_size = std::mem::size_of::<InputEvent>();
                        let mut off = 0;
                        while off + ev_size <= total {
                            let mut ev = InputEvent::default();
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    iw.buf[off..].as_ptr(),
                                    &mut ev as *mut InputEvent as *mut u8,
                                    ev_size,
                                );
                            }
                            if ev.type_ == EV_KEY && ev.code == KEY_POWER {
                                if ev.value == 1 {
                                    let was_on = read_panel_state(&panel_file, wl.compositor_power_on);
                                    power_down = Some((Instant::now(), was_on));
                                } else if ev.value == 0 {
                                    if let Some((t0, was_on)) = power_down.take() {
                                        if t0.elapsed() < Duration::from_millis(800) {
                                            let next = !was_on;
                                            eprintln!(
                                                "tb-screen-power: KEY_POWER (was {}) -> {}",
                                                if was_on { "on" } else { "off" },
                                                if next { "on" } else { "off" }
                                            );
                                            let _ = wl.set_power(next);
                                        }
                                    }
                                }
                            }
                            off += ev_size;
                        }
                        iw.carry = total - off;
                        if iw.carry > 0 {
                            iw.buf.copy_within(off..total, 0);
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => break,
                    Err(_) => {
                        drop_input = true;
                        break;
                    }
                }
            }
            if drop_input {
                inputs.remove(i);
            }
        }

        // 2. Wayland socket events (`kwin_idle::idle` -> off ONLY; `resumed` NEVER wakes an off panel)
        if fds[0].revents & (POLLIN | POLLERR | POLLHUP) != 0 {
            match wl.pump_events(timeout_secs) {
                Ok((eof, saw_idle, _saw_resumed)) => {
                    if saw_idle && read_panel_state(&panel_file, wl.compositor_power_on) {
                        eprintln!("tb-screen-power: idle -> screen off");
                        let _ = wl.set_power(false);
                    }
                    if eof {
                        eprintln!("tb-screen-power: Wayland compositor closed socket");
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("tb-screen-power: {e}");
                    std::process::exit(1);
                }
            }
        }

        // 3. Control socket (`tbui screen on|off|toggle|status`)
        if fds[1].revents & POLLIN != 0 {
            while let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                let mut line = String::new();
                if let Ok(clone) = stream.try_clone() {
                    let _ = BufReader::new(clone).read_line(&mut line);
                }
                let reply = match parse_client_op(&line) {
                    "on" => match wl.set_power(true) {
                        Ok(()) => "on\n".to_string(),
                        Err(e) => format!("ERR: {e}\n"),
                    },
                    "off" => match wl.set_power(false) {
                        Ok(()) => "off\n".to_string(),
                        Err(e) => format!("ERR: {e}\n"),
                    },
                    "toggle" => {
                        let next = !read_panel_state(&panel_file, wl.compositor_power_on);
                        match wl.set_power(next) {
                            Ok(()) => if next { "on\n".into() } else { "off\n".into() },
                            Err(e) => format!("ERR: {e}\n"),
                        }
                    }
                    "status" => {
                        if read_panel_state(&panel_file, wl.compositor_power_on) {
                            "on\n".into()
                        } else {
                            "off\n".into()
                        }
                    }
                    _ => "ERR: expected on | off | toggle | status\n".into(),
                };
                let _ = stream.write_all(reply.as_bytes());
            }
        }

        // 4. `screen-timeout` config file inotify
        if let Some(idx) = watch_idx {
            if fds[idx].revents & POLLIN != 0 {
                let mut hit = false;
                if let Some((f, target)) = timeout_watch.as_mut() {
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = f.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        let mut at = 0;
                        while at + 16 <= n {
                            let len = u32::from_ne_bytes(buf[at + 12..at + 16].try_into().unwrap()) as usize;
                            let raw = String::from_utf8_lossy(&buf[at + 16..(at + 16 + len).min(n)]);
                            if raw.trim_end_matches('\0') == target.as_str() {
                                hit = true;
                            }
                            at += 16 + len;
                        }
                    }
                }
                if hit {
                    timeout_secs = read_timeout_secs(&timeout_file);
                    let _ = wl.apply_idle_timeout(timeout_secs);
                }
            } else if fds[idx].revents & (POLLERR | POLLHUP) != 0 {
                timeout_watch = None;
            }
        }
    }
}
