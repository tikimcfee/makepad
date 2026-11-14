//! Running programs: never through a shell, always as a program and its arguments. Actions
//! and the all-off run as transient systemd units (`systemd-run`), so they have a clean
//! environment, a time limit systemd enforces and an ExecStopPost; checks and readout
//! commands are short and run directly, with a limit kept here.

use crate::json::Json;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// What a program left behind.
#[derive(Clone, Debug, Default)]
pub struct Ran {
    /// its exit status; 128 + the signal when it was killed; -1 when it could not be started
    pub rc: i32,
    /// its last non-empty stdout line, whole
    pub line: String,
    /// the object of its `TBSTATE {json}` stdout line, the state the tool read back
    pub state: Option<Json>,
    /// it ran into its time limit
    pub timed_out: bool,
}

/// The most of one stdout line that is kept. A line can be of any length; what is recorded
/// of a longer one is its start.
const LINE_KEPT: usize = 64 * 1024;

/// Takes a program's stdout as it comes and keeps what the runner wants of it: the last
/// non-empty line and the `TBSTATE` line.
#[derive(Default)]
struct Lines {
    current: Vec<u8>,
    last: String,
    state: Option<Json>,
}

impl Lines {
    fn take(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                self.end_line();
            } else if self.current.len() < LINE_KEPT {
                self.current.push(byte);
            }
        }
    }

    fn end_line(&mut self) {
        let line = String::from_utf8_lossy(&self.current).trim_end_matches('\r').to_string();
        self.current.clear();
        if let Some(state) = line.strip_prefix("TBSTATE ") {
            // only an object is a state; anything else is just a line the tool printed
            if let Ok(state @ Json::Obj(_)) = Json::parse(state) {
                self.state = Some(state);
                return;
            }
        }
        if !line.trim().is_empty() {
            self.last = line;
        }
    }

    fn done(mut self, rc: i32, timed_out: bool) -> Ran {
        if !self.current.is_empty() {
            self.end_line();
        }
        Ran { rc, line: self.last, state: self.state, timed_out }
    }
}

fn rc_of(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

fn failed(what: &str, e: std::io::Error) -> Ran {
    Ran { rc: -1, line: format!("cannot run {what}: {e}"), ..Ran::default() }
}

/// Runs a command to its end and reads its stdout meanwhile. For `systemd-run --wait` and
/// `systemctl`, whose time limits are systemd's.
pub fn run_to_end(mut command: Command) -> Ran {
    let what = command.get_program().to_string_lossy().into_owned();
    let mut child = match command.stdin(Stdio::null()).stdout(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(e) => return failed(&what, e),
    };
    let mut lines = Lines::default();
    if let Some(mut out) = child.stdout.take() {
        let mut piece = [0u8; 4096];
        loop {
            match out.read(&mut piece) {
                Ok(0) => break,
                Ok(n) => lines.take(&piece[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }
    match child.wait() {
        Ok(status) => lines.done(rc_of(status), false),
        Err(e) => failed(&what, e),
    }
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

extern "C" {
    fn poll(fds: *mut PollFd, n: std::ffi::c_ulong, timeout_ms: i32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

/// Runs `argv` directly and gives it `limit`: at the limit its whole process group is
/// killed. `user`: the uid and gid to run as, when this process is root.
pub fn run_direct(argv: &[String], user: Option<(u32, u32)>, limit: Duration) -> Ran {
    let Some(program) = argv.first() else {
        return Ran { rc: -1, line: "an empty argv".into(), ..Ran::default() };
    };
    let mut command = Command::new(program);
    command.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).process_group(0);
    if let Some((uid, gid)) = user {
        command.uid(uid).gid(gid);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return failed(program, e),
    };
    let deadline = Instant::now() + limit;
    let mut lines = Lines::default();
    let mut timed_out = false;
    let mut out = child.stdout.take();
    // until the program closes its stdout
    while let Some(pipe) = &mut out {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            timed_out = true;
            break;
        }
        let mut fd = PollFd { fd: pipe.as_raw_fd(), events: 1, revents: 0 };
        if unsafe { poll(&mut fd, 1, left.as_millis().min(i32::MAX as u128) as i32 + 1) } <= 0 {
            continue;
        }
        let mut piece = [0u8; 4096];
        match pipe.read(&mut piece) {
            Ok(0) => out = None,
            Ok(n) => lines.take(&piece[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => out = None,
        }
    }
    // and until it has ended: a program that closed stdout and stays is waited for in
    // short steps, which only happens with a program that misbehaves
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if timed_out || Instant::now() >= deadline => {
                timed_out = true;
                // SIGKILL to the group the program was started as the leader of
                unsafe { kill(-(child.id() as i32), 9) };
                break child.wait().ok();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break None,
        }
    };
    lines.done(status.map_or(-1, rc_of), timed_out)
}

/// One argument in the quoting of a systemd unit's command line: in double quotes, with
/// `\` and `"` escaped.
fn unit_quoted(arg: &str) -> String {
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A unit command line for an argv. The `:` in front tells systemd to leave `$` alone, so
/// the program gets exactly these arguments.
pub fn unit_command(argv: &[String]) -> String {
    format!(":{}", argv.iter().map(|arg| unit_quoted(arg)).collect::<Vec<_>>().join(" "))
}

/// What a transient unit is asked to run.
pub struct Unit<'a> {
    pub name: &'a str,
    pub argv: &'a [String],
    pub timeout_s: f64,
    pub stop_post: &'a [String],
    /// run as this uid instead of root
    pub uid: Option<u32>,
}

/// `systemd-run` for a unit: it waits for the unit's end, hands on the program's stdout and
/// exit status, and leaves the arguments alone (`--expand-environment=no`; systemd would
/// otherwise replace `$NAME` in them). No `--collect`: a unit that failed stays loaded until
/// its result has been read (see `unit_result`).
pub fn systemd_run(unit: &Unit) -> Command {
    let mut command = Command::new("systemd-run");
    command.arg(format!("--unit={}", unit.name)).args(["--wait", "--pipe", "--quiet", "--expand-environment=no"]);
    command.args(["-p", &format!("RuntimeMaxSec={}", unit.timeout_s), "-p", "TimeoutStopSec=2"]);
    if !unit.stop_post.is_empty() {
        command.args(["-p", &format!("ExecStopPost={}", unit_command(unit.stop_post))]);
    }
    if let Some(uid) = unit.uid {
        command.arg(format!("--uid={uid}"));
    }
    command.arg("--").args(unit.argv);
    command
}

/// How a unit that did not end with status 0 ended, in systemd's word (`exit-code`,
/// `timeout`, `signal`, …): the unit's `Result` property, read from the manager. Then the
/// unit is let go.
pub fn unit_result(name: &str) -> String {
    let unit = format!("{name}.service");
    let mut show = Command::new("systemctl");
    show.args(["show", "-p", "Result", "--value", &unit]);
    let result = run_to_end(show).line;
    let mut reset = Command::new("systemctl");
    reset.args(["reset-failed", &unit]).stderr(Stdio::null());
    run_to_end(reset);
    result
}

pub fn systemctl_stop(name: &str) -> Ran {
    let mut stop = Command::new("systemctl");
    stop.args(["stop", &format!("{name}.service")]);
    run_to_end(stop)
}
