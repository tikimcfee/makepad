//! tb-shell-sound: event-driven owner of `state.sound` for `views/sound.splash`.
//!
//! Zero timers / zero polling at rest: sleeps in `mpsc::Receiver::recv()`, woken only by:
//!   - `tb-shell.sock` events (`shown` on `sound`/`home`, `emit` from `sound`)
//!   - `inotify` on `$XDG_RUNTIME_DIR/tinybot/volume-bridge.last` and `/run/tinybot/speaker-state`
//!
//! Safety & harm rules:
//!   - Everyday volume runs through `wpctl` (`tb-volume-bridge` owns the sink->hardware path
//!     and raises the root guard when crossing `-30 dB`).
//!   - Direct tone/sweep tries run `/usr/local/sbin/tb-sound` on a worker thread so `All off`
//!     (`emit("stop")`) is never blocked and immediately stops an active try and runs
//!     `/usr/local/sbin/tb-audio-off`.
//!   - Exit `4` (`CONFIRM: ...`) and exit `5` (`UNLOCK: ...`) raise `tb-shell`'s root GUARD
//!     (`tbui ask` as root); `--confirm` / `--unlock` are passed ONLY after a physical device
//!     tap on the guard exits `0`.
//!   - Exit `2` / timeout always runs `tb-audio-off --if-free`; exit `3` always runs
//!     `tb-audio-off` unconditionally; exit `0` runs `tb-audio-off --check --if-free`.

#[path = "../page_ipc.rs"]
mod page_ipc;

use makepad_micro_serde::{DeJson, JsonValue, SerJson};
use page_ipc::{
    connect_and_watch, env_opt, env_or, obj, parse_kv, run_cmd, run_root_tool,
    spawn_inotify_watcher, sudo_prog, terminate_recorded_pid, CmdResult, ShellWriter,
};
use std::io::BufRead;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const VALID_OUTS: &[&str] = &["speaker", "receiver", "lineout", "handset"];
const VALID_LEVELS: &[&str] = &["-70", "-60", "-50", "-40", "-30", "-25", "-20", "-15", "-10", "-5", "0"];
const VALID_FREQS: &[&str] = &["400", "500", "1000", "2000", "3000", "5000", "8000"];
const VALID_SECS: &[&str] = &["0.5", "1", "2", "3"];

#[derive(Clone)]
struct Config {
    sock_path: String,
    sound_bin: String,
    audio_off_bin: String,
    wpctl_bin: String,
    tbui_bin: String,
    speaker_state_path: String,
    volume_bridge_dir: Option<String>,
}

impl Config {
    fn from_env(sock_path: String) -> Self {
        let default_tbui = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("tbui")))
            .filter(|p| p.exists())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/opt/tinybot/shell/bin/tbui".to_string());
        let volume_bridge_dir = env_opt("TB_VOLUME_BRIDGE_DIR")
            .or_else(|| env_opt("XDG_RUNTIME_DIR").map(|d| format!("{d}/tinybot")));
        Self {
            sock_path,
            sound_bin: env_or("TB_SOUND_BIN", "/usr/local/sbin/tb-sound"),
            audio_off_bin: env_or("TB_AUDIO_OFF_BIN", "/usr/local/sbin/tb-audio-off"),
            wpctl_bin: env_or("TB_WPCTL_BIN", "wpctl"),
            tbui_bin: env_or("TB_TBUI_BIN", &default_tbui),
            speaker_state_path: env_or("TB_SPEAKER_STATE", "/run/tinybot/speaker-state"),
            volume_bridge_dir,
        }
    }
}

enum Ev {
    Socket(JsonValue),
    SocketClosed,
    FsChanged,
    DelayedRefresh,
}

struct BusyGuard(Arc<AtomicBool>);
impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn opt_str(v: Option<&String>) -> JsonValue {
    v.map(|s| JsonValue::String(s.clone())).unwrap_or(JsonValue::Null)
}

