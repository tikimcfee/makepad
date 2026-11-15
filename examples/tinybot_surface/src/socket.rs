//! The socket side of the surface: one long-lived thread that owns the listening Unix socket,
//! every client connection and the protocol state (which surface and rev are shown, the taps
//! that wait for an agent). It sleeps in `poll` on the listener, the clients and a wake socket
//! the UI thread writes to; nothing here runs on a timer.
//!
//! The UI thread never waits on this thread: surfaces come to it through a channel read with
//! `try_recv` on `Event::Signal`, taps go the other way through a channel plus one byte on the
//! wake socket.

use crate::wire::{self, Layer, Request, Spec, Tap, Target, Update, WireErr};
use makepad_widgets::makepad_platform::thread::SignalToUI;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

/// Socket thread to UI.
pub enum ToUi {
    /// a layer's new surface, and for the agent's who set it
    Set(Layer, Spec, String),
    /// already checked against the layer's surface
    Update(Layer, Update),
    Clear(Layer),
    /// the layer on screen now, and whether root's layer exists (the way back from an
    /// agent's page)
    Front(Option<Layer>, bool),
    /// the voice strip's text, or no strip
    Voice(Option<String>),
}

/// UI to socket thread.
pub enum ToIo {
    Tap { layer: Layer, surface: String, rev: u64, id: String, values: Vec<(String, wire::Value)> },
    /// the user left an agent's page for root's with the band's button
    Home,
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

/// What a connection is to the app.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Peer {
    /// The panel runner: a uid-0 client that has said so (`hello` with role `runner`). Root's
    /// layer is its own, and so are the taps made on it.
    Root,
    /// Every other client, the app's own user's or root's: the agent's layer. Being root is
    /// not enough to pose as the runner; an agent that runs as root is still an agent.
    User,
}

/// The process on the other end of a connection.
#[derive(Clone)]
struct Client {
    pid: u32,
    /// uid 0: may ask to be the runner
    root: bool,
    /// "comm (uid N)": the name the process gives itself and the uid the kernel says
    who: String,
}

/// Only this user's processes and root's may talk to the surface: `None` for anyone else.
/// Root is let in for the panel runner, which runs as root to start its actions, and for an
/// agent that runs as root; the socket and its directory stay private to the user (0600,
/// 0700), which does not hold root back.
fn client_of(c: &UnixStream) -> Option<Client> {
    #[cfg(target_os = "linux")]
    let (pid, uid) = {
        extern "C" {
            fn getsockopt(fd: i32, level: i32, name: i32, val: *mut u8, len: *mut u32) -> i32;
        }
        // SOL_SOCKET 1, SO_PEERCRED 17: struct ucred { pid, uid, gid }
        let mut cred = [0u32; 3];
        let mut len = 12u32;
        if unsafe { getsockopt(c.as_raw_fd(), 1, 17, cred.as_mut_ptr() as *mut u8, &mut len) } != 0 {
            return None;
        }
        (cred[0], cred[1])
    };
    #[cfg(not(target_os = "linux"))]
    let (pid, uid) = {
        extern "C" {
            fn getpeereid(fd: i32, uid: *mut u32, gid: *mut u32) -> i32;
        }
        let (mut uid, mut gid) = (u32::MAX, 0u32);
        if unsafe { getpeereid(c.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return None;
        }
        (0u32, uid)
    };
    let own = unsafe { getuid() };
    // the name a process gives itself (it can set it to anything) and its uid (it cannot)
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
    // drawn in the app's band, so through the same filter as a client's own text
    let comm = wire::drawable(comm.trim());
    let who = format!("{} (uid {uid})", if comm.is_empty() { "?" } else { comm.as_str() });
    if uid == 0 || uid == own {
        Some(Client { pid, root: uid == 0 || test_root(pid, own), who })
    } else {
        None
    }
}

// The phone is aarch64 Linux: a build for it with the test feature does not compile.
#[cfg(all(feature = "test-userns-root", target_arch = "aarch64", target_os = "linux"))]
compile_error!("the feature test-userns-root is for tests on a build host and must never be in a build for the phone");

/// Test builds only (cargo feature `test-userns-root`): a peer of this user that is root
/// inside a user namespace (`unshare -r`) counts as root, so the rules for privileged
/// surfaces can be tried on a machine without root. Never in a build for the phone.
#[cfg(all(feature = "test-userns-root", target_os = "linux"))]
fn test_root(pid: u32, own: u32) -> bool {
    let map = std::fs::read_to_string(format!("/proc/{pid}/uid_map")).unwrap_or_default();
    let mut fields = map.split_whitespace();
    fields.next() == Some("0") && fields.next().and_then(|outside| outside.parse().ok()) == Some(own)
}

#[cfg(not(all(feature = "test-userns-root", target_os = "linux")))]
fn test_root(_pid: u32, _own: u32) -> bool {
    false
}

fn other(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg)
}

