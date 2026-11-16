//! Threads that block, so the UI thread never does: the socket (an accept
//! thread, and per connection a reader and a writer) and inotify watchers.
//! Each wakes the UI with `SignalToUI` only when something arrived.

use makepad_widgets::*;
use std::ffi::{c_void, CString};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};

/// Who is on the other end of a connection, from the kernel (SO_PEERCRED).
#[derive(Clone, Debug)]
pub struct Peer {
    pub pid: i32,
    pub uid: u32,
    pub comm: String,
    /// The process that started it (a tool runs `tbui`; the tool is the asker).
    pub parent: String,
}

impl Peer {
    pub fn label(&self) -> String {
        if self.parent.is_empty() {
            format!("{} pid {} uid {}", self.comm, self.pid, self.uid)
        } else {
            format!("{} (from {}) pid {} uid {}", self.comm, self.parent, self.pid, self.uid)
        }
    }
}

pub enum Msg {
    Open { conn: u64, tx: Sender<String>, peer: Peer },
    Line { conn: u64, line: String },
    Closed { conn: u64 },
    /// Capture device node opened or closed: re-read the recording mark status.
    MicChanged,
    /// Speaker state file written or moved into place: re-read the loud mark status.
    LoudChanged,
    /// Kernel `sysfs_notify` on `/sys/kernel/tinybot_panel/state`: re-read panel state now.
    PanelChanged,
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

extern "C" {
    fn getsockopt(fd: i32, level: i32, name: i32, val: *mut c_void, len: *mut u32) -> i32;
    fn inotify_init1(flags: i32) -> i32;
    fn inotify_add_watch(fd: i32, path: *const std::ffi::c_char, mask: u32) -> i32;
    fn poll(fds: *mut PollFd, nfds: usize, timeout: i32) -> i32;
    fn lseek(fd: i32, offset: i64, whence: i32) -> i64;
}

const SOL_SOCKET: i32 = 1;
const SO_PEERCRED: i32 = 17; // Linux, x86_64 and aarch64 alike

fn peer_of(stream: &UnixStream) -> Peer {
    #[repr(C)]
    #[derive(Default)]
    struct Ucred {
        pid: i32,
        uid: u32,
        gid: u32,
    }
    let mut cred = Ucred::default();
    let mut len = std::mem::size_of::<Ucred>() as u32;
    let ok = unsafe { getsockopt(stream.as_raw_fd(), SOL_SOCKET, SO_PEERCRED, &mut cred as *mut Ucred as *mut c_void, &mut len) } == 0;
    if !ok {
        return Peer { pid: -1, uid: u32::MAX, comm: "?".into(), parent: String::new() };
    }
    let comm_of = |pid: i32| std::fs::read_to_string(format!("/proc/{pid}/comm")).map(|s| s.trim().to_string()).ok();
    let comm = comm_of(cred.pid).unwrap_or_else(|| "?".into());
    // The parent from /proc/PID/stat (field 4, after the parenthesised name).
    let parent = std::fs::read_to_string(format!("/proc/{}/stat", cred.pid)).ok()
        .and_then(|s| s.rsplit_once(')').and_then(|(_, r)| r.split_whitespace().nth(1).and_then(|p| p.parse::<i32>().ok())))
        .filter(|p| *p > 1).and_then(comm_of).unwrap_or_default();
    Peer { pid: cred.pid, uid: cred.uid, comm, parent }
}

/// A named thread: an unnamed one takes its parent's name, `makepad-app`.
pub fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(f);
}

