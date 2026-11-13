//! The socket side of the surface: one long-lived thread that owns the listening Unix socket,
//! every client connection and the protocol state (which surface and rev are shown, the taps
//! that wait for an agent). It sleeps in `poll` on the listener, the clients and a wake socket
//! the UI thread writes to; nothing here runs on a timer.
//!
//! The UI thread never waits on this thread: surfaces come to it through a channel read with
//! `try_recv` on `Event::Signal`, taps go the other way through a channel plus one byte on the
//! wake socket.

use crate::wire::{self, Request, Spec, Tap, WireErr};
use makepad_widgets::makepad_platform::thread::SignalToUI;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

/// Socket thread to UI.
pub enum ToUi {
    Set(Spec),
    Clear,
}

/// UI to socket thread.
pub enum ToIo {
    Tap { surface: String, rev: u64, id: String, values: Vec<(String, bool)> },
}

/// The UI thread's end. Dropping it ends the socket thread.
pub struct Link {
    to_io: Sender<ToIo>,
    pub from_io: Receiver<ToUi>,
    wake: UnixStream,
    pub path: PathBuf,
}

impl Link {
    /// Never blocks: the channel is unbounded and a full wake socket means a wake is pending.
    pub fn send(&self, msg: ToIo) {
        let _ = self.to_io.send(msg);
        let _ = (&self.wake).write(&[1]);
    }
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}
#[cfg(target_os = "linux")]
type Nfds = std::ffi::c_ulong;
#[cfg(not(target_os = "linux"))]
type Nfds = std::ffi::c_uint;
const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;
const POLLERR: i16 = 0x8;
const POLLHUP: i16 = 0x10;

extern "C" {
    fn poll(fds: *mut PollFd, n: Nfds, timeout_ms: i32) -> i32;
    fn getuid() -> u32;
    fn umask(mask: u32) -> u32;
}

/// Where the socket lives, and whether that is the default place (whose directory is ours to
/// make and keep private): `--socket PATH`, `TINYBOT_SURFACE_SOCK`, or
/// `$XDG_RUNTIME_DIR/tinybot/surface.sock`.
pub fn socket_path() -> (PathBuf, bool) {
    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if arg == "--socket" {
            if let Some(path) = args.next() {
                return (PathBuf::from(path), false);
            }
        } else if let Some(path) = arg.strip_prefix("--socket=") {
            return (PathBuf::from(path), false);
        }
    }
    if let Some(path) = std::env::var_os("TINYBOT_SURFACE_SOCK").filter(|p| !p.is_empty()) {
        return (PathBuf::from(path), false);
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { getuid() })));
    (runtime.join("tinybot").join("surface.sock"), true)
}

/// Only this user's processes may talk to the surface (the socket is 0600 too).
fn same_user(c: &UnixStream) -> bool {
    #[cfg(target_os = "linux")]
    {
        extern "C" {
            fn getsockopt(fd: i32, level: i32, name: i32, val: *mut u8, len: *mut u32) -> i32;
        }
        // SOL_SOCKET 1, SO_PEERCRED 17: struct ucred { pid, uid, gid }
        let mut cred = [0u32; 3];
        let mut len = 12u32;
        unsafe { getsockopt(c.as_raw_fd(), 1, 17, cred.as_mut_ptr() as *mut u8, &mut len) == 0 && cred[1] == getuid() }
    }
    #[cfg(not(target_os = "linux"))]
    {
        extern "C" {
            fn getpeereid(fd: i32, uid: *mut u32, gid: *mut u32) -> i32;
        }
        let (mut uid, mut gid) = (u32::MAX, 0u32);
        unsafe { getpeereid(c.as_raw_fd(), &mut uid, &mut gid) == 0 && uid == getuid() }
    }
}

fn other(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg)
}

/// Binds the socket and starts the socket thread. Fails when another surface already serves
/// the path (its socket is left alone) or the path cannot be made private.
pub fn start(path: PathBuf, default_place: bool) -> io::Result<Link> {
    if default_place {
        let dir = path.parent().ok_or_else(|| other("the socket path has no directory".into()))?;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let meta = std::fs::metadata(dir)?;
        if meta.uid() != unsafe { getuid() } {
            return Err(other(format!("{} belongs to another user", dir.display())));
        }
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    // a socket file left by a dead instance is replaced; a live one is not touched
    if std::fs::symlink_metadata(&path).is_ok() {
        if UnixStream::connect(&path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another surface is serving {}", path.display()),
            ));
        }
        std::fs::remove_file(&path)?;
    }
    // the socket is private from the moment it exists, wherever it is
    let old_mask = unsafe { umask(0o077) };
    let bound = UnixListener::bind(&path);
    unsafe { umask(old_mask) };
    let listener = bound?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;

    let (wake_tx, wake_rx) = UnixStream::pair()?;
    wake_tx.set_nonblocking(true)?;
    wake_rx.set_nonblocking(true)?;
    let (to_io, from_ui) = channel();
    let (to_ui, from_io) = channel();
    let state = State { to_ui, surface: None, rev: 0, title: String::new(), items: 0, taps: VecDeque::new(), next_seq: 1 };
    std::thread::Builder::new()
        .name("tinybot-surface-io".into())
        .spawn(move || run(listener, wake_rx, from_ui, state))?;
    Ok(Link { to_io, from_io, wake: wake_tx, path })
}

