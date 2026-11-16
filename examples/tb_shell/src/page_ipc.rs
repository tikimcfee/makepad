//! Shared IPC and subprocess helpers for tb-shell page services (`tb-shell-display`,
//! `tb-shell-sound`). Uses only `std`, `libc` FFI, and `makepad_micro_serde`.

#![allow(dead_code)]

use makepad_micro_serde::{DeJson, JsonValue, SerJson};
use std::collections::HashMap;
use std::ffi::CString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub fn env_or(name: &str, default: &str) -> String {
    env_opt(name).unwrap_or_else(|| default.to_string())
}

pub fn obj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

pub fn socket_path() -> Result<String, String> {
    if let Some(p) = env_opt("TB_SHELL_SOCK") {
        return Ok(p);
    }
    if let Some(dir) = env_opt("XDG_RUNTIME_DIR") {
        return Ok(format!("{dir}/tb-shell.sock"));
    }
    if let Some(uid) = env_opt("SUDO_UID") {
        let p = format!("/run/user/{uid}/tb-shell.sock");
        if std::path::Path::new(&p).exists() {
            return Ok(p);
        }
    }
    let found: Vec<String> = std::fs::read_dir("/run/user")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("tb-shell.sock"))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    match found.len() {
        1 => Ok(found[0].clone()),
        0 => Err("no tb-shell socket found".into()),
        _ => Err(format!("multiple tb-shell sockets ({})", found.join(", "))),
    }
}

#[derive(Clone)]
pub struct ShellWriter {
    out: Arc<Mutex<UnixStream>>,
}

impl ShellWriter {
    pub fn send_raw(&self, req: &JsonValue) -> Result<(), String> {
        let line = req.serialize_json() + "\n";
        let mut guard = self.out.lock().map_err(|_| "socket lock poisoned".to_string())?;
        guard.write_all(line.as_bytes()).map_err(|e| format!("socket write: {e}"))
    }

    /// Sends a merge patch for top-level store key `key` without requesting a reply.
    pub fn patch_key(&self, key: &str, value: JsonValue) -> Result<(), String> {
        let req = obj(vec![
            ("op", "set".into()),
            ("reply", JsonValue::Bool(false)),
            ("patch", obj(vec![(key, value)])),
        ]);
        self.send_raw(&req)
    }
}

/// Connects to `tb-shell.sock`, registers a `watch` on `surfaces`, waits for `{"ok":true}`,
/// and returns `(sock_path, writer, line_reader)`.
pub fn connect_and_watch(surfaces: &[&str]) -> Result<(String, ShellWriter, BufReader<UnixStream>), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let (path, stream) = loop {
        let candidate = socket_path().and_then(|p| match UnixStream::connect(&p) {
            Ok(s) => Ok((p, s)),
            Err(e) => Err(format!("connect {p}: {e}")),
        });
        match candidate {
            Ok(pair) => break pair,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e),
        }
    };
    let write_half = stream.try_clone().map_err(|e| format!("clone socket: {e}"))?;
    let writer = ShellWriter { out: Arc::new(Mutex::new(write_half)) };
    let mut reader = BufReader::new(stream);
    let surf_arr = JsonValue::Array(surfaces.iter().map(|s| (*s).into()).collect());
    writer.send_raw(&obj(vec![("op", "watch".into()), ("surfaces", surf_arr)]))?;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| format!("read watch reply: {e}"))?;
        if n == 0 {
            return Err("tb-shell closed connection during watch handshake".into());
        }
        if let Ok(msg) = JsonValue::deserialize_json(line.trim()) {
            if let Some(ok) = msg.get("ok") {
                if ok == &JsonValue::Bool(true) {
                    return Ok((path, writer, reader));
                }
                return Err(format!("watch refused: {}", line.trim()));
            }
        }
    }
}

pub fn parse_kv(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            if !k.is_empty() && !k.contains(' ') {
                map.insert(k.to_string(), v.trim().to_string());
            }
        }
    }
    map
}

pub fn last_nonempty_line(stdout: &str, stderr: &str) -> String {
    stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()))
        .map(|l| l.trim().to_string())
        .unwrap_or_default()
}