pub fn listen(path: &str, tx: Sender<Msg>) {
    let _ = std::fs::remove_file(path);
    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            error!("tb-shell: bind {path}: {e}; no clients can reach this shell");
            return;
        }
    };
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    log!("tb-shell: listening on {path}");
    spawn("tb-accept", move || {
        for (conn, stream) in (1u64..).zip(listener.incoming()) {
            let Ok(stream) = stream else { continue };
            let Ok(mut out) = stream.try_clone() else { continue };
            let peer = peer_of(&stream);
            let (wtx, wrx) = channel::<String>();
            spawn("tb-write", move || {
                for line in wrx {
                    if out.write_all(line.as_bytes()).and_then(|_| out.write_all(b"\n")).is_err() {
                        break;
                    }
                }
            });
            let _ = tx.send(Msg::Open { conn, tx: wtx, peer });
            let tx = tx.clone();
            spawn("tb-read", move || {
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    let _ = tx.send(Msg::Line { conn, line });
                    SignalToUI::set_ui_signal();
                }
                let _ = tx.send(Msg::Closed { conn });
                SignalToUI::set_ui_signal();
            });
        }
    });
}

const IN_CLOSE_WRITE: u32 = 0x8;
const IN_CLOSE_NOWRITE: u32 = 0x10;
const IN_OPEN: u32 = 0x20;
const IN_MOVED_FROM: u32 = 0x40;
const IN_MOVED_TO: u32 = 0x80;
const IN_CREATE: u32 = 0x100;
const IN_DELETE: u32 = 0x200;
const IN_DELETE_SELF: u32 = 0x400;
const IN_MOVE_SELF: u32 = 0x800;
const IN_IGNORED: u32 = 0x8000;
const IN_CLOEXEC: i32 = 0o2000000;

