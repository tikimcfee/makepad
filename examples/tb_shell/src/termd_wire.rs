//! Pure-Rust client codec and RPC helpers for `g3d-termd` Protocol 1.5.
//!
//! Shared by both `tb-shell` (`termd_view`, `termd_bridge`) and the `tbui termd` CLI.
//! Implements the 12-byte little-endian binary frame wire (`HELLO`, `WELCOME`,
//! `REQUEST`, `REPLY`, `INPUT`, `OUTPUT`, `EVENT`, `REPAINT`, `ERROR`), socket
//! directory & `SO_PEERCRED` verification, on-demand daemon startup, and
//! `create` / `list` / `paste` (`paste_stream`) / `key` / `kill` operations.

#![allow(dead_code)]

use makepad_micro_serde::{DeJson, JsonValue, SerJson};
use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const FRAME_HELLO: u8 = 0x01;
pub const FRAME_REQUEST: u8 = 0x02;
pub const FRAME_INPUT: u8 = 0x03;
pub const FRAME_WELCOME: u8 = 0x81;
pub const FRAME_REPLY: u8 = 0x82;
pub const FRAME_OUTPUT: u8 = 0x83;
pub const FRAME_EVENT: u8 = 0x84;
pub const FRAME_REPAINT: u8 = 0x85;
pub const FRAME_ERROR: u8 = 0xFF;

pub const DEFAULT_MAX_FRAME: usize = 16 << 20;
const PASTE_CHUNK_MAX_BYTES: usize = 256 << 10;

const SOL_SOCKET: i32 = 1;
const SO_PEERCRED: i32 = 17;

extern "C" {
    fn getuid() -> u32;
    fn getsockopt(fd: i32, level: i32, name: i32, val: *mut c_void, len: *mut u32) -> i32;
}

pub fn current_uid() -> u32 {
    unsafe { getuid() }
}

pub fn local_hostname() -> String {
    if let Ok(h) = std::env::var("TB_LOCAL_HOST") {
        let t = h.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    "local".to_string()
}

pub fn validate_name(what: &str, s: &str) -> Result<(), String> {
    if s.is_empty() || s.len() > 64 {
        return Err(format!("{what} {s:?} must be 1..64 bytes of [A-Za-z0-9._-]"));
    }
    for b in s.bytes() {
        let ok = b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-';
        if !ok {
            return Err(format!("{what} {s:?} must be 1..64 bytes of [A-Za-z0-9._-]"));
        }
    }
    Ok(())
}

/// Returns `/run/user/<uid>` if it is a real directory owned by `<uid>` with mode `0700`.
pub fn user_runtime_dir() -> Option<PathBuf> {
    let uid = current_uid();
    let dir = PathBuf::from(format!("/run/user/{uid}"));
    let meta = std::fs::symlink_metadata(&dir).ok()?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return None;
    }
    if meta.uid() != uid || (meta.mode() & 0o077) != 0 {
        return None;
    }
    Some(dir)
}

/// Resolves the `g3d-termd` socket directory per `PROTOCOL.md` ("Transport"):
/// 1. `$G3D_TERMD_DIR` when set and non-empty
/// 2. `/run/user/<uid>/g3d` when `/run/user/<uid>` is owned `0700`
/// 3. `/tmp/g3d-<uid>` otherwise
pub fn socket_dir() -> PathBuf {
    if let Ok(d) = std::env::var("G3D_TERMD_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Some(rt) = user_runtime_dir() {
        return rt.join("g3d");
    }
    PathBuf::from(format!("/tmp/g3d-{}", current_uid()))
}

pub fn socket_path(ns: &str) -> PathBuf {
    socket_dir().join(format!("termd-{ns}.sock"))
}

#[derive(Clone, Debug)]
pub enum DialError {
    NotRunning(String),
    UnsafeSocket(String),
    Wire { code: String, message: String },
    Other(String),
}

impl DialError {
    pub fn is_not_running(&self) -> bool {
        matches!(self, Self::NotRunning(_))
    }

    pub fn is_code(&self, want: &str) -> bool {
        matches!(self, Self::Wire { code, .. } if code == want)
    }
}

impl std::fmt::Display for DialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning(m) => write!(f, "{m}"),
            Self::UnsafeSocket(m) => write!(f, "unsafe termd socket: {m}"),
            Self::Wire { code, message } => write!(f, "{code}: {message}"),
            Self::Other(m) => write!(f, "{m}"),
        }
    }
}