pub struct CmdResult {
    pub code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

impl CmdResult {
    pub fn summary_line(&self) -> String {
        last_nonempty_line(&self.stdout, &self.stderr)
    }
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn inotify_init1(flags: i32) -> i32;
    fn inotify_add_watch(fd: i32, path: *const std::ffi::c_char, mask: u32) -> i32;
}

const SIGTERM: i32 = 15;

/// Terminates an exact recorded child PID (via `sudo -n /bin/kill -TERM <pid>` if started
/// under `sudo`, else direct `kill(pid, SIGTERM)`). Never matches by pattern.
pub fn terminate_recorded_pid(pid: u32, sudo_bin: Option<&str>) {
    if let Some(sudo) = sudo_bin {
        let pid_str = pid.to_string();
        let _ = Command::new(sudo)
            .args(["-n", "/bin/kill", "-TERM", &pid_str])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    } else {
        unsafe {
            let _ = kill(pid as i32, SIGTERM);
        }
    }
}

/// Runs `prog` with `args` and a wall-clock `timeout`, without busy-polling.
/// Records the spawned child's PID in `slot` while running so an emergency stop can signal it.
pub fn run_cmd(
    prog: &str,
    args: &[&str],
    timeout: Duration,
    sudo_for_kill: Option<&str>,
    slot: Option<&Arc<Mutex<Option<u32>>>>,
) -> CmdResult {
    let child = match Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return CmdResult {
                code: None,
                timed_out: false,
                stdout: String::new(),
                stderr: format!("spawn {prog}: {e}"),
            };
        }
    };
    let pid = child.id();
    if let Some(s) = slot {
        if let Ok(mut g) = s.lock() {
            *g = Some(pid);
        }
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let res = child.wait_with_output();
        let _ = tx.send(res);
    });
    let out = match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => CmdResult {
            code: output.status.code(),
            timed_out: false,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Ok(Err(e)) => CmdResult {
            code: None,
            timed_out: false,
            stdout: String::new(),
            stderr: format!("wait {prog}: {e}"),
        },
        Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            terminate_recorded_pid(pid, sudo_for_kill);
            // Collect output briefly after SIGTERM if it exits promptly.
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(output)) => CmdResult {
                    code: output.status.code(),
                    timed_out: true,
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                },
                _ => CmdResult {
                    code: None,
                    timed_out: true,
                    stdout: String::new(),
                    stderr: format!("{prog} timed out after {} s", timeout.as_secs()),
                },
            }
        }
    };
    if let Some(s) = slot {
        if let Ok(mut g) = s.lock() {
            if *g == Some(pid) {
                *g = None;
            }
        }
    }
    out
}

/// Returns `Some("sudo")` (or custom `TB_SUDO`) unless `TB_SUDO` is `"none"`.
pub fn sudo_prog() -> Option<String> {
    match env_opt("TB_SUDO").as_deref() {
        Some("none") => None,
        Some(custom) => Some(custom.to_string()),
        None => Some("sudo".to_string()),
    }
}

/// Runs a root tool (`sudo -n <tool> <args...>`, or `<tool> <args...>` when `TB_SUDO=none`).
pub fn run_root_tool(
    tool: &str,
    tool_args: &[&str],
    timeout: Duration,
    slot: Option<&Arc<Mutex<Option<u32>>>>,
) -> CmdResult {
    if let Some(sudo) = sudo_prog() {
        let mut args = Vec::with_capacity(tool_args.len() + 2);
        args.push("-n");
        args.push(tool);
        args.extend_from_slice(tool_args);
        run_cmd(&sudo, &args, timeout, Some(&sudo), slot)
    } else {
        run_cmd(tool, tool_args, timeout, None, slot)
    }
}

/// Watches existing directories with `inotify` for writes/renames of specific file names.
/// Blocks in kernel `read()` with zero wakeups at rest. Does nothing if no directory exists.
pub fn spawn_inotify_watcher(
    watches: Vec<(String, Vec<String>)>,
    on_change: impl Fn() + Send + 'static,
) {
    const IN_CLOSE_WRITE: u32 = 0x008;
    const IN_MOVED_TO: u32 = 0x080;
    const IN_CREATE: u32 = 0x100;
    const IN_DELETE: u32 = 0x200;
    const IN_CLOEXEC: i32 = 0o2000000;
    let mask = IN_CLOSE_WRITE | IN_MOVED_TO | IN_CREATE | IN_DELETE;

    let fd = unsafe { inotify_init1(IN_CLOEXEC) };
    if fd < 0 {
        return;
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut wd_files: HashMap<i32, Vec<String>> = HashMap::new();
    for (dir, names) in watches {
        let Ok(c) = CString::new(dir.as_str()) else { continue };
        let wd = unsafe { inotify_add_watch(fd, c.as_ptr(), mask) };
        if wd >= 0 {
            wd_files.insert(wd, names);
        }
    }
    if wd_files.is_empty() {
        return; // Drop `file`, closing `fd`; spawn no thread.
    }
    let _ = std::thread::Builder::new().name("tb-page-watch".into()).spawn(move || {
        let mut buf = vec![0u8; 4096];
        loop {
            let Ok(n) = file.read(&mut buf) else { break };
            if n == 0 {
                break;
            }
            let mut at = 0;
            let mut hit = false;
            while at + 16 <= n {
                let wd = i32::from_ne_bytes(buf[at..at + 4].try_into().unwrap());
                let len = u32::from_ne_bytes(buf[at + 12..at + 16].try_into().unwrap()) as usize;
                let name = String::from_utf8_lossy(&buf[at + 16..(at + 16 + len).min(n)])
                    .trim_end_matches('\0')
                    .to_string();
                if let Some(wanted) = wd_files.get(&wd) {
                    if wanted.iter().any(|w| w == &name) {
                        hit = true;
                    }
                }
                at += 16 + len;
            }
            if hit {
                on_change();
            }
        }
    });
}