/// Reads `--volume get` and `/run/tinybot/speaker-state`, returning `(floor_db, top_db, patch_obj)`.
fn read_fast_state(cfg: &Config) -> (f64, f64, Vec<(&'static str, JsonValue)>) {
    let vol_res = run_root_tool(&cfg.sound_bin, &["--volume", "get"], Duration::from_secs(5), None);
    let vol_kv = parse_kv(&vol_res.stdout);

    let spk_text = std::fs::read_to_string(&cfg.speaker_state_path).unwrap_or_default();
    let spk_kv = parse_kv(&spk_text);

    let floor_db = vol_kv
        .get("volume_floor_db")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(-70.0);
    let top_db = vol_kv
        .get("volume_top_db")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(-15.0);

    let fields = vec![
        ("volume_db", opt_str(vol_kv.get("volume.speaker"))),
        ("heard_db", opt_str(vol_kv.get("volume_heard.speaker"))),
        ("plays_at_db", opt_str(vol_kv.get("volume_plays_at.speaker"))),
        ("top_db", opt_str(vol_kv.get("volume_top_db"))),
        ("top_reason", opt_str(vol_kv.get("volume_top_reason"))),
        ("floor_db", opt_str(vol_kv.get("volume_floor_db"))),
        ("step_db", opt_str(vol_kv.get("volume_step_db"))),
        ("speaker_state", opt_str(spk_kv.get("state"))),
        ("speaker_reason", opt_str(spk_kv.get("reason"))),
        ("stream_total_db", opt_str(spk_kv.get("total_db"))),
        ("stream_cap_db", opt_str(spk_kv.get("cap_db"))),
        ("streams_played", opt_str(spk_kv.get("streams_played"))),
        ("bytes_dropped", opt_str(spk_kv.get("bytes_dropped"))),
        ("underruns", opt_str(spk_kv.get("underruns"))),
        ("lock_holder", opt_str(spk_kv.get("lock_holder"))),
    ];
    (floor_db, top_db, fields)
}

fn publish_fast(
    writer: &ShellWriter,
    cfg: &Config,
    extra: Vec<(&'static str, JsonValue)>,
    last_sent: Option<&mut String>,
) -> Result<(f64, f64), String> {
    let (floor_db, top_db, mut fields) = read_fast_state(cfg);
    fields.extend(extra);
    let patch = obj(fields);
    if let Some(prev) = last_sent {
        let ser = patch.serialize_json();
        if *prev == ser {
            return Ok((floor_db, top_db));
        }
        *prev = ser;
    }
    writer.patch_key("sound", patch)?;
    Ok((floor_db, top_db))
}

fn strip_trailing_sentence<'a>(line: &'a str, suffix: &str) -> String {
    let trimmed = line.trim().trim_end_matches('.');
    if let Some(before) = trimmed.strip_suffix(suffix) {
        before.trim().to_string()
    } else {
        line.trim().to_string()
    }
}

/// Raises `tb-shell`'s root GUARD via `tbui ask` with a 15-second timeout.
/// Returns the exit code of `tbui ask` (`Some(0)` ONLY on a physical tap on `yes_label`).
fn ask_root_guard(
    cfg: &Config,
    question: &str,
    yes_label: &str,
    no_label: &str,
    slot: &Arc<Mutex<Option<u32>>>,
) -> CmdResult {
    let sock_env = format!("TB_SHELL_SOCK={}", cfg.sock_path);
    if let Some(sudo) = sudo_prog() {
        let args = [
            "-n",
            "env",
            sock_env.as_str(),
            "timeout",
            "15",
            cfg.tbui_bin.as_str(),
            "ask",
            question,
            yes_label,
            no_label,
        ];
        run_cmd(&sudo, &args, Duration::from_secs(17), Some(&sudo), Some(slot))
    } else {
        // Unprivileged host test mode (`TB_SUDO=none`).
        let args = [
            sock_env.as_str(),
            "TBUI_ASK_GUARD=1",
            "timeout",
            "15",
            cfg.tbui_bin.as_str(),
            "ask",
            question,
            yes_label,
            no_label,
        ];
        run_cmd("env", &args, Duration::from_secs(17), None, Some(slot))
    }
}

/// Executes a `tb-sound` tone/sweep try and enforces all post-checks, guard prompts,
/// and emergency `tb-audio-off` rules for every exit status.
fn execute_sound_try(
    cfg: Config,
    writer: ShellWriter,
    base_args: Vec<String>,
    abort: Arc<AtomicBool>,
    child_slot: Arc<Mutex<Option<u32>>>,
    tx: Sender<Ev>,
    _busy: BusyGuard,
) {
    let run_sound = |extra: &[&str]| -> CmdResult {
        let mut refs: Vec<&str> = base_args.iter().map(String::as_str).collect();
        refs.extend_from_slice(extra);
        run_root_tool(&cfg.sound_bin, &refs, Duration::from_secs(15), Some(&child_slot))
    };

    let finish_result = |res: CmdResult| -> String {
        if abort.load(Ordering::SeqCst) {
            return String::new();
        }
        let line = res.summary_line();
        if res.timed_out {
            let _ = run_root_tool(&cfg.audio_off_bin, &["--if-free"], Duration::from_secs(5), None);
            return if line.is_empty() {
                "TIMEOUT: tb-sound timed out after 15 s; ran tb-audio-off --if-free".into()
            } else {
                format!("TIMEOUT: {line}")
            };
        }
        match res.code {
            Some(0) => {
                let chk = run_root_tool(&cfg.audio_off_bin, &["--check", "--if-free"], Duration::from_secs(5), None);
                if chk.code != Some(0) && !chk.summary_line().is_empty() {
                    format!("{line} (post-check: {})", chk.summary_line())
                } else if line.is_empty() {
                    "DONE".into()
                } else {
                    line
                }
            }
            Some(1) | Some(64) => {
                if line.is_empty() { "REFUSED".into() } else { line }
            }
            Some(3) => {
                let _ = run_root_tool(&cfg.audio_off_bin, &[], Duration::from_secs(5), None);
                if line.is_empty() { "NOT SET BACK: ran tb-audio-off".into() } else { line }
            }
            Some(2) | _ => {
                let _ = run_root_tool(&cfg.audio_off_bin, &["--if-free"], Duration::from_secs(5), None);
                if line.is_empty() {
                    format!("FAILED: exit {:?}", res.code)
                } else {
                    line
                }
            }
        }
    };

    let _ = writer.patch_key("sound", obj(vec![("last", "Running tb-sound...".into())]));
    let mut res = run_sound(&[]);
    if abort.load(Ordering::SeqCst) {
        let _ = tx.send(Ev::DelayedRefresh);
        return;
    }

    let mut confirmed_flags: Vec<&str> = Vec::new();

    // Exit 4: CONFIRM harm rule -> raise root GUARD, re-run with --confirm ONLY on exit 0.
    if !res.timed_out && res.code == Some(4) {
        let raw_line = res.summary_line();
        let clean = strip_trailing_sentence(&raw_line, "Tap again to play it");
        let prompt = format!("Arm's length, not at the ear. {clean}");
        let _ = writer.patch_key("sound", obj(vec![("last", "Waiting for device confirmation on GUARD...".into())]));
        let ask = ask_root_guard(&cfg, &prompt, "Play it", "Cancel", &child_slot);
        if abort.load(Ordering::SeqCst) {
            let _ = tx.send(Ev::DelayedRefresh);
            return;
        }
        if !ask.timed_out && ask.code == Some(0) {
            confirmed_flags.push("--confirm");
            let _ = writer.patch_key("sound", obj(vec![("last", "Confirmed: playing...".into())]));
            res = run_sound(&confirmed_flags);
            if abort.load(Ordering::SeqCst) {
                let _ = tx.send(Ev::DelayedRefresh);
                return;
            }
        } else {
            let msg = format!("Not confirmed: {raw_line}");
            let _ = publish_fast(&writer, &cfg, vec![("last", msg.into())], None);
            let _ = tx.send(Ev::DelayedRefresh);
            return;
        }
    }

    // Exit 5: UNLOCK ceiling harm rule -> raise root GUARD, re-run with --confirm --unlock ONLY on exit 0.
    if !res.timed_out && res.code == Some(5) {
        let raw_line = res.summary_line();
        let clean = strip_trailing_sentence(&raw_line, "Unlock to go above it");
        let prompt = format!("Arm's length, not at the ear. {clean}");
        let _ = writer.patch_key("sound", obj(vec![("last", "Ceiling reached: waiting on GUARD...".into())]));
        let ask = ask_root_guard(&cfg, &prompt, "Unlock ceiling", "Cancel", &child_slot);
        if abort.load(Ordering::SeqCst) {
            let _ = tx.send(Ev::DelayedRefresh);
            return;
        }
        if !ask.timed_out && ask.code == Some(0) {
            if !confirmed_flags.contains(&"--confirm") {
                confirmed_flags.push("--confirm");
            }
            confirmed_flags.push("--unlock");
            let _ = writer.patch_key("sound", obj(vec![("last", "Unlocked: playing...".into())]));
            res = run_sound(&confirmed_flags);
            if abort.load(Ordering::SeqCst) {
                let _ = tx.send(Ev::DelayedRefresh);
                return;
            }
        } else {
            let msg = format!("Not unlocked: {raw_line}");
            let _ = publish_fast(&writer, &cfg, vec![("last", msg.into())], None);
            let _ = tx.send(Ev::DelayedRefresh);
            return;
        }
    }

    let final_msg = finish_result(res);
    if !abort.load(Ordering::SeqCst) && !final_msg.is_empty() {
        let _ = publish_fast(&writer, &cfg, vec![("last", final_msg.into())], None);
    }
    let _ = tx.send(Ev::DelayedRefresh);
}

fn schedule_delayed_refresh(tx: Sender<Ev>, delay: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let _ = tx.send(Ev::DelayedRefresh);
    });
}