pub fn check_socket_dir(dir: &Path) -> Result<(), DialError> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(DialError::NotRunning(format!(
                "termd is not running (no directory {})",
                dir.display()
            )));
        }
        Err(e) => {
            return Err(DialError::UnsafeSocket(format!(
                "stat {}: {e}",
                dir.display()
            )));
        }
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(DialError::UnsafeSocket(format!(
            "{} is not a real directory",
            dir.display()
        )));
    }
    let uid = current_uid();
    if meta.uid() != uid {
        return Err(DialError::UnsafeSocket(format!(
            "{} owned by uid {}, expected {uid}",
            dir.display(),
            meta.uid()
        )));
    }
    let perm = meta.mode() & 0o777;
    if (perm & 0o077) != 0 {
        return Err(DialError::UnsafeSocket(format!(
            "{} has mode {:04o} (group/other bits must be 0)",
            dir.display(),
            perm
        )));
    }
    Ok(())
}

pub fn verify_peer_creds(stream: &UnixStream) -> Result<(), DialError> {
    #[repr(C)]
    #[derive(Default)]
    struct Ucred {
        pid: i32,
        uid: u32,
        gid: u32,
    }
    let mut cred = Ucred::default();
    let mut len = std::mem::size_of::<Ucred>() as u32;
    let rc = unsafe {
        getsockopt(
            stream.as_raw_fd(),
            SOL_SOCKET,
            SO_PEERCRED,
            &mut cred as *mut Ucred as *mut c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(DialError::UnsafeSocket("SO_PEERCRED failed".into()));
    }
    let uid = current_uid();
    if cred.uid != uid {
        return Err(DialError::UnsafeSocket(format!(
            "socket peer uid {} != current uid {uid}",
            cred.uid
        )));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub ftype: u8,
    pub stream: u32,
    pub payload: Vec<u8>,
}

pub fn encode_frame(ftype: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + payload.len());
    let len = (8 + payload.len()) as u32;
    out.extend_from_slice(&len.to_le_bytes());
    out.push(ftype);
    out.push(0);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&stream.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn read_frame<R: Read>(r: &mut R, max_frame: usize) -> Result<Frame, DialError> {
    let mut head = [0u8; 12];
    r.read_exact(&mut head).map_err(|e| {
        if e.kind() == ErrorKind::UnexpectedEof {
            DialError::Other("termd closed the connection".into())
        } else {
            DialError::Other(format!("termd read: {e}"))
        }
    })?;
    let n = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
    let cap = if max_frame > 0 { max_frame } else { DEFAULT_MAX_FRAME };
    if n < 8 || n - 8 > cap {
        return Err(DialError::Other(format!("termd bad frame length {n}")));
    }
    let ftype = head[4];
    let stream = u32::from_le_bytes([head[8], head[9], head[10], head[11]]);
    let mut payload = vec![0u8; n - 8];
    if !payload.is_empty() {
        r.read_exact(&mut payload)
            .map_err(|e| DialError::Other(format!("termd payload read: {e}")))?;
    }
    Ok(Frame { ftype, stream, payload })
}

pub fn j_obj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::Object(
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect::<HashMap<_, _>>(),
    )
}

pub fn j_str(v: Option<&JsonValue>) -> Option<&str> {
    match v? {
        JsonValue::String(s) => Some(s.as_str()),
        _ => None,
    }
}

pub fn j_u64(v: Option<&JsonValue>) -> Option<u64> {
    match v? {
        JsonValue::U64(n) => Some(*n),
        JsonValue::I64(n) if *n >= 0 => Some(*n as u64),
        JsonValue::F64(f) if *f >= 0.0 => Some(*f as u64),
        _ => None,
    }
}

pub fn j_i64(v: Option<&JsonValue>) -> Option<i64> {
    match v? {
        JsonValue::I64(n) => Some(*n),
        JsonValue::U64(n) => Some(*n as i64),
        JsonValue::F64(f) => Some(*f as i64),
        _ => None,
    }
}

pub fn j_bool(v: Option<&JsonValue>) -> Option<bool> {
    match v? {
        JsonValue::Bool(b) => Some(*b),
        _ => None,
    }
}

pub fn j_get<'a>(v: &'a JsonValue, k: &str) -> Option<&'a JsonValue> {
    match v {
        JsonValue::Object(m) => m.get(k),
        _ => None,
    }
}