/// Watches a sound device directory for opens and closes of capture nodes
/// (`pcmC*D*c`) with inotify; the thread blocks in read(), so nothing wakes
/// at rest, and playback or control opens do not wake the UI.
pub fn watch_capture(dir: &str, tx: Sender<Msg>) {
    let mask = IN_OPEN | IN_CLOSE_WRITE | IN_CLOSE_NOWRITE | IN_DELETE_SELF | IN_MOVE_SELF;
    let fd = unsafe { inotify_init1(IN_CLOEXEC) };
    let Ok(c) = CString::new(dir) else { return };
    if fd < 0 || unsafe { inotify_add_watch(fd, c.as_ptr(), mask) } < 0 {
        error!("tb-shell: cannot watch {dir}: {}; the recording mark is not kept", std::io::Error::last_os_error());
        if fd >= 0 {
            let _ = unsafe { std::fs::File::from_raw_fd(fd) };
        }
        return;
    }
    let dir = dir.to_string();
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    spawn("tb-watch-mic", move || {
        let mut buf = vec![0u8; 4096];
        loop {
            let Ok(n) = file.read(&mut buf) else { break };
            let mut at = 0;
            let mut hit = false;
            let mut gone = false;
            while at + 16 <= n {
                let ev = u32::from_ne_bytes(buf[at + 4..at + 8].try_into().unwrap());
                let len = u32::from_ne_bytes(buf[at + 12..at + 16].try_into().unwrap()) as usize;
                let name = String::from_utf8_lossy(&buf[at + 16..(at + 16 + len).min(n)]).trim_end_matches('\0').to_string();
                hit |= name.starts_with("pcm") && name.ends_with('c');
                gone |= ev & (IN_DELETE_SELF | IN_MOVE_SELF | IN_IGNORED) != 0;
                at += 16 + len;
            }
            if gone {
                log!("tb-shell: {dir} went away; watching it again");
                loop {
                    if unsafe { inotify_add_watch(file.as_raw_fd(), c.as_ptr(), mask) } >= 0 {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
                hit = true;
            }
            if hit {
                let _ = tx.send(Msg::MicChanged);
                SignalToUI::set_ui_signal();
            }
        }
    });
}

/// Watches the parent directory of `state_file` (e.g. `/run/tinybot/speaker-state`)
/// for atomic renames (`IN_MOVED_TO`), writes (`IN_CLOSE_WRITE`), or removals of
/// that exact filename. Blocks in `read()` with zero wake-ups at rest.
pub fn watch_speaker_state(state_file: &str, tx: Sender<Msg>) {
    let p = Path::new(state_file);
    let Some(dir) = p.parent().and_then(|d| d.to_str()).filter(|d| !d.is_empty()) else { return };
    let Some(target) = p.file_name().and_then(|f| f.to_str()).filter(|f| !f.is_empty()) else { return };
    if !Path::new(dir).exists() {
        log!("tb-shell: {dir} not present; loud mark watcher inactive");
        return;
    }
    let mask = IN_CLOSE_WRITE | IN_MOVED_TO | IN_MOVED_FROM | IN_CREATE | IN_DELETE | IN_DELETE_SELF | IN_MOVE_SELF;
    let fd = unsafe { inotify_init1(IN_CLOEXEC) };
    let Ok(c) = CString::new(dir) else { return };
    if fd < 0 || unsafe { inotify_add_watch(fd, c.as_ptr(), mask) } < 0 {
        error!("tb-shell: cannot watch {dir}: {}; the loud mark is not kept", std::io::Error::last_os_error());
        if fd >= 0 {
            let _ = unsafe { std::fs::File::from_raw_fd(fd) };
        }
        return;
    }
    let dir = dir.to_string();
    let target = target.to_string();
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    spawn("tb-watch-loud", move || {
        let mut buf = vec![0u8; 4096];
        loop {
            let Ok(n) = file.read(&mut buf) else { break };
            let mut at = 0;
            let mut hit = false;
            let mut gone = false;
            while at + 16 <= n {
                let ev = u32::from_ne_bytes(buf[at + 4..at + 8].try_into().unwrap());
                let len = u32::from_ne_bytes(buf[at + 12..at + 16].try_into().unwrap()) as usize;
                let raw = String::from_utf8_lossy(&buf[at + 16..(at + 16 + len).min(n)]);
                let name = raw.trim_end_matches('\0');
                hit |= name == target;
                gone |= ev & (IN_DELETE_SELF | IN_MOVE_SELF | IN_IGNORED) != 0;
                at += 16 + len;
            }
            if gone {
                log!("tb-shell: {dir} went away; watching it again");
                for _ in 0..30 {
                    if unsafe { inotify_add_watch(file.as_raw_fd(), c.as_ptr(), mask) } >= 0 {
                        hit = true;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
            if hit {
                let _ = tx.send(Msg::LoudChanged);
                SignalToUI::set_ui_signal();
            }
        }
    });
}

/// Watches a sysfs attribute (e.g. `/sys/kernel/tinybot_panel/state`) via
/// `poll(POLLPRI | POLLERR)` after `read + lseek(0)` (`sysfs_notify` in kernel).
/// Zero timers and zero wake-ups at rest. Skipped for non-`/sys/` test files.
pub fn watch_panel_sysfs(state_file: &str, tx: Sender<Msg>) {
    const POLLPRI: i16 = 0x0002;
    const POLLERR: i16 = 0x0008;
    const SEEK_SET: i32 = 0;
    if !state_file.starts_with("/sys/") {
        return;
    }
    let mut file = match std::fs::File::open(state_file) {
        Ok(f) => f,
        Err(e) => {
            log!("tb-shell: cannot open {state_file} for sysfs poll: {e}");
            return;
        }
    };
    let path = state_file.to_string();
    spawn("tb-watch-panel", move || {
        let fd = file.as_raw_fd();
        let mut buf = [0u8; 64];
        // Initial read arms kernfs `of->event` to the current notification counter.
        let _ = file.read(&mut buf);
        unsafe {
            lseek(fd, 0, SEEK_SET);
        }
        log!("tb-shell: watching {path} via sysfs poll(POLLPRI)");
        loop {
            let mut pfd = PollFd { fd, events: POLLPRI | POLLERR, revents: 0 };
            let rc = unsafe { poll(&mut pfd, 1, -1) };
            if rc < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(4) {
                    continue; // EINTR
                }
                break;
            }
            if pfd.revents & (POLLPRI | POLLERR) == 0 {
                break;
            }
            unsafe {
                lseek(fd, 0, SEEK_SET);
            }
            if file.read(&mut buf).is_err() {
                break;
            }
            let _ = tx.send(Msg::PanelChanged);
            SignalToUI::set_ui_signal();
        }
    });
}

pub fn new_channel() -> (Sender<Msg>, Receiver<Msg>) {
    channel()
}
