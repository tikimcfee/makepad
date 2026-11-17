//! tb-shell-display: event-driven owner of `state.display` (`views/display.splash`)
//! and action runner for `views/power.splash`.
//!
//! Sleeps in `read()` on `tb-shell.sock` with zero timers and zero wakeups at rest.
//! Reads backlight, panel state, display path/fallback, and `screen-timeout` on start,
//! when `display` or `home` is shown, and on `panel` wake.

#[path = "../page_ipc.rs"]
mod page_ipc;

use makepad_micro_serde::{DeJson, JsonValue};
use page_ipc::{connect_and_watch, env_opt, env_or, obj, run_cmd, run_root_tool, sudo_prog, ShellWriter};
use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

const VALID_TIMEOUTS: &[i64] = &[0, 30, 60, 120, 300];

#[derive(Clone)]
struct Paths {
    sock_path: String,
    actual: String,
    max: String,
    panel_summary: String,
    panel_state: String,
    display_state: String,
    user_timeout: Option<String>,
    sys_timeout: String,
    brightness_bin: String,
    power_ctl_bin: String,
    tbui_bin: String,
}

impl Paths {
    fn from_env(sock_path: String) -> Self {
        let user_timeout = env_opt("TB_SCREEN_USER_TIMEOUT_FILE").or_else(|| {
            env_opt("HOME").map(|h| format!("{h}/.config/tb-shell/screen-timeout"))
        });
        let default_tbui = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("tbui")))
            .filter(|p| p.exists())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/opt/tinybot/shell/bin/tbui".to_string());
        Self {
            sock_path,
            actual: env_or("TB_DISPLAY_ACTUAL", "/sys/class/backlight/tinybot-lcd/actual_brightness"),
            max: env_or("TB_DISPLAY_MAX", "/sys/class/backlight/tinybot-lcd/max_brightness"),
            panel_summary: env_or("TB_DISPLAY_PANEL_SUMMARY", "/sys/kernel/tinybot_panel/summary"),
            panel_state: env_or("TB_DISPLAY_PANEL_STATE", "/sys/kernel/tinybot_panel/state"),
            display_state: env_or("TB_DISPLAY_STATE", "/run/tinybot/display-state"),
            user_timeout,
            sys_timeout: env_or("TB_SCREEN_TIMEOUT_FILE", "/userdata/tinybot/etc/screen-timeout"),
            brightness_bin: env_or("TB_BRIGHTNESS_BIN", "/usr/local/sbin/tb-brightness"),
            power_ctl_bin: env_or("TB_POWER_CTL", "/usr/local/sbin/tinybot-power-ctl"),
            tbui_bin: env_or("TB_TBUI_BIN", &default_tbui),
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

    fn read_display_state(&self) -> (Option<String>, Option<bool>) {
        let Ok(text) = std::fs::read_to_string(&self.display_state) else { return (None, None) };
        let mut path_val = None;
        let mut fallback = None;
        for line in text.lines() {
            if let Some((k, v)) = line.trim().split_once('=') {
                let v = v.trim();
                match k.trim() {
                    "path" if !v.is_empty() => path_val = Some(v.to_string()),
                    "fallback" => fallback = Some(v == "1" || v.eq_ignore_ascii_case("true")),
                    _ => {}
                }
            }
        }
        (path_val, fallback)
    }

    fn read_timeout(&self) -> i64 {
        if let Some(ref uf) = self.user_timeout {
            if let Some(v) = Self::read_i64(uf) {
                return v.max(0);
            }
        }
        Self::read_i64(&self.sys_timeout).unwrap_or(60).max(0)
    }

    fn write_user_timeout(&self, secs: i64) -> Result<(), String> {
        let Some(ref uf) = self.user_timeout else {
            return Err("no HOME / TB_SCREEN_USER_TIMEOUT_FILE".into());
        };
        let p = Path::new(uf);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        }
        let tmp = format!("{uf}.tmp.{}", std::process::id());
        std::fs::write(&tmp, format!("{secs}\n")).map_err(|e| format!("write {tmp}: {e}"))?;
        std::fs::rename(&tmp, uf).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("rename {uf}: {e}")
        })
    }

    fn snapshot(&self, last: &str) -> (Option<i64>, i64, JsonValue) {
        let cur = Self::read_i64(&self.actual);
        let max = Self::read_i64(&self.max).filter(|&m| m > 0).unwrap_or(255);
        let panel = self.read_panel();
        let (disp_path, fallback) = self.read_display_state();
        let timeout = self.read_timeout();
        let val = obj(vec![
            ("brightness", cur.map(JsonValue::I64).unwrap_or(JsonValue::Null)),
            ("max", JsonValue::I64(max)),
            ("panel", panel.map(JsonValue::String).unwrap_or(JsonValue::Null)),
            ("path", disp_path.map(JsonValue::String).unwrap_or(JsonValue::Null)),
            ("fallback", fallback.map(JsonValue::Bool).unwrap_or(JsonValue::Null)),
            ("timeout", JsonValue::I64(timeout)),
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

fn send_screen_off() -> String {
    let sock = env_opt("TB_SCREEN_SOCK").unwrap_or_else(|| {
        let xdg = env_opt("XDG_RUNTIME_DIR").unwrap_or_else(|| "/run/user/8000".into());
        format!("{xdg}/tb-screen.sock")
    });
    match UnixStream::connect(&sock) {
        Ok(mut s) => {
            let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
            let _ = s.write_all(b"off\n");
            "Screen sleeping".into()
        }
        Err(e) => format!("tb-screen.sock: {e}"),
    }
}

fn spawn_power_ctl(paths: Paths, action: &'static str) {
    let _ = std::thread::Builder::new().name("tb-power-ctl".into()).spawn(move || {
        // Prefer `/usr/local/sbin/tinybot-power-ctl` when present (raises root GUARD as uid 0).
        if Path::new(&paths.power_ctl_bin).exists() {
            let _ = run_root_tool(&paths.power_ctl_bin, &[action], Duration::from_secs(25), None);
            return;
        }
        // Standalone fallback: raise root GUARD via `tbui ask`, then run `systemctl`.
        let (question, yes_lbl) = if action == "reboot" {
            ("Restart the phone now?", "Restart")
        } else {
            ("Power off the phone now?", "Power off")
        };
        let sock_env = format!("TB_SHELL_SOCK={}", paths.sock_path);
        let ask = if let Some(sudo) = sudo_prog() {
            run_cmd(
                &sudo,
                &["-n", "env", &sock_env, "timeout", "15", &paths.tbui_bin, "ask", question, yes_lbl, "Cancel"],
                Duration::from_secs(18),
                Some(&sudo),
                None,
            )
        } else {
            run_cmd(
                "env",
                &[&sock_env, "TBUI_ASK_GUARD=1", "timeout", "15", &paths.tbui_bin, "ask", question, yes_lbl, "Cancel"],
                Duration::from_secs(18),
                None,
                None,
            )
        };
        if !ask.timed_out && ask.code == Some(0) {
            let systemctl = env_or("TB_SYSTEMCTL_BIN", "/bin/systemctl");
            let _ = run_root_tool(&systemctl, &[action], Duration::from_secs(10), None);
        }
    });
}

fn publish(writer: &ShellWriter, paths: &Paths, last: &str) -> Result<(Option<i64>, i64), String> {
    let (cur, max, val) = paths.snapshot(last);
    writer.patch_key("display", val)?;
    Ok((cur, max))
}

fn main() {
    let (sock_path, writer, reader) = match connect_and_watch(&["display", "home", "power"]) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tb-shell-display: {e}");
            std::process::exit(1);
        }
    };
    let paths = Paths::from_env(sock_path);

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
            "shown" | "panel" => {
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
                    "timeout" => {
                        if let Some(secs) = json_i64(data.and_then(|d| d.get("secs"))) {
                            if VALID_TIMEOUTS.contains(&secs) {
                                match paths.write_user_timeout(secs) {
                                    Ok(()) => {
                                        last = if secs == 0 {
                                            "Idle blanking disabled".into()
                                        } else {
                                            format!("Idle timeout set to {secs} s")
                                        };
                                    }
                                    Err(e) => last = format!("timeout: {e}"),
                                }
                            }
                        }
                    }
                    "sleep" => {
                        last = send_screen_off();
                    }
                    _ => continue,
                }
                if let Err(e) = publish(&writer, &paths, &last).map(|v| (cur, max) = v) {
                    eprintln!("tb-shell-display: {e}");
                    break;
                }
            }
            "emit" if ev.get("surface").and_then(|v| v.as_str()) == Some("power") => {
                let name = ev.get("name").and_then(|v| v.as_str()).unwrap_or("");
                match name {
                    "screen_off" => {
                        let _ = send_screen_off();
                    }
                    "reboot" => {
                        spawn_power_ctl(paths.clone(), "reboot");
                    }
                    "poweroff" => {
                        spawn_power_ctl(paths.clone(), "poweroff");
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}