fn json_f64(v: Option<&JsonValue>) -> Option<f64> {
    match v? {
        JsonValue::F64(f) => Some(*f),
        JsonValue::I64(n) => Some(*n as f64),
        JsonValue::U64(n) => Some(*n as f64),
        JsonValue::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn format_hw_state(stdout: &str) -> String {
    let kv = parse_kv(stdout);
    let mut parts = Vec::new();
    for (label, key) in [
        ("amp", "amp_mode"),
        ("line", "amp_line"),
        ("loudest", "loudest.speaker"),
        ("draw_mA", "loudest_ma.speaker"),
        ("confirm>", "confirm_above.speaker"),
        ("ceiling", "ceiling.speaker"),
        ("red_wait", "red_wait_s"),
    ] {
        if let Some(v) = kv.get(key) {
            parts.push(format!("{label}={v}"));
        }
    }
    if parts.is_empty() {
        stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" · ")
    } else {
        parts.join(" · ")
    }
}

fn main() {
    let (sock_path, writer, reader) = match connect_and_watch(&["sound", "home"]) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tb-shell-sound: {e}");
            std::process::exit(1);
        }
    };
    let cfg = Config::from_env(sock_path);

    // Ensure $XDG_RUNTIME_DIR/tinybot exists so inotify can watch volume-bridge.last writes.
    if let Some(dir) = &cfg.volume_bridge_dir {
        let _ = std::fs::create_dir_all(dir);
    }

    let (tx, rx) = mpsc::channel::<Ev>();

    // Socket reader thread (blocks in read(), zero wakeups at rest).
    {
        let tx = tx.clone();
        let _ = std::thread::Builder::new().name("tb-sound-sock".into()).spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if let Ok(ev) = JsonValue::deserialize_json(line.trim()) {
                    let _ = tx.send(Ev::Socket(ev));
                }
            }
            let _ = tx.send(Ev::SocketClosed);
        });
    }

    // Inotify watcher thread for speaker-state and volume-bridge.last (zero wakeups at rest).
    {
        let mut watches = Vec::new();
        if let Some(p) = Path::new(&cfg.speaker_state_path).parent() {
            if let Some(fname) = Path::new(&cfg.speaker_state_path).file_name() {
                watches.push((
                    p.to_string_lossy().to_string(),
                    vec![fname.to_string_lossy().to_string()],
                ));
            }
        }
        if let Some(dir) = &cfg.volume_bridge_dir {
            watches.push((dir.clone(), vec!["volume-bridge.last".to_string()]));
        }
        let tx = tx.clone();
        spawn_inotify_watcher(watches, move || {
            let _ = tx.send(Ev::FsChanged);
        });
    }

    let mut last_fast_json = String::new();
    let (mut floor_db, mut top_db) = match publish_fast(&writer, &cfg, vec![], Some(&mut last_fast_json)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("tb-shell-sound: initial publish: {e}");
            std::process::exit(1);
        }
    };

    let busy = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let child_slot: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

    while let Ok(msg) = rx.recv() {
        match msg {
            Ev::SocketClosed => break,
            Ev::FsChanged | Ev::DelayedRefresh => {
                if let Ok(v) = publish_fast(&writer, &cfg, vec![], Some(&mut last_fast_json)) {
                    (floor_db, top_db) = v;
                }
            }
            Ev::Socket(ev) => {
                let kind = ev.get("ev").and_then(|v| v.as_str()).unwrap_or("");
                match kind {
                    "shown" => {
                        if let Ok(v) = publish_fast(&writer, &cfg, vec![], Some(&mut last_fast_json)) {
                            (floor_db, top_db) = v;
                        }
                    }
                    "panel" if ev.get("on") == Some(&JsonValue::Bool(true)) => {
                        if let Ok(v) = publish_fast(&writer, &cfg, vec![], Some(&mut last_fast_json)) {
                            (floor_db, top_db) = v;
                        }
                    }
                    "emit" if ev.get("surface").and_then(|v| v.as_str()) == Some("sound") => {
                        let name = ev.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let data = ev.get("data");
                        match name {
                            "refresh" => {
                                last_fast_json.clear();
                                if let Ok(v) = publish_fast(&writer, &cfg, vec![], Some(&mut last_fast_json)) {
                                    (floor_db, top_db) = v;
                                }
                            }
                            "tool_state" => {
                                let res = run_root_tool(&cfg.sound_bin, &["--state"], Duration::from_secs(5), None);
                                let hw = format_hw_state(&res.stdout);
                                let summary = res.summary_line();
                                last_fast_json.clear();
                                if let Ok(v) = publish_fast(
                                    &writer,
                                    &cfg,
                                    vec![("hw_state", hw.into()), ("last", summary.into())],
                                    None,
                                ) {
                                    (floor_db, top_db) = v;
                                }
                            }
                            "vol_down" => {
                                let res = run_cmd(
                                    &cfg.wpctl_bin,
                                    &["set-volume", "-l", "1.0", "@DEFAULT_AUDIO_SINK@", "0.075-"],
                                    Duration::from_secs(5),
                                    None,
                                    None,
                                );
                                if res.code != Some(0) {
                                    let _ = writer.patch_key("sound", obj(vec![("last", format!("wpctl: {}", res.summary_line()).into())]));
                                }
                                schedule_delayed_refresh(tx.clone(), Duration::from_millis(500));
                            }
                            "vol_up" => {
                                let res = run_cmd(
                                    &cfg.wpctl_bin,
                                    &["set-volume", "-l", "1.0", "@DEFAULT_AUDIO_SINK@", "0.075+"],
                                    Duration::from_secs(5),
                                    None,
                                    None,
                                );
                                if res.code != Some(0) {
                                    let _ = writer.patch_key("sound", obj(vec![("last", format!("wpctl: {}", res.summary_line()).into())]));
                                }
                                schedule_delayed_refresh(tx.clone(), Duration::from_millis(500));
                            }
                            "vol_db" => {
                                if let Some(db) = json_f64(data.and_then(|d| d.get("db"))) {
                                    let span = (top_db - floor_db).max(1.0);
                                    let norm = ((db - floor_db) / span).clamp(0.0, 1.0);
                                    let val = format!("{norm:.3}");
                                    let res = run_cmd(
                                        &cfg.wpctl_bin,
                                        &["set-volume", "@DEFAULT_AUDIO_SINK@", &val],
                                        Duration::from_secs(5),
                                        None,
                                        None,
                                    );
                                    if res.code != Some(0) {
                                        let _ = writer.patch_key("sound", obj(vec![("last", format!("wpctl: {}", res.summary_line()).into())]));
                                    }
                                    schedule_delayed_refresh(tx.clone(), Duration::from_millis(500));
                                }
                            }
                            "stop" => {
                                abort.store(true, Ordering::SeqCst);
                                if let Ok(mut g) = child_slot.lock() {
                                    if let Some(pid) = g.take() {
                                        let sudo = sudo_prog();
                                        terminate_recorded_pid(pid, sudo.as_deref());
                                    }
                                }
                                let off = run_root_tool(&cfg.audio_off_bin, &[], Duration::from_secs(5), None);
                                let line = off.summary_line();
                                let msg = if line.is_empty() { "All off".to_string() } else { line };
                                last_fast_json.clear();
                                if let Ok(v) = publish_fast(&writer, &cfg, vec![("last", msg.into())], None) {
                                    (floor_db, top_db) = v;
                                }
                            }
                            "play" | "sweep" => {
                                let out = data.and_then(|d| d.get("out")).and_then(|v| v.as_str()).unwrap_or("speaker");
                                let level = data.and_then(|d| d.get("level")).and_then(|v| v.as_str()).unwrap_or("-50");
                                let freq = data.and_then(|d| d.get("freq")).and_then(|v| v.as_str()).unwrap_or("1000");
                                let secs = data.and_then(|d| d.get("secs")).and_then(|v| v.as_str()).unwrap_or("1");

                                let ok_common = VALID_OUTS.contains(&out) && VALID_LEVELS.contains(&level) && VALID_SECS.contains(&secs);
                                let ok_mode = name == "sweep" || VALID_FREQS.contains(&freq);
                                if !ok_common || !ok_mode {
                                    let _ = writer.patch_key("sound", obj(vec![("last", "REFUSED: invalid tone/sweep parameter".into())]));
                                    continue;
                                }
                                if busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
                                    let _ = writer.patch_key("sound", obj(vec![("last", "Busy: sound try already running (tap All off to stop)".into())]));
                                    continue;
                                }
                                abort.store(false, Ordering::SeqCst);
                                let mut args = vec![
                                    "--out".to_string(),
                                    out.to_string(),
                                    "--gain".to_string(),
                                    "-10Db".to_string(),
                                    "--level".to_string(),
                                    level.to_string(),
                                ];
                                if name == "sweep" {
                                    args.extend(["--sweep".to_string(), "300".to_string(), "3000".to_string()]);
                                } else {
                                    args.extend(["--freq".to_string(), freq.to_string()]);
                                }
                                args.extend(["--seconds".to_string(), secs.to_string()]);

                                let cfg_c = cfg.clone();
                                let writer_c = writer.clone();
                                let abort_c = abort.clone();
                                let slot_c = child_slot.clone();
                                let tx_c = tx.clone();
                                let guard = BusyGuard(busy.clone());
                                let _ = std::thread::Builder::new()
                                    .name("tb-sound-try".into())
                                    .spawn(move || execute_sound_try(cfg_c, writer_c, args, abort_c, slot_c, tx_c, guard));
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
