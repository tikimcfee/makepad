//! The runner's side of the surface socket: lines of JSON to the app and back (the protocol
//! is in the README next to the app).

use crate::json::Json;
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The protocol version the runner speaks (`v` in the app's `hello`).
pub const VERSION: u64 = 2;

/// Why a request got no `ack`.
pub enum Refused {
    /// the connection is of no use any more
    Gone(String),
    /// the app's `err` line
    Err { code: String, msg: String },
}

pub struct Conn {
    stream: UnixStream,
    buf: Vec<u8>,
}

fn other(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg)
}

impl Conn {
    /// Connects and reads the app's hello.
    pub fn connect(path: &PathBuf) -> io::Result<(Conn, Json)> {
        let mut conn = Conn { stream: UnixStream::connect(path)?, buf: Vec::new() };
        let hello = conn.read(Some(Duration::from_secs(5)))?;
        if hello.get("type").and_then(|t| t.as_str()) != Some("hello") || hello.get("v").and_then(|v| v.as_u64()) != Some(VERSION) {
            return Err(other(format!("unexpected greeting: {hello}")));
        }
        Ok((conn, hello))
    }

    /// A second handle on the same connection, to shut it down from another thread.
    pub fn handle(&self) -> io::Result<UnixStream> {
        self.stream.try_clone()
    }

    /// The uid and gid of the process that serves the socket: the app's user.
    pub fn peer(&self) -> Option<(u32, u32)> {
        extern "C" {
            fn getsockopt(fd: i32, level: i32, name: i32, val: *mut u8, len: *mut u32) -> i32;
        }
        // SOL_SOCKET 1, SO_PEERCRED 17: struct ucred { pid, uid, gid }
        let mut cred = [0u32; 3];
        let mut len = 12u32;
        let known = unsafe { getsockopt(self.stream.as_raw_fd(), 1, 17, cred.as_mut_ptr() as *mut u8, &mut len) } == 0;
        known.then_some((cred[1], cred[2]))
    }

    pub fn send(&mut self, line: &Json) -> io::Result<()> {
        self.stream.write_all(format!("{line}\n").as_bytes())
    }

    /// The next line. `None` waits for ever.
    pub fn read(&mut self, timeout: Option<Duration>) -> io::Result<Json> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let end = loop {
            if let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
                break end;
            }
            let left = match deadline {
                None => None,
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "the surface did not answer"));
                    }
                    Some(left)
                }
            };
            self.stream.set_read_timeout(left)?;
            let mut piece = [0u8; 16 * 1024];
            match self.stream.read(&mut piece) {
                Ok(0) => return Err(other("the surface closed the connection".into())),
                Ok(n) => self.buf.extend_from_slice(&piece[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "the surface did not answer"))
                }
                Err(e) => return Err(e),
            }
        };
        let rest = self.buf.split_off(end + 1);
        let mut line = std::mem::replace(&mut self.buf, rest);
        line.pop();
        Json::parse(&String::from_utf8_lossy(&line)).map_err(|e| other(format!("not JSON from the surface: {e}")))
    }

    /// One request and its answer.
    pub fn request(&mut self, request: &Json) -> Result<Json, Refused> {
        let gone = |e: io::Error| Refused::Gone(e.to_string());
        self.send(request).map_err(gone)?;
        let answer = self.read(Some(Duration::from_secs(10))).map_err(gone)?;
        if answer.get("type").and_then(|t| t.as_str()) == Some("err") {
            let field = |name: &str| answer.get(name).and_then(|v| v.as_str()).unwrap_or_default().to_string();
            return Err(Refused::Err { code: field("code"), msg: field("msg") });
        }
        Ok(answer)
    }
}

/// Where the app's socket is: `--socket PATH`, else `$TINYBOT_SURFACE_SOCK`, else
/// `$XDG_RUNTIME_DIR/tinybot/surface.sock`. A system service has no XDG_RUNTIME_DIR of the
/// app's user: then it is the one `/run/user/*/tinybot/surface.sock` there is, and more
/// than one is refused rather than guessed between. `Ok(None)`: no surface runs yet.
pub fn socket_path(arg: Option<&str>) -> Result<Option<PathBuf>, String> {
    if let Some(path) = arg.filter(|p| !p.is_empty()) {
        return Ok(Some(PathBuf::from(path)));
    }
    if let Some(path) = std::env::var_os("TINYBOT_SURFACE_SOCK").filter(|p| !p.is_empty()) {
        return Ok(Some(PathBuf::from(path)));
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|p| !p.is_empty()) {
        return Ok(Some(PathBuf::from(runtime).join("tinybot").join("surface.sock")));
    }
    let mut found = Vec::new();
    for user in std::fs::read_dir("/run/user").into_iter().flatten().flatten() {
        let path = user.path().join("tinybot").join("surface.sock");
        if path.exists() {
            found.push(path);
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(format!("more than one surface socket under /run/user ({}): name one with --socket", found.len())),
    }
}
