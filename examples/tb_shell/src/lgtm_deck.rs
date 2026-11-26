//! The lgtm tape deck, baked into tb-shell (2026-11-25, SPEC "Bake what has settled"): the
//! lgtm-deck3d app lgtm-deck3d@tinybot wrote under the on-device JIT, compiled into the shell
//! and placed from Splash as `LgtmDeck{}` (ui/views/deck.splash, surface `deck`), drawn in the
//! shell's own GL context. Board <-> Deck is a redraw: no JIT process, no frame copies, no
//! start-up wait. It reads the store keys lgtm, lgtm_mix, lgtm_out and lgtm_link (a watch on
//! the shell's socket) and the shell's own push streams lgtm_pos, lgtm_orig and lgtm_new
//! (stream::latest; the JIT app's lgtm-deck.dgram is gone); it writes lgtm_cmd and lgtm_out.
//! The shell gates it (`sync_visibility`): while the deck is not on screen its timer stops and
//! its watcher does not wake the UI, so it costs nothing hidden.
//!
//! - 4 instanced GPU draw calls per frame (beveled box, cylinder, 6-spoke cassette reel, knurled knob)
//! - PowerVR GE8320 rules: RenderBGRAu8 + DepthD32 with ClearWith every frame, 0.72x pass scale, LDR baked lighting
//! - Zero idle wakeups: NextFrame only requested while playing, coasting, or animating springs/faders/eject
//! - Live state from the shell: a watch on tb-shell.sock, and its push streams on the UI thread

#![allow(dead_code)] // the wire types keep fields the JIT app read and the deck may again

use makepad_widgets::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// lgtm-head's local socket in $XDG_RUNTIME_DIR (docs/lgtm/HEAD.md): the reel's speed goes there.
const HEAD_SOCK_NAME: &str = "lgtm-head.sock";

/// The view file that places the deck (`ui/views/deck.splash`): its surface name, the one the
/// board fronts and lgtm-link counts as "lgtm on screen".
pub const DECK_SURFACE: &str = "deck";

/// Whether a deck is on screen (set by the shell from its visible surfaces), and a count that
/// rises each time one comes back: a deck that was hidden resyncs and restarts its animation.
static DECK_VISIBLE: AtomicBool = AtomicBool::new(false);
static DECK_SHOWN_EPOCH: AtomicU64 = AtomicU64::new(0);

fn deck_visible() -> bool {
    DECK_VISIBLE.load(Ordering::SeqCst)
}

/// The shell calls this whenever its visible set changes (as for TermdView).
pub fn sync_visibility(visible: &BTreeSet<String>, bodies: &HashMap<String, String>, panel_off: bool) {
    let vis = !panel_off
        && visible.iter().any(|name| {
            name == DECK_SURFACE || bodies.get(name).is_some_and(|b| b.contains("LgtmDeck"))
        });
    let was = DECK_VISIBLE.swap(vis, Ordering::SeqCst);
    if vis && !was {
        DECK_SHOWN_EPOCH.fetch_add(1, Ordering::SeqCst);
        SignalToUI::set_ui_signal();
    }
}

/// Wakes the UI for new wire state only while a deck is on screen.
fn wake_ui_if_visible() {
    if deck_visible() {
        SignalToUI::set_ui_signal();
    }
}

const PASS_SCALE: f64 = 0.72;

// ---------------------------------------------------------------------------
// 1. Live wire state shared between background socket threads and UI
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct VoiceParamInfo {
    pub id: String,
    pub name: String,
    pub min: f32,
    pub max: f32,
    pub rest: f32,
    pub value: f32,
    pub steps: Vec<String>,
    pub step_is_num: bool,
    pub step_idx: usize,
    pub rest_step_idx: usize,
}

#[derive(Clone, Debug, Default)]
pub struct VoiceInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub state: String,
    pub params: Vec<VoiceParamInfo>,
}

#[derive(Clone, Debug, Default)]
pub struct CassetteEntry {
    pub id: String,
    pub title: String,
    pub kind: String, // "save" | "library" | "parent"
    pub gen: i32,
}

#[derive(Clone, Debug)]
pub struct DeckSharedState {
    pub status: String,
    pub note: String,
    pub link: String,
    pub song_id: String,
    pub song_title: String,
    pub song_artist: String,
    pub song_gen: i32,
    pub dur_ms: f64,
    pub line: i32,
    pub line_start_ms: f64,
    pub playing: bool,
    pub loop_active: bool,
    pub tempo_band: f32,
    pub mix_marks: [f32; 8], // live lgtm_mix values for 8 mini marks on the cassette shell

    // Position tracking (anchor + local monotonic clock for smooth dead-reckoning)
    pub pos_anchor_ms: f64,
    pub pos_anchor_at: Instant,
    pub pos_rate: f64,
    pub has_stream_pos: bool,
    /// the local head's heard position and speed (lgtm_head), and when they came: while fresh
    /// they are the tape's truth, so the reels follow the speaker within a frame
    pub head_pos_ms: f64,
    pub head_rate: f64,
    pub head_at: Option<Instant>,
    pub last_stream_at: Instant,

    // Live audio levels (0..1)
    pub level_orig: f32,
    pub level_new: f32,

    // Voices & active voice
    pub voice_id: String,
    pub voices: Vec<VoiceInfo>,

    // Output stage (lgtm_out, 0..1)
    pub out_level: f32,
    pub out_balance: f32,
    pub out_tone: f32,
    pub out_dirty_from_store: bool,

    // Cassette rack (saves + library)
    pub saves: Vec<CassetteEntry>,
    pub library: Vec<CassetteEntry>,
    pub parent_id: String,

    // Lyrics & timestamps for scrub preview under the head
    pub src_lines: Vec<String>,
    pub out_lines: Vec<String>,
    pub line_times_ms: Vec<f64>,

    // Authoritative engine transport (`lgtm.transport`) + command ack (`lgtm.ack`)
    pub has_transport: bool,
    pub transport_state: String, // "stopped" | "starting" | "playing" | "stopping" | "seeking"
    pub transport_sub: String,   // "" | "scrubbing" | "seeking"
    pub transport_rate: f64,     // signed tape rate (authoritative when transport_sub == "scrubbing")
    pub scrubbing: bool,
    pub transport_seq: u64,
    pub ack_seq: u64,
    pub ack_name: String,
    pub ack_state: String, // "ok" | "queued" | "refused"
    pub ack_why: String,

    // Revision counter to detect store updates
    pub rev: u64,
}

impl Default for DeckSharedState {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            status: "idle".to_string(),
            note: "connecting...".to_string(),
            link: "off".to_string(),
            song_id: String::new(),
            song_title: "LGTM Tape Deck".to_string(),
            song_artist: String::new(),
            song_gen: 0,
            dur_ms: 180_000.0,
            line: -1,
            line_start_ms: 0.0,
            playing: false,
            loop_active: false,
            tempo_band: 0.5,
            mix_marks: [0.0, 1.0, 0.5, 0.0, 0.5, 0.0, 0.0, 0.0],
            pos_anchor_ms: 0.0,
            pos_anchor_at: now,
            pos_rate: 1.0,
            has_stream_pos: false,
            last_stream_at: now,
            head_pos_ms: 0.0,
            head_rate: 0.0,
            head_at: None,
            level_orig: 0.0,
            level_new: 0.0,
            voice_id: "kokoro".to_string(),
            voices: vec![VoiceInfo {
                id: "kokoro".to_string(),
                name: "Kokoro".to_string(),
                kind: "speech".to_string(),
                state: "ready".to_string(),
                params: vec![
                    VoiceParamInfo {
                        id: "speed".to_string(),
                        name: "SPEED".to_string(),
                        min: 0.5,
                        max: 2.0,
                        rest: 1.0,
                        value: 1.0,
                        steps: Vec::new(),
                        step_is_num: false,
                        step_idx: 0,
                        rest_step_idx: 0,
                    },
                    VoiceParamInfo {
                        id: "voice".to_string(),
                        name: "VOICE".to_string(),
                        min: 0.0,
                        max: 1.0,
                        rest: 0.0,
                        value: 0.0,
                        steps: vec![
                            "af_heart".into(),
                            "af_bella".into(),
                            "am_michael".into(),
                            "bf_emma".into(),
                            "bm_george".into(),
                        ],
                        step_is_num: false,
                        step_idx: 0,
                        rest_step_idx: 0,
                    },
                ],
            }],
            out_level: 0.75,
            out_balance: 0.5,
            out_tone: 0.5,
            out_dirty_from_store: false,
            saves: Vec::new(),
            library: Vec::new(),
            parent_id: String::new(),
            src_lines: Vec::new(),
            out_lines: Vec::new(),
            line_times_ms: Vec::new(),
            has_transport: false,
            transport_state: "stopped".to_string(),
            transport_sub: String::new(),
            transport_rate: 1.0,
            scrubbing: false,
            transport_seq: 0,
            ack_seq: 0,
            ack_name: String::new(),
            ack_state: String::new(),
            ack_why: String::new(),
            rev: 0,
        }
    }
}

impl DeckSharedState {
    fn line_index_at_ms(&self, ms: f64) -> Option<usize> {
        if self.line_times_ms.is_empty() {
            return None;
        }
        let mut best: Option<usize> = None;
        for (i, &t0) in self.line_times_ms.iter().enumerate() {
            if t0 >= 0.0 && t0 <= ms + 150.0 {
                best = Some(i);
            }
        }
        best.or(Some(0))
    }

    fn line_text_at(&self, idx: usize) -> &str {
        if let Some(s) = self.out_lines.get(idx) {
            if !s.is_empty() {
                return s.as_str();
            }
        }
        if let Some(s) = self.src_lines.get(idx) {
            return s.as_str();
        }
        ""
    }

    /// the local head spoke in the last 200 ms
    pub fn head_fresh(&self) -> bool {
        self.head_at.map_or(false, |t| t.elapsed() < Duration::from_millis(200))
    }

    pub fn effective_rate(&self) -> f64 {
        if self.head_fresh() {
            self.head_rate
        } else if self.scrubbing {
            self.transport_rate
        } else if self.playing {
            self.pos_rate
        } else {
            0.0
        }
    }

    pub fn heard_pos_ms(&self, now: Instant, dur_ms: f64) -> f64 {
        if let (true, Some(at)) = (self.head_fresh(), self.head_at) {
            let dt_ms = now.saturating_duration_since(at).as_secs_f64() * 1000.0;
            return (self.head_pos_ms + dt_ms * self.head_rate).clamp(0.0, dur_ms.max(1.0));
        }
        let rate = self.effective_rate();
        if rate.abs() > 1e-4 {
            let dt_ms = (now.duration_since(self.pos_anchor_at).as_secs_f64() * 1000.0).min(1500.0);
            (self.pos_anchor_ms + dt_ms * rate).clamp(0.0, dur_ms.max(1.0))
        } else {
            self.pos_anchor_ms.clamp(0.0, dur_ms.max(1.0))
        }
    }
}

pub struct WireHub {
    pub state: Mutex<DeckSharedState>,
    pub tx: Mutex<Option<UnixStream>>,
    pub cmd_seq: Mutex<u64>,
}

static WIRE_HUB: OnceLock<Arc<WireHub>> = OnceLock::new();
static HEAD_DGRAM: OnceLock<Option<UnixDatagram>> = OnceLock::new();

/// Non-blocking high-rate local socket feed to the phone's native audio head (`lgtm-head.sock`).
/// Returns `true` if delivered to a live local head (skipping 20 Hz network `lgtm_cmd` scrub);
/// returns `false` (ENOENT/ECONNREFUSED) when running against the remote monolith1 stream.
fn send_head_vel(op: &str, omega: f32, rate: f32, held: bool, coast: bool) -> bool {
    let sock_opt = HEAD_DGRAM.get_or_init(|| {
        let s = UnixDatagram::unbound().ok()?;
        let _ = s.set_nonblocking(true);
        Some(s)
    });
    let Some(sock) = sock_opt.as_ref() else {
        return false;
    };
    let path = format!("{}/{}", runtime_dir(), HEAD_SOCK_NAME);
    let t_ms = unix_now_ms();
    let msg = format!(
        "{{\"op\":\"{}\",\"t\":{},\"omega\":{:.4},\"rate\":{:.4},\"held\":{},\"coast\":{}}}\n",
        op, t_ms, omega, rate, held, coast
    );
    sock.send_to(msg.as_bytes(), &path).is_ok()
}

/// One plain datagram to the head (e.g. {"scrub_end":{}}); false when no head is listening.
fn send_head_raw(msg: &str) -> bool {
    let sock_opt = HEAD_DGRAM.get_or_init(|| {
        let s = UnixDatagram::unbound().ok()?;
        let _ = s.set_nonblocking(true);
        Some(s)
    });
    let Some(sock) = sock_opt.as_ref() else {
        return false;
    };
    let path = format!("{}/{}", runtime_dir(), HEAD_SOCK_NAME);
    sock.send_to(msg.as_bytes(), &path).is_ok()
}

/// The head's heard position (ms) from its push stream `lgtm_head` [pos_ms, rate, dur_ms], and
/// when it came; the hand-back to the engine (scrub_end {pos_ms}) carries it.
static HEAD_POS: Mutex<Option<(f64, f64, Instant)>> = Mutex::new(None);

fn head_heard_ms() -> Option<f64> {
    let g = HEAD_POS.lock().ok()?;
    let (pos, rate, at) = (*g)?;
    if at.elapsed() > Duration::from_secs(2) {
        return None;
    }
    Some(pos + at.elapsed().as_secs_f64() * 1000.0 * rate)
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn wire_hub() -> Arc<WireHub> {
    WIRE_HUB
        .get_or_init(|| {
            let pair = UnixStream::pair().ok();
            let (tx_end, rx_end) = match pair {
                Some((a, b)) => (Some(a), Some(b)),
                None => (None, None),
            };
            // Start deck seq in a high non-colliding monotonic range (never collides with Board 1..N or restarts)
            let start_seq = 500_000_000 + (unix_now_ms() % 400_000_000);
            let hub = Arc::new(WireHub {
                state: Mutex::new(DeckSharedState::default()),
                tx: Mutex::new(tx_end),
                cmd_seq: Mutex::new(start_seq),
            });
            if let Some(rx) = rx_end {
                spawn_shell_writer(rx);
            }
            spawn_shell_watcher(Arc::clone(&hub));
            hub
        })
        .clone()
}

fn spawn_shell_writer(rx: UnixStream) {
    std::thread::spawn(move || {
        let path = tb_shell_sock_path();
        let mut sock: Option<UnixStream> = None;
        let mut reader = BufReader::new(rx);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if sock.is_none() {
                        sock = UnixStream::connect(&path).ok();
                        if let Some(s) = sock.as_ref() {
                            let _ = s.set_write_timeout(Some(Duration::from_millis(250)));
                        }
                    }
                    if let Some(s) = sock.as_mut() {
                        if s.write_all(line.as_bytes()).is_err() {
                            sock = None;
                            if let Ok(mut s2) = UnixStream::connect(&path) {
                                let _ = s2.set_write_timeout(Some(Duration::from_millis(250)));
                                let _ = s2.write_all(line.as_bytes());
                                sock = Some(s2);
                            }
                        }
                    }
                }
            }
        }
    });
}

fn runtime_dir() -> String {
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return d;
        }
    }
    "/run/user/8000".to_string()
}

fn tb_shell_sock_path() -> String {
    if let Ok(p) = std::env::var("TB_SHELL_SOCK") {
        if !p.is_empty() {
            return p;
        }
    }
    format!("{}/tb-shell.sock", runtime_dir())
}

fn send_shell_line(line: String) {
    let hub = wire_hub();
    let guard = hub.tx.lock();
    if let Ok(mut opt) = guard {
        if let Some(tx) = opt.as_mut() {
            let _ = tx.write_all(line.as_bytes());
            let _ = tx.write_all(b"\n");
        }
    }
}

fn send_lgtm_cmd(name: &str, data_json: &str) -> u64 {
    let hub = wire_hub();
    let t_ms = unix_now_ms();
    let seq = {
        let mut s = hub.cmd_seq.lock().unwrap();
        let cur = *s;
        *s += 1;
        cur
    };
    let line = format!(
        "{{\"op\":\"set\",\"reply\":false,\"patch\":{{\"lgtm_cmd\":{{\"seq\":{},\"name\":\"{}\",\"data\":{},\"t\":{},\"src\":\"deck\"}}}}}}",
        seq, name, data_json, t_ms
    );
    send_shell_line(line);
    seq
}

fn send_lgtm_out(level: f32, balance: f32, tone: f32) {
    let line = format!(
        "{{\"op\":\"set\",\"reply\":false,\"patch\":{{\"lgtm_out\":{{\"level\":{:.3},\"balance\":{:.3},\"tone\":{:.3}}}}}}}",
        level.clamp(0.0, 1.0),
        balance.clamp(0.0, 1.0),
        tone.clamp(0.0, 1.0)
    );
    send_shell_line(line);
}

fn send_front_surface(surface: &str) {
    let line = format!(
        "{{\"op\":\"stage\",\"sub\":\"solo\",\"surface\":\"{}\"}}",
        surface
    );
    send_shell_line(line);
}

// ---------------------------------------------------------------------------
// 2. Minimal zero-dependency JSON parser
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum JVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<JVal>),
    Obj(Vec<(String, JVal)>),
}

impl JVal {
    fn get(&self, key: &str) -> Option<&JVal> {
        if let JVal::Obj(entries) = self {
            for (k, v) in entries {
                if k == key {
                    return Some(v);
                }
            }
        }
        None
    }
    fn as_str(&self) -> Option<&str> {
        if let JVal::Str(s) = self { Some(s.as_str()) } else { None }
    }
    fn as_f64(&self) -> Option<f64> {
        if let JVal::Num(n) = self { Some(*n) } else { None }
    }
    fn as_bool(&self) -> Option<bool> {
        if let JVal::Bool(b) = self { Some(*b) } else { None }
    }
    fn as_arr(&self) -> Option<&[JVal]> {
        if let JVal::Arr(a) = self { Some(a.as_slice()) } else { None }
    }
    fn as_obj(&self) -> Option<&[(String, JVal)]> {
        if let JVal::Obj(o) = self { Some(o.as_slice()) } else { None }
    }
    fn is_null(&self) -> bool {
        matches!(self, JVal::Null)
    }
    fn to_step_string(&self) -> Option<(String, bool)> {
        match self {
            JVal::Str(s) => Some((s.clone(), false)),
            JVal::Num(n) => {
                if (*n - n.round()).abs() < 1e-6 {
                    Some((format!("{}", n.round() as i64), true))
                } else {
                    Some((format!("{}", n), true))
                }
            }
            _ => None,
        }
    }
}