fn parse_wire_error(val: &JsonValue) -> DialError {
    let err_obj = j_get(val, "error").unwrap_or(val);
    let code = j_str(j_get(err_obj, "code"))
        .unwrap_or("error")
        .to_string();
    let message = j_str(j_get(err_obj, "message"))
        .unwrap_or("termd error")
        .to_string();
    DialError::Wire { code, message }
}

#[derive(Clone, Debug, Default)]
pub struct WelcomeInfo {
    pub proto: [u32; 2],
    pub instance: String,
    pub max_frame: usize,
    pub features: Vec<String>,
}

impl WelcomeInfo {
    pub fn has(&self, feat: &str) -> bool {
        self.features.iter().any(|f| f == feat)
    }

    fn from_json(val: &JsonValue) -> Self {
        let mut proto = [1, 0];
        if let Some(JsonValue::Array(arr)) = j_get(val, "proto") {
            if arr.len() >= 2 {
                proto[0] = j_u64(arr.first()).unwrap_or(1) as u32;
                proto[1] = j_u64(arr.get(1)).unwrap_or(0) as u32;
            }
        }
        let instance = j_str(j_get(val, "instance")).unwrap_or("").to_string();
        let max_frame = j_u64(j_get(val, "max_frame"))
            .map(|n| n as usize)
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_MAX_FRAME);
        let features = match j_get(val, "features") {
            Some(JsonValue::Array(arr)) => arr
                .iter()
                .filter_map(|x| match x {
                    JsonValue::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        Self {
            proto,
            instance,
            max_frame,
            features,
        }
    }
}

/// Performs the Unix socket connect + `SO_PEERCRED` check + `HELLO` / `WELCOME` handshake,
/// returning `(raw_write_stream, buffered_reader, WelcomeInfo)`.
pub fn handshake_ns(
    ns: &str,
    client_name: &str,
) -> Result<(UnixStream, BufReader<UnixStream>, WelcomeInfo), DialError> {
    validate_name("termd namespace", ns).map_err(DialError::Other)?;
    let path = socket_path(ns);
    if let Some(parent) = path.parent() {
        check_socket_dir(parent)?;
    }
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(DialError::NotRunning(format!(
                "termd is not running at {}",
                path.display()
            )));
        }
        Err(e) => {
            return Err(DialError::Other(format!(
                "connect {}: {e}",
                path.display()
            )));
        }
    };
    verify_peer_creds(&stream)?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let reader_sock = stream
        .try_clone()
        .map_err(|e| DialError::Other(format!("clone socket: {e}")))?;
    let mut reader = BufReader::with_capacity(128 << 10, reader_sock);

    let hello = j_obj(vec![
        (
            "proto",
            JsonValue::Array(vec![JsonValue::U64(1), JsonValue::U64(5)]),
        ),
        ("client", JsonValue::String(client_name.into())),
    ])
    .serialize_json();

    // Per PROTOCOL.md ("Handshake"): even if writing HELLO fails with EPIPE/ECONNRESET,
    // still try to read an early ERROR frame (e.g. `brain_failed`).
    let write_res = stream.write_all(&encode_frame(FRAME_HELLO, 0, hello.as_bytes()));
    let frame_res = read_frame(&mut reader, DEFAULT_MAX_FRAME);

    let _ = stream.set_read_timeout(None);
    let _ = stream.set_write_timeout(None);

    if let Err(we) = write_res {
        if let Ok(ref f) = frame_res {
            if f.ftype == FRAME_ERROR {
                if let Ok(s) = std::str::from_utf8(&f.payload) {
                    if let Ok(val) = JsonValue::deserialize_json(s) {
                        return Err(parse_wire_error(&val));
                    }
                }
            }
        }
        return Err(DialError::Other(format!("hello write: {we}")));
    }

