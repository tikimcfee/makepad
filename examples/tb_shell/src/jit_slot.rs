//! Live `makepad-run` (Relax Rust JIT + 3D) guest slots hosted inside `tb-shell`'s Stage.
//!
//! Spawns `makepad-jit` (`makepad run <pkg> -- --remote` with `MAKEPAD_HIDE_WINDOWS=1`) in a
//! background supervisor, streams tightly packed raw RGBA frames over its local Unix socket
//! (`makepad_remote::frames::fetch`, zero PNG encode/decode, zero files on disk), uploads them
//! into a reusable `TextureFormat::VecBGRAu8_32` (`Texture::set_data_u32`), resizes the guest
//! window to the exact Stage slot dimensions, forwards pointer/scroll/key events off the UI
//! thread, and pauses frame streaming (`0` wake-ups) whenever the slot is hidden or `panel_off`.

use makepad_remote::{frames, Client};
use makepad_widgets::*;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

fn resolve_jit_bin() -> String {
    if let Ok(b) = std::env::var("TB_MAKEPAD_JIT_BIN") {
        let t = b.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    for cand in [
        "/opt/tinybot/makepad-run/bin/makepad-run",
        "/usr/local/bin/makepad-run",
        "/opt/tinybot/makepad-jit/bin/makepad-jit",
        "/opt/tinybot/makepad-run/bin/makepad",
        "/opt/tinybot/makepad-jit/bin/makepad",
        "/usr/local/bin/makepad-jit",
    ] {
        if Path::new(cand).is_file() {
            return cand.to_string();
        }
    }
    "makepad".to_string()
}

fn resolve_jit_workspace(explicit: Option<&str>) -> Option<String> {
    if let Some(d) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(d.to_string());
    }
    if let Ok(d) = std::env::var("TB_MAKEPAD_JIT_WORKSPACE") {
        let t = d.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    for cand in [
        "/userdata/tinybot/makepad-jit/workspace",
        "/opt/tinybot/makepad-jit/workspace",
    ] {
        if Path::new(cand).is_dir() {
            return Some(cand.to_string());
        }
    }
    None
}

/// The `systemd-run` arguments that put a guest in its own user scope, or None to spawn it
/// directly (TB_JIT_SCOPE=0, no systemd-run, or no user bus to reach the user manager).
/// TB_JIT_MEMORY_MAX sets the scope's MemoryMax (default 1800M: a cold lgtm-deck3d compile peaks near 1.1 GB; swap is off on the phone).
fn jit_scope(slot_name: &str) -> Option<Vec<String>> {
    if std::env::var("TB_JIT_SCOPE").map_or(false, |v| v.trim() == "0") {
        return None;
    }
    let has_run = ["/usr/bin/systemd-run", "/bin/systemd-run"].iter().any(|p| Path::new(p).is_file());
    let bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").map_or(false, |v| !v.trim().is_empty())
        || std::env::var("XDG_RUNTIME_DIR").map_or(false, |d| Path::new(&format!("{d}/bus")).exists());
    if !has_run || !bus {
        log!("tb-shell: jit {slot_name}: no systemd-run or user bus, the guest shares tb-shell's cgroup");
        return None;
    }
    let max = std::env::var("TB_JIT_MEMORY_MAX")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '%' || c == '.'))
        .unwrap_or_else(|| "1800M".to_string());
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    Some(vec![
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        "--collect".into(),
        format!("--unit=tb-jit-{slot_name}-{ms}"),
        format!("--description=tb-shell JIT guest {slot_name}"),
        format!("-pMemoryMax={max}"),
        "-pMemorySwapMax=0".into(),
    ])
}

pub struct JitFrame {
    pub width: usize,
    pub height: usize,
    pub bgra: Vec<u32>,
}

struct SharedSlot {
    status: String,
    frame: Option<JitFrame>,
    paused: bool,
    stopped: bool,
    desired_size: (u32, u32),
    applied_size: (u32, u32),
    child_pid: Option<u32>,
}

struct SharedHandle {
    mu: Mutex<SharedSlot>,
    cv: Condvar,
    alive: AtomicBool,
}

pub struct JitInstance {
    shared: Arc<SharedHandle>,
    cmd_tx: mpsc::Sender<String>,
    pub texture: Option<Texture>,
    last_sent_size: (u32, u32),
}