/// Binds the socket and starts the socket thread. Fails when another surface already serves
/// the path (its socket is left alone) or the path cannot be made private.
pub fn start(path: PathBuf, default_place: bool) -> io::Result<Link> {
    #[cfg(feature = "test-userns-root")]
    eprintln!("[tinybot-surface] TEST BUILD: a peer that is root of a user namespace counts as root");
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
    let state = State {
        to_ui,
        root: Held::default(),
        agent: Held::default(),
        front: None,
        layers_at: 0,
        runner: None,
        voice: None,
        taps: VecDeque::new(),
        next_seq: 1,
    };
    std::thread::Builder::new()
        .name("tinybot-surface-io".into())
        .spawn(move || run(listener, wake_rx, from_ui, state))?;
    Ok(Link { to_io, from_io, wake: wake_tx, path })
}

struct Conn {
    stream: UnixStream,
    peer: Peer,
    /// this is the voice client's connection: the strip is its own and goes with it
    voice: bool,
    client: Client,
    buf: Vec<u8>,
    asks: Asks,
    closed: bool,
}

/// What a connection waits for.
#[derive(Clone, Copy, Default)]
struct Asks {
    /// asked for taps with `wait` while none were queued: answered when one arrives
    waiting: bool,
    /// with its waiting `taps`, also wants to hear of a change of the layers
    layers: bool,
    /// the count of layer changes this connection was last told at
    layers_at: u64,
}

/// One of the app's two surfaces as the socket thread knows it.
#[derive(Default)]
struct Held {
    surface: Option<String>,
    rev: u64,
    title: String,
    items: usize,
    /// its ids: what an `update` may name
    targets: HashMap<String, Target>,
    /// who set it
    from: String,
}

struct State {
    to_ui: Sender<ToUi>,
    /// root's layer: set by a uid-0 peer, changed and cleared only by one
    root: Held,
    /// the agent's layer: set by a peer of the app's own user
    agent: Held,
    /// the layer on screen; `None` is the idle line
    front: Option<Layer>,
    /// counts the changes of `front` and of the agent's layer, so that a client that was not
    /// waiting when one happened still hears of it
    layers_at: u64,
    /// the pid of the runner while it has a connection: one runner at a time
    runner: Option<u32>,
    /// the pid of the voice client while its connection is open: one at a time
    voice: Option<u32>,
    /// every tap not yet confirmed by a `done`, oldest first
    taps: VecDeque<Tap>,
    next_seq: u64,
}

impl State {
    /// A peer's own layer: all it can set, change, clear or get the taps of.
    fn own(peer: Peer) -> Layer {
        match peer {
            Peer::Root => Layer::Root,
            Peer::User => Layer::Agent,
        }
    }

    fn held(&self, layer: Layer) -> &Held {
        match layer {
            Layer::Root => &self.root,
            Layer::Agent => &self.agent,
        }
    }

    fn held_mut(&mut self, layer: Layer) -> &mut Held {
        match layer {
            Layer::Root => &mut self.root,
            Layer::Agent => &mut self.agent,
        }
    }

    /// `"surface":…,"rev":…` of the peer's own layer.
    fn shown(&self, peer: Peer) -> String {
        let held = self.held(Self::own(peer));
        wire::surface_fields(held.surface.as_deref(), held.rev)
    }

    /// The taps a peer is given: those made on its own layer. The taps on root's layer and
    /// on the agent's are two queues that never mix, so no process of the user reads a tap
    /// meant for root's runner, and the runner never confirms away a tap an agent waits for.
    fn sees(peer: Peer, tap: &Tap) -> bool {
        tap.privileged == (peer == Peer::Root)
    }

