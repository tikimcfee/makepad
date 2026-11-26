#![allow(dead_code)]
//! Pure data model (`DeskState`), module command queue (`DeskCmds`), and standalone socket I/O
//! (`WireHub`) for the One Desk panel.
//!
//! When baked natively into `tb-shell`, only `WireHub` is replaced with direct store & push-stream
//! reads (`lgtm_head`, `lgtm_pos`, `lgtm_orig`, `lgtm_new`); `DeskState`, `DeskCmds`, `module.rs`,
//! `panel.rs`, `mesh.rs`, and all `mod_*.rs` files stay unchanged.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DECK_DGRAM_NAME: &str = "lgtm-deck.dgram";
const HEAD_SOCK_NAME: &str = "lgtm-head.sock";

// ---------------------------------------------------------------------------
// 1. Pure domain state (read by all modules)
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
pub struct VoiceFitInfo {
    pub words_heard: f32, // 0..1 (NaN or <0 if unknown)
    pub onset_ms: f32,    // ms (<0 if unknown)
    pub overrun: f32,     // 0..1
    pub has_fit: bool,
}

#[derive(Clone, Debug, Default)]
pub struct VoiceInfo {
    pub id: String,
    pub name: String,
    pub kind: String, // "speech" | "sing" | "singer"
    pub state: String,
    pub params: Vec<VoiceParamInfo>,
    pub fit: VoiceFitInfo,
}

#[derive(Clone, Debug, Default)]
pub struct BandInfo {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub does: String,
    pub lo: String,
    pub hi: String,
    pub color_hex: String,
    pub rest: f32,
    pub value: f32,
    pub steps: Vec<String>,
    pub state: String,
    pub err: String,
    pub fx_count: i32,
    pub n_done: i32,
    pub n_total: i32,
}

#[derive(Clone, Debug, Default)]
pub struct SectionInfo {
    pub idx: usize,
    pub label: String,
    pub t0_ms: f64,
    pub t1_ms: f64,
    pub state: String, // "scratch" | "singing" | "sung"
}

#[derive(Clone, Debug, Default)]
pub struct CassetteEntry {
    pub id: String,
    pub title: String,
    pub kind: String, // "save" | "library" | "parent"
    pub gen: i32,
}

#[derive(Clone, Debug)]
pub struct DeskState {
    pub status: String,
    pub note: String,
    pub link: String,

    // Song metadata
    pub song_id: String,
    pub song_title: String,
    pub song_artist: String,
    pub song_gen: i32,
    pub dur_ms: f64,
    pub parent_id: String,

    // Transport & loop
    pub has_transport: bool,
    pub transport_state: String, // "stopped" | "starting" | "playing" | "stopping" | "seeking"
    pub transport_sub: String,   // "" | "scrubbing" | "seeking"
    pub transport_rate: f64,
    pub scrubbing: bool,
    pub playing: bool,
    pub transport_seq: u64,
    pub transport_pass: u32,
    pub line: i32,
    pub loop_active: bool,
    pub loop_line: i32,
    pub loop_a_ms: f64,
    pub loop_b_ms: f64,

    // Command ack
    pub ack_seq: u64,
    pub ack_name: String,
    pub ack_state: String, // "ok" | "queued" | "refused"
    pub ack_why: String,

    // Position anchors (local head preferred while fresh < 200 ms)
    pub pos_anchor_ms: f64,
    pub pos_anchor_at: Instant,
    pub pos_rate: f64,
    pub has_stream_pos: bool,
    pub last_stream_at: Instant,

    pub head_runs: bool,
    pub head_live: bool,
    pub head_stage: i32,
    pub head_pos_ms: f64,
    pub head_rate: f64,
    pub head_at: Option<Instant>,
    pub head_cache_backing: bool,
    pub head_cache_backing_frac: f32,
    pub head_cache_lines: i32,
    pub head_underruns: i32,

    // Link / edit / progress metadata
    pub parts_done: i32,
    pub parts_total: i32,
    pub edit_id: String,

    // Live audio levels (0..1)
    pub level_orig: f32,
    pub level_new: f32,

    // Output stage (`lgtm_out`: level/bal/tone 0..1; duck 0..12 dB normalized to 0..1; presence in dB)
    pub out_level: f32,
    pub out_balance: f32,
    pub out_tone: f32,
    pub out_duck: f32,
    pub out_presence_db: f32,
    pub out_rev: u64,

    // Voices, scratch voice & Day-2 singer (`lgtm.singers`, `lgtm.singer`)
    pub voice_id: String,
    pub scratch_voice_id: String,
    pub voices: Vec<VoiceInfo>,
    pub singers: Vec<String>,
    pub singer_id: String,    // "" == none (scratch)
    pub singer_state: String, // "off" | "loading" | "ready" | "singing" | "error"
    pub singer_why: String,

    // Bands & mix
    pub bands: Vec<BandInfo>,
    pub mix_rev: u64,

    // Lyrics, word times, waveforms & sections
    pub src_lines: Vec<String>,
    pub out_lines: Vec<String>,
    pub line_times_ms: Vec<f64>,
    pub chg: Vec<Vec<(usize, i32)>>,
    pub settled: Vec<bool>,
    pub wave_back: Vec<f32>,
    pub wave_src: Vec<f32>,
    pub sections: Vec<SectionInfo>,

    // Sparse live drag overlay (`lgtm_live`: <=20/s changed lines + fx while dragging, null when settled)
    pub live_lines: Vec<(usize, String, Vec<(usize, i32)>)>,
    pub live_fx: Vec<(String, i32)>,

    // Cassette rack
    pub saves: Vec<CassetteEntry>,
    pub library: Vec<CassetteEntry>,

    pub rev: u64,

    // Per-domain dirty revisions (sync_from_wire copies ONLY changed domains & syncs ONLY changed modules)
    pub rev_tape: u64,
    pub rev_tape_meta: u64,
    pub rev_wave: u64,
    pub rev_transport: u64,
    pub rev_lyrics: u64,
    pub rev_bands: u64,
    pub rev_mix_vals: u64,
    pub rev_live: u64,
    pub rev_out: u64,
    pub rev_voice: u64,
    pub rev_rack: u64,
    pub rev_link: u64,

    // Live frame pacing telemetry (updated by DeskViewport each draw_walk, read by LINK)
    pub perf_fps: f32,
    pub perf_max_gap_ms: f32,
    pub perf_draw_ms: f32,
}

pub const DIRTY_TAPE: u32      = 1 << 0;
pub const DIRTY_TRANSPORT: u32 = 1 << 1;
pub const DIRTY_LYRICS: u32    = 1 << 2;
pub const DIRTY_BANDS: u32     = 1 << 3;
pub const DIRTY_MIX_VALS: u32  = 1 << 4;
pub const DIRTY_OUT: u32       = 1 << 5;
pub const DIRTY_VOICE: u32     = 1 << 6;
pub const DIRTY_RACK: u32      = 1 << 7;
pub const DIRTY_LINK: u32      = 1 << 8;
pub const DIRTY_WAVE: u32      = 1 << 9;

impl Default for DeskState {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            status: "idle".to_string(),
            note: "connecting...".to_string(),
            link: "off".to_string(),
            song_id: String::new(),
            song_title: "LGTM Desk".to_string(),
            song_artist: String::new(),
            song_gen: 0,
            dur_ms: 180_000.0,
            parent_id: String::new(),
            has_transport: false,
            transport_state: "stopped".to_string(),
            transport_sub: String::new(),
            transport_rate: 1.0,
            scrubbing: false,
            playing: false,
            transport_seq: 0,
            transport_pass: 0,
            line: -1,
            loop_active: false,
            loop_line: -1,
            loop_a_ms: -1.0,
            loop_b_ms: -1.0,
            ack_seq: 0,
            ack_name: String::new(),
            ack_state: String::new(),
            ack_why: String::new(),
            pos_anchor_ms: 0.0,
            pos_anchor_at: now,
            pos_rate: 1.0,
            has_stream_pos: false,
            last_stream_at: now,
            head_runs: false,
            head_live: false,
            head_stage: 0,
            head_pos_ms: 0.0,
            head_rate: 0.0,
            head_at: None,
            head_cache_backing: false,
            head_cache_backing_frac: 0.0,
            head_cache_lines: 0,
            head_underruns: 0,
            parts_done: 0,
            parts_total: 0,
            edit_id: String::new(),
            level_orig: 0.0,
            level_new: 0.0,
            out_level: 0.75,
            out_balance: 0.50,
            out_tone: 0.50,
            out_duck: 7.5 / 12.0,
            out_presence_db: 3.0,
            out_rev: 0,
            voice_id: "kokoro".to_string(),
            scratch_voice_id: "kokoro".to_string(),
            singers: Vec::new(),
            singer_id: String::new(),
            singer_state: "off".to_string(),
            singer_why: String::new(),
            voices: vec![VoiceInfo {
                id: "kokoro".to_string(),
                name: "Kokoro".to_string(),
                kind: "speech".to_string(),
                state: "ready".to_string(),
                params: vec![
                    VoiceParamInfo {
                        id: "speed".into(),
                        name: "SPEED".into(),
                        min: 0.5,
                        max: 2.0,
                        rest: 1.0,
                        value: 1.0,
                        ..Default::default()
                    },
                    VoiceParamInfo {
                        id: "voice".into(),
                        name: "VOICE".into(),
                        min: 0.0,
                        max: 1.0,
                        rest: 0.0,
                        value: 0.0,
                        steps: vec!["af_heart".into(), "af_bella".into(), "am_michael".into()],
                        ..Default::default()
                    },
                ],
                fit: VoiceFitInfo::default(),
            }],
            bands: Vec::new(),
            mix_rev: 0,
            src_lines: Vec::new(),
            out_lines: Vec::new(),
            line_times_ms: Vec::new(),
            chg: Vec::new(),
            settled: Vec::new(),
            wave_back: Vec::new(),
            wave_src: Vec::new(),
            sections: Vec::new(),
            live_lines: Vec::new(),
            live_fx: Vec::new(),
            saves: Vec::new(),
            library: Vec::new(),
            rev: 0,
            rev_tape: 0,
            rev_tape_meta: 0,
            rev_wave: 0,
            rev_transport: 0,
            rev_lyrics: 0,
            rev_bands: 0,
            rev_mix_vals: 0,
            rev_live: 0,
            rev_out: 0,
            rev_voice: 0,
            rev_rack: 0,
            rev_link: 0,
            perf_fps: 0.0,
            perf_max_gap_ms: 0.0,
            perf_draw_ms: 0.0,
        }
    }
}