impl Drop for JitInstance {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl JitInstance {
    pub fn spawn(
        slot_name: &str,
        package: &str,
        dir: Option<&str>,
        src_store: Option<&str>,
        fps: f64,
        initially_visible: bool,
    ) -> Result<Self, String> {
        let bin = resolve_jit_bin();
        let workspace = resolve_jit_workspace(dir);
        let fps = fps.clamp(1.0, 60.0);

        // The guest runs in its own systemd user scope with its own memory limit, outside
        // tb-shell's cgroup: a JIT compile that grows past tb-shell's MemoryMax would otherwise
        // OOM-kill the shell. `systemd-run --scope` moves itself into the scope and then execs
        // the command, so the child pid stays the guest's. TB_JIT_SCOPE=0 spawns it directly.
        let mut cmd = match jit_scope(slot_name) {
            Some(scope_args) => {
                let mut c = Command::new("systemd-run");
                c.args(&scope_args).arg("--").arg(&bin);
                c
            }
            None => Command::new(&bin),
        };
        cmd.arg("run");
        if let Some(store) = src_store.map(str::trim).filter(|s| !s.is_empty()) {
            cmd.arg("--src").arg(store);
        }
        for tok in package.split_whitespace() {
            cmd.arg(tok);
        }
        cmd.arg("--").arg("--remote");
        if let Some(ws) = &workspace {
            if !Path::new(ws).is_dir() {
                return Err(format!("JIT workspace does not exist: {ws}"));
            }
            cmd.current_dir(ws);
        }
        cmd.env("MAKEPAD_HIDE_WINDOWS", "1")
            .env("MAKEPAD_MUTE", "1")
            .env("MAKEPAD_GAMEPAD", "0")
            .env("NO_AT_BRIDGE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn {bin} for {package}: {e}"))?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let shared = Arc::new(SharedHandle {
            mu: Mutex::new(SharedSlot {
                status: format!("JIT compiling {package}..."),
                frame: None,
                paused: !initially_visible,
                stopped: false,
                desired_size: (360, 420),
                applied_size: (0, 0),
                child_pid: Some(pid),
            }),
            cv: Condvar::new(),
            alive: AtomicBool::new(true),
        });

        let (sock_tx, sock_rx) = mpsc::channel::<String>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<String>();

        if let Some(out) = stdout {
            let sh = shared.clone();
            let stx = sock_tx.clone();
            let tag = format!("jit-out-{slot_name}");
            let _ = std::thread::Builder::new().name(tag).spawn(move || {
                read_output_lines(out, &sh, &stx);
            });
        }
        if let Some(err) = stderr {
            let sh = shared.clone();
            let stx = sock_tx;
            let pkg_lbl = package.to_string();
            let tag = format!("jit-err-{slot_name}");
            let _ = std::thread::Builder::new().name(tag).spawn(move || {
                read_output_lines(err, &sh, &stx);
                let st = child.wait().ok();
                let code = st.and_then(|s| s.code());
                let sig = st.and_then(|s| std::os::unix::process::ExitStatusExt::signal(&s));
                if let Ok(mut g) = sh.mu.lock() {
                    g.child_pid = None;
                    if !g.stopped {
                        g.status = match (code, sig) {
                            (Some(c), _) => format!("JIT {pkg_lbl} exited ({c}): {}", g.status),
                            (None, Some(9)) => format!("JIT {pkg_lbl} was killed (SIGKILL: out of memory in its scope?)"),
                            (None, Some(s)) => format!("JIT {pkg_lbl} stopped by signal {s}"),
                            (None, None) => format!("JIT {pkg_lbl} stopped"),
                        };
                    }
                    sh.alive.store(false, Ordering::SeqCst);
                    sh.cv.notify_all();
                }
                SignalToUI::set_ui_signal();
            });
        }