    fn pending(&self, peer: Peer) -> usize {
        self.taps.iter().filter(|t| Self::sees(peer, t)).count()
    }

    /// `"privileged":…,"front":…,"agent":…`: whether root's layer exists, which layer is on
    /// screen, and the agent's layer when there is one.
    fn layers(&self) -> String {
        let front = match self.front {
            Some(layer) => wire::json_str(layer.name()),
            None => "null".to_string(),
        };
        let agent = match &self.agent.surface {
            Some(surface) => format!(
                "{{\"surface\":{},\"title\":{},\"from\":{}}}",
                wire::json_str(surface),
                wire::json_str(&self.agent.title),
                wire::json_str(&self.agent.from)
            ),
            None => "null".to_string(),
        };
        format!("\"privileged\":{},\"front\":{front},\"agent\":{agent}", self.root.surface.is_some())
    }

    fn hello(&self, peer: Peer) -> String {
        format!("{{\"type\":\"hello\",\"v\":{},{},{},\"pending\":{}}}\n", wire::VERSION, self.shown(peer), self.layers(), self.pending(peer))
    }

    /// `shown`: whether the peer's own layer is the one on screen.
    fn ack(&self, peer: Peer) -> String {
        format!("{{\"type\":\"ack\",{},\"shown\":{},\"pending\":{}}}\n", self.shown(peer), self.front == Some(Self::own(peer)), self.pending(peer))
    }

    /// Every tap that waits for `peer`, one line each, then the line that ends the answer.
    /// `layers`: a line about the layers goes first.
    fn taps_reply(&self, peer: Peer, layers: bool) -> String {
        let mut out = String::new();
        if layers {
            out.push_str(&format!("{{\"type\":\"layers\",{}}}\n", self.layers()));
        }
        let (mut count, mut last) = (0, 0);
        for tap in self.taps.iter().filter(|t| Self::sees(peer, t)) {
            out.push_str(&tap.line());
            count += 1;
            last = tap.seq;
        }
        out.push_str(&format!("{{\"type\":\"taps\",\"count\":{count},\"seq\":{last}}}\n"));
        out
    }

    /// Puts a layer on screen, or none, and tells the UI. `agent_changed`: the agent's layer
    /// is another than before. Either is news for a client that asked about the layers.
    fn set_front(&mut self, front: Option<Layer>, agent_changed: bool) {
        if self.front != front || agent_changed {
            self.layers_at += 1;
        }
        self.front = front;
        let _ = self.to_ui.send(ToUi::Front(front, self.root.surface.is_some()));
        SignalToUI::set_ui_signal();
    }

    /// An `update` is for exactly the surface and rev of the peer's layer, changes only
    /// labels and readouts, and arms only rows. One wrong id refuses all of it.
    fn check_update(&self, layer: Layer, update: &Update) -> Result<(), WireErr> {
        let held = self.held(layer);
        if held.surface.as_deref() != Some(update.surface.as_str()) || update.rev != held.rev {
            let has = match &held.surface {
                Some(surface) => format!("{surface} rev {}", held.rev),
                None => "nothing".to_string(),
            };
            return Err(WireErr { code: "stale_rev", msg: format!("update is for {} rev {}; the layer has {has}", update.surface, update.rev) });
        }
        let bad = |msg: String| Err(WireErr { code: "bad_item", msg });
        for change in &update.changes {
            match held.targets.get(&change.id) {
                Some(Target::Readout) => {}
                Some(Target::Label) if change.value.is_none() => {}
                Some(Target::Label) => return bad(format!("update {}: a heading or text has no value", change.id)),
                Some(_) => return bad(format!("update {}: only a heading, text or readout is changed in place", change.id)),
                None => return bad(format!("update {}: no item with this id", change.id)),
            }
        }
        for id in &update.arm {
            if held.targets.get(id) != Some(&Target::Row) {
                return bad(format!("update arm {id}: no row or button with this id"));
            }
        }
        Ok(())
    }

