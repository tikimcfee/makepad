//! Threads that block, so the UI thread never does: the socket (an accept
//! thread, and per connection a reader and a writer) and an inotify watcher.
//! Each wakes the UI with `SignalToUI` only when something arrived.

use makepad_widgets::*;
use std::ffi::{c_void, CString};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
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
    /// A watched path changed: the app reads its sources again.
    Changed,
}

extern "C" {
    fn getsockopt(fd: i32, level: i32, name: i32, val: *mut c_void, len: *mut u32) -> i32;
    fn inotify_init1(flags: i32) -> i32;
    fn inotify_add_watch(fd: i32, path: *const std::ffi::c_char, mask: u32) -> i32;
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
const IN_CLOEXEC: i32 = 0o2000000;

/// Watches a sound device directory for opens and closes of capture nodes
/// (`pcmC*D*c`) with inotify; the thread blocks in read(), so nothing wakes
/// at rest, and playback or control opens do not wake the UI. The events only
/// say WHEN to read the capture status: the mark is the status, never a count.
/// If the directory itself goes (replaced by the device manager), the watch is
/// armed again on the new one, and a read is asked for.
pub fn watch_capture(dir: &str, tx: Sender<Msg>) {
    const IN_DELETE_SELF: u32 = 0x400;
    const IN_MOVE_SELF: u32 = 0x800;
    const IN_IGNORED: u32 = 0x8000;
    let mask = IN_OPEN | IN_CLOSE_WRITE | IN_CLOSE_NOWRITE | IN_DELETE_SELF | IN_MOVE_SELF;
    let fd = unsafe { inotify_init1(IN_CLOEXEC) };
    let Ok(c) = CString::new(dir) else { return };
    if fd < 0 || unsafe { inotify_add_watch(fd, c.as_ptr(), mask) } < 0 {
        error!("tb-shell: cannot watch {dir}: {}; the recording mark is not kept", std::io::Error::last_os_error());
        return;
    }
    let dir = dir.to_string();
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    spawn("tb-watch", move || {
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
                // The directory was replaced: arm the watch on the new one (once a second
                // until it is there: this happens only when the device manager rebuilds it).
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
                let _ = tx.send(Msg::Changed);
                SignalToUI::set_ui_signal();
            }
        }
    });
}

pub fn new_channel() -> (Sender<Msg>, Receiver<Msg>) {
    channel()
}