impl DeskState {
    pub fn head_is_fresh(&self, now: Instant) -> bool {
        self.head_at
            .map(|t| now.duration_since(t) < Duration::from_millis(200))
            .unwrap_or(false)
    }

    pub fn effective_rate(&self, now: Instant) -> f64 {
        if self.head_is_fresh(now) {
            self.head_rate
        } else if self.scrubbing {
            self.transport_rate
        } else if self.playing {
            self.pos_rate
        } else {
            0.0
        }
    }

    pub fn heard_pos_ms(&self, now: Instant) -> f64 {
        let dur = self.dur_ms.max(1.0);
        if let Some(ht) = self.head_at {
            if now.duration_since(ht) < Duration::from_millis(200) {
                let dt_ms = (now.duration_since(ht).as_secs_f64() * 1000.0).min(250.0);
                return (self.head_pos_ms + dt_ms * self.head_rate).clamp(0.0, dur);
            }
        }
        let rate = self.effective_rate(now);
        if rate.abs() > 1e-4 {
            let dt_ms = (now.duration_since(self.pos_anchor_at).as_secs_f64() * 1000.0).min(1500.0);
            (self.pos_anchor_ms + dt_ms * rate).clamp(0.0, dur)
        } else {
            self.pos_anchor_ms.clamp(0.0, dur)
        }
    }