    let frame = frame_res?;
    match frame.ftype {
        FRAME_WELCOME => {
            let s = std::str::from_utf8(&frame.payload)
                .map_err(|_| DialError::Other("non-utf8 WELCOME".into()))?;
            let val = JsonValue::deserialize_json(s)
                .map_err(|e| DialError::Other(format!("bad WELCOME json: {e:?}")))?;
            let welcome = WelcomeInfo::from_json(&val);
            Ok((stream, reader, welcome))
        }
        FRAME_ERROR => {
            let s = std::str::from_utf8(&frame.payload).unwrap_or("{}");
            if let Ok(val) = JsonValue::deserialize_json(s) {
                Err(parse_wire_error(&val))
            } else {
                Err(DialError::Other(format!("termd refused connection: {s}")))
            }
        }
        other => Err(DialError::Other(format!(
            "expected WELCOME, got frame 0x{other:02x}"
        ))),
    }
}

pub fn find_daemon_bin() -> Option<PathBuf> {
    if let Ok(b) = std::env::var("TB_G3D_TERMD_BIN") {
        let t = b.trim();
        if !t.is_empty() && Path::new(t).is_file() {
            return Some(PathBuf::from(t));
        }
    }
    let mut candidates = vec![
        PathBuf::from("/usr/local/bin/g3d-termd"),
        PathBuf::from("/opt/tinybot/bin/g3d-termd"),
        PathBuf::from("/opt/g3d/bin/g3d-termd"),
    ];
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            candidates.push(PathBuf::from(&home).join(".local/bin/g3d-termd"));
        }
    }
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in path_env.split(':').filter(|s| !s.is_empty()) {
            candidates.push(PathBuf::from(dir).join("g3d-termd"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

pub fn ensure_socket_dir_exists() -> Result<PathBuf, String> {
    let dir = socket_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

/// Dials `ns`, starting `g3d-termd --ns <ns> --daemon` on demand if `NotRunning`.
pub fn ensure_daemon_ns(
    ns: &str,
    client_name: &str,
) -> Result<(UnixStream, BufReader<UnixStream>, WelcomeInfo), DialError> {
    match handshake_ns(ns, client_name) {
        Ok(ok) => return Ok(ok),
        Err(e) if !e.is_not_running() => return Err(e),
        Err(_) => {}
    }

    let bin = find_daemon_bin().ok_or_else(|| {
        DialError::NotRunning(
            "g3d-termd is not running and g3d-termd binary was not found on PATH".into(),
        )
    })?;
    let _ = ensure_socket_dir_exists().map_err(DialError::Other)?;

    let mut cmd = Command::new(&bin);
    cmd.arg("--ns").arg(ns).arg("--daemon");
    if let Ok(extra) = std::env::var("TB_G3D_TERMD_ARGS") {
        for tok in extra.split_whitespace() {
            cmd.arg(tok);
        }
    }
    cmd.env_remove("G3D_TERMD_SOCKET");
    cmd.env_remove("G3D_TERMD_ID");
    for (k, _) in std::env::vars() {
        if k.starts_with("G3D_RELAY_") {
            cmd.env_remove(&k);
        }
    }
    let spawn_out = cmd.output();

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut wait = Duration::from_millis(25);
    loop {
        match handshake_ns(ns, client_name) {
            Ok(ok) => return Ok(ok),
            Err(DialError::UnsafeSocket(m)) => return Err(DialError::UnsafeSocket(m)),
            Err(e) if !e.is_not_running() || Instant::now() >= deadline => {
                let extra_msg = match &spawn_out {
                    Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
                    Err(se) => se.to_string(),
                };
                return Err(DialError::Other(format!(
                    "g3d-termd did not come up for ns {ns:?} ({e}) {extra_msg}"
                )));
            }
            Err(_) => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_millis(250));
            }
        }
    }
}

/// Synchronous RPC wrapper for short one-shot commands (`create`, `list`, `paste`, `key`, `kill`).
pub struct SyncClient {
    pub writer: UnixStream,
    pub reader: BufReader<UnixStream>,
    pub welcome: WelcomeInfo,
    next_req: u64,
}

impl SyncClient {
    pub fn dial(ns: &str, client_name: &str) -> Result<Self, DialError> {
        let (writer, reader, welcome) = handshake_ns(ns, client_name)?;
        Ok(Self {
            writer,
            reader,
            welcome,
            next_req: 1,
        })
    }

    pub fn ensure(ns: &str, client_name: &str) -> Result<Self, DialError> {
        let (writer, reader, welcome) = ensure_daemon_ns(ns, client_name)?;
        Ok(Self {
            writer,
            reader,
            welcome,
            next_req: 1,
        })
    }

    pub fn request(&mut self, op: &str, mut fields: Vec<(&str, JsonValue)>) -> Result<JsonValue, DialError> {
        let req_id = self.next_req;
        self.next_req += 1;
        fields.push(("req", JsonValue::U64(req_id)));
        fields.push(("op", JsonValue::String(op.into())));
        let body = j_obj(fields).serialize_json();
        if body.len() > self.welcome.max_frame {
            return Err(DialError::Wire {
                code: "too_large".into(),
                message: format!(
                    "{op} request ({} B) exceeds max_frame ({} B)",
                    body.len(),
                    self.welcome.max_frame
                ),
            });
        }
        self.writer
            .write_all(&encode_frame(FRAME_REQUEST, 0, body.as_bytes()))
            .map_err(|e| DialError::Other(format!("{op} write: {e}")))?;

        loop {
            let f = read_frame(&mut self.reader, self.welcome.max_frame)?;
            match f.ftype {
                FRAME_ERROR => {
                    let s = std::str::from_utf8(&f.payload).unwrap_or("{}");
                    if let Ok(val) = JsonValue::deserialize_json(s) {
                        return Err(parse_wire_error(&val));
                    }
                    return Err(DialError::Other(format!("termd error frame: {s}")));
                }
                FRAME_REPLY => {
                    let s = std::str::from_utf8(&f.payload)
                        .map_err(|_| DialError::Other("non-utf8 REPLY".into()))?;
                    let val = JsonValue::deserialize_json(s)
                        .map_err(|e| DialError::Other(format!("bad REPLY json: {e:?}")))?;
                    if j_u64(j_get(&val, "req")) == Some(req_id) {
                        if j_bool(j_get(&val, "ok")) == Some(true) {
                            return Ok(val);
                        }
                        return Err(parse_wire_error(&val));
                    }
                }
                _ => {
                    // Ignore asynchronous EVENT / REPAINT / OUTPUT frames while awaiting REPLY.
                }
            }
        }
    }

    pub fn send_input(&mut self, handle: u32, bytes: &[u8]) -> Result<(), DialError> {
        self.writer
            .write_all(&encode_frame(FRAME_INPUT, handle, bytes))
            .map_err(|e| DialError::Other(format!("INPUT write: {e}")))
    }
}

static NEXT_SHELL_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn gen_term_id(prefix: &str) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        % 10_000;
    let n = NEXT_SHELL_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{secs:04}-{n}")
}

pub fn default_shell_cwd() -> Option<String> {
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() && Path::new(&h).is_dir() {
            return Some(h);
        }
    }
    None
}

