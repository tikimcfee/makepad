//! `tb-panel run`: one runner for every panel. It puts Home, a panel or a question on the
//! surface, runs what a tap asks for, and writes what happened to state files and records.
//!
//! One thread, the main one, owns all of the runner's state and takes events from a channel:
//! taps (a thread that holds the app's `taps` request open), the end of a try (a worker that
//! runs one try at a time), the end of a stop (its own worker, so Stop is live while a try
//! runs), readout commands (a third worker) and questions (the runner's socket). With no
//! readout on the page the main thread sleeps in the channel; with readouts it wakes once
//! per `every_s`. Nothing here polls the surface or a log.

use crate::app::{self, Conn, Refused};
use crate::ask::{self, Asked, Question};
use crate::exec::{self, Ran, Unit};
use crate::json::Json;
use crate::panel::{self, Does, Item, Panel, Poll};
use crate::records;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

pub struct Options {
    /// panel directories, later ones win over earlier ones for the same name
    pub dirs: Vec<PathBuf>,
    pub socket: Option<String>,
    /// an agent's new page comes straight up while Home is on screen and nothing runs
    pub agent_direct: bool,
    pub state_dir: PathBuf,
    pub tries_dir: PathBuf,
    /// the file that reads `off` while the phone's panel is dark
    pub panel_state: PathBuf,
    /// the input device and the key codes on it that run the actions panels bind to them
    pub key: Option<(PathBuf, Vec<u16>)>,
}

/// The surface ids of the runner's own pages. A panel's surface id is its name.
const HOME: &str = "_home";
const QUESTION: &str = "_question";
/// The most of a result that is shown; the record has the whole line.
const SHOWN_CHARS: usize = 120;
/// The time a check or a readout command gets.
const DIRECT_LIMIT: Duration = Duration::from_secs(2);

enum Event {
    /// The app is there: a connection for requests, a handle on the taps connection, the
    /// app's uid and gid, and the rev of what it shows.
    Connected { conn: Conn, taps: UnixStream, user: Option<(u32, u32)>, shown_rev: u64, layers: Json },
    /// the layer on screen or the agent's layer changed (a `layers` line, or the hello)
    Layers(Json),
    Disconnected(String),
    /// taps that waited in the app before the runner showed its own page: never run
    Dropped(Vec<Json>),
    Tap(Json),
    /// a `LIVE ` line of the try that runs
    Live(String),
    Tried(Tried),
    Stopped(Ran),
    /// a key the runner reads went down (`true`) or came up
    Key(u16, bool),
    /// a short action has ended
    Short(Short, Tried),
    /// something of the runner's own does not work; it goes on without it
    Problem(String),
    Polled { panel: String, id: String, value: String },
    Asked(Asked),
    Terminate,
}

/// A try for the worker: the unit to run and the checks after it.
struct Job {
    unit: String,
    command: Command,
    checks: Vec<(String, Vec<String>)>,
    /// hand the tool's `LIVE ` lines on
    live: bool,
}

/// A short action of a key: it runs beside the tries and is only recorded.
struct Short {
    seq: u64,
    panel: String,
    button: String,
    unit: String,
    key: u16,
    t_tap: f64,
}

/// A follow-up the tool asked for with its exit status: one button, for a while, that runs
/// the same action with the same values and some arguments added.
struct Offer {
    panel: String,
    button: String,
    /// the try that is answered, and its exit status
    seq: u64,
    code: i32,
    /// the values of that try: what the follow-up runs with, whatever the page says by then
    values: Json,
    /// the tool's last line: why it wants to be asked again
    why: String,
    until: Instant,
}

/// How long a follow-up is offered.
const OFFER_TIME: Duration = Duration::from_secs(30);

/// How a try ended.
struct Tried {
    ran: Ran,
    /// systemd's word for how the unit ended, when it was not with status 0
    result: String,
    checks: Vec<(String, i32)>,
    t_start: f64,
    t_end: f64,
}

/// Work for the stop worker.
enum StopJob {
    /// the all-off, after the end of the try's unit when one is named
    Stop { unit: Option<String>, command: Command },
    /// SIGTERM to the tool of this unit: the key that held the try was let go
    Term(String),
}

struct PollJob {
    panel: String,
    id: String,
    argv: Vec<String>,
    user: Option<(u32, u32)>,
}

/// The try in flight.
struct Running {
    seq: u64,
    panel: String,
    button: String,
    unit: String,
    values: Json,
    t_tap: f64,
    /// Stop was asked for while it ran
    stopped: bool,
    /// the key that started it and holds it, by its code
    key: Option<u16>,
    /// the try whose offer this one took up
    parent: Option<u64>,
}

/// The all-off in flight.
struct Stopping {
    seq: u64,
    panel: String,
    unit: String,
    t_tap: f64,
    /// the result item of the try it ended
    result: Option<String>,
    /// the key that asked for it, when it was not the button
    key: Option<u16>,
}

#[derive(Clone)]
struct Lock {
    reason: String,
    /// the try that locked the panel
    seq: u64,
    t: f64,
}

/// The words asked for after a try, shown as a row on the panel's page.
struct Asking {
    panel: String,
    button: String,
    seq: u64,
}

#[derive(Clone, PartialEq)]
enum Page {
    Home,
    Panel(String),
    Question,
}

struct Polled {
    id: String,
    poll: Poll,
    every: Duration,
    due: Instant,
}

struct Runner {
    options: Options,
    tries: Sender<Job>,
    stops: Sender<StopJob>,
    polls: Sender<PollJob>,
    shorts: Sender<(Short, Command)>,

    panels: BTreeMap<String, Panel>,
    /// the files that were left out, and why
    refused: Vec<(String, String)>,
    conn: Option<Conn>,
    /// to end the taps connection when the request connection has failed
    taps: Option<UnixStream>,
    user: Option<(u32, u32)>,
    /// the runner's own failure, for the state file
    error: Option<String>,

    page: Page,
    /// the rev of the last `set`. It starts at the time in milliseconds, so it is above
    /// every rev of a runner that ran before.
    rev: u64,
    /// the rev at which the page on screen was opened: taps from before it are not for it
    since: u64,
    seq: u64,
    running: Option<Running>,
    stopping: Option<Stopping>,
    locks: HashMap<String, Lock>,
    asking: Option<Asking>,
    /// the follow-up on offer
    offer: Option<Offer>,
    /// what results, readouts and the Stop line show now: (panel, item id) to text
    live: HashMap<(String, String), String>,
    /// the last try of each panel, for its state file
    last: HashMap<String, Json>,
    readouts: Vec<Polled>,
    /// readout commands that have not come back yet
    polling: HashSet<String>,

    questions: Vec<Question>,
    /// the answers given while this runner ran: qid to (word, position among the choices)
    answered: HashMap<String, (Option<String>, usize)>,
    waiters: Vec<(String, UnixStream)>,
    /// counts questions, for their ids
    asked: u64,

    /// the agent's layer in the app: its surface, its title and who set it
    agent: Option<(String, String, String)>,
    /// the agent's page is the one on screen, not the runner's
    agent_front: bool,
}

