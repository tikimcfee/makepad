//! tb-shell-battery: native event-driven owner of `state.battery` for `views/battery.splash`.
//!
//! Reads `/sys/class/power_supply/battery/{capacity,current_now,current_avg,voltage_now,temp,status}`
//! once at start, when `battery` or `home` is `shown`, and on `refresh` emit.
//! Sleeps in `read()` on `tb-shell.sock` with zero timers and zero wake-ups at rest.

#[path = "../page_ipc.rs"]
mod page_ipc;

use makepad_micro_serde::{DeJson, JsonValue};
use page_ipc::{connect_and_watch, env_or, obj, set_key_sync};
use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

fn read_str(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_f64(dir: &Path, name: &str) -> Option<f64> {
    read_str(dir, name)?.parse::<f64>().ok()
}

fn round_to(v: f64, scale: f64) -> f64 {
    (v * scale).round() / scale
}

fn read_kv_file(path: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            if let Some((k, v)) = line.trim().split_once('=') {
                let v = v.trim();
                if !v.is_empty() {
                    map.insert(k.trim().to_string(), v.to_string());
                }
            }
        }
    }
    map
}

fn read_battery(dir: &Path, power_state_file: &str) -> JsonValue {
    let kv = read_kv_file(power_state_file);
    let kv_f64 = |k: &str| kv.get(k).and_then(|s| s.parse::<f64>().ok());

    let pct = read_f64(dir, "capacity").or_else(|| kv_f64("soc")).unwrap_or(0.0);
    let cur_now = read_f64(dir, "current_now").or_else(|| kv_f64("current_ua")).unwrap_or(0.0);
    let cur_avg = read_f64(dir, "current_avg").or_else(|| kv_f64("current_ua")).unwrap_or(0.0);
    let volt_now = read_f64(dir, "voltage_now").or_else(|| kv_f64("voltage_uv")).unwrap_or(0.0);
    let temp_raw = read_f64(dir, "temp").or_else(|| kv_f64("temp_dc")).unwrap_or(0.0);
    let status = read_str(dir, "status").or_else(|| kv.get("status").cloned()).unwrap_or_default();
    let desk = kv.get("desk").cloned();

    // Remaining hours at average draw against the gauge's 1010 mAh nominal capacity.
    let hours = if cur_avg < -1000.0 {
        let rem_mah = (pct / 100.0) * 1010.0;
        JsonValue::F64(round_to(rem_mah * 1000.0 / (-cur_avg), 10.0))
    } else {
        JsonValue::Null
    };

    obj(vec![
        ("pct", JsonValue::I64(pct.round() as i64)),
        ("ma", JsonValue::I64((cur_now / 1000.0).round() as i64)),
        ("state", JsonValue::String(status)),
        ("hours", hours),
        ("volts", JsonValue::F64(round_to(volt_now / 1_000_000.0, 1000.0))),
        ("temp", JsonValue::F64(round_to(temp_raw / 10.0, 10.0))),
        ("desk", desk.map(JsonValue::String).unwrap_or(JsonValue::Null)),
    ])
}

fn main() {
    let bat_dir = env_or("TB_BATTERY_DIR", "/sys/class/power_supply/battery");
    let power_state = env_or("TB_POWER_STATE", "/run/tinybot/power-state");
    let dir = Path::new(&bat_dir);

    if std::env::args().any(|a| a == "--once") {
        if let Err(e) = set_key_sync("battery", read_battery(dir, &power_state)) {
            eprintln!("tb-shell-battery: {e}");
            std::process::exit(1);
        }
        return;
    }

    let (_, writer, reader) = match connect_and_watch(&["battery", "home"]) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tb-shell-battery: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = writer.patch_key("battery", read_battery(dir, &power_state)) {
        eprintln!("tb-shell-battery: initial publish: {e}");
        std::process::exit(1);
    }

    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(ev) = JsonValue::deserialize_json(line.trim()) else { continue };
        let kind = ev.get("ev").and_then(|v| v.as_str()).unwrap_or("");
        let should_update = match kind {
            "shown" => true,
            "emit" => {
                ev.get("surface").and_then(|v| v.as_str()) == Some("battery")
                    && ev.get("name").and_then(|v| v.as_str()) == Some("refresh")
            }
            _ => false,
        };
        if should_update {
            if let Err(e) = writer.patch_key("battery", read_battery(dir, &power_state)) {
                eprintln!("tb-shell-battery: {e}");
                break;
            }
        }
    }
}