struct JParser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> JParser<'a> {
    fn new(s: &'a str) -> Self {
        Self { b: s.as_bytes(), i: 0 }
    }
    fn skip_ws(&mut self) {
        while self.i < self.b.len() {
            match self.b[self.i] {
                b' ' | b'\n' | b'\r' | b'\t' => self.i += 1,
                _ => break,
            }
        }
    }
    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.b.get(self.i).copied()
    }
    fn parse_val(&mut self) -> Option<JVal> {
        match self.peek()? {
            b'{' => self.parse_obj(),
            b'[' => self.parse_arr(),
            b'"' => Some(JVal::Str(self.parse_str()?)),
            b't' => {
                if self.b[self.i..].starts_with(b"true") {
                    self.i += 4;
                    Some(JVal::Bool(true))
                } else {
                    None
                }
            }
            b'f' => {
                if self.b[self.i..].starts_with(b"false") {
                    self.i += 5;
                    Some(JVal::Bool(false))
                } else {
                    None
                }
            }
            b'n' => {
                if self.b[self.i..].starts_with(b"null") {
                    self.i += 4;
                    Some(JVal::Null)
                } else {
                    None
                }
            }
            b'-' | b'0'..=b'9' => self.parse_num(),
            _ => None,
        }
    }
    fn parse_str(&mut self) -> Option<String> {
        self.skip_ws();
        if *self.b.get(self.i)? != b'"' {
            return None;
        }
        self.i += 1;
        let mut out = String::new();
        while self.i < self.b.len() {
            let c = self.b[self.i];
            self.i += 1;
            if c == b'"' {
                return Some(out);
            }
            if c == b'\\' {
                if self.i >= self.b.len() {
                    return None;
                }
                let esc = self.b[self.i];
                self.i += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000C}'),
                    b'u' => {
                        if self.i + 4 <= self.b.len() {
                            if let Ok(hex) = std::str::from_utf8(&self.b[self.i..self.i + 4]) {
                                if let Ok(code) = u32::from_str_radix(hex, 16) {
                                    if let Some(ch) = char::from_u32(code) {
                                        out.push(ch);
                                    }
                                }
                            }
                            self.i += 4;
                        }
                    }
                    _ => out.push(esc as char),
                }
            } else {
                let start = self.i - 1;
                if c < 0x80 {
                    out.push(c as char);
                } else {
                    while self.i < self.b.len() && (self.b[self.i] & 0xC0) == 0x80 {
                        self.i += 1;
                    }
                    if let Ok(s) = std::str::from_utf8(&self.b[start..self.i]) {
                        out.push_str(s);
                    }
                }
            }
        }
        None
    }
    fn parse_num(&mut self) -> Option<JVal> {
        self.skip_ws();
        let start = self.i;
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9' => self.i += 1,
                _ => break,
            }
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).ok()?;
        let n = s.parse::<f64>().ok()?;
        Some(JVal::Num(n))
    }
    fn parse_arr(&mut self) -> Option<JVal> {
        self.skip_ws();
        self.i += 1;
        let mut arr = Vec::new();
        if self.peek()? == b']' {
            self.i += 1;
            return Some(JVal::Arr(arr));
        }
        loop {
            arr.push(self.parse_val()?);
            match self.peek()? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Some(JVal::Arr(arr));
                }
                _ => return None,
            }
        }
    }
    fn parse_obj(&mut self) -> Option<JVal> {
        self.skip_ws();
        self.i += 1;
        let mut entries = Vec::new();
        if self.peek()? == b'}' {
            self.i += 1;
            return Some(JVal::Obj(entries));
        }
        loop {
            let k = self.parse_str()?;
            if self.peek()? != b':' {
                return None;
            }
            self.i += 1;
            let v = self.parse_val()?;
            entries.push((k, v));
            match self.peek()? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    return Some(JVal::Obj(entries));
                }
                _ => return None,
            }
        }
    }
}