pub fn create_terminal(
    ns: &str,
    id_opt: Option<&str>,
    cwd_opt: Option<&str>,
    cmd_opt: Option<&str>,
    cols: u16,
    rows: u16,
) -> Result<JsonValue, DialError> {
    let mut c = SyncClient::ensure(ns, "tb-shell-create/0.1")?;
    let id = match id_opt.map(str::trim).filter(|s| !s.is_empty()) {
        Some(explicit) => {
            validate_name("terminal id", explicit).map_err(DialError::Other)?;
            explicit.to_string()
        }
        None => gen_term_id("sh"),
    };

    let mut fields: Vec<(&str, JsonValue)> = vec![
        ("id", JsonValue::String(id)),
        ("cols", JsonValue::U64(cols.max(2) as u64)),
        ("rows", JsonValue::U64(rows.max(2) as u64)),
        ("if_exists", JsonValue::String("adopt".into())),
        (
            "colors",
            j_obj(vec![
                ("fg", JsonValue::String("#c0caf5".into())),
                ("bg", JsonValue::String("#131722".into())),
                ("cursor", JsonValue::String("#7aa2f7".into())),
            ]),
        ),
    ];

    let resolved_cwd = cwd_opt
        .map(str::trim)
        .filter(|s| !s.is_empty() && Path::new(s).is_dir())
        .map(str::to_string)
        .or_else(default_shell_cwd);
    if let Some(cwd) = resolved_cwd {
        fields.push(("cwd", JsonValue::String(cwd)));
    }

    if let Some(cmd_str) = cmd_opt.map(str::trim).filter(|s| !s.is_empty()) {
        fields.push((
            "argv",
            JsonValue::Array(vec![
                JsonValue::String("/bin/sh".into()),
                JsonValue::String("-lc".into()),
                JsonValue::String(cmd_str.into()),
            ]),
        ));
    }

    let mut reply = c.request("create", fields)?;
    if let JsonValue::Object(ref mut m) = reply {
        m.insert("ns".into(), JsonValue::String(ns.into()));
    }
    Ok(reply)
}