fn shown(text: &str) -> String {
    let text = text.trim_end_matches('\n');
    if text.chars().count() <= SHOWN_CHARS {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(SHOWN_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// `rc N · line`, as a result reads.
fn result_text(ran: &Ran) -> String {
    if ran.line.is_empty() {
        format!("rc {}", ran.rc)
    } else {
        format!("rc {} · {}", ran.rc, ran.line)
    }
}

fn text_item(id: &str, text: &str) -> Json {
    Json::obj([("kind", "text".into()), ("id", id.into()), ("text", text.into())])
}

fn button_item(id: &str, text: &str, style: &str, lock: bool) -> Json {
    Json::obj([("kind", "button".into()), ("id", id.into()), ("text", text.into()), ("style", style.into()), ("lock", lock.into())])
}

/// Up to 256 bytes of a file, read here: no process is started for a readout of a file.
fn read_small(path: &str) -> String {
    let mut bytes = [0u8; 256];
    match std::fs::File::open(path).and_then(|mut file| file.read(&mut bytes)) {
        Ok(n) => {
            let text = String::from_utf8_lossy(&bytes[..n]);
            text.strip_suffix('\n').unwrap_or(&text).to_string()
        }
        Err(e) => format!("unreadable: {e}"),
    }
}

/// Holds the app's `taps` request open and hands each tap on, confirmed first: a tap that
/// was handed on is never given again, so nothing runs twice. Reconnects when the app is
/// gone, backing off to 5 s; while it is gone nothing else of the runner runs on a timer.
fn taps_thread(socket: Option<String>, events: Sender<Event>) {
    let mut pause = Duration::from_millis(200);
    let mut said = String::new();
    loop {
        match attach(&socket, &events) {
            Ok(()) => {
                pause = Duration::from_millis(200);
                said.clear();
            }
            // Why the surface cannot be used is said once, in the state file: a socket that
            // is not there yet is only waiting, but a surface that refuses this process as
            // the runner (it is not root) would otherwise wait for ever without a word.
            Err(e) => {
                let why = e.to_string();
                if why != said && why != "no surface socket" && e.kind() != std::io::ErrorKind::NotFound && e.kind() != std::io::ErrorKind::ConnectionRefused {
                    let _ = events.send(Event::Problem(format!("the surface: {why}")));
                }
                said = why;
            }
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_secs(5));
    }
}

fn io_err(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, msg)
}

/// The taps of one answer to a `taps` request, confirmed. A line about the layers that
/// came with them is handed on at once.
fn read_taps(conn: &mut Conn, first: Json, events: &Sender<Event>) -> std::io::Result<Vec<Json>> {
    let mut taps = Vec::new();
    let mut line = first;
    loop {
        match line.get("type").and_then(|t| t.as_str()) {
            Some("tap") => taps.push(line),
            Some("layers") => drop(events.send(Event::Layers(line))),
            Some("taps") => break,
            _ => return Err(io_err(format!("unexpected answer to taps: {line}"))),
        }
        line = conn.read(Some(Duration::from_secs(10)))?;
    }
    if let Some(seq) = taps.last().and_then(|t| t.get("seq")).and_then(|s| s.as_u64()) {
        conn.send(&Json::obj([("type", "done".into()), ("seq", seq.into())]))?;
        conn.read(Some(Duration::from_secs(10)))?;
    }
    Ok(taps)
}

/// One life of the connection. `Err`: the app was not reached at all.
fn attach(socket: &Option<String>, events: &Sender<Event>) -> std::io::Result<()> {
    let path = app::socket_path(socket.as_deref()).map_err(io_err)?.ok_or_else(|| io_err("no surface socket".into()))?;
    let (mut taps, _hello) = Conn::connect(&path)?;
    // what waited in the app before this runner's page was shown is not run
    taps.send(&Json::obj([("type", "taps".into()), ("wait", false.into())]))?;
    let first = taps.read(Some(Duration::from_secs(10)))?;
    let old = read_taps(&mut taps, first, events)?;
    let (conn, hello) = Conn::connect(&path)?;
    let user = conn.peer();
    let shown_rev = hello.get("rev").and_then(|r| r.as_u64()).unwrap_or(0);
    let _ = events.send(Event::Connected { conn, taps: taps.handle()?, user, shown_rev, layers: hello });
    if !old.is_empty() {
        let _ = events.send(Event::Dropped(old));
    }
    let ended = (|| -> std::io::Result<()> {
        loop {
            taps.send(&Json::obj([("type", "taps".into()), ("wait", true.into()), ("layers", true.into())]))?;
            let first = taps.read(None)?;
            for tap in read_taps(&mut taps, first, events)? {
                let _ = events.send(Event::Tap(tap));
            }
        }
    })();
    let _ = events.send(Event::Disconnected(ended.err().map_or_else(String::new, |e| e.to_string())));
    Ok(())
}

/// Runs one try at a time: the unit, then how it ended when that was not well, then the
/// checks, each directly and with its own limit.
fn tries_thread(jobs: Receiver<Job>, events: Sender<Event>) {
    for job in jobs {
        let t_start = records::now();
        let ran = exec::run_live(job.command, |text| {
            if job.live {
                let _ = events.send(Event::Live(text));
            }
        });
        let t_end = records::now();
        let result = if ran.rc == 0 { String::new() } else { exec::unit_result(&job.unit) };
        let checks = job.checks.iter().map(|(name, argv)| (name.clone(), exec::run_direct(argv, None, DIRECT_LIMIT).rc)).collect();
        let _ = events.send(Event::Tried(Tried { ran, result, checks, t_start, t_end }));
    }
}

/// Stop: the running try's unit is stopped (its ExecStopPost is the all-off), then the
/// panel's own all-off runs.
fn stops_thread(jobs: Receiver<StopJob>, events: Sender<Event>) {
    for job in jobs {
        match job {
            StopJob::Stop { unit, command } => {
                if let Some(unit) = &unit {
                    exec::systemctl_stop(unit);
                }
                let _ = events.send(Event::Stopped(exec::run_to_end(command)));
            }
            // a key let go at once can be ahead of its unit: tried again for a second
            StopJob::Term(unit) => {
                for _ in 0..10 {
                    if exec::systemctl_term(&unit) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

/// Reads the key's input device: a plain read of the kernel's events, no grab, so the
/// compositor sees the key as before. It sleeps in the read until a key of the device moves.
fn key_thread(device: PathBuf, codes: Vec<u16>, events: Sender<Event>) {
    loop {
        let problem = match std::fs::File::open(&device) {
            Ok(mut file) => loop {
                // struct input_event on a 64-bit kernel: a 16-byte time, type, code, value
                let mut event = [0u8; 24];
                if let Err(e) = file.read_exact(&mut event) {
                    break e;
                }
                let kind = u16::from_ne_bytes([event[16], event[17]]);
                let key = u16::from_ne_bytes([event[18], event[19]]);
                let value = i32::from_ne_bytes([event[20], event[21], event[22], event[23]]);
                // EV_KEY; 1 is down, 0 is up, 2 is the kernel's repeat and says nothing new
                if kind == 1 && codes.contains(&key) && (value == 0 || value == 1) {
                    let _ = events.send(Event::Key(key, value == 1));
                }
            },
            Err(e) => e,
        };
        let _ = events.send(Event::Problem(format!("the key device {}: {problem}", device.display())));
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// Runs the short actions of keys, one after the other, beside whatever try runs.
fn shorts_thread(jobs: Receiver<(Short, Command)>, events: Sender<Event>) {
    for (short, command) in jobs {
        let t_start = records::now();
        let ran = exec::run_to_end(command);
        let result = if ran.rc == 0 { String::new() } else { exec::unit_result(&short.unit) };
        let _ = events.send(Event::Short(short, Tried { ran, result, checks: Vec::new(), t_start, t_end: records::now() }));
    }
}

fn polls_thread(jobs: Receiver<PollJob>, events: Sender<Event>) {
    for job in jobs {
        let ran = exec::run_direct(&job.argv, job.user, DIRECT_LIMIT);
        let value = if ran.timed_out { "timed out".to_string() } else { ran.line };
        let _ = events.send(Event::Polled { panel: job.panel, id: job.id, value });
    }
}

/// SIGTERM and SIGINT arrive as one byte on a socket pair: the handler does nothing but
/// write it, and a thread turns it into an event.
fn on_terminate(events: Sender<Event>) -> std::io::Result<()> {
    use std::os::unix::io::IntoRawFd;
    use std::sync::atomic::{AtomicI32, Ordering};
    static WAKE: AtomicI32 = AtomicI32::new(-1);
    extern "C" {
        fn signal(signal: i32, handler: extern "C" fn(i32)) -> usize;
        fn write(fd: i32, bytes: *const std::ffi::c_void, count: usize) -> isize;
    }
    extern "C" fn handler(_signal: i32) {
        unsafe { write(WAKE.load(Ordering::Relaxed), [1u8].as_ptr().cast(), 1) };
    }
    let (wake, mut woken) = UnixStream::pair()?;
    WAKE.store(wake.into_raw_fd(), Ordering::Relaxed);
    unsafe {
        signal(15, handler);
        signal(2, handler);
    }
    std::thread::Builder::new().name("tb-panel-signal".into()).spawn(move || {
        let mut byte = [0u8; 1];
        while woken.read(&mut byte).is_ok_and(|n| n > 0) {
            let _ = events.send(Event::Terminate);
        }
    })?;
    Ok(())
}

pub fn run(options: Options) -> i32 {
    let (events, inbox) = channel();
    let (tries, tries_jobs) = channel();
    let (stops, stops_jobs) = channel();
    let (polls, polls_jobs) = channel();
    let (shorts, shorts_jobs) = channel();
    let spawn = |name: &str, body: Box<dyn FnOnce() + Send>| std::thread::Builder::new().name(name.into()).spawn(body).map(|_| ());
    let started = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&options.state_dir)?;
        let listener = ask::bind(&options.state_dir.join("ask.sock"))?;
        on_terminate(events.clone())?;
        let (socket, to) = (options.socket.clone(), events.clone());
        spawn("tb-panel-taps", Box::new(move || taps_thread(socket, to)))?;
        let to = events.clone();
        spawn("tb-panel-try", Box::new(move || tries_thread(tries_jobs, to)))?;
        let to = events.clone();
        spawn("tb-panel-stop", Box::new(move || stops_thread(stops_jobs, to)))?;
        let to = events.clone();
        spawn("tb-panel-poll", Box::new(move || polls_thread(polls_jobs, to)))?;
        if let Some((device, codes)) = options.key.clone() {
            let to = events.clone();
            spawn("tb-panel-key", Box::new(move || key_thread(device, codes, to)))?;
        }
        let to = events.clone();
        spawn("tb-panel-short", Box::new(move || shorts_thread(shorts_jobs, to)))?;
        let to = events.clone();
        spawn("tb-panel-ask", Box::new(move || ask::serve(listener, |asked| drop(to.send(Event::Asked(asked))))))?;
        Ok(())
    })();
    if let Err(e) = started {
        eprintln!("tb-panel: cannot start: {e}");
        let state = Json::obj([("t", records::now().into()), ("error", format!("cannot start: {e}").into())]);
        let _ = records::write_atomic(&options.state_dir.join("runner.json"), &state.to_string());
        return 1;
    }
    let mut runner = Runner {
        rev: (records::now() * 1000.0) as u64,
        options,
        tries,
        stops,
        polls,
        shorts,
        panels: BTreeMap::new(),
        refused: Vec::new(),
        conn: None,
        taps: None,
        user: None,
        error: None,
        page: Page::Home,
        since: 0,
        seq: 0,
        running: None,
        stopping: None,
        locks: HashMap::new(),
        asking: None,
        offer: None,
        live: HashMap::new(),
        last: HashMap::new(),
        readouts: Vec::new(),
        polling: HashSet::new(),
        questions: Vec::new(),
        answered: HashMap::new(),
        waiters: Vec::new(),
        asked: 0,
        agent: None,
        agent_front: false,
    };
    runner.restore();
    // units a runner that died left running are stopped, with their all-off
    let mut leftover = Command::new("systemctl");
    leftover.args(["stop", "tb-panel-*.service"]);
    exec::run_to_end(leftover);
    runner.reload();
    runner.write_state();
    runner.serve(inbox)
}

impl Runner {
    // ---- what a runner before this one left ----

    /// The counters and the waiting questions of the runner that ran before, from its state
    /// file. The locks are read with the panels (`reload`).
    fn restore(&mut self) {
        let Ok(text) = std::fs::read_to_string(self.options.state_dir.join("runner.json")) else {
            return;
        };
        let Ok(state) = Json::parse(&text) else {
            return;
        };
        self.seq = state.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
        self.asked = state.get("asked").and_then(|s| s.as_u64()).unwrap_or(0);
        for question in state.get("questions").and_then(|q| q.as_arr()).unwrap_or_default() {
            self.questions.extend(Question::from_state(question));
        }
    }

    /// Reads the panel directories again: the files of a later directory replace those of
    /// the same name before it. A file or a directory that is not root's alone is left out.
    fn reload(&mut self) {
        self.panels.clear();
        self.refused.clear();
        for dir in &self.options.dirs {
            let listing = std::fs::metadata(dir).map_err(|e| e.to_string()).and_then(|meta| {
                panel::roots_own(&meta)?;
                std::fs::read_dir(dir).map_err(|e| e.to_string())
            });
            let listing = match listing {
                Ok(listing) => listing,
                // a directory that is not there has no panels; any other trouble is said
                Err(_) if !dir.exists() => continue,
                Err(why) => {
                    self.refused.push((dir.display().to_string(), why));
                    continue;
                }
            };
            let mut files: Vec<PathBuf> = listing.flatten().map(|entry| entry.path()).filter(|path| path.extension().is_some_and(|e| e == "json")).collect();
            files.sort();
            for file in files {
                let name = file.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
                // the file of this name in a later directory stands for the panel, valid or not
                self.panels.remove(&name);
                self.refused.retain(|(path, _)| Path::new(path).file_stem().and_then(|s| s.to_str()) != Some(name.as_str()));
                match Panel::load(&file) {
                    Ok(panel) => {
                        self.panels.insert(name, panel);
                    }
                    Err(why) => self.refused.push((file.display().to_string(), why)),
                }
            }
        }
        // one key runs one action: panels that bind the same code are all left out
        let mut bound: HashMap<u16, Vec<String>> = HashMap::new();
        for panel in self.panels.values() {
            for (code, _) in &panel.keys {
                bound.entry(*code).or_default().push(panel.name.clone());
            }
        }
        let mut clash: BTreeMap<String, Vec<u16>> = BTreeMap::new();
        for (code, names) in bound.into_iter().filter(|(_, names)| names.len() > 1) {
            for name in names {
                clash.entry(name).or_default().push(code);
            }
        }
        for (name, mut codes) in clash {
            codes.sort();
            let codes: Vec<String> = codes.iter().map(u16::to_string).collect();
            self.panels.remove(&name);
            self.refused.push((format!("{name}.json"), format!("key {} bound by another panel too", codes.join(", "))));
        }
        // a lock outlives the runner that set it (within a boot: the state directory is in /run)
        for name in self.panels.keys() {
            if self.locks.contains_key(name) {
                continue;
            }
            let state = std::fs::read_to_string(self.state_file(name)).ok().and_then(|text| Json::parse(&text).ok());
            let Some(state) = state else {
                continue;
            };
            if let Some(lock) = state.get("locked").filter(|l| l.as_obj().is_some()) {
                let reason = lock.get("reason").and_then(|r| r.as_str()).unwrap_or("locked").to_string();
                let seq = lock.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
                self.locks.insert(name.clone(), Lock { reason, seq, t: lock.get("t").and_then(|t| t.as_f64()).unwrap_or(0.0) });
            }
            self.last.insert(name.clone(), state);
        }
    }

    // ---- state files and records ----

    fn state_file(&self, panel: &str) -> PathBuf {
        self.options.state_dir.join(format!("{panel}.json"))
    }

    fn lock_json(lock: Option<&Lock>) -> Json {
        match lock {
            Some(lock) => Json::obj([("reason", lock.reason.as_str().into()), ("seq", lock.seq.into()), ("t", lock.t.into())]),
            None => Json::Null,
        }
    }

    /// The runner's own state file: what it serves, what runs, what waits, what is wrong.
    fn write_state(&mut self) {
        let panels: Vec<Json> = self
            .panels
            .values()
            .map(|p| Json::obj([("name", p.name.as_str().into()), ("title", p.title.as_str().into()), ("locked", self.locks.get(&p.name).map(|l| l.reason.clone()).into())]))
            .collect();
        // the bound keys, and whether the runner reads the one that is bound
        let read: &[u16] = self.options.key.as_ref().map_or(&[], |(_, codes)| codes);
        let keys: Vec<Json> = self
            .panels
            .values()
            .flat_map(|p| p.keys.iter().map(move |(code, button)| (p, *code, button)))
            .map(|(p, code, button)| Json::obj([("code", (code as u64).into()), ("panel", p.name.as_str().into()), ("button", button.as_str().into()), ("read", read.contains(&code).into())]))
            .collect();
        let refused: Vec<Json> = self.refused.iter().map(|(file, why)| Json::obj([("file", file.as_str().into()), ("error", why.as_str().into())])).collect();
        let running = match &self.running {
            Some(run) => Json::obj([("seq", run.seq.into()), ("panel", run.panel.as_str().into()), ("button", run.button.as_str().into()), ("unit", run.unit.as_str().into())]),
            None => Json::Null,
        };
        let page = match &self.page {
            Page::Home => HOME,
            Page::Panel(name) => name,
            Page::Question => QUESTION,
        };
        let state = Json::obj([
            ("t", records::now().into()),
            ("pid", (std::process::id() as u64).into()),
            ("surface", if self.conn.is_some() { "connected" } else { "waiting for the surface" }.into()),
            ("page", page.into()),
            ("panels", Json::Arr(panels)),
            ("refused", Json::Arr(refused)),
            ("agent", match &self.agent {
                Some((surface, title, from)) => Json::obj([("surface", surface.as_str().into()), ("title", title.as_str().into()), ("from", from.as_str().into())]),
                None => Json::Null,
            }),
            ("front", if self.agent_front { "agent" } else { "root" }.into()),
            ("keys", Json::Arr(keys)),
            ("running", running),
            ("questions", Json::Arr(self.questions.iter().map(Question::json).collect())),
            ("seq", self.seq.into()),
            ("asked", self.asked.into()),
            ("error", self.error.clone().into()),
        ]);
        if let Err(e) = records::write_atomic(&self.options.state_dir.join("runner.json"), &state.to_string()) {
            eprintln!("tb-panel: cannot write the state file: {e}");
        }
    }

    /// A panel's state file: its last try and whether it is locked.
    fn write_panel_state(&mut self, panel: &str) {
        let mut state = self.last.get(panel).cloned().unwrap_or_else(|| Json::obj([("panel", panel.into())]));
        state.set("locked", Self::lock_json(self.locks.get(panel)));
        if let Err(e) = records::write_atomic(&self.state_file(panel), &state.to_string()) {
            eprintln!("tb-panel: cannot write {panel}'s state file: {e}");
        }
    }

    fn record(&mut self, file: &str, line: &Json) {
        if let Err(e) = records::append(&self.options.tries_dir.join(format!("{file}.jsonl")), &line.to_string()) {
            eprintln!("tb-panel: cannot write the record {file}.jsonl: {e}");
            self.error = Some(format!("cannot write the record {file}.jsonl: {e}"));
        }
    }

    /// A `try` record, which is also the panel's state: at the start of a try (`rc` null)
    /// and at its end.
    #[allow(clippy::too_many_arguments)]
    fn record_try(&mut self, seq: u64, panel: &str, button: &str, values: &Json, unit: Option<&str>, tried: Option<&Tried>, t_tap: f64, stopped: bool, extra: &[(&str, Json)]) {
        let mut line = Json::obj([
            ("t", "try".into()),
            ("seq", seq.into()),
            ("panel", panel.into()),
            ("button", button.into()),
            ("values", values.clone()),
            ("unit", unit.into()),
            ("rc", Json::Null),
            ("line", Json::Null),
            ("state", Json::Null),
            ("checks", Json::Arr(Vec::new())),
            ("t_tap", t_tap.into()),
            ("t_start", Json::Null),
            ("t_end", Json::Null),
            ("stopped", stopped.into()),
        ]);
        for (name, value) in extra {
            line.set(name, value.clone());
        }
        if let Some(tried) = tried {
            line.set("rc", tried.ran.rc.into());
            line.set("line", tried.ran.line.as_str().into());
            line.set("state", tried.ran.state.clone().unwrap_or(Json::Null));
            if !tried.result.is_empty() {
                line.set("result", tried.result.as_str().into());
            }
            let checks = tried.checks.iter().map(|(name, rc)| Json::obj([("name", name.as_str().into()), ("rc", (*rc).into())])).collect();
            line.set("checks", Json::Arr(checks));
            line.set("t_start", tried.t_start.into());
            line.set("t_end", tried.t_end.into());
            self.record(panel, &line);
        }
        self.last.insert(panel.to_string(), line);
        self.write_panel_state(panel);
    }

    // ---- the page ----

    fn surface(&self) -> &str {
        match &self.page {
            Page::Home => HOME,
            Page::Panel(name) => name,
            Page::Question => QUESTION,
        }
    }

    /// What the header says after the page's own title.
    fn title(&self, title: &str) -> String {
        match self.questions.len() {
            0 => title.to_string(),
            1 => format!("{title} · 1 question"),
            n => format!("{title} · {n} questions"),
        }
    }

    fn live(&self, panel: &str, id: &str) -> Option<&String> {
        self.live.get(&(panel.to_string(), id.to_string()))
    }

    fn stop_items(&self, panel: &str, items: &mut Vec<Json>) {
        items.push(button_item("_stop", "Stop / all off", "danger", false));
        items.push(text_item("_stop_res", self.live(panel, "_stop_res").map_or("", |s| s.as_str())));
    }

    fn home_items(&self) -> (String, Vec<Json>) {
        let mut items = Vec::new();
        match self.questions.len() {
            0 => {}
            1 => items.push(button_item("_questions", "1 question waiting", "primary", false)),
            n => items.push(button_item("_questions", &format!("{n} questions waiting"), "primary", false)),
        }
        // an agent's page waits behind root's pages until it is opened from here
        if let Some((_, title, from)) = &self.agent {
            items.push(button_item("_agent", &format!("Agent: {}", if title.is_empty() { "a page" } else { title.as_str() }), "secondary", false));
            items.push(text_item("_agent_from", &format!("from {from}")));
        }
        if let Some(run) = &self.running {
            items.push(text_item("_running", &format!("running: {}/{}", run.panel, run.button)));
            if self.panels.get(&run.panel).is_some_and(|p| p.stop.is_some()) {
                self.stop_items(&run.panel, &mut items);
            }
        }
        let mut panels: Vec<&Panel> = self.panels.values().collect();
        panels.sort_by(|a, b| a.order.total_cmp(&b.order).then_with(|| a.title.cmp(&b.title)));
        for panel in panels {
            items.push(button_item(&format!("_open_{}", panel.name), &panel.title, "secondary", false));
            match self.locks.get(&panel.name) {
                Some(lock) => items.push(text_item(&format!("_about_{}", panel.name), &format!("locked: {}", lock.reason))),
                None if !panel.summary.is_empty() => items.push(text_item(&format!("_about_{}", panel.name), &panel.summary)),
                None => {}
            }
            // the all-off of a panel is seen to have run from here too
            if let Some(stop) = self.live(&panel.name, "_stop_res").filter(|text| !text.is_empty()) {
                items.push(text_item(&format!("_stopped_{}", panel.name), &format!("last Stop: {stop}")));
            }
        }
        if self.panels.is_empty() {
            items.push(text_item("_none", "no panels"));
        }
        if !self.refused.is_empty() {
            let names: Vec<&str> = self.refused.iter().map(|(file, _)| file.rsplit('/').next().unwrap_or(file)).collect();
            items.push(text_item("_refused", &format!("{} refused: {}", if names.len() == 1 { "1 panel file" } else { "panel files" }, names.join(", "))));
        }
        (self.title("tinybot"), items)
    }

    /// A panel's page: its items, then what the runner adds. `keep_user`: the user is on
    /// this page already, and what they set stays.
    fn panel_items(&self, panel: &Panel, keep_user: bool) -> (String, Vec<Json>) {
        let locked = self.locks.get(&panel.name);
        let asking = self.asking.as_ref().filter(|a| a.panel == panel.name);
        // the answer row goes under the try's result, or under its button when it has none
        let ask_after = asking.and_then(|a| match &panel.button(&a.button)?.does {
            Does::Action(action) => Some((action.result.clone().unwrap_or_else(|| a.button.clone()), &action.ask, &action.ask_text)),
            _ => None,
        });
        // a follow-up on offer: its button, with its action's row dimmed under it
        let offer = self.offer.as_ref().filter(|o| o.panel == panel.name && locked.is_none());
        let offered = offer.and_then(|o| match &panel.button(&o.button)?.does {
            Does::Action(action) => action.retry.iter().find(|(code, _)| *code == o.code).map(|(_, retry)| (o, action, retry)),
            _ => None,
        });
        let mut items = Vec::new();
        for item in &panel.items {
            let mut is_ask_place = false;
            match item {
                Item::Row { id, buttons } => {
                    // a locked panel has no action buttons
                    let kept: Vec<&panel::Button> = buttons.iter().filter(|b| locked.is_none() || !matches!(b.does, Does::Action(_))).collect();
                    is_ask_place = ask_after.as_ref().is_some_and(|(after, ..)| buttons.iter().any(|b| b.id == *after));
                    match &offered {
                        Some((offer, action, retry)) if buttons.iter().any(|b| b.id == offer.button) => {
                            // what the tool said, then the one button that answers it, first
                            // and primary, saying the values it will run with
                            let with: Vec<String> = action.value_ids().iter().map(|id| format!("{id} {}", offer.values.get(id).map_or(String::new(), |v| v.as_str().map_or_else(|| v.to_string(), str::to_string)))).collect();
                            let label = if with.is_empty() { retry.text.clone() } else { format!("{} · {}", retry.text, with.join(", ")) };
                            items.push(text_item("_offer_why", &offer.why));
                            items.push(Json::obj([("kind", "button".into()), ("id", "_offer".into()), ("text", shown(&label).into()), ("style", "primary".into()), ("confirm", retry.confirm.into())]));
                            // the row's own buttons stay live, but they are not the answer
                            let dimmed = kept.iter().map(|b| {
                                let mut button = b.json.clone();
                                if matches!(b.does, Does::Action(_)) {
                                    button.set("dim", true.into());
                                    button.set("style", "secondary".into());
                                }
                                button
                            });
                            let mut row = Json::obj([("kind", "row".into())]);
                            if !id.is_empty() {
                                row.set("id", id.as_str().into());
                            }
                            row.set("buttons", Json::Arr(dimmed.collect()));
                            items.push(row);
                            items.push(text_item("_offer_or", &format!("or use '{}' above", retry.text)));
                        }
                        _ if !kept.is_empty() => items.push(panel::row_json(id, kept.into_iter())),
                        _ => {}
                    }
                }
                Item::Readout { id, .. } | Item::Plain { id, .. } => {
                    let mut json = item.json(keep_user);
                    if let Some(text) = self.live(&panel.name, id) {
                        json.set(if matches!(item, Item::Readout { .. }) { "value" } else { "text" }, text.as_str().into());
                    }
                    is_ask_place = ask_after.as_ref().is_some_and(|(after, ..)| after == id);
                    items.push(json);
                }
                Item::Input { .. } => items.push(item.json(keep_user)),
            }
            if let (true, Some((_, words, question))) = (is_ask_place, &ask_after) {
                if let Some(question) = question {
                    items.push(text_item("_ask_text", question));
                }
                let buttons = words.iter().enumerate().map(|(i, word)| Json::obj([("id", format!("_ans_{i}").into()), ("text", word.as_str().into())]));
                items.push(Json::obj([("kind", "row".into()), ("id", "_ask".into()), ("buttons", Json::Arr(buttons.collect()))]));
            }
        }
        if let Some(lock) = locked {
            items.push(text_item("_locked", &format!("locked: {}", lock.reason)));
            items.push(Json::obj([("kind", "button".into()), ("id", "_unlock".into()), ("text", "Unlock".into()), ("confirm", true.into())]));
        }
        if panel.stop.is_some() {
            self.stop_items(&panel.name, &mut items);
        }
        items.push(button_item("_home", "‹ Home", "secondary", false));
        (self.title(&panel.title), items)
    }

    fn question_items(&self, question: &Question) -> (String, Vec<Json>) {
        let mut items = vec![
            Json::obj([("kind", "heading".into()), ("text", format!("{} asks", question.from).into())]),
            Json::obj([("kind", "text".into()), ("text", question.text.as_str().into())]),
        ];
        // the name above is the asker's word; this is what the socket says of it
        if let Some(asked_by) = question.asked_by() {
            items.push(Json::obj([("kind", "readout".into()), ("id", "_asked_by".into()), ("text", "asked by".into()), ("value", asked_by.into())]));
        }
        for (path, sha) in &question.covers {
            items.push(Json::obj([("kind", "readout".into()), ("id", format!("_cover_{}", items.len()).into()), ("text", path.as_str().into()), ("value", sha[..sha.len().min(16)].into())]));
        }
        let choices = question.choices.iter().enumerate().map(|(i, word)| {
            Json::obj([("id", format!("_choice_{i}").into()), ("text", word.as_str().into()), ("style", if i == 0 { "primary" } else { "secondary" }.into())])
        });
        items.push(Json::obj([("kind", "row".into()), ("id", "_choices".into()), ("buttons", Json::Arr(choices.collect()))]));
        items.push(button_item("_home", "‹ Home", "secondary", false));
        let title = match self.questions.len() {
            1 => "Question".to_string(),
            n => format!("Question · {} more", n - 1),
        };
        (title, items)
    }

    /// Opens a page: from now on only taps on it count.
    fn open(&mut self, page: Page) {
        // the panel files are read again at every open, so an edit shows at the next one
        if !matches!(page, Page::Question) {
            self.reload();
        }
        self.page = match page {
            Page::Panel(name) if !self.panels.contains_key(&name) => Page::Home,
            Page::Question if self.questions.is_empty() => Page::Home,
            page => page,
        };
        self.since = self.rev + 1;
        self.show(false);
        self.write_state();
    }

    /// Sends the page as a `set`. `keep_user`: it is the page on screen with something added
    /// or taken away.
    fn show(&mut self, keep_user: bool) {
        self.readouts.clear();
        let (title, items) = match &self.page {
            Page::Home => self.home_items(),
            Page::Question => match self.questions.first() {
                Some(question) => self.question_items(question),
                None => self.home_items(),
            },
            Page::Panel(name) => match self.panels.get(name) {
                Some(panel) => {
                    let now = Instant::now();
                    for item in &panel.items {
                        if let Item::Readout { id, poll: Some(poll), every_s, .. } = item {
                            self.readouts.push(Polled { id: id.clone(), poll: poll.clone(), every: Duration::from_secs_f64(*every_s), due: now });
                        }
                    }
                    self.panel_items(panel, keep_user)
                }
                None => self.home_items(),
            },
        };
        self.rev += 1;
        let set = Json::obj([
            ("type", "set".into()),
            ("surface", self.surface().into()),
            ("rev", self.rev.into()),
            ("title", title.into()),
            ("items", Json::Arr(items)),
        ]);
        self.send(&set);
    }

    /// Changes items of the page on screen in place: (id, text, value), rows to arm, and the
    /// header when `title` is given.
    fn update(&mut self, changes: &[(&str, Option<&str>, Option<&str>)], arm: &[&str], title: Option<String>) {
        let mut items = Vec::new();
        for (id, text, value) in changes {
            let mut change = Json::Obj(Vec::new());
            if let Some(text) = text {
                change.set("text", (*text).into());
            }
            if let Some(value) = value {
                change.set("value", (*value).into());
            }
            items.push((id.to_string(), change));
        }
        let mut update = Json::obj([("type", "update".into()), ("surface", self.surface().into()), ("rev", self.rev.into())]);
        if let Some(title) = title {
            update.set("title", title.into());
        }
        update.set("items", Json::Obj(items));
        update.set("arm", arm.iter().map(|id| id.to_string()).collect::<Vec<_>>().into());
        self.send(&update);
    }

    fn send(&mut self, request: &Json) {
        let Some(conn) = &mut self.conn else {
            return;
        };
        match conn.request(request) {
            Ok(_) => {}
            Err(Refused::Err { code, msg }) => {
                eprintln!("tb-panel: the surface refused a request: {code}: {msg}");
                self.error = Some(format!("the surface refused a request: {code}: {msg}"));
            }
            Err(Refused::Gone(why)) => {
                eprintln!("tb-panel: the surface is gone: {why}");
                self.lose_surface();
            }
        }
    }

    /// The request connection failed: the taps connection is ended too, and its thread
    /// connects both again.
    fn lose_surface(&mut self) {
        self.conn = None;
        if let Some(taps) = self.taps.take() {
            let _ = taps.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Shows a result: in the item `id` of `panel`, now if that page is on screen, and at
    /// the page's next `set` in any case. `arm`: the button whose row takes presses again.
    fn show_result(&mut self, panel: &str, id: &str, text: &str, arm: Option<&str>) {
        let text = shown(text);
        self.live.insert((panel.to_string(), id.to_string()), text.clone());
        if self.page != Page::Panel(panel.to_string()) {
            return;
        }
        let readout = self.panels.get(panel).is_some_and(|p| p.is_readout(id));
        let change = if readout { (id, None, Some(text.as_str())) } else { (id, Some(text.as_str()), None) };
        self.update(&[change], &arm.into_iter().collect::<Vec<_>>(), None);
    }

    /// Something that Home or the header shows has changed.
    fn refresh_counts(&mut self) {
        match &self.page {
            Page::Home => self.show(true),
            Page::Question => {}
            Page::Panel(name) => {
                let title = self.panels.get(name).map(|p| self.title(&p.title));
                self.update(&[], &[], title);
            }
        }
    }

    // ---- taps ----

    fn tap(&mut self, tap: &Json) {
        let field = |name: &str| tap.get(name).and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let (surface, id) = (field("surface"), field("id"));
        let rev = tap.get("rev").and_then(|r| r.as_u64()).unwrap_or(0);
        // a tap on a page that is no longer the one on screen is not acted on
        if surface != self.surface() || rev < self.since {
            return;
        }
        let values = tap.get("values").cloned().unwrap_or(Json::Obj(Vec::new()));
        match (self.page.clone(), id.as_str()) {
            (_, "_home") => self.open(Page::Home),
            (Page::Home, "_questions") => self.open(Page::Question),
            (Page::Home, "_agent") => self.send(&Json::obj([("type", "show".into()), ("layer", "agent".into())])),
            (Page::Home, "_stop") => {
                if let Some(panel) = self.running.as_ref().map(|run| run.panel.clone()) {
                    self.stop(&panel, None);
                }
            }
            (Page::Home, id) => {
                if let Some(name) = id.strip_prefix("_open_") {
                    self.open(Page::Panel(name.to_string()));
                }
            }
            (Page::Question, id) => {
                if let Some(choice) = id.strip_prefix("_choice_").and_then(|n| n.parse().ok()) {
                    self.answer_question(choice);
                }
            }
            (Page::Panel(panel), "_stop") => self.stop(&panel, None),
            (Page::Panel(panel), "_unlock") => self.unlock(&panel),
            (Page::Panel(panel), "_offer") => self.take_offer(&panel, &values),
            (Page::Panel(panel), id) => match id.strip_prefix("_ans_").and_then(|n| n.parse().ok()) {
                Some(word) => self.answer_try(&panel, word),
                None => self.press(&panel, id, values),
            },
        }
    }

    /// A tap on a button of a panel file.
    fn press(&mut self, panel_name: &str, button_id: &str, values: Json) {
        let t_tap = records::now();
        let Some(panel) = self.panels.get(panel_name) else {
            return;
        };
        let Some(button) = panel.button(button_id) else {
            return;
        };
        let action = match &button.does {
            Does::Open(name) => {
                let name = name.clone();
                return self.open(Page::Panel(name));
            }
            Does::Nothing => {
                // nothing to run: the tap is a record, and the row takes presses again
                self.seq += 1;
                self.record_try(self.seq, panel_name, button_id, &values, None, None, t_tap, false, &[]);
                let line = self.last.get(panel_name).cloned().unwrap_or(Json::Null);
                self.record(panel_name, &line);
                return self.update(&[], &[button_id], None);
            }
            Does::Action(action) => action,
        };
        let result = action.result.clone();
        // a tap that starts no try says why where the result goes, and its row is armed
        if let Err(why) = self.start(panel_name, button_id, &values, t_tap, None, &[], None) {
            match &result {
                Some(result) => self.show_result(panel_name, result, why, Some(button_id)),
                None => self.update(&[], &[button_id], None),
            }
        }
    }

    /// Starts the try of an action button, for a tap or for a key. `Err`: why nothing runs.
    /// `add`: arguments after the action's own, and `parent`: the try whose offer this is.
    #[allow(clippy::too_many_arguments)]
    fn start(&mut self, panel_name: &str, button_id: &str, values: &Json, t_tap: f64, key: Option<u16>, add: &[String], parent: Option<u64>) -> Result<(), &'static str> {
        // another action than the one on offer ends the offer
        if self.offer.as_ref().is_some_and(|offer| parent != Some(offer.seq)) {
            self.expire_offer("another action");
        }
        let panel = self.panels.get(panel_name).ok_or("no such panel")?;
        let Some(Does::Action(action)) = panel.button(button_id).map(|b| &b.does) else {
            return Err("no such action");
        };
        if self.locks.contains_key(panel_name) {
            return Err("locked");
        }
        if self.running.is_some() || self.stopping.is_some() {
            return Err("busy");
        }
        let mut argv = action.fill(panel, values).ok_or("refused: stale value")?;
        argv.extend_from_slice(add);
        let result = action.result.clone();
        self.seq += 1;
        let unit = format!("tb-panel-{panel_name}-{}", self.seq);
        // as the app's user unless the file says root; a runner that is not root cannot choose
        let uid = if action.as_root { None } else { self.user.map(|(uid, _)| uid) };
        let command = exec::systemd_run(&Unit { name: &unit, argv: &argv, timeout_s: action.timeout_s, stop_post: &action.stop_post, uid });
        let checks = action.checks.iter().map(|check| (check.name.clone(), check.argv.clone())).collect();
        let live = action.live.is_some();
        // a try that starts ends the wait for an answer to the one before it
        let had_answer_row = self.asking.take().is_some_and(|a| a.panel == panel_name);
        self.running = Some(Running { seq: self.seq, panel: panel_name.to_string(), button: button_id.to_string(), unit: unit.clone(), values: values.clone(), t_tap, stopped: false, key, parent });
        self.record_try(self.seq, panel_name, button_id, values, Some(&unit), None, t_tap, false, &Self::try_extra(key, parent));
        self.write_state();
        if self.tries.send(Job { unit, command, checks, live }).is_err() {
            self.error = Some("the try worker is gone".into());
        }
        if let Some(result) = &result {
            self.show_result(panel_name, result, "running…", None);
        }
        // a try a key started can begin under any page: Home says what runs
        if (had_answer_row && self.page == Page::Panel(panel_name.to_string())) || self.page == Page::Home {
            self.show(true);
        }
        Ok(())
    }

    /// What a try's record says beyond the usual: the key that started it, the try it follows.
    fn try_extra(key: Option<u16>, parent: Option<u64>) -> Vec<(&'static str, Json)> {
        let mut extra = Vec::new();
        if let Some(code) = key {
            extra.push(("key", (code as u64).into()));
        }
        if let Some(seq) = parent {
            extra.push(("parent", seq.into()));
        }
        extra
    }

    /// The offer is over: recorded, and off the page.
    fn expire_offer(&mut self, why: &str) {
        let Some(offer) = self.offer.take() else {
            return;
        };
        self.record(&offer.panel, &Json::obj([("t", "expired".into()), ("seq", offer.seq.into()), ("why", why.into()), ("t_expired", records::now().into())]));
        if self.page == Page::Panel(offer.panel) {
            self.show(true);
        }
    }

    /// The offer's button: the same action again with the values of the try it answers and
    /// the offer's arguments added. If the page's values are no longer those, nothing runs:
    /// what would run is not what is on the page.
    fn take_offer(&mut self, panel_name: &str, now: &Json) {
        let Some(offer) = self.offer.as_ref().filter(|o| o.panel == panel_name) else {
            return;
        };
        let (button, seq, code, values) = (offer.button.clone(), offer.seq, offer.code, offer.values.clone());
        let Some(Does::Action(action)) = self.panels.get(panel_name).and_then(|p| p.button(&button)).map(|b| &b.does) else {
            return self.expire_offer("the panel changed");
        };
        let Some((_, retry)) = action.retry.iter().find(|(c, _)| *c == code) else {
            return self.expire_offer("the panel changed");
        };
        let (add, result) = (retry.add.clone(), action.result.clone());
        if action.value_ids().iter().any(|id| now.get(id) != values.get(id)) {
            self.expire_offer("a value changed");
            if let Some(result) = &result {
                self.show_result(panel_name, result, "offer expired: a value changed", None);
            }
            return;
        }
        let started = self.start(panel_name, &button, &values, records::now(), None, &add, Some(seq));
        // taken or not, the offer is spent; the page loses its button
        self.offer = None;
        if let (Err(why), Some(result)) = (started, &result) {
            self.live.insert((panel_name.to_string(), result.clone()), why.to_string());
        }
        self.show(true);
    }

    /// A key the runner reads went down: its action runs, whatever page is on screen. It
    /// does not take the screen.
    fn key_down(&mut self, code: u16) {
        let bound = self.panels.values().find_map(|p| p.keys.iter().find(|(c, _)| *c == code).map(|(_, button)| (p.name.clone(), button.clone())));
        let Some((panel, button)) = bound else {
            return;
        };
        // a key on the panel's Stop is its Stop button: always live, whatever runs or is locked
        if button == "_stop" {
            return self.stop(&panel, Some(code));
        }
        let t = records::now();
        let short = matches!(self.panels.get(&panel).and_then(|p| p.button(&button)).map(|b| &b.does), Some(Does::Action(action)) if action.short);
        let started = if short { self.start_short(&panel, &button, code, t) } else { self.start(&panel, &button, &Json::Obj(Vec::new()), t, Some(code), &[], None) };
        if let Err(why) = started {
            self.record(&panel, &Json::obj([("t", "dropped".into()), ("key", (code as u64).into()), ("button", button.as_str().into()), ("why", why.into()), ("t_seen", t.into())]));
        }
    }

    /// A short action: beside the tries, with no part in one-at-a-time, no lock of its own
    /// and no end at key up. A locked panel holds it back like any action: a lock means a
    /// person must look first, and what a short action does (a volume blip) is a sound too.
    fn start_short(&mut self, panel_name: &str, button_id: &str, key: u16, t_tap: f64) -> Result<(), &'static str> {
        if self.locks.contains_key(panel_name) {
            return Err("locked");
        }
        let panel = self.panels.get(panel_name).ok_or("no such panel")?;
        let Some(Does::Action(action)) = panel.button(button_id).map(|b| &b.does) else {
            return Err("no such action");
        };
        let argv = action.fill(panel, &Json::Obj(Vec::new())).ok_or("refused: stale value")?;
        self.seq += 1;
        let unit = format!("tb-panel-{panel_name}-{}", self.seq);
        let uid = if action.as_root { None } else { self.user.map(|(uid, _)| uid) };
        let command = exec::systemd_run(&Unit { name: &unit, argv: &argv, timeout_s: action.timeout_s, stop_post: &action.stop_post, uid });
        let short = Short { seq: self.seq, panel: panel_name.to_string(), button: button_id.to_string(), unit, key, t_tap };
        self.write_state();
        self.shorts.send((short, command)).map_err(|_| "the worker for short actions is gone")
    }

    /// A short action has ended: a record, and its result where the action names one.
    fn short_done(&mut self, short: Short, tried: Tried) {
        let result = match self.panels.get(&short.panel).and_then(|p| p.button(&short.button)).map(|b| &b.does) {
            Some(Does::Action(action)) => action.result.clone(),
            _ => None,
        };
        // the panel's last try stays its state: a short action is a record only
        let before = self.last.get(&short.panel).cloned();
        let extra = [("key", Json::from(short.key as u64)), ("short", Json::Bool(true))];
        self.record_try(short.seq, &short.panel, &short.button, &Json::Obj(Vec::new()), Some(&short.unit), Some(&tried), short.t_tap, false, &extra);
        if let Some(before) = before {
            self.last.insert(short.panel.clone(), before);
            self.write_panel_state(&short.panel);
        }
        if let Some(result) = &result {
            self.show_result(&short.panel, result, &result_text(&tried.ran), None);
        }
    }

    /// The key came up: the tool it holds is asked to end (SIGTERM). The try then ends as
    /// any other, with the tool's result.
    fn key_up(&mut self, code: u16) {
        if let Some(run) = self.running.as_ref().filter(|run| run.key == Some(code) && !run.stopped) {
            let _ = self.stops.send(StopJob::Term(run.unit.clone()));
        }
    }

    /// A `LIVE ` line of the try that runs: into the item its action names.
    fn live_line(&mut self, text: &str) {
        let Some(run) = &self.running else {
            return;
        };
        let (panel, button) = (run.panel.clone(), run.button.clone());
        let item = match self.panels.get(&panel).and_then(|p| p.button(&button)).map(|b| &b.does) {
            Some(Does::Action(action)) => action.live.clone(),
            _ => None,
        };
        if let Some(item) = item {
            self.show_result(&panel, &item, text, None);
        }
    }

    /// A try has ended: its result, its record, and whether the panel is locked.
    fn tried(&mut self, tried: Tried) {
        let Some(run) = self.running.take() else {
            return;
        };
        let (result, asks, exit_3, offers) = match self.panels.get(&run.panel).and_then(|p| p.button(&run.button)).map(|b| &b.does) {
            Some(Does::Action(action)) => (action.result.clone(), !action.ask.is_empty(), tried.ran.rc == 3, action.retry.iter().any(|(code, _)| *code == tried.ran.rc)),
            _ => (None, false, false, false),
        };
        let failed: Vec<&str> = tried.checks.iter().filter(|(_, rc)| *rc != 0).map(|(name, _)| name.as_str()).collect();
        let timed_out = tried.result == "timeout";
        let what = format!("{}/{}", run.panel, run.button);
        // why the next try needs a person first; a stop the user asked for is not one
        let lock = if run.stopped {
            None
        } else if exit_3 {
            Some(format!("{what} ended with 3: not set back"))
        } else if timed_out {
            Some(format!("{what} ran into its time limit"))
        } else if !failed.is_empty() {
            Some(format!("{what}: failed checks: {}", failed.join(", ")))
        } else {
            None
        };
        let mut text = if run.stopped { "stopped".to_string() } else { result_text(&tried.ran) };
        if timed_out && !run.stopped {
            text.push_str(" · time limit");
        }
        if !failed.is_empty() {
            text.push_str(&format!(" · failed checks: {}", failed.join(", ")));
        }
        self.record_try(run.seq, &run.panel, &run.button, &run.values, Some(&run.unit), Some(&tried), run.t_tap, run.stopped, &Self::try_extra(run.key, run.parent));
        if let Some(reason) = lock {
            self.locks.insert(run.panel.clone(), Lock { reason, seq: run.seq, t: records::now() });
            self.write_panel_state(&run.panel);
        }
        // a try the tool did not carry out ("ask me again") is not one to ask about
        if asks && !offers && !run.stopped {
            self.asking = Some(Asking { panel: run.panel.clone(), button: run.button.clone(), seq: run.seq });
        }
        // the tool answered "only if you mean it": the follow-up is on offer for a while
        if offers && !run.stopped && !self.locks.contains_key(&run.panel) {
            self.offer = Some(Offer {
                panel: run.panel.clone(),
                button: run.button.clone(),
                seq: run.seq,
                code: tried.ran.rc,
                values: run.values.clone(),
                why: shown(&tried.ran.line),
                until: Instant::now() + OFFER_TIME,
            });
        }
        self.write_state();
        // a tool that could not set things back gets the all-off at once
        if exit_3 && !run.stopped {
            self.stop(&run.panel, None);
        }
        let on_page = self.page == Page::Panel(run.panel.clone());
        let changes_page = self.locks.contains_key(&run.panel) || self.asking.is_some() || self.offer.is_some();
        match &result {
            Some(result) if on_page && !changes_page => self.show_result(&run.panel, result, &text, Some(&run.button)),
            Some(result) => {
                self.live.insert((run.panel.clone(), result.clone()), shown(&text));
            }
            None if on_page && !changes_page => self.update(&[], &[run.button.as_str()], None),
            None => {}
        }
        // a lock or an answer row is a page with other items; Home says what runs
        if (on_page && changes_page) || self.page == Page::Home {
            self.show(true);
        }
    }

    /// Stop: the all-off of `panel`, and first the end of its try when one runs. Never asks,
    /// never waits for a try, and a lock does not hold it. `key`: a key asked for it.
    fn stop(&mut self, panel_name: &str, key: Option<u16>) {
        let Some(stop) = self.panels.get(panel_name).and_then(|p| p.stop.as_ref()) else {
            return;
        };
        if self.stopping.is_some() {
            return;
        }
        let t_tap = records::now();
        self.seq += 1;
        let unit = format!("tb-panel-{panel_name}-stop-{}", self.seq);
        let command = exec::systemd_run(&Unit { name: &unit, argv: &stop.argv, timeout_s: stop.timeout_s, stop_post: &[], uid: None });
        let mut ends = None;
        let mut result = None;
        if let Some(run) = self.running.as_mut().filter(|run| run.panel == panel_name) {
            run.stopped = true;
            ends = Some(run.unit.clone());
            result = match self.panels.get(panel_name).and_then(|p| p.button(&run.button)).map(|b| &b.does) {
                Some(Does::Action(action)) => action.result.clone(),
                _ => None,
            };
        }
        self.stopping = Some(Stopping { seq: self.seq, panel: panel_name.to_string(), unit: unit.clone(), t_tap, result, key });
        if self.stops.send(StopJob::Stop { unit: ends, command }).is_err() {
            self.error = Some("the stop worker is gone".into());
        }
        self.show_stop(panel_name, "stopping…");
    }

    /// The line under the Stop button, on the panel's page or on Home.
    fn show_stop(&mut self, panel: &str, text: &str) {
        let text = shown(text);
        self.live.insert((panel.to_string(), "_stop_res".to_string()), text.clone());
        let on_screen = match &self.page {
            Page::Panel(name) => name == panel,
            Page::Home => self.running.as_ref().is_some_and(|run| run.panel == panel),
            Page::Question => false,
        };
        if on_screen {
            self.update(&[("_stop_res", Some(text.as_str()), None)], &[], None);
        }
    }

    fn stopped(&mut self, ran: Ran) {
        let Some(stop) = self.stopping.take() else {
            return;
        };
        let text = result_text(&ran);
        let tried = Tried { ran, result: String::new(), checks: Vec::new(), t_start: stop.t_tap, t_end: records::now() };
        // the last try of the panel stays its state; the stop is a record of its own
        let before = self.last.get(&stop.panel).cloned();
        self.record_try(stop.seq, &stop.panel, "_stop", &Json::Obj(Vec::new()), Some(&stop.unit), Some(&tried), stop.t_tap, false, &Self::try_extra(stop.key, None));
        if let Some(before) = before {
            self.last.insert(stop.panel.clone(), before);
            self.write_panel_state(&stop.panel);
        }
        self.show_stop(&stop.panel, &text);
        if let Some(result) = &stop.result {
            self.show_result(&stop.panel, result, &format!("stopped · {text}"), None);
        }
        if self.page == Page::Home {
            self.show(true);
        }
    }

    fn unlock(&mut self, panel: &str) {
        let Some(lock) = self.locks.remove(panel) else {
            return;
        };
        self.record(panel, &Json::obj([("t", "answer".into()), ("seq", lock.seq.into()), ("word", "unlock".into()), ("t_answer", records::now().into())]));
        self.write_panel_state(panel);
        self.write_state();
        self.show(true);
    }

    /// The user's word for a try: a record, and for some words the all-off and a lock.
    fn answer_try(&mut self, panel_name: &str, word: usize) {
        let Some(asking) = self.asking.take().filter(|a| a.panel == panel_name) else {
            return;
        };
        let action = match self.panels.get(panel_name).and_then(|p| p.button(&asking.button)).map(|b| &b.does) {
            Some(Does::Action(action)) => action,
            _ => return,
        };
        let Some(word) = action.ask.get(word).cloned() else {
            return;
        };
        let stops = action.stop_on.contains(&word);
        self.record(panel_name, &Json::obj([("t", "answer".into()), ("seq", asking.seq.into()), ("word", word.as_str().into()), ("t_answer", records::now().into())]));
        if stops {
            let reason = format!("{panel_name}/{}: answered {word}", asking.button);
            self.locks.insert(panel_name.to_string(), Lock { reason, seq: asking.seq, t: records::now() });
            self.write_panel_state(panel_name);
            self.write_state();
            self.stop(panel_name, None);
        }
        self.show(true);
    }

    // ---- questions ----

    fn asked(&mut self, asked: Asked) {
        match asked {
            Asked::Question(mut question, from) => {
                self.asked += 1;
                // the time makes the id new across boots, the count across one second
                question.qid = format!("q{}-{}", question.t_asked as u64, self.asked);
                ask::reply(&from, &Json::obj([("type", "asked".into()), ("qid", question.qid.as_str().into())]));
                self.questions.push(question);
                self.write_state();
                // a question never takes the screen: it is a count until the user opens it
                self.refresh_counts();
            }
            Asked::Wait(qid, from) => match self.answered.get(&qid) {
                Some((word, index)) => ask::reply(&from, &Self::answer_line(&qid, word.as_deref(), *index)),
                None if self.questions.iter().any(|q| q.qid == qid) => self.waiters.push((qid, from)),
                None => ask::refuse(&from, &format!("no question {qid} waits here")),
            },
        }
    }

    fn answer_line(qid: &str, word: Option<&str>, index: usize) -> Json {
        Json::obj([("type", "answer".into()), ("qid", qid.into()), ("answer", word.into()), ("index", (index as u64).into())])
    }

    /// The user's answer to the question on screen. If a covered file is no longer what the
    /// question was asked about, the answer is void: it was given for other bytes.
    fn answer_question(&mut self, choice: usize) {
        if self.questions.first().is_none_or(|q| choice >= q.choices.len()) {
            return;
        }
        let question = self.questions.remove(0);
        let changed = question.changed();
        let word = changed.is_empty().then(|| question.choices[choice].clone());
        let mut line = question.json();
        if let Json::Obj(members) = &mut line {
            members.insert(0, ("t".to_string(), "question".into()));
        }
        line.set("answer", word.clone().into());
        if !changed.is_empty() {
            line.set("void", format!("file changed: {}", changed.join(", ")).into());
        }
        line.set("t_answered", records::now().into());
        self.record("_questions", &line);
        let reply = Self::answer_line(&question.qid, word.as_deref(), choice);
        self.waiters.retain(|(qid, to)| {
            if *qid == question.qid {
                ask::reply(to, &reply);
            }
            *qid != question.qid
        });
        self.answered.insert(question.qid, (word, choice));
        self.open(if self.questions.is_empty() { Page::Home } else { Page::Question });
    }

    // ---- the agent's layer ----

    /// What the app says of its layers. An agent's page is listed on Home; a new one comes
    /// straight up only while Home is on screen and nothing runs, and never when the user
    /// has just left it.
    fn layers(&mut self, layers: &Json) {
        let agent = layers.get("agent").filter(|a| a.as_obj().is_some()).map(|agent| {
            let field = |name: &str| agent.get(name).and_then(|v| v.as_str()).unwrap_or_default().to_string();
            (field("surface"), field("title"), field("from"))
        });
        let was_front = self.agent_front;
        self.agent_front = layers.get("front").and_then(|f| f.as_str()) == Some("agent");
        let new_page = agent.is_some() && agent != self.agent;
        let changed = agent != self.agent;
        self.agent = agent;
        self.write_state();
        if self.agent_front {
            return;
        }
        if changed && self.page == Page::Home {
            self.show(true);
        }
        let quiet = self.page == Page::Home && self.running.is_none() && self.stopping.is_none();
        if self.options.agent_direct && new_page && !was_front && quiet {
            self.send(&Json::obj([("type", "show".into()), ("layer", "agent".into())]));
        }
    }

    // ---- readouts ----

    /// The readouts that are due. With the phone's panel off nothing is read.
    fn poll(&mut self) {
        let now = Instant::now();
        let Page::Panel(panel) = self.page.clone() else {
            return;
        };
        // behind an agent's page the runner's page is not on screen either
        let off = self.agent_front || std::fs::read_to_string(&self.options.panel_state).is_ok_and(|state| state.trim() == "off");
        let mut read = Vec::new();
        for readout in self.readouts.iter_mut().filter(|r| r.due <= now) {
            readout.due = now + readout.every;
            if off {
                continue;
            }
            match &readout.poll {
                Poll::File(path) => read.push((readout.id.clone(), read_small(path))),
                Poll::Argv(argv) => {
                    // one at a time per readout: a command that takes its 2 s is not stacked
                    if self.polling.insert(readout.id.clone()) {
                        let _ = self.polls.send(PollJob { panel: panel.clone(), id: readout.id.clone(), argv: argv.clone(), user: self.user });
                    }
                }
            }
        }
        self.show_readouts(&panel, read);
    }

    /// The values that changed go to the page, in one update; a value that is the same is
    /// not sent, so a steady readout does not wake the app.
    fn show_readouts(&mut self, panel: &str, read: Vec<(String, String)>) {
        let mut changed = Vec::new();
        for (id, value) in read {
            let value = shown(&value);
            if self.live(panel, &id) != Some(&value) {
                self.live.insert((panel.to_string(), id.clone()), value.clone());
                changed.push((id, value));
            }
        }
        if !changed.is_empty() && self.page == Page::Panel(panel.to_string()) {
            let changes: Vec<(&str, Option<&str>, Option<&str>)> = changed.iter().map(|(id, value)| (id.as_str(), None, Some(value.as_str()))).collect();
            self.update(&changes, &[], None);
        }
    }

    // ---- the loop ----

    fn serve(&mut self, inbox: Receiver<Event>) -> i32 {
        loop {
            // with the surface there and readouts on the page, wake for the next one
            let due = if self.conn.is_some() { self.readouts.iter().map(|r| r.due).min() } else { None };
            // and for the end of an offer
            let due = match (due, self.offer.as_ref().map(|offer| offer.until)) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let event = match due {
                None => inbox.recv().ok(),
                Some(due) => match inbox.recv_timeout(due.saturating_duration_since(Instant::now())) {
                    Ok(event) => Some(event),
                    Err(RecvTimeoutError::Timeout) => {
                        if self.offer.as_ref().is_some_and(|offer| offer.until <= Instant::now()) {
                            self.expire_offer("30 s");
                        }
                        if self.conn.is_some() {
                            self.poll();
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => None,
                },
            };
            match event {
                None => return 1,
                Some(Event::Layers(layers)) => self.layers(&layers),
                Some(Event::Connected { conn, taps, user, shown_rev, layers }) => {
                    self.agent = None;
                    self.agent_front = false;
                    self.layers(&layers);
                    // whatever kept the surface away before is over
                    if self.error.as_ref().is_some_and(|e| e.starts_with("the surface: ")) {
                        self.error = None;
                    }
                    self.conn = Some(conn);
                    self.taps = Some(taps);
                    self.user = user;
                    // never under what the app shows (a clock set back since a runner before
                    // this one): a lower rev for the same surface would be refused as stale
                    self.rev = self.rev.max(shown_rev);
                    // an app that came back is empty: the page is sent again
                    let page = self.page.clone();
                    self.open(page);
                }
                Some(Event::Disconnected(why)) => {
                    if !why.is_empty() {
                        eprintln!("tb-panel: the surface is gone: {why}");
                    }
                    self.conn = None;
                    self.taps = None;
                    self.write_state();
                }
                Some(Event::Dropped(taps)) => {
                    for tap in taps {
                        let surface = tap.get("surface").and_then(|s| s.as_str()).unwrap_or_default().to_string();
                        let file = if panel::valid_name(&surface) { surface.as_str() } else { "_runner" };
                        self.record(file, &Json::obj([("t", "dropped".into()), ("tap", tap.clone()), ("t_seen", records::now().into())]));
                    }
                }
                Some(Event::Tap(tap)) => self.tap(&tap),
                Some(Event::Live(text)) => self.live_line(&text),
                Some(Event::Key(code, true)) => self.key_down(code),
                Some(Event::Key(code, false)) => self.key_up(code),
                Some(Event::Short(short, tried)) => self.short_done(short, tried),
                Some(Event::Problem(what)) => {
                    eprintln!("tb-panel: {what}");
                    self.error = Some(what);
                    self.write_state();
                }
                Some(Event::Tried(tried)) => self.tried(tried),
                Some(Event::Stopped(ran)) => self.stopped(ran),
                Some(Event::Polled { panel, id, value }) => {
                    self.polling.remove(&id);
                    self.show_readouts(&panel, vec![(id, value)]);
                }
                Some(Event::Asked(asked)) => self.asked(asked),
                Some(Event::Terminate) => {
                    // a try that runs is ended with its all-off, the screen is given back
                    if let Some(run) = &self.running {
                        exec::systemctl_stop(&run.unit);
                    }
                    self.send(&Json::obj([("type", "clear".into())]));
                    self.conn = None;
                    self.write_state();
                    let _ = std::fs::remove_file(self.options.state_dir.join("ask.sock"));
                    return 0;
                }
            }
        }
    }
}