fn parse_json(s: &str) -> Option<JVal> {
    JParser::new(s).parse_val()
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

fn apply_store_root(st: &mut DeckSharedState, root: &JVal) {
    let now = Instant::now();
    if let Some(link) = root.get("lgtm_link").and_then(|v| v.as_str()) {
        st.link = link.to_string();
    }
    if let Some(mix) = root.get("lgtm_mix") {
        if let Some(t) = mix.get("tempo").and_then(|v| v.as_f64()) {
            st.tempo_band = t as f32;
            st.pos_rate = (0.65 + t * 0.70).clamp(0.5, 1.8);
        }
        let ids = ["literal", "own", "register", "lang", "tempo", "place", "stress", "theme"];
        for (i, id) in ids.iter().enumerate() {
            if let Some(v) = mix.get(id).and_then(|x| x.as_f64()) {
                st.mix_marks[i] = (v as f32).clamp(0.0, 1.0);
            }
        }
    }
    if let Some(out) = root.get("lgtm_out") {
        let mut any = false;
        if let Some(v) = out.get("level").and_then(|v| v.as_f64()) {
            st.out_level = (v as f32).clamp(0.0, 1.0);
            any = true;
        }
        if let Some(v) = out.get("balance").and_then(|v| v.as_f64()) {
            st.out_balance = (v as f32).clamp(0.0, 1.0);
            any = true;
        }
        if let Some(v) = out.get("tone").and_then(|v| v.as_f64()) {
            st.out_tone = (v as f32).clamp(0.0, 1.0);
            any = true;
        }
        if any {
            st.out_dirty_from_store = true;
        }
    }
    if let Some(lg) = root.get("lgtm") {
        if let Some(status) = lg.get("status").and_then(|v| v.as_str()) {
            let was_playing = st.playing;
            st.status = status.to_string();
            st.playing = status == "playing";
            if st.playing && !was_playing {
                st.pos_anchor_at = now;
            } else if !st.playing && was_playing {
                let dt = now.duration_since(st.pos_anchor_at).as_secs_f64() * 1000.0;
                st.pos_anchor_ms = (st.pos_anchor_ms + dt * st.pos_rate).clamp(0.0, st.dur_ms.max(1.0));
                st.pos_anchor_at = now;
            }
        }
        if let Some(note) = lg.get("note").and_then(|v| v.as_str()) {
            st.note = note.to_string();
        }
        if let Some(song) = lg.get("song") {
            if let Some(id) = song.get("id").and_then(|v| v.as_str()) {
                st.song_id = id.to_string();
            }
            if let Some(title) = song.get("title").and_then(|v| v.as_str()) {
                st.song_title = title.to_string();
            }
            if let Some(artist) = song.get("artist").and_then(|v| v.as_str()) {
                st.song_artist = artist.to_string();
            }
            if let Some(d) = song.get("dur_ms").and_then(|v| v.as_f64()) {
                if d > 100.0 {
                    st.dur_ms = d;
                }
            }
            if let Some(g) = song.get("gen").and_then(|v| v.as_f64()) {
                st.song_gen = g as i32;
            }
            if let Some(p) = song.get("parent").and_then(|v| v.as_str()) {
                st.parent_id = p.to_string();
            } else if song.get("parent").map(|v| v.is_null()).unwrap_or(false) {
                st.parent_id.clear();
            }
        }
        if let Some(src_arr) = lg.get("src").and_then(|v| v.as_arr()) {
            st.src_lines = src_arr
                .iter()
                .map(|x| x.as_str().unwrap_or("").to_string())
                .collect();
            if st.out_lines.len() < st.src_lines.len() {
                st.out_lines.resize(st.src_lines.len(), String::new());
            }
        }
        if let Some(out_v) = lg.get("out") {
            if let Some(arr) = out_v.as_arr() {
                st.out_lines = arr
                    .iter()
                    .map(|x| x.as_str().unwrap_or("").to_string())
                    .collect();
            } else if let Some(entries) = out_v.as_obj() {
                for (k, v) in entries {
                    if let Ok(idx) = k.parse::<usize>() {
                        if let Some(txt) = v.as_str() {
                            if idx >= st.out_lines.len() {
                                st.out_lines.resize(idx + 1, String::new());
                            }
                            st.out_lines[idx] = txt.to_string();
                        }
                    }
                }
            }
        }
        if let Some(words_lines) = lg.get("words").and_then(|v| v.as_arr()) {
            let mut times = Vec::with_capacity(words_lines.len());
            for ln in words_lines {
                let mut t0 = -1.0;
                if let Some(ws) = ln.as_arr() {
                    if let Some(first) = ws.first().and_then(|w| w.as_arr()) {
                        if let Some(ms) = first.get(1).and_then(|n| n.as_f64()) {
                            t0 = ms;
                        }
                    }
                }
                times.push(t0);
            }
            st.line_times_ms = times;
        }
        if let Some(line_n) = lg.get("line").and_then(|v| v.as_f64()) {
            let idx = line_n as i32;
            st.line = idx;
            if idx >= 0 && (idx as usize) < st.line_times_ms.len() {
                let ms = st.line_times_ms[idx as usize];
                if ms >= 0.0 {
                    st.line_start_ms = ms;
                    if !st.scrubbing
                        && (!st.has_stream_pos || now.duration_since(st.last_stream_at) > Duration::from_millis(600))
                    {
                        st.pos_anchor_ms = ms;
                        st.pos_anchor_at = now;
                    }
                }
            }
        }
        if let Some(pos_at) = lg.get("pos_at") {
            if !st.scrubbing {
                if let Some(ms) = pos_at.get("ms").and_then(|v| v.as_f64()) {
                    st.pos_anchor_ms = ms.clamp(0.0, st.dur_ms.max(1.0));
                    st.pos_anchor_at = now;
                }
            }
            if let Some(r) = pos_at.get("rate").and_then(|v| v.as_f64()) {
                if r > 0.1 && r < 4.0 {
                    st.pos_rate = r;
                }
            }
            if !st.has_transport {
                if let Some(p) = pos_at.get("playing").and_then(|v| v.as_bool()) {
                    st.playing = p;
                    st.transport_state = if p { "playing".into() } else { "stopped".into() };
                }
            }
        }
        // Single authoritative engine transport (`lgtm.transport {state, sub, seq, line, pos_ms, voice, rate, t}`)
        if let Some(tr) = lg.get("transport") {
            let prev_rate = st.effective_rate();
            let tr_has_pos = tr
                .get("pos_ms")
                .and_then(|v| v.as_f64())
                .map(|ms| ms >= 0.0)
                .unwrap_or(false);
            if !tr_has_pos && prev_rate.abs() > 1e-4 {
                let dt = (now.duration_since(st.pos_anchor_at).as_secs_f64() * 1000.0).min(1500.0);
                st.pos_anchor_ms = (st.pos_anchor_ms + dt * prev_rate).clamp(0.0, st.dur_ms.max(1.0));
                st.pos_anchor_at = now;
            }
            st.has_transport = true;
            if let Some(sub) = tr.get("sub").and_then(|v| v.as_str()) {
                st.transport_sub = sub.to_string();
                st.scrubbing = sub == "scrubbing";
            }
            if let Some(r) = tr.get("rate").and_then(|v| v.as_f64()) {
                if r.is_finite() {
                    st.transport_rate = r.clamp(-16.0, 16.0);
                }
            }
            if let Some(ts) = tr.get("state").and_then(|v| v.as_str()) {
                let was_playing = st.playing;
                st.transport_state = ts.to_string();
                st.playing = ts == "playing" || ts == "seeking";
                if st.playing && !was_playing && !tr_has_pos {
                    st.pos_anchor_at = now;
                }
            }
            if let Some(sq) = tr.get("seq").and_then(|v| v.as_f64()) {
                st.transport_seq = sq.max(0.0) as u64;
            }
            if let Some(ln) = tr.get("line").and_then(|v| v.as_f64()) {
                st.line = ln as i32;
            }
            if let Some(ms) = tr.get("pos_ms").and_then(|v| v.as_f64()) {
                if ms >= 0.0 {
                    st.pos_anchor_ms = ms.clamp(0.0, st.dur_ms.max(1.0));
                    st.pos_anchor_at = now;
                }
            }
            if let Some(v_id) = tr.get("voice").and_then(|v| v.as_str()) {
                if !v_id.is_empty() {
                    st.voice_id = v_id.to_string();
                }
            }
        }
        // Command acknowledgement (`lgtm.ack {seq, name, state, why, t_rx, t_done}`)
        if let Some(ack) = lg.get("ack") {
            if let Some(sq) = ack.get("seq").and_then(|v| v.as_f64()) {
                st.ack_seq = sq.max(0.0) as u64;
            }
            if let Some(nm) = ack.get("name").and_then(|v| v.as_str()) {
                st.ack_name = nm.to_string();
            }
            if let Some(as_str) = ack.get("state").and_then(|v| v.as_str()) {
                st.ack_state = as_str.to_string();
            }
            if let Some(why) = ack.get("why").and_then(|v| v.as_str()) {
                st.ack_why = why.to_string();
            } else if ack.get("why").map(|v| v.is_null()).unwrap_or(false) {
                st.ack_why.clear();
            }
        }
        if let Some(lp) = lg.get("loop") {
            st.loop_active = !lp.is_null();
        }
        if let Some(v_id) = lg.get("voice").and_then(|v| v.as_str()) {
            st.voice_id = v_id.to_string();
        }
        if let Some(voices_arr) = lg.get("voices").and_then(|v| v.as_arr()) {
            let mut parsed = Vec::new();
            for item in voices_arr {
                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if id.is_empty() {
                    continue;
                }
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or(&id).to_string();
                let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("speech").to_string();
                let state = item.get("state").and_then(|v| v.as_str()).unwrap_or("ready").to_string();
                let set_obj = item.get("set");
                let mut params = Vec::new();
                let list = item
                    .get("params")
                    .and_then(|v| v.as_arr())
                    .or_else(|| item.get("settings").and_then(|v| v.as_arr()));
                if let Some(plist) = list {
                    for p in plist {
                        let pid = p.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        if pid.is_empty() {
                            continue;
                        }
                        let pname = p.get("name").and_then(|v| v.as_str()).unwrap_or(&pid).to_uppercase();
                        let mut steps = Vec::new();
                        let mut step_is_num = false;
                        if let Some(sa) = p.get("steps").and_then(|v| v.as_arr()) {
                            for sv in sa {
                                if let Some((s, is_num)) = sv.to_step_string() {
                                    if is_num {
                                        step_is_num = true;
                                    }
                                    steps.push(s);
                                }
                            }
                        }
                        if !steps.is_empty() {
                            let cur_s = p
                                .get("value")
                                .and_then(|v| v.to_step_string().map(|x| x.0))
                                .or_else(|| set_obj.and_then(|o| o.get(&pid)).and_then(|v| v.to_step_string().map(|x| x.0)))
                                .or_else(|| p.get("rest").and_then(|v| v.to_step_string().map(|x| x.0)))
                                .unwrap_or_default();
                            let rest_s = p
                                .get("rest")
                                .and_then(|v| v.to_step_string().map(|x| x.0))
                                .unwrap_or_default();
                            let idx = steps.iter().position(|x| x == &cur_s).unwrap_or(0);
                            let rest_idx = steps.iter().position(|x| x == &rest_s).unwrap_or(0);
                            let norm = if steps.len() > 1 {
                                idx as f32 / (steps.len() - 1) as f32
                            } else {
                                0.0
                            };
                            params.push(VoiceParamInfo {
                                id: pid,
                                name: pname,
                                min: 0.0,
                                max: 1.0,
                                rest: 0.0,
                                value: norm,
                                steps,
                                step_is_num,
                                step_idx: idx,
                                rest_step_idx: rest_idx,
                            });
                        } else {
                            let lo = p
                                .get("min")
                                .and_then(|v| v.as_f64())
                                .or_else(|| p.get("lo").and_then(|v| v.as_f64()))
                                .unwrap_or(0.0) as f32;
                            let hi = p
                                .get("max")
                                .and_then(|v| v.as_f64())
                                .or_else(|| p.get("hi").and_then(|v| v.as_f64()))
                                .unwrap_or(1.0) as f32;
                            let rest = p.get("rest").and_then(|v| v.as_f64()).unwrap_or(((lo + hi) * 0.5) as f64) as f32;
                            let val = p
                                .get("value")
                                .and_then(|v| v.as_f64())
                                .or_else(|| set_obj.and_then(|o| o.get(&pid)).and_then(|v| v.as_f64()))
                                .unwrap_or(rest as f64) as f32;
                            params.push(VoiceParamInfo {
                                id: pid,
                                name: pname,
                                min: lo,
                                max: hi,
                                rest,
                                value: val,
                                steps: Vec::new(),
                                step_is_num: false,
                                step_idx: 0,
                                rest_step_idx: 0,
                            });
                        }
                    }
                }
                parsed.push(VoiceInfo { id, name, kind, state, params });
            }
            if !parsed.is_empty() {
                st.voices = parsed;
            }
        }
        if let Some(saves_arr) = lg.get("saves").and_then(|v| v.as_arr()) {
            let mut saves = Vec::new();
            for s in saves_arr {
                let id = s.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let name = s.get("name").and_then(|v| v.as_str()).unwrap_or(&id).to_string();
                if !id.is_empty() {
                    saves.push(CassetteEntry { id, title: name, kind: "save".into(), gen: st.song_gen });
                }
            }
            st.saves = saves;
        }
        if let Some(lib_arr) = lg.get("library").and_then(|v| v.as_arr()) {
            let mut lib = Vec::new();
            for item in lib_arr {
                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let title = item.get("title").and_then(|v| v.as_str()).unwrap_or(&id).to_string();
                let gen = item.get("gen").and_then(|v| v.as_f64()).unwrap_or(0.0) as i32;
                if !id.is_empty() {
                    lib.push(CassetteEntry { id, title, kind: "library".into(), gen });
                }
            }
            st.library = lib;
        }
    }
    st.rev += 1;
}

fn spawn_shell_watcher(hub: Arc<WireHub>) {
    std::thread::spawn(move || {
        let path = tb_shell_sock_path();
        loop {
            if let Ok(mut sock) = UnixStream::connect(&path) {
                let get_req = "{\"op\":\"get\",\"keys\":[\"lgtm\",\"lgtm_mix\",\"lgtm_out\",\"lgtm_link\"]}\n";
                if sock.write_all(get_req.as_bytes()).is_ok() {
                    let mut reader = BufReader::new(sock.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) > 0 {
                        if let Some(v) = parse_json(&line) {
                            if let Some(st_obj) = v.get("state") {
                                if let Ok(mut guard) = hub.state.lock() {
                                    apply_store_root(&mut guard, st_obj);
                                }
                                wake_ui_if_visible();
                            }
                        }
                    }
                    let watch_req = "{\"op\":\"watch\",\"keys\":[\"lgtm\",\"lgtm_mix\",\"lgtm_out\",\"lgtm_link\"]}\n";
                    if sock.write_all(watch_req.as_bytes()).is_ok() {
                        loop {
                            line.clear();
                            match reader.read_line(&mut line) {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {
                                    if let Some(v) = parse_json(&line) {
                                        if let Some(patch) = v.get("patch") {
                                            if let Ok(mut guard) = hub.state.lock() {
                                                apply_store_root(&mut guard, patch);
                                            }
                                            wake_ui_if_visible();
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

/// The shell's push streams that carry the position and the two levels (lgtm-down pushes them
/// while playing): read on the UI thread, as a StreamView reads its stream.
fn take_streams(hub: &WireHub, seen: &mut [u64; 4]) -> bool {
    // the local head's own number first: [pos_ms, rate, dur_ms] (docs/lgtm/HEAD.md)
    let mut head_moved = false;
    if let Some((v, values)) = crate::stream::latest("lgtm_head", seen[3]) {
        seen[3] = v;
        if values.len() >= 2 {
            let (pos, rate, now) = (values[0] as f64, values[1] as f64, Instant::now());
            if let Ok(mut g) = HEAD_POS.lock() {
                *g = Some((pos, rate, now));
            }
            if let Ok(mut st) = hub.state.lock() {
                let stale = st.head_at.map_or(true, |t| t.elapsed() > Duration::from_millis(50));
                if (pos - st.head_pos_ms).abs() > 0.01 || (rate - st.head_rate).abs() > 0.001 || stale {
                    st.head_pos_ms = pos;
                    st.head_rate = rate;
                    st.head_at = Some(now);
                    if values.len() >= 3 && values[2] as f64 > 100.0 {
                        st.dur_ms = values[2] as f64;
                    }
                    st.rev += 1;
                    head_moved = true;
                }
            }
        }
    }
    let pos = crate::stream::latest("lgtm_pos", seen[0]);
    let head_fresh = hub.state.lock().map(|st| st.head_fresh()).unwrap_or(false);
    let orig = crate::stream::latest("lgtm_orig", seen[1]);
    let new = crate::stream::latest("lgtm_new", seen[2]);
    if pos.is_none() && orig.is_none() && new.is_none() {
        return head_moved;
    }
    let now = Instant::now();
    let Ok(mut st) = hub.state.lock() else { return false };
    if let Some((v, values)) = pos {
        seen[0] = v;
        if let (false, Some(f)) = (head_fresh, values.last()) {
            let clamped = (*f as f64).clamp(0.0, 1.0) * st.dur_ms.max(1.0);
            // re-anchor only on a real move (or a stale or still anchor), so the reels keep
            // dead-reckoning smoothly between pushes (lgtm-deck3d's scrub rule)
            let changed = (clamped - st.pos_anchor_ms).abs() > 0.05;
            let stale = now.duration_since(st.last_stream_at) > Duration::from_millis(80);
            if changed || !st.has_stream_pos || stale || st.effective_rate().abs() < 1e-4 {
                st.pos_anchor_ms = clamped;
                st.pos_anchor_at = now;
                st.has_stream_pos = true;
            }
            st.last_stream_at = now;
        }
    }
    if let Some((v, values)) = orig {
        seen[1] = v;
        if let Some(f) = values.last() {
            st.level_orig = f.clamp(0.0, 1.0);
        }
    }
    if let Some((v, values)) = new {
        seen[2] = v;
        if let Some(f) = values.last() {
            st.level_new = f.clamp(0.0, 1.0);
        }
    }
    st.rev += 1;
    true
}

// ---------------------------------------------------------------------------
// 3. Makepad script_mod! (shaders + UI shell)
// ---------------------------------------------------------------------------

script_mod! {
    use mod.prelude.widgets.*
    use mod.widgets.*
    use mod.math.*
    use mod.shader.*
    use mod.draw
    use mod.geom

    mod.draw.DrawDeckMesh = mod.std.set_type_default() do #(DrawDeckMesh::script_shader(vm)){
        alpha_blend: false
        backface_culling: true

        vertex_pos: vertex_position(vec4f)
        fb0: fragment_output(0, vec4f)

        draw_call: uniform_buffer(draw.DrawCallUniforms)
        draw_pass: uniform_buffer(draw.DrawPassUniforms)
        draw_list: uniform_buffer(draw.DrawListUniforms)
        geom: vertex_buffer(geom.IcoVertex, geom.IcoGeom)

        light_dir: uniform(vec3(0.35, 0.72, 0.60))
        fill_dir: uniform(vec3(-0.55, -0.30, 0.45))

        world_pos: varying(vec3f)
        world_normal: varying(vec3f)

        vertex: fn() {
            let world = self.model * self.geom.pos
            self.world_pos = world.xyz
            self.world_normal = (self.model * self.geom.normal).xyz
            let view_pos = self.draw_pass.camera_view * world
            self.vertex_pos = self.draw_pass.camera_projection * view_pos
        }

        pixel: fn() {
            let normal = normalize(self.world_normal)
            let to_light = normalize(self.light_dir)
            let to_fill = normalize(self.fill_dir)
            let camera_pos = (self.draw_pass.camera_inv * vec4(0.0, 0.0, 0.0, 1.0)).xyz
            let to_camera = normalize(camera_pos - self.world_pos)

            let hemi = mix(0.24, 0.44, clamp(normal.y * 0.5 + 0.5, 0.0, 1.0))
            let diff_key = max(dot(normal, to_light), 0.0) * 0.68
            let diff_fill = max(dot(normal, to_fill), 0.0) * 0.22

            let half_dir = normalize(to_light + to_camera)
            let spec_pow = mix(12.0, 64.0, clamp(self.mat.x, 0.0, 1.0))
            let spec_amp = mix(0.06, 0.50, clamp(self.mat.x, 0.0, 1.0))
            let spec = pow(max(dot(normal, half_dir), 0.0), spec_pow) * spec_amp

            let rim = pow(1.0 - max(dot(normal, to_camera), 0.0), 3.0) * (0.12 + self.mat.z * 0.22)
            let warm_key = vec3(1.0, 0.97, 0.92)
            let cool_fill = vec3(0.68, 0.80, 1.0)
            let lit = self.color.xyz * (hemi + diff_key * warm_key + diff_fill * cool_fill)
                + vec3(spec, spec, spec)
                + cool_fill * rim
            let emit = self.color.xyz * clamp(self.mat.y, 0.0, 1.0) * 1.15
            return vec4(lit + emit, 1.0)
        }

        fragment: fn() {
            self.fb0 = self.pixel()
        }
    }

    mod.widgets.DeckViewport = set_type_default() do #(DeckViewport::register_widget(vm)){
        width: Fill
        height: Fill
        draw_image +: {
            scene_texture: texture_2d(float)
            pixel: fn() {
                return self.scene_texture.sample(self.pos)
            }
        }
        draw_txt +: {
            text_style.font_size: 10.5
            color: #xe2e8f0
        }
        draw_card +: {
            color: #x0f172a
        }
    }

    let TopPill = ButtonFlat{
        width: Fit height: 30 margin: 0
        padding: Inset{left: 6 right: 6 top: 0 bottom: 0}
        draw_bg.color: #x151d2b
        draw_bg.color_focus: #x151d2b
        draw_bg.color_hover: #x24344d
        draw_bg.border_radius: 8.0
        draw_text.text_style.font_size: 10.5
        draw_text.color: #xcbd5e1
    }

    // The deck as a shell surface: the header (title and counter, the param bank, the rack,
    // and the shared [ Board | Deck ] toggle in the same place as on the board) over the 3D
    // viewport. The shell's strip is above it; the view file pads the bottom safe area.
    mod.widgets.LgtmDeckBase = #(LgtmDeck::register_widget(vm))
    mod.widgets.LgtmDeck = set_type_default() do mod.widgets.LgtmDeckBase{
        width: Fill
        height: Fill
        flow: Down
        show_bg: true
        draw_bg.color: #x090d14

        header := View{
            width: Fill height: 52
            flow: Right spacing: 5
            padding: Inset{left: 10 right: 10 top: 4 bottom: 4}
            align: Align{y: 0.0}
            show_bg: true
            draw_bg.color: #x0e1420

            View{
                width: Fill height: Fit flow: Down spacing: 2
                padding: Inset{top: 2}
                title_lbl := Label{
                    width: Fill
                    text: "LGTM 3D Deck"
                    draw_text.text_style.font_size: 10.8
                    draw_text.color: #xf1f5f9
                }
                sub_lbl := Label{
                    width: Fill
                    text: "00:00.0 / 03:00.0 · stopped"
                    draw_text.text_style.font_size: 9.5
                    draw_text.color: #x94a3b8
                }
            }

            bank_btn := TopPill{ text: "P1-2" }
            rack_btn := TopPill{ text: "Rack" }

            view_toggle := RoundedView{
                width: 108 height: 30
                flow: Right spacing: 0
                padding: 2
                show_bg: true
                draw_bg.color: #x151d2b
                draw_bg.border_radius: 8.0

                toggle_board := ButtonFlat{
                    width: Fill height: Fill margin: 0 padding: 0
                    text: "Board"
                    draw_bg.color: #x00000000
                    draw_bg.color_focus: #x00000000
                    draw_bg.color_hover: #x1e293b
                    draw_bg.border_radius: 6.0
                    draw_text.text_style.font_size: 10.5
                    draw_text.color: #x94a3b8
                }
                toggle_deck := ButtonFlat{
                    width: Fill height: Fill margin: 0 padding: 0
                    text: "Deck"
                    draw_bg.color: #x24344d
                    draw_bg.color_focus: #x24344d
                    draw_bg.color_hover: #x2d4060
                    draw_bg.border_radius: 6.0
                    draw_text.text_style.font_size: 10.5
                    draw_text.color: #xf8fafc
                }
            }
        }

        viewport := mod.widgets.DeckViewport{}
    }
}

// ---------------------------------------------------------------------------
// 4. Procedural 3D meshes (IcoVertex layout: pos vec4, normal vec4)
// ---------------------------------------------------------------------------

fn push_tri(verts: &mut Vec<f32>, idx: &mut Vec<u32>, a: Vec3f, b: Vec3f, c: Vec3f) {
    let ab = vec3(b.x - a.x, b.y - a.y, b.z - a.z);
    let ac = vec3(c.x - a.x, c.y - a.y, c.z - a.z);
    let n = vec3(
        ab.y * ac.z - ab.z * ac.y,
        ab.z * ac.x - ab.x * ac.z,
        ab.x * ac.y - ab.y * ac.x,
    );
    let len = (n.x * n.x + n.y * n.y + n.z * n.z).sqrt().max(1e-6);
    let n = vec3(n.x / len, n.y / len, n.z / len);
    let base = (verts.len() / 8) as u32;
    for p in [a, b, c] {
        verts.extend_from_slice(&[p.x, p.y, p.z, 1.0, n.x, n.y, n.z, 0.0]);
    }
    idx.extend_from_slice(&[base, base + 1, base + 2]);
}

fn push_quad(verts: &mut Vec<f32>, idx: &mut Vec<u32>, a: Vec3f, b: Vec3f, c: Vec3f, d: Vec3f, n: Vec3f) {
    let base = (verts.len() / 8) as u32;
    for p in [a, b, c, d] {
        verts.extend_from_slice(&[p.x, p.y, p.z, 1.0, n.x, n.y, n.z, 0.0]);
    }
    idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

fn append_box_part(verts: &mut Vec<f32>, idx: &mut Vec<u32>, cx: f32, cy: f32, cz: f32, hx: f32, hy: f32, hz: f32) {
    let faces = [
        (vec3(1.0, 0.0, 0.0), vec3(0.0, 1.0, 0.0), vec3(0.0, 0.0, 1.0)),
        (vec3(-1.0, 0.0, 0.0), vec3(0.0, 0.0, 1.0), vec3(0.0, 1.0, 0.0)),
        (vec3(0.0, 1.0, 0.0), vec3(0.0, 0.0, 1.0), vec3(1.0, 0.0, 0.0)),
        (vec3(0.0, -1.0, 0.0), vec3(1.0, 0.0, 0.0), vec3(0.0, 0.0, 1.0)),
        (vec3(0.0, 0.0, 1.0), vec3(1.0, 0.0, 0.0), vec3(0.0, 1.0, 0.0)),
        (vec3(0.0, 0.0, -1.0), vec3(0.0, 1.0, 0.0), vec3(1.0, 0.0, 0.0)),
    ];
    let corners: [(f32, f32); 4] = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)];
    for (normal, au, av) in faces {
        let base = (verts.len() / 8) as u32;
        for (u, v) in corners {
            let p = vec3(
                cx + (normal.x + au.x * u + av.x * v) * hx,
                cy + (normal.y + au.y * u + av.y * v) * hy,
                cz + (normal.z + au.z * u + av.z * v) * hz,
            );
            verts.extend_from_slice(&[p.x, p.y, p.z, 1.0, normal.x, normal.y, normal.z, 0.0]);
        }
        idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
}

fn append_cyl_z(verts: &mut Vec<f32>, idx: &mut Vec<u32>, r_inner: f32, r_outer: f32, z0: f32, z1: f32, segs: usize) {
    let tau = std::f32::consts::TAU;
    for i in 0..segs {
        let a0 = (i as f32 / segs as f32) * tau;
        let a1 = ((i + 1) as f32 / segs as f32) * tau;
        let (c0, s0) = (a0.cos(), a0.sin());
        let (c1, s1) = (a1.cos(), a1.sin());
        let base = (verts.len() / 8) as u32;
        verts.extend_from_slice(&[c0 * r_outer, s0 * r_outer, z0, 1.0, c0, s0, 0.0, 0.0]);
        verts.extend_from_slice(&[c1 * r_outer, s1 * r_outer, z0, 1.0, c1, s1, 0.0, 0.0]);
        verts.extend_from_slice(&[c1 * r_outer, s1 * r_outer, z1, 1.0, c1, s1, 0.0, 0.0]);
        verts.extend_from_slice(&[c0 * r_outer, s0 * r_outer, z1, 1.0, c0, s0, 0.0, 0.0]);
        idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);

        if r_inner > 0.001 {
            let base = (verts.len() / 8) as u32;
            verts.extend_from_slice(&[c1 * r_inner, s1 * r_inner, z0, 1.0, -c1, -s1, 0.0, 0.0]);
            verts.extend_from_slice(&[c0 * r_inner, s0 * r_inner, z0, 1.0, -c0, -s0, 0.0, 0.0]);
            verts.extend_from_slice(&[c0 * r_inner, s0 * r_inner, z1, 1.0, -c0, -s0, 0.0, 0.0]);
            verts.extend_from_slice(&[c1 * r_inner, s1 * r_inner, z1, 1.0, -c1, -s1, 0.0, 0.0]);
            idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);

            push_quad(
                verts, idx,
                vec3(c0 * r_inner, s0 * r_inner, z1),
                vec3(c0 * r_outer, s0 * r_outer, z1),
                vec3(c1 * r_outer, s1 * r_outer, z1),
                vec3(c1 * r_inner, s1 * r_inner, z1),
                vec3(0.0, 0.0, 1.0),
            );
            push_quad(
                verts, idx,
                vec3(c1 * r_inner, s1 * r_inner, z0),
                vec3(c1 * r_outer, s1 * r_outer, z0),
                vec3(c0 * r_outer, s0 * r_outer, z0),
                vec3(c0 * r_inner, s0 * r_inner, z0),
                vec3(0.0, 0.0, -1.0),
            );
        } else {
            push_tri(
                verts, idx,
                vec3(0.0, 0.0, z1),
                vec3(c0 * r_outer, s0 * r_outer, z1),
                vec3(c1 * r_outer, s1 * r_outer, z1),
            );
            push_tri(
                verts, idx,
                vec3(0.0, 0.0, z0),
                vec3(c1 * r_outer, s1 * r_outer, z0),
                vec3(c0 * r_outer, s0 * r_outer, z0),
            );
        }
    }
}

fn build_box_mesh(cx: &mut Cx) -> Geometry {
    let mut verts = Vec::new();
    let mut idx = Vec::new();
    append_box_part(&mut verts, &mut idx, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0);
    let g = Geometry::new(cx);
    g.update(cx, idx, verts);
    g
}

fn build_cyl_mesh(cx: &mut Cx) -> Geometry {
    let mut verts = Vec::new();
    let mut idx = Vec::new();
    append_cyl_z(&mut verts, &mut idx, 0.0, 1.0, -0.5, 0.5, 24);
    let g = Geometry::new(cx);
    g.update(cx, idx, verts);
    g
}

fn build_reel_mesh(cx: &mut Cx) -> Geometry {
    let mut verts = Vec::new();
    let mut idx = Vec::new();
    append_cyl_z(&mut verts, &mut idx, 0.88, 1.00, -0.10, 0.14, 28);
    append_cyl_z(&mut verts, &mut idx, 0.24, 0.46, -0.14, 0.20, 20);
    let tau = std::f32::consts::TAU;
    for i in 0..6 {
        let a = (i as f32 / 6.0) * tau;
        let (ca, sa) = (a.cos(), a.sin());
        let (tx, ty) = (-sa, ca);
        let hw = if i == 0 { 0.095 } else { 0.055 };
        let r0 = 0.42;
        let r1 = 0.91;
        let z0 = -0.06;
        let z1 = if i == 0 { 0.19 } else { 0.13 };
        let p00 = vec3(ca * r0 - tx * hw, sa * r0 - ty * hw, z1);
        let p10 = vec3(ca * r1 - tx * hw, sa * r1 - ty * hw, z1);
        let p11 = vec3(ca * r1 + tx * hw, sa * r1 + ty * hw, z1);
        let p01 = vec3(ca * r0 + tx * hw, sa * r0 + ty * hw, z1);
        let b00 = vec3(p00.x, p00.y, z0);
        let b10 = vec3(p10.x, p10.y, z0);
        let b11 = vec3(p11.x, p11.y, z0);
        let b01 = vec3(p01.x, p01.y, z0);
        push_quad(&mut verts, &mut idx, p00, p10, p11, p01, vec3(0.0, 0.0, 1.0));
        push_quad(&mut verts, &mut idx, b01, b11, b10, b00, vec3(0.0, 0.0, -1.0));
        push_quad(&mut verts, &mut idx, b00, b10, p10, p00, vec3(-tx, -ty, 0.0));
        push_quad(&mut verts, &mut idx, b11, b01, p01, p11, vec3(tx, ty, 0.0));

        let tw = 0.042;
        let tr0 = 0.16;
        let tr1 = 0.27;
        let tz0 = -0.10;
        let tz1 = 0.19;
        let q00 = vec3(ca * tr0 - tx * tw, sa * tr0 - ty * tw, tz1);
        let q10 = vec3(ca * tr1 - tx * tw, sa * tr1 - ty * tw, tz1);
        let q11 = vec3(ca * tr1 + tx * tw, sa * tr1 + ty * tw, tz1);
        let q01 = vec3(ca * tr0 + tx * tw, sa * tr0 + ty * tw, tz1);
        push_quad(&mut verts, &mut idx, q00, q10, q11, q01, vec3(0.0, 0.0, 1.0));
        let qb00 = vec3(q00.x, q00.y, tz0);
        let qb10 = vec3(q10.x, q10.y, tz0);
        let qb11 = vec3(q11.x, q11.y, tz0);
        let qb01 = vec3(q01.x, q01.y, tz0);
        push_quad(&mut verts, &mut idx, qb00, qb10, q10, q00, vec3(-tx, -ty, 0.0));
        push_quad(&mut verts, &mut idx, qb11, qb01, q01, q11, vec3(tx, ty, 0.0));
    }
    let g = Geometry::new(cx);
    g.update(cx, idx, verts);
    g
}

fn build_knob_mesh(cx: &mut Cx) -> Geometry {
    let mut verts = Vec::new();
    let mut idx = Vec::new();
    append_cyl_z(&mut verts, &mut idx, 0.0, 1.00, 0.0, 0.18, 20);
    append_cyl_z(&mut verts, &mut idx, 0.0, 0.86, 0.18, 0.72, 16);
    append_box_part(&mut verts, &mut idx, 0.0, 0.42, 0.75, 0.10, 0.36, 0.08);
    let g = Geometry::new(cx);
    g.update(cx, idx, verts);
    g
}

// ---------------------------------------------------------------------------
// 5. Matrix & 3D picking math
// ---------------------------------------------------------------------------

fn mat4_mul(a: Mat4f, b: Mat4f) -> Mat4f {
    let mut r = [0.0f32; 16];
    for col in 0..4 {
        for row in 0..4 {
            let mut sum = 0.0;
            for k in 0..4 {
                sum += a.v[k * 4 + row] * b.v[col * 4 + k];
            }
            r[col * 4 + row] = sum;
        }
    }
    Mat4f { v: r }
}

fn mat4_trs_z(tx: f32, ty: f32, tz: f32, rot_z: f32, sx: f32, sy: f32, sz: f32) -> Mat4f {
    let (c, s) = (rot_z.cos(), rot_z.sin());
    Mat4f {
        v: [
            c * sx, s * sx, 0.0, 0.0,
            -s * sy, c * sy, 0.0, 0.0,
            0.0, 0.0, sz, 0.0,
            tx, ty, tz, 1.0,
        ],
    }
}

fn mat4_trs_xz(tx: f32, ty: f32, tz: f32, rot_x: f32, sx: f32, sy: f32, sz: f32) -> Mat4f {
    let (c, s) = (rot_x.cos(), rot_x.sin());
    Mat4f {
        v: [
            sx, 0.0, 0.0, 0.0,
            0.0, c * sy, s * sy, 0.0,
            0.0, -s * sz, c * sz, 0.0,
            tx, ty, tz, 1.0,
        ],
    }
}

fn mat4_transform_vec4(m: Mat4f, x: f32, y: f32, z: f32, w: f32) -> [f32; 4] {
    [
        m.v[0] * x + m.v[4] * y + m.v[8] * z + m.v[12] * w,
        m.v[1] * x + m.v[5] * y + m.v[9] * z + m.v[13] * w,
        m.v[2] * x + m.v[6] * y + m.v[10] * z + m.v[14] * w,
        m.v[3] * x + m.v[7] * y + m.v[11] * z + m.v[15] * w,
    ]
}

fn project_to_screen(vp: Mat4f, rect: Rect, p: Vec3f) -> Vec2d {
    let c = mat4_transform_vec4(vp, p.x, p.y, p.z, 1.0);
    let inv_w = if c[3].abs() > 1e-5 { 1.0 / c[3] } else { 1.0 };
    let ndc_x = c[0] * inv_w;
    let ndc_y = c[1] * inv_w;
    dvec2(
        rect.pos.x + (ndc_x as f64 * 0.5 + 0.5) * rect.size.x,
        rect.pos.y + (0.5 - ndc_y as f64 * 0.5) * rect.size.y,
    )
}

fn unproject_to_plane_z(
    eye: Vec3f,
    target: Vec3f,
    fov_y_deg: f32,
    aspect: f32,
    rect: Rect,
    abs: Vec2d,
    plane_z: f32,
) -> Vec3f {
    let nx = (((abs.x - rect.pos.x) / rect.size.x.max(1.0)) * 2.0 - 1.0) as f32;
    let ny = (1.0 - ((abs.y - rect.pos.y) / rect.size.y.max(1.0)) * 2.0) as f32;

    let fwd = vec3(target.x - eye.x, target.y - eye.y, target.z - eye.z);
    let fl = (fwd.x * fwd.x + fwd.y * fwd.y + fwd.z * fwd.z).sqrt().max(1e-6);
    let fwd = vec3(fwd.x / fl, fwd.y / fl, fwd.z / fl);

    let up_w = vec3(0.0, 1.0, 0.0);
    let right = vec3(
        fwd.y * up_w.z - fwd.z * up_w.y,
        fwd.z * up_w.x - fwd.x * up_w.z,
        fwd.x * up_w.y - fwd.y * up_w.x,
    );
    let rl = (right.x * right.x + right.y * right.y + right.z * right.z).sqrt().max(1e-6);
    let right = vec3(right.x / rl, right.y / rl, right.z / rl);

    let up = vec3(
        right.y * fwd.z - right.z * fwd.y,
        right.z * fwd.x - right.x * fwd.z,
        right.x * fwd.y - right.y * fwd.x,
    );

    let tan_half = (fov_y_deg.to_radians() * 0.5).tan();
    let dir = vec3(
        fwd.x + right.x * (nx * aspect * tan_half) + up.x * (ny * tan_half),
        fwd.y + right.y * (nx * aspect * tan_half) + up.y * (ny * tan_half),
        fwd.z + right.z * (nx * aspect * tan_half) + up.z * (ny * tan_half),
    );
    let t = if dir.z.abs() > 1e-5 { (plane_z - eye.z) / dir.z } else { 0.0 };
    vec3(eye.x + dir.x * t, eye.y + dir.y * t, plane_z)
}

// ---------------------------------------------------------------------------
// 6. Shader struct & Interaction state
// ---------------------------------------------------------------------------

#[derive(Script, ScriptHook)]
#[repr(C)]
pub struct DrawDeckMesh {
    #[deref]
    draw_vars: DrawVars,
    #[live]
    model: Mat4f,
    #[live]
    color: Vec4f,
    #[live]
    mat: Vec4f,
}

impl DrawDeckMesh {
    fn draw(&mut self, cx: &mut CxDraw, mesh: &Geometry) {
        self.draw_vars.geometry_id = Some(mesh.geometry_id());
        if self.draw_vars.can_instance() {
            let new_area = cx.add_instance(&mut self.draw_vars);
            self.draw_vars.area = cx.update_area_refs(self.draw_vars.area, new_area);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum GrabTarget {
    #[default]
    None,
    LeftReel,
    RightReel,
    Cassette,
    TapeHead,
    Key(usize),
    Knob(usize),   // 0..3: VOICE, PARAM A, PARAM B
    Slider(usize), // 0..3: LEVEL, BALANCE, TONE (weighted 3D faders)
    CameraOrbit,
}

#[derive(Clone, Debug, Default)]
pub enum DeckAction {
    StateUpdated,
    #[default]
    None,
}

// Layout constants in deck world space (XY faceplate at z=0)
const LEFT_REEL_POS: (f32, f32) = (-1.06, 2.32);
const RIGHT_REEL_POS: (f32, f32) = (1.06, 2.32);
const REEL_Z: f32 = 0.34;
const KEY_Y: f32 = -0.26;
const KEY_XS: [f32; 6] = [-1.85, -1.11, -0.37, 0.37, 1.11, 1.85];
const KEY_LABELS: [&str; 6] = ["REW", "PLAY", "STOP", "REC", "DUB", "EJECT"];
// Row 1: 3 detented rotary knobs (Voice + 2 voice params)
const KNOB_POS: [(f32, f32); 3] = [
    (-1.48, -1.52),
    (0.00, -1.52),
    (1.48, -1.52),
];
// Row 2: 3 weighted vertical 3D output sliders (Level, Balance, Tone)
const SLIDER_XS: [f32; 3] = [-1.42, 0.00, 1.42];
const SLIDER_Y_CENTER: f32 = -3.32;
const SLIDER_HALF_H: f32 = 0.68;

#[derive(Script, Widget)]
pub struct DeckViewport {
    #[uid]
    uid: WidgetUid,
    #[walk]
    walk: Walk,
    #[redraw]
    #[rust]
    area: Area,
    #[live]
    draw_mesh: DrawDeckMesh,
    #[live]
    draw_image: DrawQuad,
    #[live]
    draw_txt: DrawText,
    #[live]
    draw_card: DrawColor,
    #[new]
    pass: DrawPass,
    #[new]
    draw_list: DrawList,
    #[new]
    color_texture: Texture,
    #[new]
    depth_texture: Texture,

    // 4 shared procedural meshes
    #[rust]
    mesh_box: Option<Geometry>,
    #[rust]
    mesh_cyl: Option<Geometry>,
    #[rust]
    mesh_reel: Option<Geometry>,
    #[rust]
    mesh_knob: Option<Geometry>,

    // Camera state
    #[rust]
    cam_yaw: f32,
    #[rust]
    cam_pitch: f32,
    #[rust]
    cam_dist: f32,
    #[rust]
    last_rect: Rect,

    // Mechanical reel & tape-motor scrub physics
    #[rust]
    pos_ms: f64,
    #[rust]
    dur_ms: f64,
    #[rust]
    left_angle: f32,
    #[rust]
    right_angle: f32,
    #[rust]
    tape_phase: f32,
    #[rust]
    scrub_active: bool,
    #[rust]
    scrub_coasting: bool,
    #[rust]
    scrub_omega: f32, // signed rad/s (+ = clockwise = forward)
    #[rust]
    scrub_rate: f32,
    #[rust]
    last_scrub_rate_sent: f32,
    #[rust]
    last_scrub_sent: Option<Instant>,
    #[rust]
    last_reel_move_at: Option<Instant>,
    #[rust]
    reel_unwrapped_ang: f32,
    #[rust]
    reel_hist: [(f64, f32); 8],
    #[rust]
    reel_hist_len: usize,
    #[rust]
    reel_hist_head: usize,
    #[rust]
    last_finger_time: f64,

    // Key travel (0 = up, 1 = fully pressed) & LED flash
    #[rust]
    key_travel: [f32; 6],
    #[rust]
    key_target: [f32; 6],
    #[rust]
    key_flash: [f32; 6],

    // 3 rotary knobs (0: voice, 1..2: active voice param bank) + 3 weighted output sliders (3: level, 4: bal, 5: tone)
    #[rust]
    ctrl_val: [f32; 6],
    #[rust]
    ctrl_disp: [f32; 6],
    #[rust]
    ctrl_vel: [f32; 6],
    #[rust]
    param_bank: usize, // 0 -> params 0..2, 1 -> params 2..4, 2 -> params 4..6

    // Cassette eject & rack state
    #[rust]
    rack_open: bool,
    #[rust]
    eject_t: f32,
    #[rust]
    eject_drag_extra: f32,

    // Pointer interaction state
    #[rust]
    grab: GrabTarget,
    #[rust]
    grab_start_abs: Vec2d,
    #[rust]
    last_abs: Vec2d,
    #[rust]
    last_angle: f32,
    #[rust]
    grab_ctrl_start: f32,
    #[rust]
    last_tap_time: f64,
    #[rust]
    last_tap_ctrl: i32,
    #[rust]
    last_seek_sent: Option<Instant>,
    #[rust]
    last_out_sent: Option<Instant>,
    #[rust]
    last_frame_time: f64,
    #[rust]
    last_drag_draw_at: Option<Instant>,
    #[rust]
    anim_timer: Timer,
    #[rust]
    timer_running: bool,
    #[rust]
    last_wire_rev: u64,
    #[rust]
    cached_state: DeckSharedState,

    // Pending command tracking (touch -> lgtm.ack)
    #[rust]
    pending_seq: u64,
    #[rust]
    pending_name: String,
    #[rust]
    pending_key: i32,
    #[rust]
    pending_since: Option<Instant>,
    #[rust]
    pending_pulse: f32,
    #[rust]
    refused_why: String,
    #[rust]
    refused_until: Option<Instant>,
    /// DECK_SHOWN_EPOCH as last seen: a change means the deck was hidden and is back
    #[rust]
    shown_epoch: u64,
    /// versions of the push streams lgtm_pos, lgtm_orig, lgtm_new, lgtm_head last read
    #[rust]
    streams_seen: [u64; 4],
    /// the engine has been told this gesture is a scrub (stage 1: it stops the live stream)
    #[rust]
    engine_scrub_told: bool,
}

impl ScriptHook for DeckViewport {
    fn on_after_new(&mut self, vm: &mut ScriptVm) {
        self.cam_yaw = 0.0;
        self.cam_pitch = -0.14;
        self.cam_dist = 15.0;
        self.dur_ms = 180_000.0;
        self.ctrl_val = [0.0, 0.33, 0.0, 0.75, 0.5, 0.5];
        self.ctrl_disp = self.ctrl_val;
        self.last_tap_ctrl = -1;
        self.pending_key = -1;

        let hub = wire_hub();
        if let Ok(st) = hub.state.lock() {
            self.cached_state = st.clone();
            self.dur_ms = st.dur_ms.max(1000.0);
        }

        vm.with_cx_mut(|cx| {
            self.mesh_box = Some(build_box_mesh(cx));
            self.mesh_cyl = Some(build_cyl_mesh(cx));
            self.mesh_reel = Some(build_reel_mesh(cx));
            self.mesh_knob = Some(build_knob_mesh(cx));

            self.color_texture = Texture::new_with_format(
                cx,
                TextureFormat::RenderBGRAu8 { size: TextureSize::Auto, initial: true },
            );
            self.depth_texture = Texture::new_with_format(
                cx,
                TextureFormat::DepthD32 { size: TextureSize::Auto, initial: true },
            );
            let bg = vec4(0.04, 0.055, 0.085, 1.0);
            self.pass.set_color_texture(cx, &self.color_texture, DrawPassClearColor::ClearWith(bg));
            self.pass.set_depth_texture(cx, &self.depth_texture, DrawPassClearDepth::ClearWith(1.0));
            self.pass.set_keep_camera_matrix(cx, true);
            self.pass.set_dpi_factor(cx, PASS_SCALE);
        });
    }
}

impl DeckViewport {
    fn camera_eye(&self) -> Vec3f {
        let cp = self.cam_pitch.cos();
        vec3(
            self.cam_dist * cp * self.cam_yaw.sin(),
            -0.28 + self.cam_dist * self.cam_pitch.sin(),
            self.cam_dist * cp * self.cam_yaw.cos(),
        )
    }

    fn camera_target(&self) -> Vec3f {
        vec3(0.0, -0.28, 0.0)
    }

    fn camera_matrices(&self, aspect: f32) -> (Mat4f, Mat4f) {
        let view = Mat4f::look_at(self.camera_eye(), self.camera_target(), vec3(0.0, 1.0, 0.0));
        let proj = Mat4f::perspective(36.0, aspect, 0.2, 60.0);
        (view, proj)
    }

    fn active_voice(&self) -> Option<&VoiceInfo> {
        self.cached_state
            .voices
            .iter()
            .find(|v| v.id == self.cached_state.voice_id)
            .or_else(|| self.cached_state.voices.first())
    }

    fn param_bank_count(&self) -> usize {
        let n = self.active_voice().map(|v| v.params.len()).unwrap_or(0);
        ((n + 1) / 2).max(1)
    }

    fn sync_voice_param_knobs(&mut self) {
        let banks = self.param_bank_count();
        if self.param_bank >= banks {
            self.param_bank = 0;
        }
        let base = self.param_bank * 2;
        if let Some(v) = self
            .cached_state
            .voices
            .iter()
            .find(|v| v.id == self.cached_state.voice_id)
            .or_else(|| self.cached_state.voices.first())
        {
            for slot in 0..2 {
                let ci = slot + 1;
                if !matches!(self.grab, GrabTarget::Knob(k) if k == ci) {
                    if let Some(p) = v.params.get(base + slot) {
                        self.ctrl_val[ci] = if !p.steps.is_empty() {
                            if p.steps.len() > 1 {
                                p.step_idx as f32 / (p.steps.len() - 1) as f32
                            } else {
                                0.5
                            }
                        } else {
                            let span = (p.max - p.min).max(1e-4);
                            ((p.value - p.min) / span).clamp(0.0, 1.0)
                        };
                    } else {
                        self.ctrl_val[ci] = 0.0;
                    }
                }
            }
        }
    }

    pub fn cycle_param_bank(&mut self, cx: &mut Cx) {
        let banks = self.param_bank_count();
        self.param_bank = (self.param_bank + 1) % banks;
        self.sync_voice_param_knobs();
        self.ensure_anim_or_redraw(cx);
    }

    pub fn bank_label(&self) -> String {
        let n = self.active_voice().map(|v| v.params.len()).unwrap_or(0);
        if n <= 2 {
            return format!("P1-{}", n.max(1));
        }
        let a = self.param_bank * 2 + 1;
        let b = (a + 1).min(n);
        format!("P{}-{}/{}", a, b, n)
    }

    fn set_pending(&mut self, seq: u64, name: &str, key_idx: i32) {
        self.pending_seq = seq;
        self.pending_name = name.to_string();
        self.pending_key = key_idx;
        self.pending_since = Some(Instant::now());
        self.pending_pulse = 0.0;
        self.refused_why.clear();
        self.refused_until = None;
    }

    fn is_pending(&self) -> bool {
        self.pending_seq > 0 && self.pending_since.is_some()
    }

    fn sync_from_wire(&mut self, cx: &mut Cx) {
        let hub = wire_hub();
        let snap = {
            let mut st = hub.state.lock().unwrap();
            if st.rev == self.last_wire_rev {
                return;
            }
            self.last_wire_rev = st.rev;
            let dirty_out = st.out_dirty_from_store;
            st.out_dirty_from_store = false;
            (st.clone(), dirty_out)
        };
        let (st, dirty_out) = snap;
        self.dur_ms = st.dur_ms.max(1000.0);

        // Clear local pending indicator once engine acks (or transport seq advances past it)
        if self.pending_seq > 0 {
            if st.ack_seq >= self.pending_seq {
                if st.ack_state == "refused" {
                    self.refused_why = if st.ack_why.is_empty() {
                        format!("{} refused", st.ack_name)
                    } else {
                        st.ack_why.clone()
                    };
                    self.refused_until = Some(Instant::now() + Duration::from_secs(4));
                }
                if st.ack_state != "queued" {
                    self.pending_seq = 0;
                    self.pending_key = -1;
                    self.pending_since = None;
                }
            } else if st.transport_seq >= self.pending_seq {
                self.pending_seq = 0;
                self.pending_key = -1;
                self.pending_since = None;
            }
        }

        // Follow static anchor jumps cleanly when idle, or snap large (>350ms) jumps immediately on show/seek
        if !self.scrub_active && !st.scrubbing && !st.playing {
            let live_ms = st.pos_anchor_ms.clamp(0.0, self.dur_ms);
            let delta_ms = live_ms - self.pos_ms;
            if delta_ms.abs() > 0.1 {
                self.pos_ms = live_ms;
                self.advance_reels_by_ms(delta_ms);
            }
        } else if st.scrubbing || st.playing {
            let heard_ms = st.heard_pos_ms(Instant::now(), self.dur_ms);
            let err_ms = heard_ms - self.pos_ms;
            if err_ms.abs() > 350.0 {
                self.pos_ms = heard_ms;
                self.advance_reels_by_ms(err_ms.clamp(-180.0, 180.0));
            }
        }

        // Drive PLAY key physical latch strictly from authoritative transport state
        self.key_target[1] = match st.transport_state.as_str() {
            "playing" | "seeking" => 0.85,
            "starting" => 0.45,
            _ => if !st.has_transport && st.playing { 0.85 } else { 0.0 },
        };

        if !matches!(self.grab, GrabTarget::Knob(0)) && !st.voices.is_empty() {
            let idx = st.voices.iter().position(|v| v.id == st.voice_id).unwrap_or(0);
            self.ctrl_val[0] = if st.voices.len() > 1 {
                idx as f32 / (st.voices.len() - 1) as f32
            } else {
                0.5
            };
        }
        if dirty_out {
            if !matches!(self.grab, GrabTarget::Slider(0)) { self.ctrl_val[3] = st.out_level; }
            if !matches!(self.grab, GrabTarget::Slider(1)) { self.ctrl_val[4] = st.out_balance; }
            if !matches!(self.grab, GrabTarget::Slider(2)) { self.ctrl_val[5] = st.out_tone; }
        }

        self.cached_state = st;
        self.sync_voice_param_knobs();
        if !self.timer_running {
            if self.needs_animation() {
                cx.widget_action(self.uid, DeckAction::StateUpdated);
                self.ensure_anim_or_redraw(cx);
            } else {
                // Throttle bursty dgram/store updates strictly <= 24 fps
                let now = Instant::now();
                let due = self
                    .last_drag_draw_at
                    .map(|last| now.duration_since(last) >= Duration::from_millis(41))
                    .unwrap_or(true);
                if due {
                    self.last_drag_draw_at = Some(now);
                    cx.widget_action(self.uid, DeckAction::StateUpdated);
                    self.area.redraw(cx);
                }
            }
        } else if !self.needs_animation() {
            cx.stop_timer(self.anim_timer);
            self.anim_timer = Timer::default();
            self.timer_running = false;
            cx.widget_action(self.uid, DeckAction::StateUpdated);
            self.area.redraw(cx);
        }
    }

    fn redraw_throttled(&mut self, cx: &mut Cx) {
        let now = Instant::now();
        if let Some(last) = self.last_drag_draw_at {
            if now.duration_since(last) < Duration::from_millis(32) {
                return;
            }
        }
        self.last_drag_draw_at = Some(now);
        self.area.redraw(cx);
    }

    fn reel_radii(&self) -> (f32, f32) {
        let frac = (self.pos_ms / self.dur_ms.max(1.0)).clamp(0.0, 1.0) as f32;
        let r_min: f32 = 0.45;
        let r_max: f32 = 0.90;
        let r_left = (r_min * r_min + (1.0 - frac) * (r_max * r_max - r_min * r_min)).sqrt();
        let r_right = (r_min * r_min + frac * (r_max * r_max - r_min * r_min)).sqrt();
        (r_left, r_right)
    }

    fn advance_reels_by_ms(&mut self, d_ms: f64) {
        let (r_left, r_right) = self.reel_radii();
        let clamped_ms = d_ms.clamp(-400.0, 400.0);
        let tape_ds = (clamped_ms / 1000.0) as f32 * 1.45;
        self.left_angle -= tape_ds / r_left;
        self.right_angle -= tape_ds / r_right;
        self.tape_phase = (self.tape_phase + tape_ds * 0.6).rem_euclid(1.0);
    }

    fn quantize_scrub_rate(rate: f32) -> f32 {
        let r = rate.clamp(-4.0, 4.0);
        if r.abs() < 0.05 {
            0.0
        } else {
            (r * 100.0).round() / 100.0
        }
    }

    fn reset_reel_hist(&mut self, time: f64) {
        self.reel_unwrapped_ang = 0.0;
        self.reel_hist[0] = (time, 0.0);
        self.reel_hist_len = 1;
        self.reel_hist_head = 0;
    }

    fn push_reel_hist(&mut self, time: f64, unwrapped_ang: f32) -> f32 {
        self.reel_hist_head = (self.reel_hist_head + 1) % 8;
        self.reel_hist[self.reel_hist_head] = (time, unwrapped_ang);
        if self.reel_hist_len < 8 {
            self.reel_hist_len += 1;
        }
        // Sliding-window slope over ~45-60 ms eliminates single-sample touch dt jitter
        let mut best_old: Option<(f64, f32)> = None;
        for back in 1..self.reel_hist_len {
            let idx = (self.reel_hist_head + 8 - back) % 8;
            let (t_old, a_old) = self.reel_hist[idx];
            let dt = time - t_old;
            if dt >= 0.008 {
                best_old = Some((t_old, a_old));
                if dt >= 0.045 {
                    break;
                }
            }
        }
        if let Some((t_old, a_old)) = best_old {
            let dt = (time - t_old).max(0.008) as f32;
            // Clockwise (d_ang < 0) = forward (+omega)
            (-(unwrapped_ang - a_old) / dt).clamp(-24.0, 24.0)
        } else {
            0.0
        }
    }

    fn emit_scrub_rate(&mut self, raw_rate: f32, force: bool) {
        let held = matches!(self.grab, GrabTarget::LeftReel | GrabTarget::RightReel);
        let op = if force && raw_rate == 0.0 && held { "grab" } else { "vel" };
        // Always feed the local native phone head at full frame/touch rate (no 20 Hz throttle)
        if send_head_vel(op, self.scrub_omega, raw_rate, held, self.scrub_coasting) {
            // stage 1: the engine is told once that this is a scrub (it marks the transport
            // scrubbing and stops the live stream to the head); the speed is the head's alone
            if !self.engine_scrub_told {
                self.engine_scrub_told = true;
                let _ = send_lgtm_cmd("scrub", "{\"rate\":0.0}");
            }
            return;
        }
        // Fallback when no local head is running: remote monolith1 stream <= 20 Hz
        let rate = Self::quantize_scrub_rate(raw_rate);
        let now = Instant::now();
        if !force {
            if let Some(last) = self.last_scrub_sent {
                let min_gap_ms = if self.last_scrub_rate_sent == 0.0 && rate != 0.0 {
                    30
                } else {
                    50
                };
                if now.duration_since(last) < Duration::from_millis(min_gap_ms) {
                    return;
                }
            }
            let changed_to_zero = rate == 0.0 && self.last_scrub_rate_sent != 0.0;
            if !changed_to_zero && (rate - self.last_scrub_rate_sent).abs() < 0.02 {
                return;
            }
        }
        self.last_scrub_sent = Some(now);
        self.last_scrub_rate_sent = rate;
        let payload = if rate == 0.0 {
            "{\"rate\":0.0}".to_string()
        } else {
            format!("{{\"rate\":{:.2}}}", rate)
        };
        let _ = send_lgtm_cmd("scrub", &payload);
    }

    fn finish_scrub(&mut self) {
        if self.scrub_active || self.scrub_coasting || self.cached_state.scrubbing {
            self.scrub_active = false;
            self.scrub_coasting = false;
            self.scrub_omega = 0.0;
            self.scrub_rate = 0.0;
            self.last_scrub_rate_sent = 0.0;
            self.last_scrub_sent = None;
            self.last_reel_move_at = None;
            let had_head = send_head_vel("release", 0.0, 0.0, false, false);
            if had_head {
                // the hand-back: the head stops, and the engine goes on from where the tape is heard
                let _ = send_head_raw("{\"scrub_end\":{}}");
                let payload = match head_heard_ms() {
                    Some(ms) => format!("{{\"pos_ms\":{:.1}}}", ms.max(0.0)),
                    None => "{}".to_string(),
                };
                if self.engine_scrub_told {
                    let _ = send_lgtm_cmd("scrub_end", &payload);
                }
            } else {
                let _ = send_lgtm_cmd("scrub_end", "{}");
            }
            self.engine_scrub_told = false;
        }
    }

    pub fn end_scrub_if_active(&mut self) {
        if self.scrub_active || self.scrub_coasting {
            self.finish_scrub();
        }
    }

    fn needs_animation(&self) -> bool {
        if self.cached_state.playing
            || self.cached_state.scrubbing
            || self.scrub_active
            || self.scrub_coasting
            || self.cached_state.transport_state == "starting"
            || self.cached_state.transport_state == "stopping"
        {
            return true;
        }
        if self.is_pending() {
            return true;
        }
        let eject_goal = if self.rack_open { 1.0 } else { 0.0 };
        if (self.eject_t - eject_goal).abs() > 0.005 {
            return true;
        }
        for i in 0..6 {
            if (self.key_travel[i] - self.key_target[i]).abs() > 0.005 || self.key_flash[i] > 0.01 {
                return true;
            }
            if (self.ctrl_disp[i] - self.ctrl_val[i]).abs() > 0.003 || self.ctrl_vel[i].abs() > 0.01 {
                return true;
            }
        }
        false
    }

    fn ensure_anim_or_redraw(&mut self, cx: &mut Cx) {
        self.area.redraw(cx);
        // hidden in the shell: no timer (it starts again when the deck is shown)
        if self.needs_animation() && deck_visible() {
            if !self.timer_running {
                // Even 30 fps (exact 2x divisor of 60 Hz display vsync, <= 30 fps thermal cap; 0 fps at idle)
                self.anim_timer = cx.start_interval(1.0 / 30.0);
                self.timer_running = true;
                self.last_frame_time = 0.0;
            }
        } else if self.timer_running {
            cx.stop_timer(self.anim_timer);
            self.anim_timer = Timer::default();
            self.timer_running = false;
        }
    }

    fn step_animation(&mut self, cx: &mut Cx, dt: f32) {
        let dt = dt.clamp(0.001, 0.05);

        // 1. Reel-motor scrub control (continuous hold-still damping + flick-coast decay)
        let holding_reel = matches!(self.grab, GrabTarget::LeftReel | GrabTarget::RightReel);
        if self.scrub_active {
            if holding_reel {
                let idle_ms = self
                    .last_reel_move_at
                    .map(|t| t.elapsed().as_millis())
                    .unwrap_or(999);
                if idle_ms >= 55 {
                    let decay = (-18.0 * dt).exp();
                    self.scrub_omega *= decay;
                    self.scrub_rate *= decay;
                    if self.scrub_rate.abs() < 0.05 {
                        self.scrub_omega = 0.0;
                        self.scrub_rate = 0.0;
                    }
                }
                self.emit_scrub_rate(self.scrub_rate, false);
            } else if self.scrub_coasting {
                let decay = (-2.8 * dt).exp();
                self.scrub_omega *= decay;
                self.scrub_rate *= decay;
                let hit_end = (self.scrub_rate < 0.0 && self.pos_ms <= 1.0)
                    || (self.scrub_rate > 0.0 && self.pos_ms >= self.dur_ms - 1.0);
                if hit_end || self.scrub_rate.abs() < 0.05 {
                    self.finish_scrub();
                } else {
                    self.emit_scrub_rate(self.scrub_rate, false);
                }
            }
        }

        // 2. Turn reels smoothly from heard position moved along at current signed rate (zero jump)
        if self.cached_state.scrubbing || self.cached_state.playing {
            let now = Instant::now();
            let heard_ms = self.cached_state.heard_pos_ms(now, self.dur_ms);
            let err_ms = heard_ms - self.pos_ms;
            if err_ms.abs() > 350.0 {
                self.pos_ms = heard_ms;
                self.advance_reels_by_ms(err_ms.clamp(-180.0, 180.0));
            } else {
                let rate = self.cached_state.effective_rate();
                let step_ms = rate * (dt as f64) * 1000.0;
                let corr = (heard_ms - (self.pos_ms + step_ms)) * (1.0 - (-15.0 * dt as f64).exp());
                let next_ms = (self.pos_ms + step_ms + corr).clamp(0.0, self.dur_ms);
                let actual_d_ms = next_ms - self.pos_ms;
                self.pos_ms = next_ms;
                self.advance_reels_by_ms(actual_d_ms);
            }
        } else if self.scrub_active {
            // Brief local dead-reckoning before first engine sub=='scrubbing' packet arrives (or offline)
            let step_ms = (self.scrub_rate as f64) * (dt as f64) * 1000.0;
            let next_ms = (self.pos_ms + step_ms).clamp(0.0, self.dur_ms);
            let actual_d_ms = next_ms - self.pos_ms;
            self.pos_ms = next_ms;
            self.advance_reels_by_ms(actual_d_ms);
        }

        // Keys & detent knobs (crisp spring)
        let k_alpha = 1.0 - (-18.0 * dt).exp();
        for i in 0..6 {
            self.key_travel[i] += (self.key_target[i] - self.key_travel[i]) * k_alpha;
            if (self.key_travel[i] - self.key_target[i]).abs() < 0.004 {
                self.key_travel[i] = self.key_target[i];
            }
            self.key_flash[i] = (self.key_flash[i] - dt * 1.6).max(0.0);
        }
        // Knobs 0..3: snappy mechanical detent follow
        let knob_alpha = 1.0 - (-24.0 * dt).exp();
        for i in 0..3 {
            self.ctrl_disp[i] += (self.ctrl_val[i] - self.ctrl_disp[i]) * knob_alpha;
            if (self.ctrl_disp[i] - self.ctrl_val[i]).abs() < 0.003 {
                self.ctrl_disp[i] = self.ctrl_val[i];
            }
            self.ctrl_vel[i] = 0.0;
        }
        // Sliders 3..6: weighted faders with mass + viscous damping (smooth coast & settle!)
        for i in 3..6 {
            let si = i - 3;
            let held = matches!(self.grab, GrabTarget::Slider(s) if s == si);
            if !held && self.ctrl_vel[i].abs() > 0.01 {
                self.ctrl_val[i] = (self.ctrl_val[i] + self.ctrl_vel[i] * dt).clamp(0.0, 1.0);
                self.ctrl_vel[i] *= (-8.5 * dt).exp();
                if self.ctrl_vel[i].abs() <= 0.01 {
                    self.ctrl_vel[i] = 0.0;
                    self.commit_ctrl(i, true);
                } else {
                    self.commit_ctrl(i, false);
                }
            }
            let diff = self.ctrl_val[i] - self.ctrl_disp[i];
            self.ctrl_disp[i] += diff * (1.0 - (-16.0 * dt).exp());
            if diff.abs() < 0.003 {
                self.ctrl_disp[i] = self.ctrl_val[i];
            }
        }

        let eject_goal = if self.rack_open { 1.0 } else { 0.0 };
        self.eject_t += (eject_goal - self.eject_t) * (1.0 - (-11.0 * dt).exp());
        if (self.eject_t - eject_goal).abs() < 0.004 {
            self.eject_t = eject_goal;
        }

        // Pending pulse + 3s offline timeout (so pending never stays stuck if link is off)
        if let Some(since) = self.pending_since {
            self.pending_pulse = (self.pending_pulse + dt * 7.0) % std::f32::consts::TAU;
            if since.elapsed() > Duration::from_secs(3) {
                self.pending_seq = 0;
                self.pending_key = -1;
                self.pending_since = None;
            }
        }
        if let Some(until) = self.refused_until {
            if Instant::now() >= until {
                self.refused_why.clear();
                self.refused_until = None;
            }
        }

        self.area.redraw(cx);
        if !self.needs_animation() && self.timer_running {
            cx.stop_timer(self.anim_timer);
            self.anim_timer = Timer::default();
            self.timer_running = false;
        }
    }

    fn emit_seek_now(&mut self, force: bool) {
        let now = Instant::now();
        if !force {
            if let Some(last) = self.last_seek_sent {
                if now.duration_since(last) < Duration::from_millis(55) {
                    return;
                }
            }
        }
        self.last_seek_sent = Some(now);
        let ms = self.pos_ms.round() as i64;
        let seq = send_lgtm_cmd("seek", &format!("{{\"ms\":{}}}", ms));
        if force {
            self.set_pending(seq, "seek", 0);
        }
    }

    fn trigger_key(&mut self, idx: usize, cx: &mut Cx) {
        self.key_flash[idx] = 1.0;
        match idx {
            0 => {
                self.finish_scrub();
                self.pos_ms = (self.pos_ms - 5000.0).max(0.0);
                self.left_angle += 2.2;
                self.right_angle += 2.2;
                self.emit_seek_now(true);
            }
            1 => {
                // Do not guess transport state: show immediate pending (<100ms) until lgtm.ack / lgtm.transport
                self.key_target[1] = 0.45;
                let seq = send_lgtm_cmd("play", "{}");
                self.set_pending(seq, "play", 1);
            }
            2 => {
                self.finish_scrub();
                let seq = send_lgtm_cmd("stop", "{}");
                self.set_pending(seq, "stop", 2);
            }
            3 => {
                let seq = send_lgtm_cmd("save", "{}");
                self.set_pending(seq, "save", 3);
            }
            4 => {
                let seq = send_lgtm_cmd("again", "{}");
                self.set_pending(seq, "again", 4);
            }
            5 => {
                if !self.rack_open {
                    self.finish_scrub();
                }
                self.rack_open = !self.rack_open;
                self.key_target[5] = if self.rack_open { 0.7 } else { 0.0 };
            }
            _ => {}
        }
        cx.widget_action(self.uid, DeckAction::StateUpdated);
        self.ensure_anim_or_redraw(cx);
    }

    fn toggle_head_loop(&mut self, cx: &mut Cx) {
        let seq = if self.cached_state.loop_active {
            send_lgtm_cmd("loop", "{}")
        } else {
            let ln = self.cached_state.line_index_at_ms(self.pos_ms).unwrap_or(0);
            send_lgtm_cmd("loop", &format!("{{\"line\":{}}}", ln))
        };
        self.set_pending(seq, "loop", -1);
        cx.widget_action(self.uid, DeckAction::StateUpdated);
        self.ensure_anim_or_redraw(cx);
    }

    fn commit_ctrl(&mut self, idx: usize, force: bool) {
        let now = Instant::now();
        if !force {
            if let Some(last) = self.last_out_sent {
                if now.duration_since(last) < Duration::from_millis(55) {
                    return;
                }
            }
        }
        self.last_out_sent = Some(now);

        match idx {
            0 => {
                let voices = &self.cached_state.voices;
                if !voices.is_empty() {
                    let n = voices.len();
                    let vi = if n > 1 {
                        ((self.ctrl_val[0] * (n - 1) as f32).round() as usize).min(n - 1)
                    } else {
                        0
                    };
                    self.ctrl_val[0] = if n > 1 { vi as f32 / (n - 1) as f32 } else { 0.5 };
                    let vid = voices[vi].id.clone();
                    if vid != self.cached_state.voice_id || force {
                        self.cached_state.voice_id = vid.clone();
                        self.param_bank = 0;
                        self.sync_voice_param_knobs();
                        send_lgtm_cmd("voice", &format!("{{\"id\":\"{}\"}}", json_escape(&vid)));
                    }
                }
            }
            1 | 2 => {
                let pi = self.param_bank * 2 + (idx - 1);
                let vid = self.cached_state.voice_id.clone();
                if let Some(v) = self.cached_state.voices.iter_mut().find(|v| v.id == vid) {
                    if let Some(p) = v.params.get_mut(pi) {
                        let pid = json_escape(&p.id);
                        let vesc = json_escape(&vid);
                        if !p.steps.is_empty() {
                            let n = p.steps.len();
                            let si = if n > 1 {
                                ((self.ctrl_val[idx] * (n - 1) as f32).round() as usize).min(n - 1)
                            } else {
                                0
                            };
                            p.step_idx = si;
                            self.ctrl_val[idx] = if n > 1 { si as f32 / (n - 1) as f32 } else { 0.5 };
                            let raw_val = if p.step_is_num {
                                p.steps[si].clone()
                            } else {
                                format!("\"{}\"", json_escape(&p.steps[si]))
                            };
                            send_lgtm_cmd(
                                "voice_param",
                                &format!("{{\"voice\":\"{}\",\"id\":\"{}\",\"value\":{}}}", vesc, pid, raw_val),
                            );
                        } else {
                            let real_v = p.min + self.ctrl_val[idx].clamp(0.0, 1.0) * (p.max - p.min);
                            p.value = real_v;
                            send_lgtm_cmd(
                                "voice_param",
                                &format!("{{\"voice\":\"{}\",\"id\":\"{}\",\"value\":{:.3}}}", vesc, pid, real_v),
                            );
                        }
                    }
                }
            }
            3 | 4 | 5 => {
                send_lgtm_out(self.ctrl_val[3], self.ctrl_val[4], self.ctrl_val[5]);
            }
            _ => {}
        }
    }

    fn reset_ctrl_to_rest(&mut self, idx: usize, cx: &mut Cx) {
        self.ctrl_vel[idx] = 0.0;
        match idx {
            0 => self.ctrl_val[0] = 0.0,
            1 | 2 => {
                let pi = self.param_bank * 2 + (idx - 1);
                if let Some(v) = self.active_voice() {
                    if let Some(p) = v.params.get(pi) {
                        self.ctrl_val[idx] = if !p.steps.is_empty() {
                            if p.steps.len() > 1 {
                                p.rest_step_idx as f32 / (p.steps.len() - 1) as f32
                            } else {
                                0.0
                            }
                        } else {
                            let span = (p.max - p.min).max(1e-4);
                            ((p.rest - p.min) / span).clamp(0.0, 1.0)
                        };
                    }
                }
            }
            3 => self.ctrl_val[3] = 0.75,
            4 => self.ctrl_val[4] = 0.50,
            5 => self.ctrl_val[5] = 0.50,
            _ => {}
        }
        self.commit_ctrl(idx, true);
        cx.widget_action(self.uid, DeckAction::StateUpdated);
        self.ensure_anim_or_redraw(cx);
    }

    fn ctrl_readout(&self, idx: usize) -> (String, String) {
        match idx {
            0 => {
                let name = self
                    .active_voice()
                    .map(|v| v.id.as_str())
                    .unwrap_or(&self.cached_state.voice_id);
                let short: String = name.chars().take(11).collect();
                ("VOICE".into(), short)
            }
            1 | 2 => {
                let pi = self.param_bank * 2 + (idx - 1);
                if let Some(v) = self.active_voice() {
                    if let Some(p) = v.params.get(pi) {
                        if !p.steps.is_empty() {
                            let val = p.steps.get(p.step_idx).map(|s| s.as_str()).unwrap_or("-");
                            let short: String = val.chars().take(10).collect();
                            return (p.name.clone(), short);
                        } else {
                            let real_v = p.min + self.ctrl_val[idx] * (p.max - p.min);
                            return (p.name.clone(), format!("{:.2}", real_v));
                        }
                    }
                }
                (format!("PARAM {}", pi + 1), "-".into())
            }
            3 => {
                let db = (self.ctrl_val[3] - 0.75) * 24.0;
                ("LEVEL".into(), format!("{:+.1}dB", db))
            }
            4 => {
                let pct = (self.ctrl_val[4] * 100.0).round() as i32;
                ("BAL".into(), format!("{}%", pct))
            }
            5 => {
                let pct = ((self.ctrl_val[5] - 0.5) * 200.0).round() as i32;
                ("TONE".into(), format!("{:+}", pct))
            }
            _ => (String::new(), String::new()),
        }
    }

    fn draw_box(&mut self, cx: &mut Cx2d, m: Mat4f, color: Vec4f, mat: Vec4f) {
        if let Some(mesh) = self.mesh_box.as_ref() {
            self.draw_mesh.model = m;
            self.draw_mesh.color = color;
            self.draw_mesh.mat = mat;
            self.draw_mesh.draw(cx, mesh);
        }
    }

    fn draw_cyl(&mut self, cx: &mut Cx2d, m: Mat4f, color: Vec4f, mat: Vec4f) {
        if let Some(mesh) = self.mesh_cyl.as_ref() {
            self.draw_mesh.model = m;
            self.draw_mesh.color = color;
            self.draw_mesh.mat = mat;
            self.draw_mesh.draw(cx, mesh);
        }
    }

    fn draw_reel(&mut self, cx: &mut Cx2d, m: Mat4f, color: Vec4f, mat: Vec4f) {
        if let Some(mesh) = self.mesh_reel.as_ref() {
            self.draw_mesh.model = m;
            self.draw_mesh.color = color;
            self.draw_mesh.mat = mat;
            self.draw_mesh.draw(cx, mesh);
        }
    }

    fn draw_knob(&mut self, cx: &mut Cx2d, m: Mat4f, color: Vec4f, mat: Vec4f) {
        if let Some(mesh) = self.mesh_knob.as_ref() {
            self.draw_mesh.model = m;
            self.draw_mesh.color = color;
            self.draw_mesh.mat = mat;
            self.draw_mesh.draw(cx, mesh);
        }
    }

    /// 4 instanced GPU draw calls total (boxes, cylinders, reels, knobs).
    fn draw_scene(&mut self, cx: &mut Cx2d) {
        let ej = (self.eject_t + self.eject_drag_extra).clamp(0.0, 1.35);
        let cas_y = ej * 0.85;
        let cas_z = ej * 1.15;
        let (r_left, r_right) = self.reel_radii();

        // === DRAW CALL 1: ALL BOXES ===
        // Main dark-anodized deck faceplate & side rails
        self.draw_box(cx, mat4_trs_z(0.0, -0.28, -0.36, 0.0, 2.38, 4.46, 0.34), vec4(0.11, 0.13, 0.17, 1.0), vec4(0.35, 0.0, 0.4, 0.0));
        self.draw_box(cx, mat4_trs_z(-2.40, -0.28, -0.18, 0.0, 0.09, 4.48, 0.42), vec4(0.32, 0.36, 0.42, 1.0), vec4(0.75, 0.0, 0.8, 0.0));
        self.draw_box(cx, mat4_trs_z(2.40, -0.28, -0.18, 0.0, 0.09, 4.48, 0.42), vec4(0.32, 0.36, 0.42, 1.0), vec4(0.75, 0.0, 0.8, 0.0));

        // Recessed cassette well & bezels
        self.draw_box(cx, mat4_trs_z(0.0, 2.20, -0.02, 0.0, 2.16, 1.62, 0.05), vec4(0.04, 0.055, 0.08, 1.0), vec4(0.15, 0.0, 0.1, 0.0));
        self.draw_box(cx, mat4_trs_z(0.0, 3.88, 0.08, 0.0, 2.25, 0.09, 0.12), vec4(0.24, 0.28, 0.35, 1.0), vec4(0.6, 0.0, 0.5, 0.0));
        self.draw_box(cx, mat4_trs_z(0.0, 0.54, 0.08, 0.0, 2.25, 0.08, 0.12), vec4(0.24, 0.28, 0.35, 1.0), vec4(0.6, 0.0, 0.5, 0.0));

        // Cassette shell (slides out on eject!)
        let shell_col = if self.cached_state.song_gen > 0 {
            vec4(0.18, 0.21, 0.28, 1.0)
        } else {
            vec4(0.16, 0.18, 0.22, 1.0)
        };
        // Vintage cassette label strip (accent color shifts with generation!)
        let label_col = match self.cached_state.song_gen {
            0 => vec4(0.88, 0.56, 0.20, 1.0),
            1 => vec4(0.90, 0.38, 0.24, 1.0),
            2 => vec4(0.22, 0.74, 0.76, 1.0),
            _ => vec4(0.64, 0.42, 0.90, 1.0),
        };
        self.draw_box(cx, mat4_trs_z(0.0, 3.42 + cas_y, 0.18 + cas_z, 0.0, 1.98, 0.25, 0.14), label_col, vec4(0.25, 0.08, 0.1, 0.0));
        // 8 miniature live fader marks across the cassette shell showing current lgtm_mix
        for mi in 0..8 {
            let mx = -1.12 + mi as f32 * 0.32;
            let mv = self.cached_state.mix_marks[mi].clamp(0.0, 1.0);
            let mh = 0.03 + mv * 0.12;
            self.draw_box(
                cx,
                mat4_trs_z(mx, 3.06 + cas_y + mh * 0.5, 0.25 + cas_z, 0.0, 0.06, mh, 0.03),
                vec4(0.95, 0.88, 0.72, 1.0),
                vec4(0.3, 0.35 + mv * 0.4, 0.1, 0.0),
            );
        }
        self.draw_box(cx, mat4_trs_z(-1.90, 2.20 + cas_y, 0.18 + cas_z, 0.0, 0.14, 1.02, 0.14), shell_col, vec4(0.4, 0.0, 0.3, 0.0));
        self.draw_box(cx, mat4_trs_z(1.90, 2.20 + cas_y, 0.18 + cas_z, 0.0, 0.14, 1.02, 0.14), shell_col, vec4(0.4, 0.0, 0.3, 0.0));
        self.draw_box(cx, mat4_trs_z(0.0, 1.10 + cas_y, 0.20 + cas_z, 0.0, 1.80, 0.14, 0.15), vec4(0.13, 0.15, 0.19, 1.0), vec4(0.4, 0.0, 0.3, 0.0));
        self.draw_box(cx, mat4_trs_z(0.0, 2.28 + cas_y, 0.08 + cas_z, 0.0, 1.85, 0.96, 0.04), vec4(0.06, 0.075, 0.11, 1.0), vec4(0.2, 0.0, 0.2, 0.0));

        // Magnetic tape ribbon + moving amber shimmer tick
        let tape_col = vec4(0.24, 0.14, 0.10, 1.0);
        let tape_glint = if self.cached_state.playing || self.cached_state.scrubbing || self.scrub_active {
            0.72
        } else {
            0.42
        };
        self.draw_box(cx, mat4_trs_z(0.0, 1.28 + cas_y, 0.28 + cas_z, 0.0, 1.48, 0.032, 0.06), tape_col, vec4(tape_glint, 0.0, 0.3, 0.0));
        self.draw_box(cx, mat4_trs_z(-1.46, 1.66 + cas_y, 0.28 + cas_z, -0.35, 0.03, 0.45, 0.05), tape_col, vec4(tape_glint, 0.0, 0.2, 0.0));
        self.draw_box(cx, mat4_trs_z(1.46, 1.66 + cas_y, 0.28 + cas_z, 0.35, 0.03, 0.45, 0.05), tape_col, vec4(tape_glint, 0.0, 0.2, 0.0));
        let tick_x = -1.25 + self.tape_phase * 2.50;
        self.draw_box(cx, mat4_trs_z(tick_x, 1.28 + cas_y, 0.33 + cas_z, 0.0, 0.13, 0.038, 0.03), vec4(0.96, 0.58, 0.20, 1.0), vec4(0.5, 0.70, 0.2, 0.0));

        // Fixed Chrome Magnetic Tape Head (tap to toggle loop!)
        let head_emit = if self.cached_state.loop_active { 0.75 } else if self.cached_state.playing { 0.40 } else { 0.0 };
        let head_accent = if self.key_flash[3] > 0.1 {
            vec4(0.95, 0.22, 0.28, 1.0)
        } else if self.cached_state.loop_active {
            vec4(0.20, 0.92, 1.00, 1.0)
        } else if self.cached_state.playing {
            vec4(0.28, 0.85, 0.70, 1.0)
        } else {
            vec4(0.70, 0.76, 0.84, 1.0)
        };
        self.draw_box(cx, mat4_trs_z(0.0, 0.99, 0.24, 0.0, 0.35, 0.20, 0.18), vec4(0.68, 0.73, 0.80, 1.0), vec4(0.9, 0.0, 0.9, 0.0));
        self.draw_box(cx, mat4_trs_z(0.0, 1.16, 0.36, 0.0, 0.18, 0.045, 0.08), head_accent, vec4(0.8, head_emit, 0.6, 0.0));

        // Dual 3D VU Level Meter Bars
        let lvl_o = self.cached_state.level_orig.max(if self.cached_state.playing { 0.08 } else { 0.02 });
        let lvl_n = self.cached_state.level_new.max(if self.cached_state.playing { 0.08 } else { 0.02 });
        self.draw_box(cx, mat4_trs_z(-1.12, 0.72, 0.06, 0.0, 0.88, 0.065, 0.04), vec4(0.06, 0.09, 0.13, 1.0), vec4(0.2, 0.0, 0.1, 0.0));
        self.draw_box(cx, mat4_trs_z(1.12, 0.72, 0.06, 0.0, 0.88, 0.065, 0.04), vec4(0.06, 0.09, 0.13, 1.0), vec4(0.2, 0.0, 0.1, 0.0));
        let w_o = (lvl_o * 0.84).max(0.03);
        let w_n = (lvl_n * 0.84).max(0.03);
        self.draw_box(cx, mat4_trs_z(-1.96 + w_o, 0.72, 0.11, 0.0, w_o, 0.05, 0.04), vec4(0.96, 0.68, 0.22, 1.0), vec4(0.4, 0.55 + lvl_o * 0.4, 0.1, 0.0));
        self.draw_box(cx, mat4_trs_z(0.28 + w_n, 0.72, 0.11, 0.0, w_n, 0.05, 0.04), vec4(0.22, 0.78, 0.98, 1.0), vec4(0.4, 0.55 + lvl_n * 0.4, 0.1, 0.0));

        // 6 Mechanical Deck Keys
        let key_colors = [
            vec4(0.20, 0.25, 0.34, 1.0),
            vec4(0.14, 0.42, 0.34, 1.0),
            vec4(0.22, 0.26, 0.33, 1.0),
            vec4(0.48, 0.16, 0.20, 1.0),
            vec4(0.32, 0.22, 0.48, 1.0),
            vec4(0.26, 0.30, 0.38, 1.0),
        ];
        let led_colors = [
            vec4(0.40, 0.75, 0.98, 1.0),
            vec4(0.22, 0.95, 0.60, 1.0),
            vec4(0.95, 0.75, 0.30, 1.0),
            vec4(1.00, 0.24, 0.30, 1.0),
            vec4(0.78, 0.48, 1.00, 1.0),
            vec4(0.98, 0.65, 0.22, 1.0),
        ];
        let pulse = 0.55 + 0.45 * self.pending_pulse.sin();
        for i in 0..6 {
            let t = self.key_travel[i];
            let z = 0.22 - t * 0.16;
            let tilt = t * 0.16;
            let is_key_pending = self.is_pending() && self.pending_key == i as i32;
            let is_starting = i == 1 && self.cached_state.transport_state == "starting";
            let is_stopping = i == 2 && self.cached_state.transport_state == "stopping";
            let key_emit = if is_key_pending { 0.28 * pulse } else { self.key_flash[i] * 0.25 };
            self.draw_box(cx, mat4_trs_xz(KEY_XS[i], KEY_Y, z, tilt, 0.31, 0.32, 0.18), key_colors[i], vec4(0.55, key_emit, 0.4, 0.0));
            let (led_col, led_on) = if is_key_pending || is_starting || is_stopping {
                (vec4(1.0, 0.88, 0.35, 1.0), pulse)
            } else {
                let on = self.key_flash[i].max(if i == 1 && self.cached_state.playing {
                    1.0
                } else if i == 5 && self.rack_open {
                    0.9
                } else {
                    0.08
                });
                (led_colors[i], on)
            };
            self.draw_box(cx, mat4_trs_xz(KEY_XS[i], KEY_Y + 0.19, z + 0.17, tilt, 0.14, 0.032, 0.03), led_col, vec4(0.3, led_on, 0.1, 0.0));
        }

        // Divider trim bars
        self.draw_box(cx, mat4_trs_z(0.0, -0.78, 0.06, 0.0, 2.18, 0.03, 0.08), vec4(0.25, 0.30, 0.38, 1.0), vec4(0.7, 0.0, 0.6, 0.0));
        self.draw_box(cx, mat4_trs_z(0.0, -2.42, 0.06, 0.0, 2.18, 0.025, 0.07), vec4(0.22, 0.26, 0.34, 1.0), vec4(0.6, 0.0, 0.5, 0.0));

        // Row 2: 3 Weighted 3D Output Sliders (LEVEL, BALANCE, TONE)
        let slider_accents = [
            vec4(0.98, 0.62, 0.20, 1.0), // LEVEL (warm amber)
            vec4(0.24, 0.80, 0.98, 1.0), // BALANCE (cyan)
            vec4(0.74, 0.54, 0.98, 1.0), // TONE (violet)
        ];
        for si in 0..3 {
            let sx = SLIDER_XS[si];
            let val = self.ctrl_disp[3 + si].clamp(0.0, 1.0);
            let active = matches!(self.grab, GrabTarget::Slider(k) if k == si);
            let y_bot = SLIDER_Y_CENTER - SLIDER_HALF_H;
            let y_cap = y_bot + val * (SLIDER_HALF_H * 2.0);

            // Recessed track bezel & dark inner slot
            self.draw_box(cx, mat4_trs_z(sx, SLIDER_Y_CENTER, 0.02, 0.0, 0.24, SLIDER_HALF_H + 0.12, 0.05), vec4(0.16, 0.20, 0.27, 1.0), vec4(0.4, 0.0, 0.3, 0.0));
            self.draw_box(cx, mat4_trs_z(sx, SLIDER_Y_CENTER, 0.05, 0.0, 0.075, SLIDER_HALF_H + 0.04, 0.04), vec4(0.03, 0.04, 0.06, 1.0), vec4(0.1, 0.0, 0.1, 0.0));

            // Glowing LED level fill from bottom up to cap
            let fill_h = (val * SLIDER_HALF_H).max(0.02);
            let fill_cy = y_bot + fill_h;
            self.draw_box(cx, mat4_trs_z(sx, fill_cy, 0.08, 0.0, 0.055, fill_h, 0.03), slider_accents[si], vec4(0.3, 0.65, 0.1, 0.0));

            // Center rest tick mark (0.75 for level, 0.50 for balance/tone)
            let rest_v = if si == 0 { 0.75 } else { 0.50 };
            let rest_y = y_bot + rest_v * (SLIDER_HALF_H * 2.0);
            self.draw_box(cx, mat4_trs_z(sx - 0.30, rest_y, 0.05, 0.0, 0.06, 0.018, 0.03), vec4(0.55, 0.62, 0.72, 1.0), vec4(0.4, 0.1, 0.2, 0.0));
            self.draw_box(cx, mat4_trs_z(sx + 0.30, rest_y, 0.05, 0.0, 0.06, 0.018, 0.03), vec4(0.55, 0.62, 0.72, 1.0), vec4(0.4, 0.1, 0.2, 0.0));

            // Tactile 3D weighted fader cap (machined aluminum + glowing grip stripe)
            let cap_col = if active { vec4(0.42, 0.52, 0.66, 1.0) } else { vec4(0.28, 0.33, 0.42, 1.0) };
            self.draw_box(cx, mat4_trs_z(sx, y_cap, 0.26, 0.0, 0.36, 0.19, 0.18), cap_col, vec4(0.78, if active { 0.15 } else { 0.0 }, 0.8, 0.0));
            self.draw_box(cx, mat4_trs_z(sx, y_cap, 0.44, 0.0, 0.31, 0.032, 0.03), slider_accents[si], vec4(0.4, if active { 0.95 } else { 0.55 }, 0.2, 0.0));
        }

        // === DRAW CALL 2: ALL CYLINDERS (tape packs, rollers, brass spindles, reel index caps, knob collars & detent dots) ===
        self.draw_cyl(cx, mat4_trs_z(LEFT_REEL_POS.0, LEFT_REEL_POS.1 + cas_y, 0.22 + cas_z, self.left_angle, r_left, r_left, 0.24), vec4(0.20, 0.11, 0.08, 1.0), vec4(0.32, 0.0, 0.15, 0.0));
        self.draw_cyl(cx, mat4_trs_z(RIGHT_REEL_POS.0, RIGHT_REEL_POS.1 + cas_y, 0.22 + cas_z, self.right_angle, r_right, r_right, 0.24), vec4(0.20, 0.11, 0.08, 1.0), vec4(0.32, 0.0, 0.15, 0.0));

        // Bright amber rotation index dots mounted right on spoke 0 of each reel!
        let lx_dot = LEFT_REEL_POS.0 + self.left_angle.cos() * 0.64;
        let ly_dot = LEFT_REEL_POS.1 + cas_y + self.left_angle.sin() * 0.64;
        let rx_dot = RIGHT_REEL_POS.0 + self.right_angle.cos() * 0.64;
        let ry_dot = RIGHT_REEL_POS.1 + cas_y + self.right_angle.sin() * 0.64;
        self.draw_cyl(cx, mat4_trs_z(lx_dot, ly_dot, REEL_Z + cas_z + 0.15, 0.0, 0.11, 0.11, 0.08), vec4(0.98, 0.52, 0.16, 1.0), vec4(0.5, 0.55, 0.2, 0.0));
        self.draw_cyl(cx, mat4_trs_z(rx_dot, ry_dot, REEL_Z + cas_z + 0.15, 0.0, 0.11, 0.11, 0.08), vec4(0.22, 0.84, 0.98, 1.0), vec4(0.5, 0.55, 0.2, 0.0));

        // Steel guide rollers & warm brass hub spindles
        self.draw_cyl(cx, mat4_trs_z(-1.54, 1.28 + cas_y, 0.28 + cas_z, self.left_angle * 2.0, 0.11, 0.11, 0.22), vec4(0.76, 0.80, 0.86, 1.0), vec4(0.92, 0.0, 0.9, 0.0));
        self.draw_cyl(cx, mat4_trs_z(1.54, 1.28 + cas_y, 0.28 + cas_z, self.right_angle * 2.0, 0.11, 0.11, 0.22), vec4(0.76, 0.80, 0.86, 1.0), vec4(0.92, 0.0, 0.9, 0.0));
        self.draw_cyl(cx, mat4_trs_z(LEFT_REEL_POS.0, LEFT_REEL_POS.1 + cas_y, 0.34 + cas_z, 0.0, 0.14, 0.14, 0.28), vec4(0.84, 0.66, 0.32, 1.0), vec4(0.85, 0.05, 0.8, 0.0));
        self.draw_cyl(cx, mat4_trs_z(RIGHT_REEL_POS.0, RIGHT_REEL_POS.1 + cas_y, 0.34 + cas_z, 0.0, 0.14, 0.14, 0.28), vec4(0.84, 0.66, 0.32, 1.0), vec4(0.85, 0.05, 0.8, 0.0));

        // Knob base collars + illuminated detent pips (Row 1: VOICE, PARAM A, PARAM B)
        for (ki, &(kx, ky)) in KNOB_POS.iter().enumerate() {
            let active = matches!(self.grab, GrabTarget::Knob(k) if k == ki);
            let collar_col = if active { vec4(0.24, 0.72, 0.98, 1.0) } else { vec4(0.18, 0.22, 0.29, 1.0) };
            self.draw_cyl(cx, mat4_trs_z(kx, ky, 0.05, 0.0, 0.56, 0.56, 0.10), collar_col, vec4(0.5, if active { 0.45 } else { 0.0 }, 0.4, 0.0));

            let dots = self.knob_detent_count(ki);
            let val = self.ctrl_disp[ki];
            for di in 0..dots {
                let f = if dots > 1 { di as f32 / (dots - 1) as f32 } else { 0.5 };
                let ang = (135.0 - f * 270.0).to_radians();
                let dx = kx - ang.sin() * 0.64;
                let dy = ky + ang.cos() * 0.64;
                let lit = if ki == 0 { (f - val).abs() < (0.52 / (dots.max(2) - 1) as f32) } else { f <= val + 0.04 };
                let pip_col = if lit { vec4(0.26, 0.84, 0.98, 1.0) } else { vec4(0.16, 0.20, 0.26, 1.0) };
                self.draw_cyl(cx, mat4_trs_z(dx, dy, 0.08, 0.0, 0.044, 0.044, 0.06), pip_col, vec4(0.3, if lit { 0.80 } else { 0.0 }, 0.1, 0.0));
            }
        }

        // === DRAW CALL 3: BOTH 6-SPOKE MACHINED CASSETTE REELS ===
        let left_grab = matches!(self.grab, GrabTarget::LeftReel);
        let right_grab = matches!(self.grab, GrabTarget::RightReel);
        let reel_col_l = if left_grab { vec4(0.98, 0.72, 0.34, 1.0) } else { vec4(0.84, 0.87, 0.92, 1.0) };
        let reel_col_r = if right_grab { vec4(0.98, 0.72, 0.34, 1.0) } else { vec4(0.84, 0.87, 0.92, 1.0) };
        self.draw_reel(cx, mat4_trs_z(LEFT_REEL_POS.0, LEFT_REEL_POS.1 + cas_y, REEL_Z + cas_z, self.left_angle, 0.94, 0.94, 0.70), reel_col_l, vec4(0.82, if left_grab { 0.22 } else { 0.0 }, 0.85, 0.0));
        self.draw_reel(cx, mat4_trs_z(RIGHT_REEL_POS.0, RIGHT_REEL_POS.1 + cas_y, REEL_Z + cas_z, self.right_angle, 0.94, 0.94, 0.70), reel_col_r, vec4(0.82, if right_grab { 0.22 } else { 0.0 }, 0.85, 0.0));

        // === DRAW CALL 4: 3 DETENTED ROTARY KNOBS ===
        for (ki, &(kx, ky)) in KNOB_POS.iter().enumerate() {
            let active = matches!(self.grab, GrabTarget::Knob(k) if k == ki);
            let rot = (135.0 - self.ctrl_disp[ki] * 270.0).to_radians();
            let col = if active {
                vec4(0.45, 0.78, 0.98, 1.0)
            } else if ki == 0 {
                vec4(0.32, 0.38, 0.50, 1.0)
            } else {
                vec4(0.26, 0.32, 0.42, 1.0)
            };
            self.draw_knob(cx, mat4_trs_z(kx, ky, 0.10, rot, 0.48, 0.48, 0.56), col, vec4(0.75, if active { 0.18 } else { 0.0 }, 0.7, 0.0));
        }
    }

    fn knob_detent_count(&self, idx: usize) -> usize {
        match idx {
            0 => self.cached_state.voices.len().clamp(2, 9),
            1 | 2 => {
                let pi = self.param_bank * 2 + (idx - 1);
                if let Some(v) = self.active_voice() {
                    if let Some(p) = v.params.get(pi) {
                        if !p.steps.is_empty() {
                            return p.steps.len().clamp(2, 9);
                        }
                    }
                }
                7
            }
            _ => 7,
        }
    }

    fn draw_overlay_labels(&mut self, cx: &mut Cx2d, rect: Rect, vp: Mat4f) {
        // 1. Key labels
        for i in 0..6 {
            let p = project_to_screen(vp, rect, vec3(KEY_XS[i], KEY_Y - 0.42, 0.10));
            self.draw_txt.text_style.font_size = 9.0;
            self.draw_txt.color = if i == 1 && self.cached_state.playing {
                vec4(0.35, 0.98, 0.65, 1.0)
            } else {
                vec4(0.74, 0.81, 0.90, 0.94)
            };
            let lbl = KEY_LABELS[i];
            let est_w = lbl.len() as f64 * 5.2;
            self.draw_txt.draw_abs(cx, dvec2(p.x - est_w * 0.5, p.y - 4.0), lbl);
        }

        // 2. Knob titles + values (Row 1)
        for (ki, &(kx, ky)) in KNOB_POS.iter().enumerate() {
            let (title, val) = self.ctrl_readout(ki);
            let p = project_to_screen(vp, rect, vec3(kx, ky - 0.72, 0.08));
            self.draw_txt.text_style.font_size = 8.8;
            self.draw_txt.color = vec4(0.58, 0.84, 0.98, 0.95);
            let tw = title.len() as f64 * 5.0;
            self.draw_txt.draw_abs(cx, dvec2(p.x - tw * 0.5, p.y - 4.0), &title);

            self.draw_txt.text_style.font_size = 9.5;
            self.draw_txt.color = vec4(0.93, 0.96, 1.0, 0.96);
            let vw = val.len() as f64 * 5.2;
            self.draw_txt.draw_abs(cx, dvec2(p.x - vw * 0.5, p.y + 7.5), &val);
        }

        // 3. Weighted Output Slider titles + values (Row 2)
        for si in 0..3 {
            let sx = SLIDER_XS[si];
            let (title, val) = self.ctrl_readout(3 + si);
            let p = project_to_screen(vp, rect, vec3(sx, SLIDER_Y_CENTER - SLIDER_HALF_H - 0.28, 0.08));
            self.draw_txt.text_style.font_size = 8.8;
            self.draw_txt.color = vec4(0.98, 0.76, 0.44, 0.95);
            let tw = title.len() as f64 * 5.0;
            self.draw_txt.draw_abs(cx, dvec2(p.x - tw * 0.5, p.y - 4.0), &title);

            self.draw_txt.text_style.font_size = 9.5;
            self.draw_txt.color = vec4(0.93, 0.96, 1.0, 0.96);
            let vw = val.len() as f64 * 5.2;
            self.draw_txt.draw_abs(cx, dvec2(p.x - vw * 0.5, p.y + 7.5), &val);
        }

        // 4. Scrub/play lyric preview directly on the cassette top label!
        let cas_p = project_to_screen(vp, rect, vec3(0.0, 3.42 + self.eject_t * 0.85, 0.34 + self.eject_t * 1.15));
        let preview_idx = self.cached_state.line_index_at_ms(self.pos_ms);
        let banner_str = preview_idx
            .map(|i| self.cached_state.line_text_at(i))
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(36).collect::<String>())
            .unwrap_or_else(|| "Turn either reel to jog · flick to coast".to_string());
        self.draw_txt.text_style.font_size = 9.5;
        self.draw_txt.color = vec4(0.08, 0.05, 0.03, 0.96);
        let bw = banner_str.len() as f64 * 5.0;
        self.draw_txt.draw_abs(cx, dvec2(cas_p.x - bw * 0.5, cas_p.y - 5.0), &banner_str);

        // 5. Loop badge under tape head if active
        if self.cached_state.loop_active {
            let hp = project_to_screen(vp, rect, vec3(0.0, 0.78, 0.30));
            self.draw_txt.text_style.font_size = 8.5;
            self.draw_txt.color = vec4(0.24, 0.94, 1.0, 1.0);
            self.draw_txt.draw_abs(cx, dvec2(hp.x - 13.0, hp.y - 4.0), "LOOP");
        }

        // 6. Cassette Rack selector overlay when EJECTED
        if self.rack_open {
            let rx = rect.pos.x + 14.0;
            let ry = rect.pos.y + 18.0;
            let rw = (rect.size.x - 28.0).max(160.0);
            let items = self.rack_items();
            let row_h = 34.0;
            let rh = (46.0 + items.len().min(7) as f64 * row_h).min(rect.size.y * 0.56);

            self.draw_card.color = vec4(0.06, 0.09, 0.15, 0.95);
            self.draw_card.draw_abs(cx, Rect { pos: dvec2(rx, ry), size: dvec2(rw, rh) });

            self.draw_txt.text_style.font_size = 11.0;
            self.draw_txt.color = vec4(0.98, 0.72, 0.32, 1.0);
            self.draw_txt.draw_abs(cx, dvec2(rx + 12.0, ry + 10.0), "CASSETTE RACK  (tap tape to insert)");

            for (idx, item) in items.iter().take(7).enumerate() {
                let iy = ry + 34.0 + idx as f64 * row_h;
                let bg = if item.id == self.cached_state.song_id {
                    vec4(0.16, 0.32, 0.52, 0.95)
                } else {
                    vec4(0.12, 0.16, 0.24, 0.92)
                };
                self.draw_card.color = bg;
                self.draw_card.draw_abs(cx, Rect { pos: dvec2(rx + 8.0, iy), size: dvec2(rw - 16.0, row_h - 5.0) });

                let tag = match item.kind.as_str() {
                    "parent" => "[BACK]",
                    "save" => "[SAVE]",
                    _ => "[TAPE]",
                };
                let label = format!("{}  {}", tag, item.title);
                let trunc: String = label.chars().take(42).collect();
                self.draw_txt.text_style.font_size = 10.5;
                self.draw_txt.color = vec4(0.93, 0.96, 1.0, 1.0);
                self.draw_txt.draw_abs(cx, dvec2(rx + 16.0, iy + 8.0), &trunc);
            }
        }
    }

    fn rack_items(&self) -> Vec<CassetteEntry> {
        let mut items = Vec::new();
        if self.cached_state.song_gen > 0 || !self.cached_state.parent_id.is_empty() {
            items.push(CassetteEntry {
                id: "__parent__".into(),
                title: format!("Parent Cassette (gen {})", (self.cached_state.song_gen - 1).max(0)),
                kind: "parent".into(),
                gen: (self.cached_state.song_gen - 1).max(0),
            });
        }
        for s in &self.cached_state.saves {
            items.push(s.clone());
        }
        for l in &self.cached_state.library {
            items.push(l.clone());
        }
        items
    }

    fn handle_rack_tap(&mut self, abs: Vec2d, cx: &mut Cx) -> bool {
        if !self.rack_open {
            return false;
        }
        let rect = self.last_rect;
        let rx = rect.pos.x + 14.0;
        let ry = rect.pos.y + 18.0;
        let rw = (rect.size.x - 28.0).max(160.0);
        let items = self.rack_items();
        let row_h = 34.0;
        let rh = (46.0 + items.len().min(7) as f64 * row_h).min(rect.size.y * 0.56);
        if abs.x >= rx && abs.x <= rx + rw && abs.y >= ry && abs.y <= ry + rh {
            let rel_y = abs.y - (ry + 34.0);
            if rel_y >= 0.0 {
                let idx = (rel_y / row_h) as usize;
                if let Some(item) = items.get(idx) {
                    self.finish_scrub();
                    let (nm, seq) = match item.kind.as_str() {
                        "parent" => ("back", send_lgtm_cmd("back", "{}")),
                        "save" => ("load", send_lgtm_cmd("load", &format!("{{\"id\":\"{}\"}}", json_escape(&item.id)))),
                        _ => ("pick", send_lgtm_cmd("pick", &format!("{{\"q\":\"{}\"}}", json_escape(&item.id)))),
                    };
                    self.set_pending(seq, nm, 5);
                    self.rack_open = false;
                    self.key_target[5] = 0.0;
                    self.ensure_anim_or_redraw(cx);
                }
            }
            return true;
        }
        // Tapping outside the rack closes it cleanly (pushes cassette back in)
        self.rack_open = false;
        self.key_target[5] = 0.0;
        self.ensure_anim_or_redraw(cx);
        true
    }

    fn hit_test_3d(&self, abs: Vec2d) -> (GrabTarget, Vec3f) {
        let rect = self.last_rect;
        let aspect = (rect.size.x / rect.size.y.max(1.0)) as f32;
        let p = unproject_to_plane_z(self.camera_eye(), self.camera_target(), 36.0, aspect, rect, abs, REEL_Z);

        // 1. Both reels (generous thumb hit radius)
        let dl = ((p.x - LEFT_REEL_POS.0).powi(2) + (p.y - LEFT_REEL_POS.1).powi(2)).sqrt();
        let dr = ((p.x - RIGHT_REEL_POS.0).powi(2) + (p.y - RIGHT_REEL_POS.1).powi(2)).sqrt();
        if dl < 1.04 && dl <= dr {
            return (GrabTarget::LeftReel, p);
        }
        if dr < 1.04 {
            return (GrabTarget::RightReel, p);
        }

        // 2. Chrome tape head (tap to loop current line!)
        if p.x.abs() < 0.42 && (p.y - 1.02).abs() < 0.34 {
            return (GrabTarget::TapeHead, p);
        }

        // 3. Cassette top header (pull up to eject)
        if p.x.abs() < 1.95 && p.y > 3.05 && p.y < 3.90 {
            return (GrabTarget::Cassette, p);
        }

        // 4. 6 mechanical deck keys
        if (p.y - KEY_Y).abs() < 0.44 {
            for (i, &kx) in KEY_XS.iter().enumerate() {
                if (p.x - kx).abs() < 0.36 {
                    return (GrabTarget::Key(i), p);
                }
            }
        }

        // 5. Row 1: 3 rotary knobs
        for (i, &(kx, ky)) in KNOB_POS.iter().enumerate() {
            let d = ((p.x - kx).powi(2) + (p.y - ky).powi(2)).sqrt();
            if d < 0.72 {
                return (GrabTarget::Knob(i), p);
            }
        }

        // 6. Row 2: 3 weighted output sliders
        if (p.y - SLIDER_Y_CENTER).abs() < SLIDER_HALF_H + 0.32 {
            for (si, &sx) in SLIDER_XS.iter().enumerate() {
                if (p.x - sx).abs() < 0.56 {
                    return (GrabTarget::Slider(si), p);
                }
            }
        }

        (GrabTarget::CameraOrbit, p)
    }

    pub fn format_counter(&self) -> String {
        fn fmt_ms(ms: f64) -> String {
            let s = ms.max(0.0) / 1000.0;
            let m = (s / 60.0).floor() as i32;
            let rem = s - (m as f64) * 60.0;
            format!("{:02}:{:04.1}", m, rem)
        }
        if !self.refused_why.is_empty() {
            return format!("refused: {}", self.refused_why);
        }
        if self.is_pending() {
            return format!("{} · pending {}…", fmt_ms(self.pos_ms), self.pending_name);
        }
        if self.scrub_active || self.cached_state.scrubbing {
            let r = if self.cached_state.scrubbing {
                self.cached_state.transport_rate as f32
            } else {
                self.scrub_rate
            };
            let st_txt = if r.abs() < 0.05 {
                "scrub hold".to_string()
            } else {
                format!("scrub {:+.1}x", r)
            };
            return format!("{} / {} · {}", fmt_ms(self.pos_ms), fmt_ms(self.dur_ms), st_txt);
        }
        let status = if self.cached_state.has_transport {
            &self.cached_state.transport_state
        } else if self.cached_state.playing {
            "playing"
        } else {
            &self.cached_state.status
        };
        format!(
            "{} / {} · {}",
            fmt_ms(self.pos_ms),
            fmt_ms(self.dur_ms),
            status,
        )
    }
}

impl Widget for DeckViewport {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let rect = cx.walk_turtle(walk);
        if rect.size.x < 1.0 || rect.size.y < 1.0 {
            return DrawStep::done();
        }
        self.last_rect = rect;

        let aspect = (rect.size.x / rect.size.y) as f32;
        let (view, proj) = self.camera_matrices(aspect);
        self.pass.set_camera(cx, view, proj);

        cx.make_child_pass(&self.pass);
        cx.begin_pass(&self.pass, None);
        self.draw_list.begin_always(cx);
        self.draw_scene(cx);
        self.draw_list.end(cx);
        cx.end_pass(&self.pass);

        self.draw_image.draw_vars.set_texture(0, &self.color_texture);
        self.draw_image.draw_abs(cx, rect);
        self.area = self.draw_image.area();
        cx.set_pass_area(&self.pass, self.area);
        self.pass.set_dpi_factor(cx, PASS_SCALE);

        let vp = mat4_mul(proj, view);
        self.draw_overlay_labels(cx, rect, vp);

        DrawStep::done()
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, _scope: &mut Scope) {
        if let Event::Signal = event {
            let epoch = DECK_SHOWN_EPOCH.load(Ordering::SeqCst);
            if epoch != self.shown_epoch {
                // back on screen: take the wire state as new and restart what moves
                self.shown_epoch = epoch;
                self.last_wire_rev = u64::MAX;
            }
            if deck_visible() {
                let _ = take_streams(&wire_hub(), &mut self.streams_seen);
                self.sync_from_wire(cx);
            } else {
                // hidden mid-gesture or mid-coast: the tape stops being scrubbed
                self.end_scrub_if_active();
            }
            if !deck_visible() && self.timer_running {
                cx.stop_timer(self.anim_timer);
                self.anim_timer = Timer::default();
                self.timer_running = false;
            }
        }

        if self.anim_timer.is_event(event).is_some() && !deck_visible() {
            cx.stop_timer(self.anim_timer);
            self.anim_timer = Timer::default();
            self.timer_running = false;
            return;
        }
        if let Some(te) = self.anim_timer.is_event(event) {
            let now_t = te.time.unwrap_or(self.last_frame_time + (1.0 / 30.0));
            let dt = if self.last_frame_time > 0.0 {
                (now_t - self.last_frame_time) as f32
            } else {
                1.0 / 30.0
            };
            self.last_frame_time = now_t;
            self.step_animation(cx, dt);
            cx.widget_action(self.uid, DeckAction::StateUpdated);
        }

        match event.hits(cx, self.area) {
            Hit::FingerDown(fe) => {
                if self.handle_rack_tap(fe.abs, cx) {
                    return;
                }
                let (target, wp) = self.hit_test_3d(fe.abs);
                self.grab = target;
                self.grab_start_abs = fe.abs;
                self.last_abs = fe.abs;
                self.last_finger_time = fe.time;

                match target {
                    GrabTarget::LeftReel => {
                        self.scrub_active = true;
                        self.scrub_coasting = false;
                        self.scrub_omega = 0.0;
                        self.scrub_rate = 0.0;
                        self.reset_reel_hist(fe.time);
                        self.last_reel_move_at = Some(Instant::now());
                        self.last_angle = (wp.y - LEFT_REEL_POS.1).atan2(wp.x - LEFT_REEL_POS.0);
                        self.emit_scrub_rate(0.0, true);
                        cx.widget_action(self.uid, DeckAction::StateUpdated);
                        self.ensure_anim_or_redraw(cx);
                    }
                    GrabTarget::RightReel => {
                        self.scrub_active = true;
                        self.scrub_coasting = false;
                        self.scrub_omega = 0.0;
                        self.scrub_rate = 0.0;
                        self.reset_reel_hist(fe.time);
                        self.last_reel_move_at = Some(Instant::now());
                        self.last_angle = (wp.y - RIGHT_REEL_POS.1).atan2(wp.x - RIGHT_REEL_POS.0);
                        self.emit_scrub_rate(0.0, true);
                        cx.widget_action(self.uid, DeckAction::StateUpdated);
                        self.ensure_anim_or_redraw(cx);
                    }
                    GrabTarget::Key(idx) => {
                        self.key_target[idx] = 1.0;
                        self.ensure_anim_or_redraw(cx);
                    }
                    GrabTarget::Knob(idx) => {
                        if self.last_tap_ctrl == idx as i32 && (fe.time - self.last_tap_time) < 0.35 {
                            self.reset_ctrl_to_rest(idx, cx);
                            self.grab = GrabTarget::None;
                            self.last_tap_ctrl = -1;
                        } else {
                            self.last_tap_ctrl = idx as i32;
                            self.last_tap_time = fe.time;
                            self.grab_ctrl_start = self.ctrl_val[idx];
                            let (kx, ky) = KNOB_POS[idx];
                            self.last_angle = (wp.y - ky).atan2(wp.x - kx);
                            self.ensure_anim_or_redraw(cx);
                        }
                    }
                    GrabTarget::Slider(si) => {
                        let ci = 3 + si;
                        self.ctrl_vel[ci] = 0.0;
                        if self.last_tap_ctrl == ci as i32 && (fe.time - self.last_tap_time) < 0.35 {
                            self.reset_ctrl_to_rest(ci, cx);
                            self.grab = GrabTarget::None;
                            self.last_tap_ctrl = -1;
                        } else {
                            self.last_tap_ctrl = ci as i32;
                            self.last_tap_time = fe.time;
                            self.grab_ctrl_start = self.ctrl_val[ci];
                            self.ensure_anim_or_redraw(cx);
                        }
                    }
                    _ => {}
                }
            }
            Hit::FingerMove(fe) => {
                let dt = (fe.time - self.last_finger_time).clamp(0.008, 0.08) as f32;
                self.last_finger_time = fe.time;
                let rect = self.last_rect;
                let aspect = (rect.size.x / rect.size.y.max(1.0)) as f32;

                match self.grab {
                    GrabTarget::LeftReel | GrabTarget::RightReel => {
                        let is_left = matches!(self.grab, GrabTarget::LeftReel);
                        let center = if is_left { LEFT_REEL_POS } else { RIGHT_REEL_POS };
                        let wp = unproject_to_plane_z(self.camera_eye(), self.camera_target(), 36.0, aspect, rect, fe.abs, REEL_Z);
                        let dx = wp.x - center.0;
                        let dy = wp.y - center.1;
                        let dist = (dx * dx + dy * dy).sqrt();
                        let ang = dy.atan2(dx);
                        let mut d_ang = ang - self.last_angle;
                        while d_ang > std::f32::consts::PI { d_ang -= std::f32::consts::TAU; }
                        while d_ang < -std::f32::consts::PI { d_ang += std::f32::consts::TAU; }
                        self.last_angle = ang;

                        // Smoothly damp atan2 angular spikes near the center brass hub spindle
                        let hub_gain = ((dist - 0.14) / 0.16).clamp(0.0, 1.0);
                        self.reel_unwrapped_ang += d_ang * hub_gain;
                        let omega = self.push_reel_hist(fe.time, self.reel_unwrapped_ang);
                        self.scrub_omega = omega;
                        let (r_left, r_right) = self.reel_radii();
                        let r_grab = if is_left { r_left } else { r_right };
                        let inst_rate = ((omega * r_grab) / 1.45).clamp(-4.0, 4.0);
                        self.scrub_rate = (self.scrub_rate * 0.25 + inst_rate * 0.75).clamp(-4.0, 4.0);
                        if self.scrub_rate.abs() < 0.05 {
                            self.scrub_rate = 0.0;
                        }
                        self.last_reel_move_at = Some(Instant::now());
                        self.emit_scrub_rate(self.scrub_rate, false);
                        cx.widget_action(self.uid, DeckAction::StateUpdated);
                    }
                    GrabTarget::Cassette => {
                        let dy = (self.grab_start_abs.y - fe.abs.y) as f32;
                        self.eject_drag_extra = (dy / 140.0).clamp(-0.3, 0.9);
                        self.redraw_throttled(cx);
                    }
                    GrabTarget::Knob(idx) => {
                        // True rotary drag around the knob perimeter, seamlessly falling back to vertical drag near center!
                        let (kx, ky) = KNOB_POS[idx];
                        let wp = unproject_to_plane_z(self.camera_eye(), self.camera_target(), 36.0, aspect, rect, fe.abs, 0.10);
                        let dx_k = wp.x - kx;
                        let dy_k = wp.y - ky;
                        let dist = (dx_k * dx_k + dy_k * dy_k).sqrt();
                        let ang = dy_k.atan2(dx_k);
                        let delta = if dist >= 0.26 {
                            let mut d_ang = ang - self.last_angle;
                            while d_ang > std::f32::consts::PI { d_ang -= std::f32::consts::TAU; }
                            while d_ang < -std::f32::consts::PI { d_ang += std::f32::consts::TAU; }
                            // Clockwise (-d_ang) increases value across the 270 deg sweep
                            -d_ang / 270.0_f32.to_radians()
                        } else {
                            ((self.last_abs.y - fe.abs.y) as f32) / 120.0
                        };
                        self.last_angle = ang;
                        self.ctrl_val[idx] = (self.ctrl_val[idx] + delta).clamp(0.0, 1.0);
                        self.commit_ctrl(idx, false);
                        cx.widget_action(self.uid, DeckAction::StateUpdated);
                        self.redraw_throttled(cx);
                    }
                    GrabTarget::Slider(si) => {
                        let ci = 3 + si;
                        let prev = self.ctrl_val[ci];
                        let dy = (self.grab_start_abs.y - fe.abs.y) as f32;
                        let next = (self.grab_ctrl_start + dy / 110.0).clamp(0.0, 1.0);
                        self.ctrl_val[ci] = next;
                        let inst_v = ((next - prev) / dt).clamp(-4.5, 4.5);
                        self.ctrl_vel[ci] = self.ctrl_vel[ci] * 0.5 + inst_v * 0.5;
                        self.commit_ctrl(ci, false);
                        self.redraw_throttled(cx);
                    }
                    GrabTarget::CameraOrbit => {
                        let d = fe.abs - self.last_abs;
                        self.cam_yaw = (self.cam_yaw - d.x as f32 * 0.0045).clamp(-0.38, 0.38);
                        self.cam_pitch = (self.cam_pitch + d.y as f32 * 0.0045).clamp(-0.42, 0.22);
                        self.redraw_throttled(cx);
                    }
                    _ => {}
                }
                self.last_abs = fe.abs;
            }
            Hit::FingerUp(fe) => {
                match self.grab {
                    GrabTarget::LeftReel | GrabTarget::RightReel => {
                        let stale_hold = self
                            .last_reel_move_at
                            .map(|t| t.elapsed() > Duration::from_millis(120))
                            .unwrap_or(true);
                        if stale_hold {
                            self.scrub_omega = 0.0;
                            self.scrub_rate = 0.0;
                        }
                        if self.scrub_rate.abs() < 0.35 {
                            self.finish_scrub();
                        } else {
                            self.scrub_coasting = true;
                            if !send_head_vel("release", self.scrub_omega, self.scrub_rate, false, true) {
                                self.emit_scrub_rate(self.scrub_rate, false);
                            }
                        }
                        cx.widget_action(self.uid, DeckAction::StateUpdated);
                        self.ensure_anim_or_redraw(cx);
                    }
                    GrabTarget::TapeHead => {
                        let (hit_now, _) = self.hit_test_3d(fe.abs);
                        if hit_now == GrabTarget::TapeHead {
                            self.toggle_head_loop(cx);
                        }
                    }
                    GrabTarget::Cassette => {
                        if self.eject_drag_extra > 0.25 {
                            self.rack_open = true;
                            self.key_target[5] = 0.7;
                        } else if self.eject_drag_extra < -0.15 {
                            self.rack_open = false;
                            self.key_target[5] = 0.0;
                        }
                        self.eject_drag_extra = 0.0;
                        self.ensure_anim_or_redraw(cx);
                    }
                    GrabTarget::Key(idx) => {
                        if idx != 1 && idx != 5 {
                            self.key_target[idx] = 0.0;
                        }
                        let (hit_now, _) = self.hit_test_3d(fe.abs);
                        if hit_now == GrabTarget::Key(idx) {
                            self.trigger_key(idx, cx);
                        } else {
                            self.key_target[1] = if self.cached_state.playing { 0.85 } else { 0.0 };
                            self.key_target[5] = if self.rack_open { 0.7 } else { 0.0 };
                            self.ensure_anim_or_redraw(cx);
                        }
                    }
                    GrabTarget::Knob(idx) => {
                        self.commit_ctrl(idx, true);
                        cx.widget_action(self.uid, DeckAction::StateUpdated);
                        self.ensure_anim_or_redraw(cx);
                    }
                    GrabTarget::Slider(si) => {
                        let ci = 3 + si;
                        if self.ctrl_vel[ci].abs() < 0.25 {
                            self.ctrl_vel[ci] = 0.0;
                            self.commit_ctrl(ci, true);
                        }
                        self.ensure_anim_or_redraw(cx);
                    }
                    _ => {}
                }
                self.grab = GrabTarget::None;
            }
            Hit::FingerScroll(fe) if fe.scroll.y != 0.0 => {
                let step = if fe.scroll.y > 0.0 { 1.06 } else { 1.0 / 1.06 };
                self.cam_dist = (self.cam_dist * step).clamp(11.0, 20.0);
                self.area.redraw(cx);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// 7. The deck as a shell surface (was the JIT app's App)
// ---------------------------------------------------------------------------

#[derive(Script, ScriptHook, Widget)]
pub struct LgtmDeck {
    #[deref]
    view: View,
}

impl LgtmDeck {
    fn sync_header(&mut self, cx: &mut Cx) {
        let vp = self.view.widget(cx, ids!(viewport));
        let Some(deck) = vp.borrow::<DeckViewport>() else {
            return;
        };
        let raw_title = &deck.cached_state.song_title;
        let full_title = if deck.cached_state.song_gen > 0 && !raw_title.contains("(gen ") {
            format!("{} (g{})", raw_title, deck.cached_state.song_gen)
        } else {
            raw_title.clone()
        };
        let title: String = if full_title.chars().count() > 15 {
            let mut s: String = full_title.chars().take(14).collect();
            s.push('…');
            s
        } else {
            full_title
        };
        let sub = deck.format_counter();
        let bank_lbl = deck.bank_label();
        let rack_lbl = if deck.rack_open { "Close" } else { "Rack" };
        drop(deck);

        self.view.label(cx, ids!(title_lbl)).set_text(cx, &title);
        self.view.label(cx, ids!(sub_lbl)).set_text(cx, &sub);
        self.view.button(cx, ids!(bank_btn)).set_text(cx, &bank_lbl);
        self.view.button(cx, ids!(rack_btn)).set_text(cx, rack_lbl);
    }

    fn handle_deck_actions(&mut self, cx: &mut Cx, actions: &Actions) {
        let mut header = false;
        for action in actions {
            if let DeckAction::StateUpdated = action.as_widget_action().cast() {
                header = true;
            }
        }
        // Either half of the [ Board | Deck ] toggle goes back to the board: the sound, the
        // link and the play position stay as they are (a stage change, nothing else).
        if self.view.button(cx, ids!(toggle_board)).clicked(actions)
            || self.view.button(cx, ids!(toggle_deck)).clicked(actions)
        {
            let vp = self.view.widget(cx, ids!(viewport));
            if let Some(mut deck) = vp.borrow_mut::<DeckViewport>() {
                deck.end_scrub_if_active();
            }
            let _ = send_lgtm_cmd("view", "{\"to\":\"board\"}");
            send_front_surface("lgtm");
        }
        if self.view.button(cx, ids!(bank_btn)).clicked(actions) {
            let vp = self.view.widget(cx, ids!(viewport));
            if let Some(mut deck) = vp.borrow_mut::<DeckViewport>() {
                deck.cycle_param_bank(cx);
            }
            header = true;
        }
        if self.view.button(cx, ids!(rack_btn)).clicked(actions) {
            let vp = self.view.widget(cx, ids!(viewport));
            if let Some(mut deck) = vp.borrow_mut::<DeckViewport>() {
                deck.rack_open = !deck.rack_open;
                deck.key_target[5] = if deck.rack_open { 0.7 } else { 0.0 };
                deck.ensure_anim_or_redraw(cx);
            }
            header = true;
        }
        if header {
            self.sync_header(cx);
        }
    }
}

impl Widget for LgtmDeck {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let actions = cx.capture_actions(|cx| self.view.handle_event(cx, event, scope));
        if !actions.is_empty() {
            self.handle_deck_actions(cx, &actions);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.view.draw_walk(cx, scope, walk)
    }
}

/// Splash isolates (view files) see `LgtmDeck` with the other widgets.
pub fn isolate_mod(vm: &mut ScriptVm) {
    self::script_mod(vm);
    script_eval!(vm, {
        mod.prelude.widgets = {
            ..mod.prelude.widgets,
            LgtmDeck: mod.widgets.LgtmDeck,
        }
    });
}
