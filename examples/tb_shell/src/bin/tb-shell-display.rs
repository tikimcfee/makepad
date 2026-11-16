//! tb-shell-display: event-driven owner of `state.display` for `views/display.splash`.
//!
//! Sleeps in `read()` on `tb-shell.sock` with zero timers and zero wakeups at rest.
//! Reads backlight and panel state from sysfs on start, when `display` or `home` is
//! shown, and on `refresh`. Runs `/usr/local/sbin/tb-brightness set <1..255>` on
//! preset/step taps (never writes 0; carries no harm rule).

#[path = "../page_ipc.rs"]
mod page_ipc;

use makepad_micro_serde::{DeJson, JsonValue};
use page_ipc::{connect_and_watch, env_or, obj, run_root_tool, ShellWriter};
use std::io::BufRead;
use std::time::Duration;

struct Paths {
    actual: String,
    max: String,
    panel_summary: String,
    panel_state: String,
    brightness_bin: String,
}

impl Paths {
    fn from_env() -> Self {
        Self {
            actual: env_or("TB_DISPLAY_ACTUAL", "/sys/class/backlight/tinybot-lcd/actual_brightness"),
            max: env_or("TB_DISPLAY_MAX", "/sys/class/backlight/tinybot-lcd/max_brightness"),
            panel_summary: env_or("TB_DISPLAY_PANEL_SUMMARY", "/sys/kernel/tinybot_panel/summary"),
            panel_state: env_or("TB_DISPLAY_PANEL_STATE", "/sys/kernel/tinybot_panel/state"),
            brightness_bin: env_or("TB_BRIGHTNESS_BIN", "/usr/local/sbin/tb-brightness"),
        }
    }

    fn read_i64(path: &str) -> Option<i64> {
        std::fs::read_to_string(path).ok()?.trim().parse::<i64>().ok()
    }

    fn read_panel(&self) -> Option<String> {
        if let Ok(s) = std::fs::read_to_string(&self.panel_summary) {
            if let Some(tok) = s.split_whitespace().nth(1) {
                return Some(tok.to_string());
            }
        }
        std::fs::read_to_string(&self.panel_state).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    fn snapshot(&self, last: &str) -> (Option<i64>, i64, JsonValue) {
        let cur = Self::read_i64(&self.actual);
        let max = Self::read_i64(&self.max).filter(|&m| m > 0).unwrap_or(255);
        let panel = self.read_panel();
        let val = obj(vec![
            ("brightness", cur.map(JsonValue::I64).unwrap_or(JsonValue::Null)),
            ("max", JsonValue::I64(max)),
            ("panel", panel.map(JsonValue::String).unwrap_or(JsonValue::Null)),
            ("last", JsonValue::String(last.to_string())),
        ]);
        (cur, max, val)
    }
}

fn json_i64(v: Option<&JsonValue>) -> Option<i64> {
    match v? {
        JsonValue::I64(n) => Some(*n),
        JsonValue::U64(n) => Some(*n as i64),
        JsonValue::F64(f) => Some(*f as i64),
        JsonValue::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Perceptual-ish step across 1..=max, strictly clamped to >= 1 (never 0).
fn step_brightness(cur: i64, dir: i64, max: i64) -> i64 {
    let target = if dir < 0 {
        let d = if cur <= 5 { 1 } else if cur <= 25 { 5 } else if cur <= 64 { 8 } else { 16 };
        cur - d
    } else {
        let d = if cur < 5 { 1 } else if cur < 25 { 5 } else if cur < 64 { 8 } else { 16 };
        cur + d
    };
    target.clamp(1, max.max(1))
}

fn apply_level(paths: &Paths, level: i64, max: i64) -> String {
    let clamped = level.clamp(1, max.max(1));
    let arg = clamped.to_string();
    let res = run_root_tool(&paths.brightness_bin, &["set", &arg], Duration::from_secs(5), None);
    let line = res.summary_line();
    if !line.is_empty() {
        line
    } else if res.timed_out {
        "TIMEOUT: tb-brightness set timed out".into()
    } else {
        format!("exit {:?}", res.code)
    }
}

fn publish(writer: &ShellWriter, paths: &Paths, last: &str) -> Result<(Option<i64>, i64), String> {
    let (cur, max, val) = paths.snapshot(last);
    writer.patch_key("display", val)?;
    Ok((cur, max))
}

fn main() {
    let paths = Paths::from_env();
    let (_, writer, reader) = match connect_and_watch(&["display", "home"]) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tb-shell-display: {e}");
            std::process::exit(1);
        }
    };

    let mut last = String::new();
    let (mut cur, mut max) = match publish(&writer, &paths, &last) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("tb-shell-display: initial publish: {e}");
            std::process::exit(1);
        }
    };

    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(ev) = JsonValue::deserialize_json(line.trim()) else { continue };
        let kind = ev.get("ev").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "shown" => {
                if let Ok(v) = publish(&writer, &paths, &last) {
                    (cur, max) = v;
                }
            }
            "emit" if ev.get("surface").and_then(|v| v.as_str()) == Some("display") => {
                let name = ev.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let data = ev.get("data");
                match name {
                    "refresh" => {}
                    "set" => {
                        if let Some(lvl) = json_i64(data.and_then(|d| d.get("level"))) {
                            last = apply_level(&paths, lvl, max);
                        }
                    }
                    "step" => {
                        let delta = json_i64(data.and_then(|d| d.get("delta"))).unwrap_or(0);
                        if delta != 0 {
                            let fresh_cur = Paths::read_i64(&paths.actual).or(cur).unwrap_or(64);
                            let target = step_brightness(fresh_cur, delta, max);
                            last = apply_level(&paths, target, max);
                        }
                    }
                    _ => continue,
                }
                if let Err(e) = publish(&writer, &paths, &last).map(|v| (cur, max) = v) {
                    eprintln!("tb-shell-display: {e}");
                    break;
                }
            }
            _ => {}
        }
    }
}