struct Conn {
    stream: UnixStream,
    buf: Vec<u8>,
    /// asked for taps with `wait` while none were queued: answered when one arrives
    waiting: bool,
    closed: bool,
}

struct State {
    to_ui: Sender<ToUi>,
    surface: Option<String>,
    rev: u64,
    title: String,
    items: usize,
    /// every tap not yet confirmed by a `done`, oldest first
    taps: VecDeque<Tap>,
    next_seq: u64,
}

impl State {
    fn shown(&self) -> String {
        wire::surface_fields(self.surface.as_deref(), self.rev)
    }

    fn hello(&self) -> String {
        format!("{{\"type\":\"hello\",\"v\":{},{},\"pending\":{}}}\n", wire::VERSION, self.shown(), self.taps.len())
    }

    /// Every waiting tap, one line each, then the line that ends the answer.
    fn taps_reply(&self) -> String {
        let mut out = String::new();
        for tap in &self.taps {
            out.push_str(&tap.line());
        }
        let last = self.taps.back().map_or(0, |t| t.seq);
        out.push_str(&format!("{{\"type\":\"taps\",\"count\":{},\"seq\":{last}}}\n", self.taps.len()));
        out
    }

    /// Answers one request line. `None`: the answer comes later (a `taps` that waits).
    fn request(&mut self, line: &str, waiting: &mut bool, waiters: usize) -> Option<String> {
        let request = match wire::parse_request(line) {
            Ok(r) => r,
            Err(e) => return Some(e.line()),
        };
        Some(match request {
            Request::Set(spec) => {
                if self.surface.as_deref() == Some(spec.surface.as_str()) && spec.rev < self.rev {
                    return Some(
                        WireErr {
                            code: "stale_rev",
                            msg: format!("rev {} is older than rev {} on screen", spec.rev, self.rev),
                        }
                        .line(),
                    );
                }
                self.surface = Some(spec.surface.clone());
                self.rev = spec.rev;
                self.title = spec.title.clone();
                self.items = spec.items.len();
                let _ = self.to_ui.send(ToUi::Set(spec));
                SignalToUI::set_ui_signal();
                format!("{{\"type\":\"ack\",{},\"pending\":{}}}\n", self.shown(), self.taps.len())
            }
            Request::Clear => {
                self.surface = None;
                self.rev = 0;
                self.title.clear();
                self.items = 0;
                let _ = self.to_ui.send(ToUi::Clear);
                SignalToUI::set_ui_signal();
                format!("{{\"type\":\"ack\",{},\"pending\":{}}}\n", self.shown(), self.taps.len())
            }
            Request::Status => format!(
                "{{\"type\":\"status\",\"v\":{},{},\"title\":{},\"items\":{},\"pending\":{},\"waiting\":{}}}\n",
                wire::VERSION,
                self.shown(),
                wire::json_str(&self.title),
                self.items,
                self.taps.len(),
                waiters
            ),
            Request::Taps { wait } => {
                if self.taps.is_empty() && wait {
                    *waiting = true;
                    return None;
                }
                self.taps_reply()
            }
            Request::Done { seq } => {
                let before = self.taps.len();
                while self.taps.front().is_some_and(|t| t.seq <= seq) {
                    self.taps.pop_front();
                }
                format!("{{\"type\":\"ok\",\"dropped\":{},\"pending\":{}}}\n", before - self.taps.len(), self.taps.len())
            }
        })
    }
}