pub fn list_terminals(ns: &str) -> Result<Vec<JsonValue>, DialError> {
    let mut c = SyncClient::dial(ns, "tb-shell-list/0.1")?;
    let reply = c.request("list", vec![])?;
    Ok(match j_get(&reply, "terminals") {
        Some(JsonValue::Array(arr)) => arr.clone(),
        _ => Vec::new(),
    })
}

pub fn resolve_default_id_in_ns(c: &mut SyncClient) -> Result<String, DialError> {
    let reply = c.request("list", vec![])?;
    let arr = match j_get(&reply, "terminals") {
        Some(JsonValue::Array(a)) => a,
        _ => return Err(DialError::Other("no live terminals in namespace".into())),
    };
    arr.last()
        .and_then(|t| j_str(j_get(t, "id")))
        .map(str::to_string)
        .ok_or_else(|| DialError::Other("no live terminals in namespace".into()))
}

#[derive(Clone, Debug)]
pub enum PasteOutcome {
    Written { written: u64, entered: bool },
    UnsafePaste { lines: usize, preview: String },
}

fn split_utf8_chunks(s: &str, max_bytes: usize) -> Vec<&str> {
    if s.len() <= max_bytes {
        return vec![s];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < s.len() {
        let mut end = (start + max_bytes).min(s.len());
        while end > start && !s.is_char_boundary(end) {
            end -= 1;
        }
        out.push(&s[start..end]);
        start = end;
    }
    out
}

pub fn paste_preview(text: &str) -> (usize, String) {
    let lines = text.lines().count().max(1);
    let first = text.lines().next().unwrap_or("").trim();
    let mut prev: String = first.chars().take(44).collect();
    if first.chars().count() > 44 || lines > 1 {
        prev.push_str("...");
    }
    (lines, prev)
}

/// Pastes `raw_text` into `(ns, id)` using Protocol 1.5 `paste` / `paste_stream`.
/// By default (`enter == false`), trailing `\r`/`\n` on single-command pastes are
/// stripped so PTT voice-paste stages text cleanly at the prompt without executing.
pub fn paste_terminal(
    ns: &str,
    id_opt: Option<&str>,
    raw_text: &str,
    enter: bool,
    allow_unsafe: bool,
) -> Result<(String, PasteOutcome), DialError> {
    let mut c = SyncClient::dial(ns, "tb-shell-paste/0.1")?;
    let id = match id_opt.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => resolve_default_id_in_ns(&mut c)?,
    };

    let sub = c.request(
        "subscribe",
        vec![
            ("id", JsonValue::String(id.clone())),
            ("mode", JsonValue::String("none".into())),
            ("role", JsonValue::String("attach".into())),
        ],
    )?;
    let handle = j_u64(j_get(&sub, "handle")).ok_or_else(|| {
        DialError::Other("subscribe reply missing handle".into())
    })? as u32;

    // Strip trailing newline(s): if `enter` is true we send a distinct `key Enter`
    // afterwards; if `enter` is false and there are no interior newlines, stripping
    // trailing `\n` prevents accidental execution!
    let clean = raw_text.trim_end_matches(['\r', '\n']);

    let mut total_written = 0u64;
    if !clean.is_empty() {
        if c.welcome.has("paste") {
            let can_stream = c.welcome.has("paste_stream");
            let chunks = if can_stream {
                split_utf8_chunks(clean, PASTE_CHUNK_MAX_BYTES)
            } else {
                vec![clean]
            };
            for (idx, chunk) in chunks.iter().enumerate() {
                let is_last = idx + 1 == chunks.len();
                let mut fields = vec![
                    ("handle", JsonValue::U64(handle as u64)),
                    ("text", JsonValue::String((*chunk).into())),
                ];
                if idx == 0 {
                    fields.push(("allow_unsafe", JsonValue::Bool(allow_unsafe)));
                }
                if !is_last {
                    fields.push(("more", JsonValue::Bool(true)));
                }
                match c.request("paste", fields) {
                    Ok(rep) => {
                        total_written += j_u64(j_get(&rep, "written")).unwrap_or(0);
                    }
                    Err(e) if e.is_code("unsafe_paste") => {
                        if !is_last {
                            let _ = c.request(
                                "paste",
                                vec![
                                    ("handle", JsonValue::U64(handle as u64)),
                                    ("text", JsonValue::String(String::new())),
                                    ("more", JsonValue::Bool(false)),
                                ],
                            );
                        }
                        let (lines, preview) = paste_preview(clean);
                        return Ok((id, PasteOutcome::UnsafePaste { lines, preview }));
                    }
                    Err(e) => return Err(e),
                }
            }
        } else {
            c.send_input(handle, clean.as_bytes())?;
            total_written = clean.len() as u64;
        }
    }

    if enter {
        send_key_on_handle(&mut c, handle, "Enter", false, false, false)?;
    }

    Ok((
        id,
        PasteOutcome::Written {
            written: total_written,
            entered: enter,
        },
    ))
}