        // Waiter + command + frame streaming worker
        {
            let sh = shared.clone();
            let pkg_lbl = package.to_string();
            let slot_lbl = slot_name.to_string();
            let tag = format!("jit-link-{slot_name}");
            let _ = std::thread::Builder::new().name(tag).spawn(move || {
                let Ok(sock_path) = sock_rx.recv() else { return };
                if !sh.alive.load(Ordering::SeqCst) {
                    return;
                }
                let client = match Client::connect(&sock_path) {
                    Ok(c) => c,
                    Err(e) => {
                        if let Ok(mut g) = sh.mu.lock() {
                            g.status = format!("JIT {pkg_lbl} connect failed: {e}");
                        }
                        SignalToUI::set_ui_signal();
                        return;
                    }
                };
                if let Ok(mut g) = sh.mu.lock() {
                    g.status = format!("JIT {pkg_lbl} ready — rendering first frame...");
                }
                SignalToUI::set_ui_signal();

                // Dedicated off-UI command forwarder (resize + mouse + key)
                {
                    let cmd_client = client.clone();
                    let sh_cmd = sh.clone();
                    let _ = std::thread::Builder::new().name("jit-cmd".into()).spawn(move || {
                        while let Ok(line) = cmd_rx.recv() {
                            if !sh_cmd.alive.load(Ordering::Relaxed) {
                                break;
                            }
                            let _ = cmd_client.cmd(&line).and_then(|mut it| {
                                for l in &mut it {
                                    let _ = l;
                                }
                                Ok(())
                            });
                        }
                    });
                }

                // Frame stream loop (pauses cleanly with 0 wake-ups when hidden / panel_off)
                let mut logged_first = false;
                while sh.alive.load(Ordering::SeqCst) {
                    let (resize_needed, need_initial) = {
                        let Ok(mut g) = sh.mu.lock() else { break };
                        while g.paused && !g.stopped && sh.alive.load(Ordering::SeqCst) {
                            g = match sh.cv.wait(g) {
                                Ok(guard) => guard,
                                Err(_) => return,
                            };
                        }
                        if g.stopped || !sh.alive.load(Ordering::SeqCst) {
                            break;
                        }
                        let rsz = if g.desired_size != g.applied_size && g.desired_size.0 >= 32 && g.desired_size.1 >= 32 {
                            g.applied_size = g.desired_size;
                            Some(g.desired_size)
                        } else {
                            None
                        };
                        (rsz, !logged_first || rsz.is_some())
                    };
                    if let Some((w, h)) = resize_needed {
                        let _ = client.cmd(&format!("window k=resize width={w} height={h}")).and_then(|mut it| {
                            for l in &mut it {
                                let _ = l;
                            }
                            Ok(())
                        });
                    }

                    // Pull one immediate frame (`once=1`) on initial boot or resize so static UIs
                    // paint right away, then stream presented frames (`cap=1`).
                    if need_initial {
                        let once_req = frames::Request {
                            window: None,
                            scale: Some(1.0),
                            raw: true,
                            codec: None,
                            n: Some(1),
                            fps: None,
                            cap: false,
                        };
                        let _ = frames::fetch(&client, &once_req, &mut |rec| {
                            let w = rec.num("w").unwrap_or(0.0) as usize;
                            let h = rec.num("h").unwrap_or(0.0) as usize;
                            if w > 0 && h > 0 && rec.bytes.len() >= w * h * 4 {
                                let bgra = rgba_to_bgra_u32(&rec.bytes[..w * h * 4]);
                                if let Ok(mut g) = sh.mu.lock() {
                                    g.frame = Some(JitFrame { width: w, height: h, bgra });
                                }
                                if !logged_first {
                                    logged_first = true;
                                    log!("tb-shell: jit {slot_lbl} first frame {w}x{h}");
                                }
                                SignalToUI::set_ui_signal();
                            }
                            Ok(())
                        });
                    }

                    let req = frames::Request {
                        window: None,
                        scale: Some(1.0),
                        raw: true,
                        codec: None,
                        n: None,
                        fps: Some(fps),
                        cap: true,
                    };
                    let mut last_auto_resize = Instant::now() - Duration::from_secs(2);
                    let _ = frames::fetch(&client, &req, &mut |rec| {
                        let w = rec.num("w").unwrap_or(0.0) as usize;
                        let h = rec.num("h").unwrap_or(0.0) as usize;
                        if w > 0 && h > 0 && rec.bytes.len() >= w * h * 4 {
                            let bgra = rgba_to_bgra_u32(&rec.bytes[..w * h * 4]);
                            let mut should_stop = false;
                            if let Ok(mut g) = sh.mu.lock() {
                                let (dw, dh) = g.desired_size;
                                if dw >= 64
                                    && dh >= 64
                                    && ((w as u32).abs_diff(dw) > 6 || (h as u32).abs_diff(dh) > 6)
                                    && last_auto_resize.elapsed() >= Duration::from_millis(450)
                                {
                                    last_auto_resize = Instant::now();
                                    g.applied_size = (0, 0);
                                }
                                g.frame = Some(JitFrame { width: w, height: h, bgra });
                                should_stop = g.paused || g.stopped || g.desired_size != g.applied_size;
                            }
                            if !logged_first {
                                logged_first = true;
                                log!("tb-shell: jit {slot_lbl} first frame {w}x{h}");
                            }
                            SignalToUI::set_ui_signal();
                            if should_stop {
                                return Err("pause_or_resize".into());
                            }
                        }
                        Ok(())
                    });
                    if sh.alive.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(35));
                    }
                }
            });
        }

        Ok(Self {
            shared,
            cmd_tx,
            texture: None,
            last_sent_size: (0, 0),
        })
    }

    pub fn set_paused(&self, paused: bool) {
        if let Ok(mut g) = self.shared.mu.lock() {
            if g.paused != paused {
                g.paused = paused;
                self.shared.cv.notify_all();
            }
        }
    }

    pub fn set_slot_size(&mut self, w: u32, h: u32) {
        let w = w.clamp(64, 2048);
        let h = h.clamp(64, 2048);
        if (w.abs_diff(self.last_sent_size.0) <= 2) && (h.abs_diff(self.last_sent_size.1) <= 2) {
            return;
        }
        self.last_sent_size = (w, h);
        if let Ok(mut g) = self.shared.mu.lock() {
            g.desired_size = (w, h);
            self.shared.cv.notify_all();
        }
        let _ = self.cmd_tx.send(format!("window k=resize width={w} height={h}"));
    }

    pub fn send_cmd(&self, cmd: String) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// Pulls any newly arrived RGBA->BGRA frame into `self.texture` and returns `(Option<Texture>, status)`.
    pub fn poll_texture(&mut self, cx: &mut Cx) -> (Option<Texture>, String) {
        let (frame, status) = match self.shared.mu.lock() {
            Ok(mut g) => (g.frame.take(), g.status.clone()),
            Err(_) => (None, "JIT lock poisoned".into()),
        };
        if let Some(f) = frame {
            if let Some(tex) = &self.texture {
                tex.set_data_u32(cx, f.width, f.height, f.bgra);
            } else {
                self.texture = Some(Texture::new_with_format(
                    cx,
                    TextureFormat::VecBGRAu8_32 {
                        width: f.width,
                        height: f.height,
                        data: Some(f.bgra),
                        updated: TextureUpdated::Full,
                    },
                ));
            }
        }
        (self.texture.clone(), status)
    }

    pub fn shutdown(&mut self) {
        self.shared.alive.store(false, Ordering::SeqCst);
        let pid_opt = if let Ok(mut g) = self.shared.mu.lock() {
            g.stopped = true;
            self.shared.cv.notify_all();
            g.child_pid.take()
        } else {
            None
        };
        if let Some(pid) = pid_opt {
            unsafe {
                let _ = kill(pid as i32, SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_millis(120);
            while Instant::now() < deadline {
                if unsafe { kill(pid as i32, 0) } != 0 {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            unsafe {
                let _ = kill(pid as i32, SIGKILL);
            }
        }
    }
}

fn read_output_lines(
    stream: impl std::io::Read,
    sh: &Arc<SharedHandle>,
    sock_tx: &mpsc::Sender<String>,
) {
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(sock) = extract_socket_path(trimmed) {
            let _ = sock_tx.send(sock.to_string());
            continue;
        }
        if trimmed.starts_with("[atspi]") {
            continue;
        }
        if let Ok(mut g) = sh.mu.lock() {
            let clean: String = trimmed.chars().take(120).collect();
            g.status = clean;
        }
        SignalToUI::set_ui_signal();
    }
}

fn extract_socket_path(line: &str) -> Option<&str> {
    if let Some(idx) = line.find("socket=") {
        let rest = &line[idx + "socket=".len()..];
        let tok = rest.split_whitespace().next()?.trim_end_matches(|c: char| c == ')' || c == ',');
        if !tok.is_empty() {
            return Some(tok);
        }
    }
    if let Some(idx) = line.find(" socket ") {
        let rest = &line[idx + " socket ".len()..];
        let tok = rest.split_whitespace().next()?.trim_end_matches(|c: char| c == ')' || c == ',');
        if !tok.is_empty() {
            return Some(tok);
        }
    }
    None
}

fn rgba_to_bgra_u32(rgba: &[u8]) -> Vec<u32> {
    let mut out = Vec::with_capacity(rgba.len() / 4);
    for px in rgba.chunks_exact(4) {
        let r = px[0] as u32;
        let g = px[1] as u32;
        let b = px[2] as u32;
        let a = px[3] as u32;
        out.push((a << 24) | (r << 16) | (g << 8) | b);
    }
    out
}

#[derive(Default)]
pub struct JitRegistry {
    pub slots: HashMap<String, JitInstance>,
}

impl JitRegistry {
    pub fn contains(&self, name: &str) -> bool {
        self.slots.contains_key(name)
    }

    pub fn insert(&mut self, name: String, inst: JitInstance) {
        if let Some(mut old) = self.slots.insert(name, inst) {
            old.shutdown();
        }
    }

    pub fn remove(&mut self, name: &str) -> bool {
        if let Some(mut old) = self.slots.remove(name) {
            old.shutdown();
            true
        } else {
            false
        }
    }

    pub fn sync_visibility(&self, visible: &std::collections::BTreeSet<String>, panel_off: bool) {
        for (name, inst) in &self.slots {
            let vis = !panel_off && visible.contains(name);
            inst.set_paused(!vis);
        }
    }

    pub fn shutdown_all(&mut self) {
        for (_, mut inst) in self.slots.drain() {
            inst.shutdown();
        }
    }
}