fn write_some(stream: &UnixStream, buf: &[u8]) -> io::Result<usize> {
    #[cfg(target_os = "linux")]
    {
        extern "C" {
            fn send(fd: i32, buf: *const u8, len: usize, flags: i32) -> isize;
        }
        // MSG_NOSIGNAL: a client that left is an error here, not a SIGPIPE for the app
        let n = unsafe { send(stream.as_raw_fd(), buf.as_ptr(), buf.len(), 0x4000) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut stream = stream;
        stream.write(buf)
    }
}

/// Writes all of `text`. A client that takes nothing for a second is given up on (`false`);
/// what it was owed stays queued, since taps are only dropped on `done`.
fn send_all(stream: &UnixStream, text: &str) -> bool {
    let mut rest = text.as_bytes();
    while !rest.is_empty() {
        match write_some(stream, rest) {
            Ok(0) => return false,
            Ok(n) => rest = &rest[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let mut fd = PollFd { fd: stream.as_raw_fd(), events: POLLOUT, revents: 0 };
                if unsafe { poll(&mut fd, 1, 1000) } <= 0 {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
    true
}

impl Conn {
    fn send(&mut self, text: &str) {
        if !self.closed && !send_all(&self.stream, text) {
            self.closed = true;
        }
    }

    /// Reads what has arrived and answers every complete line.
    fn service(&mut self, state: &mut State, waiters: usize) {
        let mut chunk = [0u8; 4096];
        loop {
            match (&self.stream).read(&mut chunk) {
                Ok(0) => {
                    self.closed = true;
                    break;
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.closed = true;
                    break;
                }
            }
            // do not buffer without bound for a client that never ends its line
            if self.buf.len() > wire::MAX_LINE && !self.buf.contains(&b'\n') {
                break;
            }
        }
        let mut start = 0;
        while let Some(len) = self.buf[start..].iter().position(|b| *b == b'\n') {
            let line = &self.buf[start..start + len];
            start += len + 1;
            if line.len() > wire::MAX_LINE {
                self.too_long();
                return;
            }
            let reply = match std::str::from_utf8(line) {
                Ok(text) if text.trim().is_empty() => None,
                Ok(text) => {
                    let mut waiting = self.waiting;
                    let reply = state.request(text, &mut waiting, waiters);
                    self.waiting = waiting;
                    reply
                }
                Err(_) => Some(WireErr { code: "bad_json", msg: "the line is not UTF-8".into() }.line()),
            };
            if let Some(reply) = reply {
                self.send(&reply);
            }
            if self.closed {
                return;
            }
        }
        self.buf.drain(..start);
        if self.buf.len() > wire::MAX_LINE {
            self.too_long();
        }
    }

    /// The framing is lost: say so and close.
    fn too_long(&mut self) {
        let msg = format!("a line is longer than {} bytes", wire::MAX_LINE);
        self.send(&WireErr { code: "too_long", msg }.line());
        self.closed = true;
    }
}

fn run(listener: UnixListener, wake: UnixStream, from_ui: Receiver<ToIo>, mut state: State) {
    let mut conns: Vec<Conn> = Vec::new();
    loop {
        let mut fds = Vec::with_capacity(conns.len() + 2);
        fds.push(PollFd { fd: wake.as_raw_fd(), events: POLLIN, revents: 0 });
        fds.push(PollFd { fd: listener.as_raw_fd(), events: POLLIN, revents: 0 });
        for conn in &conns {
            fds.push(PollFd { fd: conn.stream.as_raw_fd(), events: POLLIN, revents: 0 });
        }
        if unsafe { poll(fds.as_mut_ptr(), fds.len() as Nfds, -1) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!("[tinybot-surface] poll failed: {}", io::Error::last_os_error());
            return;
        }

        // the UI thread: new taps, or the app is going away
        if fds[0].revents != 0 {
            let mut bytes = [0u8; 64];
            loop {
                match (&wake).read(&mut bytes) {
                    Ok(0) => return,
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let mut arrived = false;
            loop {
                match from_ui.try_recv() {
                    Ok(ToIo::Tap { surface, rev, id, values }) => {
                        let seq = state.next_seq;
                        state.next_seq += 1;
                        state.taps.push_back(Tap { seq, surface, rev, id, values });
                        arrived = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            if arrived {
                let reply = state.taps_reply();
                for conn in conns.iter_mut().filter(|c| c.waiting) {
                    conn.waiting = false;
                    conn.send(&reply);
                }
            }
        }

        // clients that sent something or left (only those that were in this poll)
        let polled = fds.len() - 2;
        let waiters = conns.iter().filter(|c| c.waiting).count();
        for (conn, fd) in conns.iter_mut().zip(&fds[2..]).take(polled) {
            if fd.revents & (POLLIN | POLLHUP | POLLERR) != 0 {
                conn.service(&mut state, waiters);
            }
        }
        conns.retain(|c| !c.closed);

        // new clients
        if fds[1].revents != 0 {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if !same_user(&stream) {
                            eprintln!("[tinybot-surface] refused a client of another user");
                            continue;
                        }
                        if stream.set_nonblocking(true).is_err() {
                            continue;
                        }
                        let mut conn = Conn { stream, buf: Vec::new(), waiting: false, closed: false };
                        conn.send(&state.hello());
                        if !conn.closed {
                            conns.push(conn);
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        }
    }
}

/// Removes the socket file of this instance (on shutdown).
pub fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
}