    pub fn line_index_at_ms(&self, ms: f64) -> Option<usize> {
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

    pub fn line_text_at(&self, idx: usize) -> &str {
        if let Some((_, out, _)) = self.live_lines.iter().find(|(i, _, _)| *i == idx) {
            if !out.is_empty() {
                return out.as_str();
            }
        }
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

    pub fn line_chg_at(&self, idx: usize) -> &[(usize, i32)] {
        if let Some((_, _, pairs)) = self.live_lines.iter().find(|(i, _, _)| *i == idx) {
            return pairs.as_slice();
        }
        self.chg.get(idx).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn band_fx_count(&self, b: &BandInfo) -> i32 {
        if let Some((_, n)) = self.live_fx.iter().find(|(id, _)| id == &b.id) {
            return *n;
        }
        b.fx_count
    }

    pub fn active_voice(&self) -> Option<&VoiceInfo> {
        self.voices
            .iter()
            .find(|v| v.id == self.voice_id)
            .or_else(|| self.voices.first())
    }

    /// Incrementally copy ONLY domains whose `rev_*` changed from `src`, returning a `DIRTY_*` bitmask.
    /// Hot paths (`lgtm_mix`, `lgtm_out`, `lgtm_head` stream) perform zero heap allocations.
    pub fn sync_changed_from(&mut self, src: &DeskState) -> u32 {
        let mut dirty = 0u32;
        if self.rev_out != src.rev_out {
            self.rev_out = src.rev_out;
            self.out_level = src.out_level;
            self.out_balance = src.out_balance;
            self.out_tone = src.out_tone;
            self.out_duck = src.out_duck;
            self.out_presence_db = src.out_presence_db;
            self.out_rev = src.out_rev;
            self.level_orig = src.level_orig;
            self.level_new = src.level_new;
            dirty |= DIRTY_OUT;
        }
        if self.rev_bands != src.rev_bands {
            self.rev_bands = src.rev_bands;
            self.rev_mix_vals = src.rev_mix_vals;
            self.bands = src.bands.clone();
            self.mix_rev = src.mix_rev;
            self.edit_id = src.edit_id.clone();
            dirty |= DIRTY_BANDS | DIRTY_MIX_VALS;
        } else if self.rev_mix_vals != src.rev_mix_vals {
            self.rev_mix_vals = src.rev_mix_vals;
            self.mix_rev = src.mix_rev;
            // Zero-allocation in-place float copy when only mix fader values changed
            if self.bands.len() == src.bands.len() {
                for (dst, sb) in self.bands.iter_mut().zip(src.bands.iter()) {
                    dst.value = sb.value;
                }
            } else {
                self.bands = src.bands.clone();
            }
            dirty |= DIRTY_MIX_VALS;
        }
        if self.rev_live != src.rev_live {
            self.rev_live = src.rev_live;
            self.live_lines = src.live_lines.clone();
            self.live_fx = src.live_fx.clone();
            dirty |= DIRTY_LYRICS | DIRTY_BANDS;
        }
        if self.rev_wave != src.rev_wave {
            self.rev_wave = src.rev_wave;
            self.wave_back = src.wave_back.clone();
            self.wave_src = src.wave_src.clone();
            dirty |= DIRTY_WAVE | DIRTY_TAPE;
        }
        if self.rev_tape != src.rev_tape {
            self.rev_tape = src.rev_tape;
            self.dur_ms = src.dur_ms;
            self.pos_anchor_ms = src.pos_anchor_ms;
            self.pos_anchor_at = src.pos_anchor_at;
            self.pos_rate = src.pos_rate;
            self.has_stream_pos = src.has_stream_pos;
            self.last_stream_at = src.last_stream_at;
            self.head_pos_ms = src.head_pos_ms;
            self.head_rate = src.head_rate;
            self.head_at = src.head_at;
            self.head_runs = src.head_runs;
            self.head_live = src.head_live;
            self.scrubbing = src.scrubbing;
            self.transport_rate = src.transport_rate;
            dirty |= DIRTY_TAPE;
        }
        if self.rev_tape_meta != src.rev_tape_meta {
            self.rev_tape_meta = src.rev_tape_meta;
            self.song_title = src.song_title.clone();
            self.song_gen = src.song_gen;
            self.sections = src.sections.clone();
            dirty |= DIRTY_TAPE;
        }
        if self.rev_transport != src.rev_transport {
            self.rev_transport = src.rev_transport;
            self.has_transport = src.has_transport;
            self.transport_state = src.transport_state.clone();
            self.transport_sub = src.transport_sub.clone();
            self.transport_rate = src.transport_rate;
            self.scrubbing = src.scrubbing;
            self.playing = src.playing;
            self.transport_seq = src.transport_seq;
            self.transport_pass = src.transport_pass;
            self.line = src.line;
            self.loop_active = src.loop_active;
            self.loop_line = src.loop_line;
            self.loop_a_ms = src.loop_a_ms;
            self.loop_b_ms = src.loop_b_ms;
            self.ack_seq = src.ack_seq;
            self.ack_name = src.ack_name.clone();
            self.ack_state = src.ack_state.clone();
            self.ack_why = src.ack_why.clone();
            self.status = src.status.clone();
            dirty |= DIRTY_TRANSPORT;
        }
        if self.rev_lyrics != src.rev_lyrics {
            self.rev_lyrics = src.rev_lyrics;
            self.src_lines = src.src_lines.clone();
            self.out_lines = src.out_lines.clone();
            self.line_times_ms = src.line_times_ms.clone();
            self.chg = src.chg.clone();
            self.settled = src.settled.clone();
            self.line = src.line;
            self.loop_active = src.loop_active;
            self.loop_line = src.loop_line;
            self.loop_a_ms = src.loop_a_ms;
            self.loop_b_ms = src.loop_b_ms;
            self.sections = src.sections.clone();
            dirty |= DIRTY_LYRICS;
        }
        if self.rev_voice != src.rev_voice {
            self.rev_voice = src.rev_voice;
            self.voice_id = src.voice_id.clone();
            self.scratch_voice_id = src.scratch_voice_id.clone();
            self.voices = src.voices.clone();
            self.singers = src.singers.clone();
            self.singer_id = src.singer_id.clone();
            self.singer_state = src.singer_state.clone();
            self.singer_why = src.singer_why.clone();
            dirty |= DIRTY_VOICE;
        }
        if self.rev_rack != src.rev_rack {
            self.rev_rack = src.rev_rack;
            self.song_id = src.song_id.clone();
            self.song_title = src.song_title.clone();
            self.song_gen = src.song_gen;
            self.parent_id = src.parent_id.clone();
            self.saves = src.saves.clone();
            self.library = src.library.clone();
            dirty |= DIRTY_RACK;
        }
        if self.rev_link != src.rev_link {
            self.rev_link = src.rev_link;
            self.link = src.link.clone();
            self.status = src.status.clone();
            self.note = src.note.clone();
            self.song_artist = src.song_artist.clone();
            self.parts_done = src.parts_done;
            self.parts_total = src.parts_total;
            self.head_runs = src.head_runs;
            self.head_live = src.head_live;
            self.head_stage = src.head_stage;
            self.head_cache_backing = src.head_cache_backing;
            self.head_cache_backing_frac = src.head_cache_backing_frac;
            self.head_cache_lines = src.head_cache_lines;
            self.head_underruns = src.head_underruns;
            self.ack_name = src.ack_name.clone();
            self.ack_state = src.ack_state.clone();
            self.ack_why = src.ack_why.clone();
            dirty |= DIRTY_LINK;
        }
        self.rev = src.rev;
        dirty
    }
}

// ---------------------------------------------------------------------------
// 2. Outbound command sink passed to modules (host-agnostic)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum DeskCmd {
    LgtmCmd { name: String, data_json: String, pending_tag: Option<String> },
    OutPatch { level: f32, balance: f32, tone: f32, duck: f32 },
    MixPatch { band_id: String, value: f32 },
    VoiceParam { voice_id: String, param_id: String, raw_value: String },
    HeadVel { op: &'static str, omega: f32, rate: f32, held: bool, coast: bool },
    HeadEnd { end_ms: f64 },
    OpenPromptEdit { band_id: String, name: String, prompt: String },
    ClosePromptEdit,
    Reconnect,
    FocusModule { module_id: &'static str },
}

#[derive(Default)]
pub struct DeskCmds {
    pub items: Vec<DeskCmd>,
    pub request_redraw: bool,
}

impl DeskCmds {
    pub fn cmd(&mut self, name: &str, data_json: &str) {
        self.items.push(DeskCmd::LgtmCmd {
            name: name.to_string(),
            data_json: data_json.to_string(),
            pending_tag: None,
        });
        self.request_redraw = true;
    }

    pub fn cmd_pending(&mut self, name: &str, data_json: &str, tag: &str) {
        self.items.push(DeskCmd::LgtmCmd {
            name: name.to_string(),
            data_json: data_json.to_string(),
            pending_tag: Some(tag.to_string()),
        });
        self.request_redraw = true;
    }

    pub fn out_patch(&mut self, level: f32, balance: f32, tone: f32, duck: f32) {
        self.items.push(DeskCmd::OutPatch { level, balance, tone, duck });
        self.request_redraw = true;
    }

    pub fn mix_patch(&mut self, band_id: &str, value: f32) {
        self.items.push(DeskCmd::MixPatch { band_id: band_id.to_string(), value });
        self.request_redraw = true;
    }

    pub fn voice_param(&mut self, voice_id: &str, param_id: &str, raw_value: &str) {
        self.items.push(DeskCmd::VoiceParam {
            voice_id: voice_id.to_string(),
            param_id: param_id.to_string(),
            raw_value: raw_value.to_string(),
        });
        self.request_redraw = true;
    }

    pub fn head_vel(&mut self, op: &'static str, omega: f32, rate: f32, held: bool, coast: bool) {
        self.items.push(DeskCmd::HeadVel { op, omega, rate, held, coast });
        self.request_redraw = true;
    }

    pub fn head_end(&mut self, end_ms: f64) {
        self.items.push(DeskCmd::HeadEnd { end_ms });
        self.request_redraw = true;
    }

    pub fn open_prompt_edit(&mut self, band_id: &str, name: &str, prompt: &str) {
        self.items.push(DeskCmd::OpenPromptEdit {
            band_id: band_id.to_string(),
            name: name.to_string(),
            prompt: prompt.to_string(),
        });
        self.request_redraw = true;
    }

    pub fn close_prompt_edit(&mut self) {
        self.items.push(DeskCmd::ClosePromptEdit);
        self.request_redraw = true;
    }

    pub fn emit_reconnect(&mut self) {
        self.items.push(DeskCmd::Reconnect);
        self.request_redraw = true;
    }

    pub fn focus_module(&mut self, module_id: &'static str) {
        self.items.push(DeskCmd::FocusModule { module_id });
        self.request_redraw = true;
    }
}

// ---------------------------------------------------------------------------
// 3. Standalone WireHub (tb-shell.sock + lgtm-deck.dgram + lgtm-head.sock)
// ---------------------------------------------------------------------------

pub struct WireHub {
    pub state: Mutex<DeskState>,
    pub tx: Mutex<Option<UnixStream>>,
    pub cmd_seq: Mutex<u64>,
}

static WIRE_HUB: OnceLock<Arc<WireHub>> = OnceLock::new();
static HEAD_DGRAM: OnceLock<Option<UnixDatagram>> = OnceLock::new();

pub fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

pub fn wire_hub() -> Arc<WireHub> {
    WIRE_HUB
        .get_or_init(|| {
            let (tx_end, rx_end) = match UnixStream::pair().ok() {
                Some((a, b)) => (Some(a), Some(b)),
                None => (None, None),
            };
            let start_seq = 500_000_000 + (unix_now_ms() % 400_000_000);
            let hub = Arc::new(WireHub {
                state: Mutex::new(DeskState::default()),
                tx: Mutex::new(tx_end),
                cmd_seq: Mutex::new(start_seq),
            });
            if let Some(rx) = rx_end {
                spawn_shell_writer(rx);
            }
            spawn_shell_watcher(Arc::clone(&hub));
            spawn_dgram_listener(Arc::clone(&hub));
            hub
        })
        .clone()
}

pub fn send_head_json(msg: &str) -> bool {
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

pub fn send_head_vel(op: &str, omega: f32, rate: f32, held: bool, coast: bool) -> bool {
    let t_ms = unix_now_ms();
    let msg = format!(
        "{{\"op\":\"{}\",\"t\":{},\"omega\":{:.4},\"rate\":{:.4},\"held\":{},\"coast\":{}}}\n",
        op, t_ms, omega, rate, held, coast
    );
    send_head_json(&msg)
}

/// Immediate zero-store-latency OUT control datagram directly to `lgtm-head.sock`.
pub fn send_head_out(level: f32, balance: f32, tone: f32, duck_norm: f32, presence_db: f32) -> bool {
    let duck_db = (duck_norm.clamp(0.0, 1.0) * 12.0 * 10.0).round() / 10.0;
    let pres_db = (presence_db.clamp(0.0, 6.0) * 10.0).round() / 10.0;
    let msg = format!(
        "{{\"out\":{{\"level\":{:.3},\"balance\":{:.3},\"tone\":{:.3},\"duck\":{:.1},\"presence\":{:.1}}}}}\n",
        level.clamp(0.0, 1.0),
        balance.clamp(0.0, 1.0),
        tone.clamp(0.0, 1.0),
        duck_db,
        pres_db,
    );
    send_head_json(&msg)
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

pub fn send_lgtm_cmd(name: &str, data_json: &str) -> u64 {
    let hub = wire_hub();
    let t_ms = unix_now_ms();
    let seq = {
        let mut s = hub.cmd_seq.lock().unwrap();
        let cur = *s;
        *s += 1;
        cur
    };
    let line = format!(
        "{{\"op\":\"set\",\"reply\":false,\"patch\":{{\"lgtm_cmd\":{{\"seq\":{},\"name\":\"{}\",\"data\":{},\"t\":{},\"src\":\"desk\"}}}}}}",
        seq, name, data_json, t_ms
    );
    send_shell_line(line);
    seq
}

pub fn send_lgtm_out(level: f32, balance: f32, tone: f32, duck_norm: f32, presence_db: f32) {
    let duck_db = (duck_norm.clamp(0.0, 1.0) * 12.0 * 10.0).round() / 10.0;
    let pres_db = (presence_db.clamp(0.0, 6.0) * 10.0).round() / 10.0;
    let line = format!(
        "{{\"op\":\"set\",\"reply\":false,\"patch\":{{\"lgtm_out\":{{\"level\":{:.3},\"balance\":{:.3},\"tone\":{:.3},\"duck\":{:.1},\"presence\":{:.1}}}}}}}",
        level.clamp(0.0, 1.0),
        balance.clamp(0.0, 1.0),
        tone.clamp(0.0, 1.0),
        duck_db,
        pres_db,
    );
    send_shell_line(line);
}

pub fn send_lgtm_mix_one(band_id: &str, value: f32) {
    let line = format!(
        "{{\"op\":\"set\",\"reply\":false,\"patch\":{{\"lgtm_mix\":{{\"{}\":{:.3}}}}}}}",
        json_escape(band_id),
        value.clamp(0.0, 1.0),
    );
    send_shell_line(line);
}

/// Coalesced single store write for one or more changed mix faders in one frame.
pub fn send_lgtm_mix_batch(items: &[(String, f32)]) {
    if items.is_empty() {
        return;
    }
    let mut body = String::with_capacity(48 + items.len() * 24);
    body.push_str("{\"op\":\"set\",\"reply\":false,\"patch\":{\"lgtm_mix\":{");
    for (i, (id, val)) in items.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push('"');
        body.push_str(&json_escape(id));
        body.push_str("\":");
        body.push_str(&format!("{:.3}", val.clamp(0.0, 1.0)));
    }
    body.push_str("}}}");
    send_shell_line(body);
}

pub fn send_open_prompt_edit(band_id: &str, name: &str, prompt: &str) {
    let title = format!("Band: {}", if name.is_empty() { band_id } else { name });
    let line = format!(
        "{{\"op\":\"set\",\"reply\":false,\"patch\":{{\"lgtm_edit\":\"{}\",\"lgtm_edit_text\":\"{}\",\"kbd_target\":{{\"key\":\"lgtm_edit_text\",\"surface\":\"lgtm\",\"submit_emit\":\"kbd_submit\",\"cancel_emit\":\"kbd_cancel\",\"title\":\"{}\",\"meta\":{{\"id\":\"{}\"}}}}}}}}",
        json_escape(band_id),
        json_escape(prompt),
        json_escape(&title),
        json_escape(band_id),
    );
    send_shell_line(line);
    send_shell_line("{\"op\":\"stage\",\"sub\":\"add\",\"surface\":\"keyboard\"}".to_string());
}

pub fn send_close_prompt_edit() {
    send_shell_line("{\"op\":\"set\",\"reply\":false,\"patch\":{\"lgtm_edit\":null,\"lgtm_edit_text\":\"\",\"kbd_target\":null}}".to_string());
    send_shell_line("{\"op\":\"stage\",\"sub\":\"rm\",\"surface\":\"keyboard\"}".to_string());
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

fn spawn_shell_watcher(hub: Arc<WireHub>) {
    std::thread::spawn(move || {
        let path = tb_shell_sock_path();
        // Stage 1: fetch lightweight control/lyric/band/head keys first so all 8 modules populate immediately
        let core_keys = "[\"lgtm\",\"lgtm_mix\",\"lgtm_out\",\"lgtm_link\",\"lgtm_head\",\"lgtm_transport\",\"lgtm_ack\",\"lgtm_edit\",\"lgtm_live\"]";
        let watch_keys = "[\"lgtm\",\"lgtm_mix\",\"lgtm_out\",\"lgtm_link\",\"lgtm_head\",\"lgtm_transport\",\"lgtm_ack\",\"lgtm_wave\",\"lgtm_edit\",\"lgtm_live\"]";
        loop {
            if let Ok(mut sock) = UnixStream::connect(&path) {
                let mut reader = BufReader::new(sock.try_clone().unwrap());
                let mut line = String::new();

                let get_core = format!("{{\"op\":\"get\",\"keys\":{}}}\n", core_keys);
                if sock.write_all(get_core.as_bytes()).is_ok() {
                    if reader.read_line(&mut line).unwrap_or(0) > 0 {
                        if let Some(v) = parse_json(&line) {
                            if let Some(st_obj) = v.get("state") {
                                if let Ok(mut guard) = hub.state.lock() {
                                    apply_store_root(&mut guard, st_obj);
                                }
                                super::wake_ui_if_visible();
                            }
                        }
                    }

                    // Stage 2: fetch lgtm_wave separately and stream-downsample directly with 0 JVal allocs
                    line.clear();
                    if sock.write_all(b"{\"op\":\"get\",\"keys\":[\"lgtm_wave\"]}\n").is_ok()
                        && reader.read_line(&mut line).unwrap_or(0) > 0
                    {
                        if let Ok(mut guard) = hub.state.lock() {
                            if apply_raw_wave_json(&mut guard, &line) {
                                drop(guard);
                                super::wake_ui_if_visible();
                            }
                        }
                    }

                    let watch_req = format!("{{\"op\":\"watch\",\"keys\":{}}}\n", watch_keys);
                    if sock.write_all(watch_req.as_bytes()).is_ok() {
                        loop {
                            line.clear();
                            match reader.read_line(&mut line) {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {
                                    let mut wave_changed = false;
                                    if line.contains("\"lgtm_wave\"") {
                                        if let Ok(mut guard) = hub.state.lock() {
                                            wave_changed = apply_raw_wave_json(&mut guard, &line);
                                        }
                                    }
                                    // JParser skips lgtm_wave/nw/lgtm_outw with zero allocations
                                    let mut root_changed = false;
                                    if let Some(v) = parse_json(&line) {
                                        if let Some(patch) = v.get("patch") {
                                            if let Ok(mut guard) = hub.state.lock() {
                                                apply_store_root(&mut guard, patch);
                                                root_changed = true;
                                            }
                                        }
                                    }
                                    if wave_changed || root_changed {
                                        super::wake_ui_if_visible();
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

fn spawn_dgram_listener(hub: Arc<WireHub>) {
    std::thread::spawn(move || {
        let sock_path = format!("{}/{}", runtime_dir(), DECK_DGRAM_NAME);
        let _ = std::fs::remove_file(&sock_path);
        let Ok(dgram) = UnixDatagram::bind(&sock_path) else {
            return;
        };
        let mut buf = [0u8; 512];
        loop {
            match dgram.recv(&mut buf) {
                Ok(n) if n > 0 => {
                    if let Ok(s) = std::str::from_utf8(&buf[..n]) {
                        let now = Instant::now();
                        let s = s.trim();
                        if s.starts_with('{') {
                            if let Some(v) = parse_json(s) {
                                if let Ok(mut st) = hub.state.lock() {
                                    if let Some(d) = v.get("dur_ms").and_then(|x| x.as_f64()) {
                                        if d > 100.0 {
                                            st.dur_ms = d;
                                        }
                                    }
                                    if let Some(sub) = v.get("sub").and_then(|x| x.as_str()) {
                                        st.transport_sub = sub.to_string();
                                        st.scrubbing = sub == "scrubbing";
                                    }
                                    if let Some(r) = v.get("rate").and_then(|x| x.as_f64()) {
                                        if r.is_finite() {
                                            st.transport_rate = r.clamp(-16.0, 16.0);
                                        }
                                    }
                                    if let Some(ms) = v.get("pos_ms").and_then(|x| x.as_f64()) {
                                        let clamped = ms.clamp(0.0, st.dur_ms.max(1.0));
                                        let changed = (clamped - st.pos_anchor_ms).abs() > 0.05;
                                        let stale = now.duration_since(st.last_stream_at) > Duration::from_millis(80);
                                        if changed || !st.has_stream_pos || stale || st.effective_rate(now).abs() < 1e-4 {
                                            st.pos_anchor_ms = clamped;
                                            st.pos_anchor_at = now;
                                            st.has_stream_pos = true;
                                            st.last_stream_at = now;
                                        }
                                    }
                                    let mut out_dirty = false;
                                    if let Some(o) = v.get("orig").and_then(|x| x.as_f64()) {
                                        st.level_orig = (o as f32).clamp(0.0, 1.0);
                                        out_dirty = true;
                                    }
                                    if let Some(nw) = v.get("new").and_then(|x| x.as_f64()) {
                                        st.level_new = (nw as f32).clamp(0.0, 1.0);
                                        out_dirty = true;
                                    }
                                    st.rev_tape += 1;
                                    if out_dirty {
                                        st.rev_out += 1;
                                    }
                                    st.rev += 1;
                                }
                                super::wake_ui_if_visible();
                            }
                        }
                    }
                }
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 4. Minimal zero-dependency JSON parser & store patch application
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum JVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<JVal>),
    Obj(Vec<(String, JVal)>),
}

impl JVal {
    pub fn get(&self, key: &str) -> Option<&JVal> {
        if let JVal::Obj(entries) = self {
            for (k, v) in entries {
                if k == key {
                    return Some(v);
                }
            }
        }
        None
    }
    pub fn as_str(&self) -> Option<&str> {
        if let JVal::Str(s) = self { Some(s.as_str()) } else { None }
    }
    pub fn as_f64(&self) -> Option<f64> {
        if let JVal::Num(n) = self { Some(*n) } else { None }
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let JVal::Bool(b) = self { Some(*b) } else { None }
    }
    pub fn as_arr(&self) -> Option<&[JVal]> {
        if let JVal::Arr(a) = self { Some(a.as_slice()) } else { None }
    }
    pub fn as_obj(&self) -> Option<&[(String, JVal)]> {
        if let JVal::Obj(o) = self { Some(o.as_slice()) } else { None }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, JVal::Null)
    }
    pub fn to_step_string(&self) -> Option<(String, bool)> {
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
    fn new(s: &'a str) -> Self { Self { b: s.as_bytes(), i: 0 } }
    fn skip_ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\n' | b'\r' | b'\t') {
            self.i += 1;
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
            b't' if self.b[self.i..].starts_with(b"true") => { self.i += 4; Some(JVal::Bool(true)) }
            b'f' if self.b[self.i..].starts_with(b"false") => { self.i += 5; Some(JVal::Bool(false)) }
            b'n' if self.b[self.i..].starts_with(b"null") => { self.i += 4; Some(JVal::Null) }
            b'-' | b'0'..=b'9' => self.parse_num(),
            _ => None,
        }
    }
    fn parse_str(&mut self) -> Option<String> {
        self.skip_ws();
        if *self.b.get(self.i)? != b'"' { return None; }
        self.i += 1;
        let mut out = String::new();
        while self.i < self.b.len() {
            let c = self.b[self.i];
            self.i += 1;
            if c == b'"' { return Some(out); }
            if c == b'\\' {
                if self.i >= self.b.len() { return None; }
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
                    b'u' if self.i + 4 <= self.b.len() => {
                        if let Ok(hex) = std::str::from_utf8(&self.b[self.i..self.i + 4]) {
                            if let Ok(code) = u32::from_str_radix(hex, 16) {
                                if let Some(ch) = char::from_u32(code) { out.push(ch); }
                            }
                        }
                        self.i += 4;
                    }
                    _ => out.push(esc as char),
                }
            } else if c < 0x80 {
                out.push(c as char);
            } else {
                let start = self.i - 1;
                while self.i < self.b.len() && (self.b[self.i] & 0xC0) == 0x80 { self.i += 1; }
                if let Ok(s) = std::str::from_utf8(&self.b[start..self.i]) { out.push_str(s); }
            }
        }
        None
    }
    fn parse_num(&mut self) -> Option<JVal> {
        self.skip_ws();
        let start = self.i;
        while self.i < self.b.len()
            && matches!(self.b[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        {
            self.i += 1;
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).ok()?;
        Some(JVal::Num(s.parse::<f64>().ok()?))
    }
    fn parse_arr(&mut self) -> Option<JVal> {
        self.skip_ws();
        self.i += 1;
        let mut arr = Vec::new();
        if self.peek()? == b']' { self.i += 1; return Some(JVal::Arr(arr)); }
        loop {
            arr.push(self.parse_val()?);
            match self.peek()? {
                b',' => self.i += 1,
                b']' => { self.i += 1; return Some(JVal::Arr(arr)); }
                _ => return None,
            }
        }
    }
    fn skip_val(&mut self) -> Option<()> {
        match self.peek()? {
            b'{' => {
                self.i += 1;
                let mut depth = 1usize;
                let mut in_str = false;
                while self.i < self.b.len() && depth > 0 {
                    let c = self.b[self.i];
                    self.i += 1;
                    if in_str {
                        if c == b'\\' {
                            self.i += 1;
                        } else if c == b'"' {
                            in_str = false;
                        }
                    } else {
                        match c {
                            b'"' => in_str = true,
                            b'{' | b'[' => depth += 1,
                            b'}' | b']' => depth -= 1,
                            _ => {}
                        }
                    }
                }
                if depth == 0 { Some(()) } else { None }
            }
            b'[' => {
                self.i += 1;
                let mut depth = 1usize;
                let mut in_str = false;
                while self.i < self.b.len() && depth > 0 {
                    let c = self.b[self.i];
                    self.i += 1;
                    if in_str {
                        if c == b'\\' {
                            self.i += 1;
                        } else if c == b'"' {
                            in_str = false;
                        }
                    } else {
                        match c {
                            b'"' => in_str = true,
                            b'[' | b'{' => depth += 1,
                            b']' | b'}' => depth -= 1,
                            _ => {}
                        }
                    }
                }
                if depth == 0 { Some(()) } else { None }
            }
            b'"' => {
                self.i += 1;
                while self.i < self.b.len() {
                    let c = self.b[self.i];
                    self.i += 1;
                    if c == b'\\' {
                        self.i += 1;
                    } else if c == b'"' {
                        return Some(());
                    }
                }
                None
            }
            _ => {
                while self.i < self.b.len()
                    && !matches!(self.b[self.i], b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t')
                {
                    self.i += 1;
                }
                Some(())
            }
        }
    }
    fn parse_obj(&mut self) -> Option<JVal> {
        self.skip_ws();
        self.i += 1;
        let mut entries = Vec::new();
        if self.peek()? == b'}' { self.i += 1; return Some(JVal::Obj(entries)); }
        loop {
            let k = self.parse_str()?;
            if self.peek()? != b':' { return None; }
            self.i += 1;
            // Skip massive envelope/waveform arrays inside generic JSON parse (lgtm_wave is streamed separately)
            if matches!(k.as_str(), "lgtm_wave" | "nw" | "lgtm_outw") {
                self.skip_val()?;
            } else {
                let v = self.parse_val()?;
                entries.push((k, v));
            }
            match self.peek()? {
                b',' => self.i += 1,
                b'}' => { self.i += 1; return Some(JVal::Obj(entries)); }
                _ => return None,
            }
        }
    }
}

/// Zero-allocation byte scanner for large JSON float arrays like `"back":[0.12,0.34,...]` or `"src":[...]`.
/// Downsamples directly into `bins` peak buckets in <2 ms even under Tier-0 JIT.
fn extract_wave_bins(json: &str, key: &str, bins: usize) -> Option<Vec<f32>> {
    let needle = format!("\"{}\":", key);
    let pos = json.find(&needle)?;
    let b = json.as_bytes();
    let mut i = pos + needle.len();
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    if i >= b.len() || b[i] != b'[' {
        return None;
    }
    let start = i + 1;
    let mut end = start;
    let mut commas = 0usize;
    let mut has_digit = false;
    while end < b.len() && b[end] != b']' {
        let c = b[end];
        if c == b',' {
            commas += 1;
        } else if c.is_ascii_digit() {
            has_digit = true;
        }
        end += 1;
    }
    if end >= b.len() || !has_digit {
        return Some(Vec::new());
    }
    let total = commas + 1;
    let n_bins = bins.min(total).max(1);
    let mut out = vec![0.0f32; n_bins];
    let mut idx = 0usize;
    let mut p = start;
    while p < end {
        while p < end && !matches!(b[p], b'0'..=b'9' | b'-' | b'.') {
            p += 1;
        }
        if p >= end {
            break;
        }
        let neg = if b[p] == b'-' { p += 1; true } else { false };
        let mut int_part = 0.0f32;
        while p < end && b[p].is_ascii_digit() {
            int_part = int_part * 10.0 + (b[p] - b'0') as f32;
            p += 1;
        }
        let mut frac = 0.0f32;
        let mut scale = 0.1f32;
        if p < end && b[p] == b'.' {
            p += 1;
            while p < end && b[p].is_ascii_digit() {
                frac += (b[p] - b'0') as f32 * scale;
                scale *= 0.1;
                p += 1;
            }
        }
        // Skip optional exponent if present
        if p < end && (b[p] == b'e' || b[p] == b'E') {
            p += 1;
            if p < end && (b[p] == b'+' || b[p] == b'-') { p += 1; }
            let mut exp_v = 0i32;
            let exp_neg = p > 0 && b[p - 1] == b'-';
            while p < end && b[p].is_ascii_digit() {
                exp_v = exp_v * 10 + (b[p] - b'0') as i32;
                p += 1;
            }
            let mut val = int_part + frac;
            if exp_neg {
                for _ in 0..exp_v.min(10) { val *= 0.1; }
            } else {
                for _ in 0..exp_v.min(10) { val *= 10.0; }
            }
            int_part = val;
            frac = 0.0;
        }
        let val = if neg { 0.0 } else { (int_part + frac).clamp(0.0, 1.0) };
        let bi = ((idx * n_bins) / total).min(n_bins - 1);
        if val > out[bi] {
            out[bi] = val;
        }
        idx += 1;
        while p < end && b[p] != b',' {
            p += 1;
        }
        if p < end && b[p] == b',' {
            p += 1;
        }
    }
    Some(out)
}

fn apply_raw_wave_json(st: &mut DeskState, line: &str) -> bool {
    let Some(wp) = line.find("\"lgtm_wave\"") else {
        return false;
    };
    let wave_slice = &line[wp..];
    let mut changed = false;
    if let Some(wb) = extract_wave_bins(wave_slice, "back", 120) {
        if !wb.is_empty() {
            st.wave_back = wb;
            changed = true;
        }
    }
    if let Some(ws) = extract_wave_bins(wave_slice, "src", 120) {
        if !ws.is_empty() {
            st.wave_src = ws;
            changed = true;
        }
    }
    if changed {
        st.rev_wave += 1;
        st.rev_tape += 1;
        st.rev += 1;
    }
    changed
}

pub fn parse_json(s: &str) -> Option<JVal> {
    JParser::new(s).parse_val()
}

pub fn json_escape(s: &str) -> String {
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

fn downsample_wave(arr: &[JVal], max_bins: usize) -> Vec<f32> {
    let n = arr.len();
    if n == 0 {
        return Vec::new();
    }
    if n <= max_bins {
        return arr.iter().map(|v| v.as_f64().unwrap_or(0.0).clamp(0.0, 1.0) as f32).collect();
    }
    let mut out = Vec::with_capacity(max_bins);
    for b in 0..max_bins {
        let i0 = (b * n) / max_bins;
        let i1 = (((b + 1) * n) / max_bins).max(i0 + 1).min(n);
        let mut pk = 0.0f32;
        for v in &arr[i0..i1] {
            let x = v.as_f64().unwrap_or(0.0) as f32;
            if x > pk { pk = x; }
        }
        out.push(pk.clamp(0.0, 1.0));
    }
    out
}

fn apply_transport(st: &mut DeskState, tr: &JVal, now: Instant) {
    let prev_rate = st.effective_rate(now);
    let tr_has_pos = tr.get("pos_ms").and_then(|v| v.as_f64()).map(|ms| ms >= 0.0).unwrap_or(false);
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
    if let Some(ps) = tr.get("pass").and_then(|v| v.as_f64()) {
        st.transport_pass = ps.max(0.0) as u32;
    }
    if let Some(ln) = tr.get("line").and_then(|v| v.as_f64()) {
        st.line = ln as i32;
    }
    if let Some(lp) = tr.get("loop") {
        apply_loop(st, lp);
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

fn apply_loop(st: &mut DeskState, lp: &JVal) {
    if lp.is_null() {
        st.loop_active = false;
        st.loop_line = -1;
        st.loop_a_ms = -1.0;
        st.loop_b_ms = -1.0;
    } else {
        st.loop_active = true;
        st.loop_line = lp.get("line").and_then(|v| v.as_f64()).map(|x| x as i32).unwrap_or(-1);
        st.loop_a_ms = lp.get("a").or_else(|| lp.get("a_ms")).and_then(|v| v.as_f64()).unwrap_or(-1.0);
        st.loop_b_ms = lp.get("b").or_else(|| lp.get("b_ms")).and_then(|v| v.as_f64()).unwrap_or(-1.0);
    }
}

fn apply_ack(st: &mut DeskState, ack: &JVal) {
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

pub fn apply_store_root(st: &mut DeskState, root: &JVal) {
    let now = Instant::now();
    if let Some(link) = root.get("lgtm_link").and_then(|v| v.as_str()) {
        st.link = link.to_string();
        st.rev_link += 1;
    }
    if let Some(ed) = root.get("lgtm_edit") {
        if ed.is_null() {
            st.edit_id.clear();
        } else if let Some(s) = ed.as_str() {
            st.edit_id = s.to_string();
        }
        st.rev_bands += 1;
    }
    if let Some(lv) = root.get("lgtm_live") {
        if lv.is_null() {
            st.live_lines.clear();
            st.live_fx.clear();
        } else {
            if let Some(lines_obj) = lv.get("lines").and_then(|v| v.as_obj()) {
                let mut parsed = Vec::with_capacity(lines_obj.len());
                for (k, entry) in lines_obj {
                    if let Ok(idx) = k.parse::<usize>() {
                        let out_txt = entry.get("out").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let mut pairs = Vec::new();
                        if let Some(arr) = entry.get("chg").and_then(|v| v.as_arr()) {
                            for item in arr {
                                if let Some(sub) = item.as_arr() {
                                    if let Some(pos) = sub.first().and_then(|x| x.as_f64()) {
                                        let bidx = sub.get(1).and_then(|x| x.as_f64()).unwrap_or(0.0) as i32;
                                        pairs.push((pos.max(0.0) as usize, bidx));
                                    }
                                } else if let Some(pos) = item.as_f64() {
                                    pairs.push((pos.max(0.0) as usize, 0));
                                }
                            }
                        }
                        parsed.push((idx, out_txt, pairs));
                    }
                }
                st.live_lines = parsed;
            }
            if let Some(fx_obj) = lv.get("fx").and_then(|v| v.as_obj()) {
                let mut fxs = Vec::with_capacity(fx_obj.len());
                for (k, v) in fx_obj {
                    let n = v.as_f64().unwrap_or(0.0).round() as i32;
                    fxs.push((k.clone(), n));
                }
                st.live_fx = fxs;
            }
        }
        st.rev_live += 1;
    }
    if let Some(lh) = root.get("lgtm_head") {
        let mut link_chg = false;
        if let Some(r) = lh.get("runs").and_then(|v| v.as_bool()) {
            st.head_runs = r;
            link_chg = true;
        }
        if let Some(lv) = lh.get("live").and_then(|v| v.as_bool()) {
            st.head_live = lv;
            link_chg = true;
        }
        if let Some(sg) = lh.get("stage").and_then(|v| v.as_f64()) {
            st.head_stage = sg as i32;
            link_chg = true;
        }
        if let Some(d) = lh.get("dur_ms").and_then(|v| v.as_f64()) {
            if d > 100.0 { st.dur_ms = d; }
        }
        let mut head_moved = false;
        if let Some(ms) = lh.get("pos_ms").and_then(|v| v.as_f64()) {
            let clamped = ms.clamp(0.0, st.dur_ms.max(1.0));
            if (clamped - st.head_pos_ms).abs() > 0.01 {
                head_moved = true;
            }
            st.head_pos_ms = clamped;
        }
        if let Some(r) = lh.get("rate").and_then(|v| v.as_f64()) {
            if r.is_finite() {
                if (r - st.head_rate).abs() > 0.001 || r.abs() > 1e-4 {
                    head_moved = true;
                }
                st.head_rate = r;
            }
        }
        let head_state = lh.get("state").and_then(|v| v.as_str()).unwrap_or("");
        if head_moved || head_state == "playing" || head_state == "scrubbing" {
            st.head_at = Some(now);
        }
        if let Some(u) = lh.get("underruns").and_then(|v| v.as_f64()) {
            st.head_underruns = u as i32;
            link_chg = true;
        }
        if let Some(cache) = lh.get("cache") {
            if let Some(b) = cache.get("backing").and_then(|v| v.as_f64()) {
                st.head_cache_backing_frac = (b as f32).clamp(0.0, 1.0);
                st.head_cache_backing = b > 0.0;
                link_chg = true;
            }
            if let Some(l) = cache.get("lines").and_then(|v| v.as_f64()) {
                st.head_cache_lines = l as i32;
                link_chg = true;
            }
        }
        st.rev_tape += 1;
        if link_chg {
            st.rev_link += 1;
        }
    }
    if let Some(wave) = root.get("lgtm_wave") {
        let mut any = false;
        if let Some(b) = wave.get("back").and_then(|v| v.as_arr()) {
            st.wave_back = downsample_wave(b, 180);
            any = true;
        }
        if let Some(s) = wave.get("src").and_then(|v| v.as_arr()) {
            st.wave_src = downsample_wave(s, 180);
            any = true;
        }
        if any {
            st.rev_wave += 1;
            st.rev_tape += 1;
        }
    }
    if let Some(mix) = root.get("lgtm_mix") {
        if let Some(t) = mix.get("tempo").and_then(|v| v.as_f64()) {
            st.pos_rate = (0.65 + t * 0.70).clamp(0.5, 1.8);
            st.rev_tape += 1;
        }
        if let Some(entries) = mix.as_obj() {
            for (k, v) in entries {
                if let Some(val) = v.as_f64() {
                    for b in &mut st.bands {
                        if &b.id == k {
                            b.value = (val as f32).clamp(0.0, 1.0);
                        }
                    }
                }
            }
            st.mix_rev += 1;
            st.rev_mix_vals += 1;
        }
    }
    if let Some(out) = root.get("lgtm_out") {
        let mut any = false;
        if let Some(v) = out.get("level").and_then(|v| v.as_f64()) { st.out_level = (v as f32).clamp(0.0, 1.0); any = true; }
        if let Some(v) = out.get("balance").and_then(|v| v.as_f64()) { st.out_balance = (v as f32).clamp(0.0, 1.0); any = true; }
        if let Some(v) = out.get("tone").and_then(|v| v.as_f64()) { st.out_tone = (v as f32).clamp(0.0, 1.0); any = true; }
        if let Some(v) = out.get("duck").and_then(|v| v.as_f64()) {
            // speakerd-16/17 uses duck in dB (0..12, default 7.5)
            let norm = if v > 1.0 { v / 12.0 } else { v };
            st.out_duck = (norm as f32).clamp(0.0, 1.0);
            any = true;
        }
        if let Some(v) = out.get("presence").and_then(|v| v.as_f64()) {
            st.out_presence_db = (v as f32).clamp(0.0, 6.0);
            any = true;
        }
        if any {
            st.out_rev += 1;
            st.rev_out += 1;
        }
    }
    if let Some(tr) = root.get("lgtm_transport") {
        if !tr.is_null() {
            let had_voice = tr.get("voice").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false);
            apply_transport(st, tr, now);
            st.rev_transport += 1;
            st.rev_tape += 1;
            st.rev_lyrics += 1;
            if had_voice {
                st.rev_voice += 1;
            }
        }
    }
    if let Some(ack) = root.get("lgtm_ack") {
        if !ack.is_null() {
            apply_ack(st, ack);
            st.rev_transport += 1;
            st.rev_link += 1;
        }
    }
    if let Some(lg) = root.get("lgtm") {
        if let Some(status) = lg.get("status").and_then(|v| v.as_str()) {
            st.status = status.to_string();
            if !st.has_transport {
                st.playing = status == "playing";
            }
            st.rev_link += 1;
            st.rev_transport += 1;
        }
        if let Some(note) = lg.get("note").and_then(|v| v.as_str()) {
            st.note = note.to_string();
            st.rev_link += 1;
        }
        if let Some(song) = lg.get("song") {
            if let Some(id) = song.get("id").and_then(|v| v.as_str()) { st.song_id = id.to_string(); }
            if let Some(title) = song.get("title").and_then(|v| v.as_str()) { st.song_title = title.to_string(); }
            if let Some(artist) = song.get("artist").and_then(|v| v.as_str()) { st.song_artist = artist.to_string(); }
            if let Some(d) = song.get("dur_ms").and_then(|v| v.as_f64()) { if d > 100.0 { st.dur_ms = d; } }
            if let Some(g) = song.get("gen").and_then(|v| v.as_f64()) { st.song_gen = g as i32; }
            if let Some(p) = song.get("parent").and_then(|v| v.as_str()) {
                st.parent_id = p.to_string();
            } else if song.get("parent").map(|v| v.is_null()).unwrap_or(false) {
                st.parent_id.clear();
            }
            st.rev_tape += 1;
            st.rev_tape_meta += 1;
            st.rev_rack += 1;
            st.rev_link += 1;
        }
        if let Some(wave) = lg.get("wave") {
            if let Some(b) = wave.get("back").and_then(|v| v.as_arr()) { st.wave_back = downsample_wave(b, 180); }
            if let Some(s) = wave.get("src").and_then(|v| v.as_arr()) { st.wave_src = downsample_wave(s, 180); }
            st.rev_wave += 1;
            st.rev_tape += 1;
        }
        if let Some(secs) = lg.get("sections").and_then(|v| v.as_arr()) {
            let mut parsed = Vec::with_capacity(secs.len());
            for (i, sec) in secs.iter().enumerate() {
                let t0 = sec.get("t0").or_else(|| sec.get("t0_ms")).and_then(|v| v.as_f64()).unwrap_or(0.0);
                let t1 = sec.get("t1").or_else(|| sec.get("t1_ms")).and_then(|v| v.as_f64()).unwrap_or(t0);
                let label = sec.get("label").or_else(|| sec.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let state = sec.get("state").and_then(|v| v.as_str()).unwrap_or("scratch").to_string();
                parsed.push(SectionInfo { idx: i, label, t0_ms: t0, t1_ms: t1, state });
            }
            st.sections = parsed;
            st.rev_tape_meta += 1;
            st.rev_lyrics += 1;
            st.rev_link += 1;
        }
        if let Some(src_arr) = lg.get("src").and_then(|v| v.as_arr()) {
            st.src_lines = src_arr.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect();
            if st.out_lines.len() < st.src_lines.len() {
                st.out_lines.resize(st.src_lines.len(), String::new());
            }
            st.rev_lyrics += 1;
            st.rev_link += 1;
        }
        if let Some(out_v) = lg.get("out") {
            if let Some(arr) = out_v.as_arr() {
                st.out_lines = arr.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect();
            } else if let Some(entries) = out_v.as_obj() {
                for (k, v) in entries {
                    if let Ok(idx) = k.parse::<usize>() {
                        if let Some(txt) = v.as_str() {
                            if idx >= st.out_lines.len() { st.out_lines.resize(idx + 1, String::new()); }
                            st.out_lines[idx] = txt.to_string();
                        }
                    }
                }
            }
            st.rev_lyrics += 1;
        }
        if let Some(chg_obj) = lg.get("chg").and_then(|v| v.as_obj()) {
            for (k, v) in chg_obj {
                if let Ok(idx) = k.parse::<usize>() {
                    if idx >= st.chg.len() {
                        st.chg.resize(idx + 1, Vec::new());
                    }
                    let mut pairs = Vec::new();
                    if let Some(arr) = v.as_arr() {
                        for item in arr {
                            if let Some(sub) = item.as_arr() {
                                if let Some(pos) = sub.first().and_then(|x| x.as_f64()) {
                                    let bidx = sub.get(1).and_then(|x| x.as_f64()).unwrap_or(0.0) as i32;
                                    pairs.push((pos.max(0.0) as usize, bidx));
                                }
                            } else if let Some(pos) = item.as_f64() {
                                pairs.push((pos.max(0.0) as usize, 0));
                            }
                        }
                    }
                    st.chg[idx] = pairs;
                }
            }
            st.rev_lyrics += 1;
        }
        if let Some(set_obj) = lg.get("settled").and_then(|v| v.as_obj()) {
            for (k, v) in set_obj {
                if let Ok(idx) = k.parse::<usize>() {
                    if idx >= st.settled.len() {
                        st.settled.resize(idx + 1, false);
                    }
                    st.settled[idx] = v.as_bool().unwrap_or(false);
                }
            }
            st.rev_lyrics += 1;
        }
        if let Some(parts) = lg.get("parts").and_then(|v| v.as_arr()) {
            st.parts_total = parts.len() as i32;
            st.parts_done = parts
                .iter()
                .filter(|p| {
                    p.as_bool().unwrap_or(false)
                        || p.as_f64().map(|n| n > 0.0).unwrap_or(false)
                        || p.get("state")
                            .and_then(|s| s.as_str())
                            .map(|s| matches!(s, "done" | "ok" | "ready" | "live" | "idle"))
                            .unwrap_or(false)
                })
                .count() as i32;
            st.rev_link += 1;
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
            st.rev_lyrics += 1;
            st.rev_tape += 1;
        }
        if let Some(line_n) = lg.get("line").and_then(|v| v.as_f64()) {
            st.line = line_n as i32;
            st.rev_lyrics += 1;
        }
        if let Some(pos_at) = lg.get("pos_at") {
            if !st.scrubbing {
                if let Some(ms) = pos_at.get("ms").and_then(|v| v.as_f64()) {
                    st.pos_anchor_ms = ms.clamp(0.0, st.dur_ms.max(1.0));
                    st.pos_anchor_at = now;
                }
            }
            if let Some(r) = pos_at.get("rate").and_then(|v| v.as_f64()) {
                if r > 0.1 && r < 4.0 { st.pos_rate = r; }
            }
            st.rev_tape += 1;
        }
        let stage2_head_owns = st.head_runs && !st.head_live;
        if !stage2_head_owns {
            if let Some(tr) = lg.get("transport") {
                if !tr.is_null() {
                    apply_transport(st, tr, now);
                    st.rev_transport += 1;
                    st.rev_tape += 1;
                    st.rev_lyrics += 1;
                }
            }
            if let Some(ack) = lg.get("ack") {
                if !ack.is_null() {
                    apply_ack(st, ack);
                    st.rev_transport += 1;
                    st.rev_link += 1;
                }
            }
        }
        if let Some(lp) = lg.get("loop") {
            if !stage2_head_owns {
                apply_loop(st, lp);
                st.rev_transport += 1;
                st.rev_tape += 1;
                st.rev_lyrics += 1;
            }
        }
        let mut voice_chg = false;
        if let Some(v_id) = lg.get("voice").and_then(|v| v.as_str()) {
            st.voice_id = v_id.to_string();
            voice_chg = true;
        }
        if let Some(sv_id) = lg.get("scratch_voice").and_then(|v| v.as_str()) {
            st.scratch_voice_id = sv_id.to_string();
            voice_chg = true;
        }
        if let Some(s_arr) = lg.get("singers").and_then(|v| v.as_arr()) {
            let mut list = Vec::with_capacity(s_arr.len());
            for item in s_arr {
                if let Some(s) = item.as_str() {
                    if !s.is_empty() {
                        list.push(s.to_string());
                    }
                } else if let Some(s) = item.get("id").and_then(|v| v.as_str()) {
                    if !s.is_empty() {
                        list.push(s.to_string());
                    }
                }
            }
            st.singers = list;
            voice_chg = true;
        }
        if let Some(sg) = lg.get("singer") {
            if sg.is_null() {
                st.singer_id.clear();
                st.singer_state = "off".to_string();
                st.singer_why.clear();
            } else if let Some(s) = sg.as_str() {
                st.singer_id = s.to_string();
                st.singer_why.clear();
            } else {
                match sg.get("id") {
                    Some(v) if v.is_null() => st.singer_id.clear(),
                    Some(v) => {
                        st.singer_id = v.as_str().unwrap_or("").to_string();
                    }
                    None => st.singer_id.clear(),
                }
                if let Some(sst) = sg.get("state").and_then(|v| v.as_str()) {
                    st.singer_state = sst.to_string();
                } else if st.singer_id.is_empty() {
                    st.singer_state = "off".to_string();
                }
                match sg.get("why") {
                    Some(v) if v.is_null() => st.singer_why.clear(),
                    Some(v) => st.singer_why = v.as_str().unwrap_or("").to_string(),
                    None => st.singer_why.clear(),
                }
            }
            voice_chg = true;
        }
        if let Some(voices_arr) = lg.get("voices").and_then(|v| v.as_arr()) {
            let mut parsed = Vec::new();
            for item in voices_arr {
                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if id.is_empty() { continue; }
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or(&id).to_string();
                let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("speech").to_string();
                let state = item.get("state").and_then(|v| v.as_str()).unwrap_or("ready").to_string();
                let mut fit = VoiceFitInfo::default();
                if let Some(fo) = item.get("fit") {
                    if !fo.is_null() {
                        fit.has_fit = true;
                        fit.words_heard = fo
                            .get("words_heard")
                            .or_else(|| fo.get("heard"))
                            .and_then(|v| v.as_f64())
                            .map(|x| if x > 1.0 { (x / 100.0) as f32 } else { x as f32 })
                            .unwrap_or(-1.0);
                        fit.onset_ms = fo.get("onset_ms").and_then(|v| v.as_f64()).map(|x| x as f32).unwrap_or(-1.0);
                        fit.overrun = fo.get("overrun").and_then(|v| v.as_f64()).map(|x| x as f32).unwrap_or(0.0);
                    }
                }
                let set_obj = item.get("set");
                let mut params = Vec::new();
                let list = item
                    .get("params")
                    .and_then(|v| v.as_arr())
                    .or_else(|| item.get("settings").and_then(|v| v.as_arr()));
                if let Some(plist) = list {
                    for p in plist {
                        let pid = p.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        if pid.is_empty() { continue; }
                        let pname = p.get("name").and_then(|v| v.as_str()).unwrap_or(&pid).to_uppercase();
                        let mut steps = Vec::new();
                        let mut step_is_num = false;
                        if let Some(sa) = p.get("steps").and_then(|v| v.as_arr()) {
                            for sv in sa {
                                if let Some((s, is_num)) = sv.to_step_string() {
                                    if is_num { step_is_num = true; }
                                    steps.push(s);
                                }
                            }
                        }
                        if !steps.is_empty() {
                            let cur_s = p.get("value").and_then(|v| v.to_step_string().map(|x| x.0))
                                .or_else(|| set_obj.and_then(|o| o.get(&pid)).and_then(|v| v.to_step_string().map(|x| x.0)))
                                .or_else(|| p.get("rest").and_then(|v| v.to_step_string().map(|x| x.0)))
                                .unwrap_or_default();
                            let rest_s = p.get("rest").and_then(|v| v.to_step_string().map(|x| x.0)).unwrap_or_default();
                            let idx = steps.iter().position(|x| x == &cur_s).unwrap_or(0);
                            let rest_idx = steps.iter().position(|x| x == &rest_s).unwrap_or(0);
                            let norm = if steps.len() > 1 { idx as f32 / (steps.len() - 1) as f32 } else { 0.0 };
                            params.push(VoiceParamInfo {
                                id: pid, name: pname, min: 0.0, max: 1.0, rest: 0.0, value: norm,
                                steps, step_is_num, step_idx: idx, rest_step_idx: rest_idx,
                            });
                        } else {
                            let lo = p.get("min").or_else(|| p.get("lo")).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                            let hi = p.get("max").or_else(|| p.get("hi")).and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
                            let rest = p.get("rest").and_then(|v| v.as_f64()).unwrap_or(((lo + hi) * 0.5) as f64) as f32;
                            let val = p.get("value").and_then(|v| v.as_f64())
                                .or_else(|| set_obj.and_then(|o| o.get(&pid)).and_then(|v| v.as_f64()))
                                .unwrap_or(rest as f64) as f32;
                            params.push(VoiceParamInfo {
                                id: pid, name: pname, min: lo, max: hi, rest, value: val,
                                steps: Vec::new(), step_is_num: false, step_idx: 0, rest_step_idx: 0,
                            });
                        }
                    }
                }
                parsed.push(VoiceInfo { id, name, kind, state, params, fit });
            }
            if !parsed.is_empty() {
                st.voices = parsed;
                voice_chg = true;
            }
        }
        if voice_chg {
            st.rev_voice += 1;
        }
        if let Some(bands_arr) = lg.get("bands").and_then(|v| v.as_arr()) {
            let old_vals: Vec<(String, f32)> = st.bands.iter().map(|b| (b.id.clone(), b.value)).collect();
            let mut parsed = Vec::with_capacity(bands_arr.len());
            for item in bands_arr {
                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if id.is_empty() { continue; }
                let rest = item.get("rest").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                let prev = old_vals.iter().find(|(oid, _)| oid == &id).map(|(_, v)| *v).unwrap_or(rest);
                let mut steps = Vec::new();
                if let Some(sa) = item.get("steps").and_then(|v| v.as_arr()) {
                    for sv in sa {
                        if let Some((s, _)) = sv.to_step_string() { steps.push(s); }
                    }
                }
                let (n_done, n_total) = if let Some(na) = item.get("n").and_then(|v| v.as_arr()) {
                    (
                        na.first().and_then(|v| v.as_f64()).unwrap_or(0.0) as i32,
                        na.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as i32,
                    )
                } else {
                    let v = item.get("n").and_then(|v| v.as_f64()).unwrap_or(0.0) as i32;
                    (v, v)
                };
                parsed.push(BandInfo {
                    id,
                    name: item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    prompt: item.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    does: item.get("does").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    lo: item.get("lo").and_then(|v| v.as_str()).unwrap_or("0").to_string(),
                    hi: item.get("hi").and_then(|v| v.as_str()).unwrap_or("1").to_string(),
                    color_hex: item.get("color").and_then(|v| v.as_str()).unwrap_or("#38bdf8").to_string(),
                    rest,
                    value: prev,
                    steps,
                    state: item.get("state").and_then(|v| v.as_str()).unwrap_or("live").to_string(),
                    err: item.get("err").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    fx_count: 0,
                    n_done,
                    n_total,
                });
            }
            st.bands = parsed;
            st.rev_bands += 1;
        }
        if let Some(fx_obj) = lg.get("fx").and_then(|v| v.as_obj()) {
            for (k, v) in fx_obj {
                let n = v.as_f64().unwrap_or(0.0).round() as i32;
                for b in &mut st.bands {
                    if &b.id == k { b.fx_count = n; }
                }
            }
            st.rev_bands += 1;
        }
        if let Some(saves_arr) = lg.get("saves").and_then(|v| v.as_arr()) {
            let mut saves = Vec::new();
            for s in saves_arr {
                let id = s.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let name = s.get("name").and_then(|v| v.as_str()).unwrap_or(&id).to_string();
                let gen = s.get("gen").and_then(|v| v.as_f64()).map(|g| g as i32).unwrap_or(st.song_gen);
                if !id.is_empty() {
                    saves.push(CassetteEntry { id, title: name, kind: "save".into(), gen });
                }
            }
            st.saves = saves;
            st.rev_rack += 1;
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
            st.rev_rack += 1;
        }
    }
    st.rev += 1;
}
