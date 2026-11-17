//! tb-shell-wifi: native Rust event-driven owner of `state.wifi` for `views/wifi.splash`.
//!
//! Reads `/run/tinybot/wifi-state` (strictly non-secret operational fields ONLY: `state`,
//! `wpa_state`, `ipv4`, `rssi_dbm`, `freq_mhz`, `function`, `resets` — never SSID/PSK/BSSID!)
//! plus `usb0` and `tailscale0` link status.
//! Zero timers, zero wakeups while asleep (`panel_off` suppresses background refreshes).

#[path = "../page_ipc.rs"]
mod page_ipc;

use makepad_micro_serde::{DeJson, JsonValue, SerJson};
use page_ipc::{connect_and_watch, env_or, obj, parse_kv, run_cmd, set_key_sync, spawn_inotify_watcher, ShellWriter};
use std::io::BufRead;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

enum WifiEv {
    Socket(JsonValue),
    SocketClosed,
    FileChanged,
}

fn opt_str(v: Option<&String>) -> JsonValue {
    v.filter(|s| !s.is_empty()).map(|s| JsonValue::String(s.clone())).unwrap_or(JsonValue::Null)
}

fn read_iface_ipv4(iface: &str, ip_bin: &str) -> Option<String> {
    let oper = std::fs::read_to_string(format!("/sys/class/net/{iface}/operstate")).ok();
    if let Some(ref st) = oper {
        if st.trim() == "down" {
            return None;
        }
    } else if !Path::new(&format!("/sys/class/net/{iface}")).exists() {
        return None;
    }
    let res = run_cmd(ip_bin, &["-4", "-o", "addr", "show", "dev", iface], Duration::from_secs(2), None, None);
    for line in res.stdout.lines() {
        let mut it = line.split_whitespace();
        while let Some(tok) = it.next() {
            if tok == "inet" {
                if let Some(cidr) = it.next() {
                    return Some(cidr.split('/').next().unwrap_or(cidr).to_string());
                }
            }
        }
    }
    oper.map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && s != "down")
}

fn snapshot_wifi(wifi_state_path: &str, ip_bin: &str, include_ifaces: bool) -> JsonValue {
    let raw = std::fs::read_to_string(wifi_state_path).unwrap_or_default();
    let kv = parse_kv(&raw);

    let usb_ip = if include_ifaces { read_iface_ipv4("usb0", ip_bin) } else { None };
    let ts_ip = if include_ifaces { read_iface_ipv4("tailscale0", ip_bin) } else { None };

    obj(vec![
        ("state", opt_str(kv.get("state"))),
        ("wpa_state", opt_str(kv.get("wpa_state"))),
        ("ipv4", opt_str(kv.get("ipv4"))),
        ("rssi_dbm", opt_str(kv.get("rssi_dbm"))),
        ("freq_mhz", opt_str(kv.get("freq_mhz"))),
        ("function", opt_str(kv.get("function"))),
        ("resets", opt_str(kv.get("resets"))),
        ("usb_ipv4", usb_ip.map(JsonValue::String).unwrap_or(JsonValue::Null)),
        ("tailscale", ts_ip.map(JsonValue::String).unwrap_or(JsonValue::Null)),
    ])
}

fn push_wifi(writer: &ShellWriter, val: JsonValue, last_ser: &mut String) -> Result<(), String> {
    let ser = val.serialize_json();
    if ser == *last_ser {
        return Ok(());
    }
    writer.patch_key("wifi", val)?;
    *last_ser = ser;
    Ok(())
}

fn main() {
    let wifi_file = env_or("TB_WIFI_STATE", "/run/tinybot/wifi-state");
    let ip_bin = env_or("TB_IP_BIN", "ip");

    if std::env::args().any(|a| a == "--once") {
        let val = snapshot_wifi(&wifi_file, &ip_bin, true);
        if let Err(e) = set_key_sync("wifi", val) {
            eprintln!("tb-shell-wifi: {e}");
            std::process::exit(1);
        }
        return;
    }

    let (_, writer, reader) = match connect_and_watch(&["wifi", "home"]) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tb-shell-wifi: {e}");
            std::process::exit(1);
        }
    };

    let mut last_ser = String::new();
    if let Err(e) = push_wifi(&writer, snapshot_wifi(&wifi_file, &ip_bin, true), &mut last_ser) {
        eprintln!("tb-shell-wifi: initial publish: {e}");
        std::process::exit(1);
    }

    let panel_off = Arc::new(AtomicBool::new(false));
    let mut wifi_visible = false;
    let (tx, rx) = mpsc::channel::<WifiEv>();

    {
        let tx = tx.clone();
        let _ = std::thread::Builder::new().name("tb-wifi-sock".into()).spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if let Ok(ev) = JsonValue::deserialize_json(line.trim()) {
                    let _ = tx.send(WifiEv::Socket(ev));
                }
            }
            let _ = tx.send(WifiEv::SocketClosed);
        });
    }

    {
        let p = Path::new(&wifi_file);
        if let (Some(dir), Some(fname)) = (p.parent(), p.file_name()) {
            let watches = vec![(dir.to_string_lossy().to_string(), vec![fname.to_string_lossy().to_string()])];
            let tx = tx.clone();
            let panel_off = panel_off.clone();
            spawn_inotify_watcher(watches, move || {
                if !panel_off.load(Ordering::Relaxed) {
                    let _ = tx.send(WifiEv::FileChanged);
                }
            });
        }
    }

    while let Ok(msg) = rx.recv() {
        match msg {
            WifiEv::SocketClosed => break,
            WifiEv::FileChanged => {
                if !panel_off.load(Ordering::Relaxed) {
                    let val = snapshot_wifi(&wifi_file, &ip_bin, wifi_visible);
                    let _ = push_wifi(&writer, val, &mut last_ser);
                }
            }
            WifiEv::Socket(ev) => {
                let kind = ev.get("ev").and_then(|v| v.as_str()).unwrap_or("");
                let surf = ev.get("surface").and_then(|v| v.as_str()).unwrap_or("");
                match kind {
                    "shown" => {
                        if surf == "wifi" {
                            wifi_visible = true;
                        }
                        let val = snapshot_wifi(&wifi_file, &ip_bin, wifi_visible);
                        let _ = push_wifi(&writer, val, &mut last_ser);
                    }
                    "hidden" if surf == "wifi" => {
                        wifi_visible = false;
                    }
                    "panel" => {
                        let on = ev.get("on") == Some(&JsonValue::Bool(true));
                        panel_off.store(!on, Ordering::Relaxed);
                        if on {
                            let val = snapshot_wifi(&wifi_file, &ip_bin, wifi_visible);
                            let _ = push_wifi(&writer, val, &mut last_ser);
                        }
                    }
                    "emit" if surf == "wifi" && ev.get("name").and_then(|v| v.as_str()) == Some("refresh") => {
                        last_ser.clear();
                        let val = snapshot_wifi(&wifi_file, &ip_bin, true);
                        let _ = push_wifi(&writer, val, &mut last_ser);
                    }
                    _ => {}
                }
            }
        }
    }
}