    /// Answers one request line. `None`: the answer comes later (a `taps` that waits).
    fn request(&mut self, line: &str, peer: &mut Peer, voice: &mut bool, client: &Client, asks: &mut Asks, waiters: usize) -> Option<String> {
        let request = match wire::parse_request(line) {
            Ok(r) => r,
            Err(e) => return Some(e.line()),
        };
        if let Request::Runner = request {
            // the runner's layer is for one process: root, and the only one that says so
            let refusal = if !client.root {
                Some("only a root client can be the runner".to_string())
            } else {
                self.runner.filter(|pid| *pid != client.pid).map(|pid| format!("a runner is connected already (pid {pid})"))
            };
            if let Some(msg) = refusal {
                return Some(WireErr { code: "privileged", msg }.line());
            }
            self.runner = Some(client.pid);
            *peer = Peer::Root;
            asks.layers_at = self.layers_at;
            return Some(self.hello(Peer::Root));
        }
        let peer = *peer;
        match request {
            Request::Voice => {
                // the strip is for one connection of a root client: the voice service's
                let refusal = if !client.root {
                    Some("only a root client can be the voice".to_string())
                } else if *voice {
                    None
                } else {
                    self.voice.map(|pid| format!("a voice client is connected already (pid {pid})"))
                };
                if let Some(msg) = refusal {
                    return Some(WireErr { code: "privileged", msg }.line());
                }
                self.voice = Some(client.pid);
                *voice = true;
                return Some(self.hello(peer));
            }
            Request::Say(text) => {
                if !*voice {
                    let msg = "voice: only the voice client has the strip (hello with role \"voice\")".to_string();
                    return Some(WireErr { code: "privileged", msg }.line());
                }
                let _ = self.to_ui.send(ToUi::Voice(text));
                SignalToUI::set_ui_signal();
                return Some("{\"type\":\"ok\"}\n".to_string());
            }
            _ => {}
        }
        let who = client.who.as_str();
        let layer = Self::own(peer);
        Some(match request {
            Request::Set(spec) => {
                let held = self.held(layer);
                if held.surface.as_deref() == Some(spec.surface.as_str()) && spec.rev < held.rev {
                    return Some(WireErr { code: "stale_rev", msg: format!("rev {} is older than rev {} of the layer", spec.rev, held.rev) }.line());
                }
                let first = held.surface.is_none();
                *self.held_mut(layer) = Held {
                    surface: Some(spec.surface.clone()),
                    rev: spec.rev,
                    title: spec.title.clone(),
                    items: spec.items.len(),
                    targets: spec.targets().into_iter().collect(),
                    from: who.to_string(),
                };
                let _ = self.to_ui.send(ToUi::Set(layer, spec, who.to_string()));
                // Root's first page takes the screen. An agent's page comes up by itself
                // only when root has none: it never covers one, root shows it (`show`).
                let front = match layer {
                    Layer::Root if first => Some(Layer::Root),
                    Layer::Agent if self.root.surface.is_none() => Some(Layer::Agent),
                    _ => self.front,
                };
                self.set_front(front, layer == Layer::Agent);
                self.ack(peer)
            }
            Request::Update(update) => {
                if let Err(e) = self.check_update(layer, &update) {
                    return Some(e.line());
                }
                if let Some(title) = &update.title {
                    self.held_mut(layer).title.clone_from(title);
                    if layer == Layer::Agent {
                        self.layers_at += 1;
                    }
                }
                let _ = self.to_ui.send(ToUi::Update(layer, update));
                SignalToUI::set_ui_signal();
                self.ack(peer)
            }
            Request::Clear => {
                *self.held_mut(layer) = Held::default();
                let _ = self.to_ui.send(ToUi::Clear(layer));
                // the other layer, if there is one, has the screen now
                let other = match layer {
                    Layer::Root => Layer::Agent,
                    Layer::Agent => Layer::Root,
                };
                let front = if self.held(other).surface.is_some() { Some(other) } else { None };
                self.set_front(front, layer == Layer::Agent);
                self.ack(peer)
            }
            Request::Show(target) => {
                if peer != Peer::Root {
                    return Some(WireErr { code: "privileged", msg: "only root chooses the layer on screen".into() }.line());
                }
                if self.held(target).surface.is_none() {
                    return Some(WireErr { code: "bad_request", msg: format!("show: there is no {} layer", target.name()) }.line());
                }
                self.set_front(Some(target), false);
                self.ack(peer)
            }
            Request::Status => {
                let held = self.held(layer);
                format!(
                    "{{\"type\":\"status\",\"v\":{},{},{},\"title\":{},\"items\":{},\"pending\":{},\"waiting\":{}}}\n",
                    wire::VERSION,
                    self.shown(peer),
                    self.layers(),
                    wire::json_str(&held.title),
                    held.items,
                    self.pending(peer),
                    waiters
                )
            }
            Request::Taps { wait, layers } => {
                asks.layers = layers;
                // a change of the layers this client has not heard of is news as well
                let news = layers && asks.layers_at != self.layers_at;
                if self.pending(peer) == 0 && wait && !news {
                    asks.waiting = true;
                    return None;
                }
                asks.layers_at = self.layers_at;
                self.taps_reply(peer, news)
            }
            Request::Runner | Request::Voice | Request::Say(_) => unreachable!("answered above"),
            Request::Done { seq } => {
                let before = self.taps.len();
                self.taps.retain(|t| !(t.seq <= seq && Self::sees(peer, t)));
                format!("{{\"type\":\"ok\",\"dropped\":{},\"pending\":{}}}\n", before - self.taps.len(), self.pending(peer))
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
                    let (mut asks, mut peer, mut voice) = (self.asks, self.peer, self.voice);
                    let reply = state.request(text, &mut peer, &mut voice, &self.client, &mut asks, waiters);
                    (self.asks, self.peer, self.voice) = (asks, peer, voice);
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
            // whether each new tap is root's
            let mut arrived = Vec::new();
            loop {
                match from_ui.try_recv() {
                    Ok(ToIo::Tap { layer, surface, rev, id, values }) => {
                        let seq = state.next_seq;
                        state.next_seq += 1;
                        let privileged = layer == Layer::Root;
                        state.taps.push_back(Tap { seq, surface, rev, id, values, privileged });
                        arrived.push(privileged);
                    }
                    // the UI has switched already; the layer on screen is root's again
                    Ok(ToIo::Home) => {
                        if state.root.surface.is_some() {
                            state.front = Some(Layer::Root);
                            state.layers_at += 1;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            // the waiting clients one of the new taps is for
            for conn in conns.iter_mut().filter(|c| c.asks.waiting) {
                if arrived.contains(&(conn.peer == Peer::Root)) {
                    conn.asks.waiting = false;
                    let news = conn.asks.layers && conn.asks.layers_at != state.layers_at;
                    conn.asks.layers_at = state.layers_at;
                    conn.send(&state.taps_reply(conn.peer, news));
                }
            }
        }

        // clients that sent something or left (only those that were in this poll)
        let polled = fds.len() - 2;
        let waiters = conns.iter().filter(|c| c.asks.waiting).count();
        for (conn, fd) in conns.iter_mut().zip(&fds[2..]).take(polled) {
            if fd.revents & (POLLIN | POLLHUP | POLLERR) != 0 {
                conn.service(&mut state, waiters);
            }
        }
        conns.retain(|c| !c.closed);
        // a runner that has left makes room for the next one; its page stays until then
        if state.runner.is_some() && !conns.iter().any(|c| c.peer == Peer::Root) {
            state.runner = None;
        }
        // the voice client has left: its strip goes with it, whatever it last said
        if state.voice.is_some() && !conns.iter().any(|c| c.voice) {
            state.voice = None;
            let _ = state.to_ui.send(ToUi::Voice(None));
            SignalToUI::set_ui_signal();
        }

        // the layers changed (a request above, or the user went Home): the waiting clients
        // that asked to hear of it are answered
        for conn in conns.iter_mut().filter(|c| c.asks.waiting && c.asks.layers && c.asks.layers_at != state.layers_at) {
            conn.asks.waiting = false;
            conn.asks.layers_at = state.layers_at;
            conn.send(&state.taps_reply(conn.peer, true));
        }

        // new clients
        if fds[1].revents != 0 {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let Some(client) = client_of(&stream) else {
                            eprintln!("[tinybot-surface] refused a client of another user");
                            continue;
                        };
                        if stream.set_nonblocking(true).is_err() {
                            continue;
                        }
                        // the hello says how the layers are: this client has heard
                        let asks = Asks { layers_at: state.layers_at, ..Asks::default() };
                        // every client starts on the agent's layer, root too
                        let mut conn = Conn { stream, peer: Peer::User, voice: false, client, buf: Vec::new(), asks, closed: false };
                        conn.send(&state.hello(Peer::User));
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