pub struct ParsedKey {
    pub code: String,
    pub key: String,
    pub text: String,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub fallback_bytes: Vec<u8>,
}

pub fn parse_key_spec(spec: &str, mut ctrl: bool, mut alt: bool, mut shift: bool) -> Result<ParsedKey, String> {
    let mut base = spec.trim();
    if base.is_empty() {
        return Err("empty key spec".into());
    }

    // Support inline shorthand like `^C`, `Ctrl+C`, `Alt+Left`, `Shift+Tab`.
    if let Some(rest) = base.strip_prefix('^') {
        if !rest.is_empty() {
            ctrl = true;
            base = rest;
        }
    } else if base.contains('+') && base.len() > 1 {
        let parts: Vec<&str> = base.split('+').collect();
        for &m in &parts[..parts.len() - 1] {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "c" => ctrl = true,
                "alt" | "meta" | "opt" | "option" => alt = true,
                "shift" | "s" => shift = true,
                other => return Err(format!("unknown modifier {other:?} in {spec:?}")),
            }
        }
        base = parts[parts.len() - 1];
    }

    let lower = base.to_ascii_lowercase();
    let (code, key, text, mut fb): (&str, &str, String, Vec<u8>) = match lower.as_str() {
        "enter" | "return" | "cr" => ("Enter", "Enter", String::new(), vec![b'\r']),
        "esc" | "escape" => ("Escape", "Escape", String::new(), vec![0x1b]),
        "tab" => (
            "Tab",
            "Tab",
            String::new(),
            if shift { b"\x1b[Z".to_vec() } else { vec![b'\t'] },
        ),
        "backspace" | "bksp" | "bs" => (
            "Backspace",
            "Backspace",
            String::new(),
            vec![if ctrl { 0x17 } else { 0x7f }],
        ),
        "delete" | "del" => ("Delete", "Delete", String::new(), b"\x1b[3~".to_vec()),
        "up" | "arrowup" => ("ArrowUp", "ArrowUp", String::new(), b"\x1b[A".to_vec()),
        "down" | "arrowdown" => ("ArrowDown", "ArrowDown", String::new(), b"\x1b[B".to_vec()),
        "right" | "arrowright" => ("ArrowRight", "ArrowRight", String::new(), b"\x1b[C".to_vec()),
        "left" | "arrowleft" => ("ArrowLeft", "ArrowLeft", String::new(), b"\x1b[D".to_vec()),
        "home" => ("Home", "Home", String::new(), b"\x1b[H".to_vec()),
        "end" => ("End", "End", String::new(), b"\x1b[F".to_vec()),
        "pageup" | "pgup" => ("PageUp", "PageUp", String::new(), b"\x1b[5~".to_vec()),
        "pagedown" | "pgdn" => ("PageDown", "PageDown", String::new(), b"\x1b[6~".to_vec()),
        "space" => ("Space", " ", " ".into(), vec![if ctrl { 0 } else { b' ' }]),
        _ => {
            let mut chars = base.chars();
            let Some(ch) = chars.next() else {
                return Err(format!("invalid key spec {spec:?}"));
            };
            if chars.next().is_some() {
                return Err(format!("unknown key spec {spec:?}"));
            }
            if ch.is_control() {
                return Err("key spec cannot be a raw control character".into());
            }
            let effective_ch = if shift && ch.is_ascii_lowercase() {
                ch.to_ascii_uppercase()
            } else if ctrl && ch.is_ascii_uppercase() && !shift {
                ch.to_ascii_lowercase()
            } else {
                ch
            };
            let code_s = if ch.is_ascii_alphabetic() {
                format!("Key{}", ch.to_ascii_uppercase())
            } else if ch.is_ascii_digit() {
                format!("Digit{ch}")
            } else {
                String::new()
            };
            let key_s = effective_ch.to_string();
            let text_s = effective_ch.to_string();
            let raw_bytes = if ctrl && ch.is_ascii_alphabetic() {
                vec![(ch.to_ascii_lowercase() as u8) & 0x1f]
            } else {
                let mut b = [0u8; 4];
                effective_ch.encode_utf8(&mut b).as_bytes().to_vec()
            };
            return Ok(ParsedKey {
                code: code_s,
                key: key_s,
                text: text_s,
                ctrl,
                alt,
                shift,
                fallback_bytes: if alt {
                    let mut v = vec![0x1b];
                    v.extend_from_slice(&raw_bytes);
                    v
                } else {
                    raw_bytes
                },
            });
        }
    };

    if alt && !fb.starts_with(&[0x1b]) {
        fb.insert(0, 0x1b);
    }

    Ok(ParsedKey {
        code: code.into(),
        key: key.into(),
        text,
        ctrl,
        alt,
        shift,
        fallback_bytes: fb,
    })
}

pub fn key_fields(handle: u32, pk: &ParsedKey) -> Vec<(&'static str, JsonValue)> {
    let mut mods = Vec::new();
    if pk.shift {
        mods.push(JsonValue::String("Shift".into()));
    }
    if pk.ctrl {
        mods.push(JsonValue::String("Control".into()));
    }
    if pk.alt {
        mods.push(JsonValue::String("Alt".into()));
    }
    vec![
        ("handle", JsonValue::U64(handle as u64)),
        ("code", JsonValue::String(pk.code.clone())),
        ("key", JsonValue::String(pk.key.clone())),
        ("mods", JsonValue::Array(mods)),
        ("text", JsonValue::String(pk.text.clone())),
        ("action", JsonValue::String("press".into())),
    ]
}

fn send_key_on_handle(
    c: &mut SyncClient,
    handle: u32,
    spec: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Result<u64, DialError> {
    let pk = parse_key_spec(spec, ctrl, alt, shift).map_err(DialError::Other)?;
    if c.welcome.has("key") {
        let rep = c.request("key", key_fields(handle, &pk))?;
        Ok(j_u64(j_get(&rep, "written")).unwrap_or(0))
    } else {
        c.send_input(handle, &pk.fallback_bytes)?;
        Ok(pk.fallback_bytes.len() as u64)
    }
}

pub fn send_key_terminal(
    ns: &str,
    id_opt: Option<&str>,
    spec: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Result<(String, u64), DialError> {
    let mut c = SyncClient::dial(ns, "tb-shell-key/0.1")?;
    let id = match id_opt.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => resolve_default_id_in_ns(&mut c)?,
    };
    let sub = c.request(
        "subscribe",
        vec![
            ("id", JsonValue::String(id.clone())),
            ("mode", JsonValue::String("none".into())),
            ("role", JsonValue::String("attach".into())),
        ],
    )?;
    let handle = j_u64(j_get(&sub, "handle"))
        .ok_or_else(|| DialError::Other("subscribe reply missing handle".into()))?
        as u32;
    let written = send_key_on_handle(&mut c, handle, spec, ctrl, alt, shift)?;
    Ok((id, written))
}

pub fn kill_terminal(
    ns: &str,
    id_opt: Option<&str>,
    signal_opt: Option<&str>,
) -> Result<String, DialError> {
    let mut c = SyncClient::dial(ns, "tb-shell-kill/0.1")?;
    let id = match id_opt.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => resolve_default_id_in_ns(&mut c)?,
    };
    let mut fields = vec![("id", JsonValue::String(id.clone()))];
    if let Some(sig) = signal_opt.map(str::trim).filter(|s| !s.is_empty()) {
        fields.push(("signal", JsonValue::String(sig.into())));
    }
    let _ = c.request("kill", fields)?;
    Ok(id)
}
