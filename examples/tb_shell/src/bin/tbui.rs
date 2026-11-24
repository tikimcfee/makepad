//! tbui: the client of tb-shell. One request per line, one reply per line.
//!
//!   tbui show NAME [-f FILE] [--back] [--wait MS]
//!        body from FILE or stdin; --back keeps the front surface; --wait prints the
//!        surface's errors (first render, taps, timers, frame budget) for MS ms, exit 1 if any
//!   tbui reload [VIEW | --all] [--wait MS]
//!        reloads `base.splash` + `ui/lib` + built-in view(s) in place without losing state/front
//!   tbui screen <on|off|toggle|status>
//!        drives or queries Wayland screen power via `tb-screen-power` (`tb-screen.sock`)
//!   tbui save NAME [--title TITLE]
//!        persists an active agent surface in ~/.local/share/tb-shell/screens/ for start restore
//!   tbui unsave NAME
//!        removes a saved screen from ~/.local/share/tb-shell/screens/
//!   tbui close [--unsave] NAME
//!        closes surface NAME (and removes any saved copy with --unsave)
//!   tbui lib ls [--unused [DAYS]]
//!        lists reusable Splash pieces in ~/.local/share/tb-shell/lib/ with line-1 descriptions
//!   tbui lib cat NAME
//!        prints library piece NAME
//!   tbui lib put NAME [-f FILE | -]
//!        validates and saves piece NAME, re-showing active surfaces that reference it
//!   tbui lib rm NAME [--force]
//!        removes piece NAME from the library
//!   tbui set -j JSON | tbui set KEY=JSON... | tbui set -
//!   tbui push NAME [--append] [--clear]
//!   tbui get [KEY...]
//!   tbui watch [--surfaces A,B] [--keys K,L]
//!   tbui ask "QUESTION" CHOICE... [--from TEXT] [--timeout SECS]
//!   tbui guard "TEXT" [--yes LABEL] [--no LABEL]

#[path = "../termd_wire.rs"]
mod termd_wire;

use makepad_micro_serde::{DeJson, JsonValue, SerJson};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::exit;
use std::time::Duration;

fn socket_path() -> String {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let Some(p) = var("TB_SHELL_SOCK") {
        return p;
    }
    if let Some(dir) = var("XDG_RUNTIME_DIR") {
        return format!("{dir}/tb-shell.sock");
    }
    if let Some(uid) = var("SUDO_UID") {
        let p = format!("/run/user/{uid}/tb-shell.sock");
        if std::path::Path::new(&p).exists() {
            return p;
        }
    }
    let found: Vec<String> = std::fs::read_dir("/run/user")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("tb-shell.sock"))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    match found.len() {
        1 => found[0].clone(),
        0 => die("no tb-shell socket: set TB_SHELL_SOCK or XDG_RUNTIME_DIR (none in /run/user/*/)"),
        _ => die(&format!("more than one tb-shell socket ({}): set TB_SHELL_SOCK", found.join(", "))),
    }
}

fn die(msg: &str) -> ! {
    eprintln!("tbui: {msg}");
    exit(1)
}

fn obj(pairs: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<HashMap<_, _>>())
}

fn list(csv: &str) -> JsonValue {
    JsonValue::Array(csv.split(',').filter(|s| !s.is_empty()).map(|s| s.into()).collect())
}

fn parse(text: &str) -> JsonValue {
    JsonValue::deserialize_json(text).unwrap_or_else(|e| die(&format!("bad json {text:?}: {e:?}")))
}

struct Client {
    out: UnixStream,
    lines: std::io::Lines<BufReader<UnixStream>>,
}

impl Client {
    fn connect() -> Client {
        let path = socket_path();
        let out = UnixStream::connect(&path).unwrap_or_else(|e| die(&format!("connect {path}: {e}")));
        let lines = BufReader::new(out.try_clone().unwrap()).lines();
        Client { out, lines }
    }

    fn send(&mut self, req: &JsonValue) {
        let line = req.serialize_json() + "\n";
        self.out.write_all(line.as_bytes()).unwrap_or_else(|e| die(&format!("write: {e}")));
    }

    fn next(&mut self) -> Option<JsonValue> {
        let line = self.lines.next()?.ok()?;
        Some(parse(&line))
    }

    fn call(&mut self, req: JsonValue) -> JsonValue {
        self.send(&req);
        loop {
            let msg = self.next().unwrap_or_else(|| die("the shell closed the connection"));
            if msg.get("ok").is_some() {
                return msg;
            }
        }
    }
}

fn report(reply: &JsonValue) {
    println!("{}", reply.serialize_json());
    if reply.get("ok") != Some(&JsonValue::Bool(true)) {
        exit(1);
    }
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn ask_body(id: &str, question: &str, from: &str, choices: &[String]) -> String {
    let mut buttons = String::new();
    for c in choices {
        let c = quote(c);
        buttons.push_str(&format!(
            "        Big{{text: {c} on_click: || emit(\"answer\", {{id: {id}, choice: {c}}})}}\n",
            id = quote(id)
        ));
    }
    format!(
        "width: Fill height: Fill flow: Down padding: 24 spacing: 16 align: Align{{y: 1.0}}\n\
         main := View{{\n    width: Fill height: Fit flow: Down spacing: 16\n    on_render: || {{\n\
         \x20       Title{{width: Fill text: {q}}}\n\
         \x20       Small{{width: Fill text: {f}}}\n{buttons}    }}\n}}\n",
        q = quote(question),
        f = quote(from)
    )
}

fn ask(args: &[String]) -> ! {
    let mut words = Vec::new();
    let mut from = String::new();
    let mut timeout = 0u64;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--from" => from = it.next().cloned().unwrap_or_default(),
            "--timeout" => timeout = it.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| die("--timeout SECS")),
            _ => words.push(a.clone()),
        }
    }
    if words.len() < 2 {
        die("ask QUESTION CHOICE...");
    }
    let (question, choices) = (words[0].clone(), words[1..].to_vec());
    let id = format!("ask-{}", std::process::id());
    if from.is_empty() {
        from = format!("asked by pid {}", std::os::unix::process::parent_id());
    }
    let mut c = Client::connect();
    report_quiet(c.call(obj(vec![("op", "watch".into()), ("surfaces", list(&id))])));
    let body = ask_body(&id, &question, &from, &choices);
    report_quiet(c.call(obj(vec![
        ("op", "show".into()),
        ("surface", id.as_str().into()),
        ("body", body.into()),
        ("owned", JsonValue::Bool(true)),
    ])));
    if timeout > 0 {
        c.out.set_read_timeout(Some(Duration::from_secs(timeout))).ok();
    }
    let choice = loop {
        let Some(ev) = c.next() else { break None };
        if ev.get("ev").and_then(|v| v.as_str()) == Some("emit") && ev.get("name").and_then(|v| v.as_str()) == Some("answer") {
            break ev.get("data").and_then(|d| d.get("choice")).and_then(|v| v.as_str()).map(str::to_string);
        }
    };
    c.out.set_read_timeout(None).ok();
    c.call(obj(vec![("op", "close".into()), ("surface", id.as_str().into())]));
    match choice {
        Some(choice) => {
            println!("{choice}");
            exit(if choice == choices[0] { 0 } else { 1 })
        }
        None => exit(2),
    }
}

fn report_quiet(reply: JsonValue) {
    if reply.get("ok") != Some(&JsonValue::Bool(true)) {
        eprintln!("{}", reply.serialize_json());
        exit(1);
    }
}

fn format_age(age: Option<u64>) -> String {
    let Some(s) = age else { return "unused".into() };
    if s < 60 {
        format!("used {s}s ago")
    } else if s < 3600 {
        format!("used {}m ago", s / 60)
    } else if s < 86_400 {
        format!("used {}h ago", s / 3600)
    } else {
        format!("used {}d ago", s / 86_400)
    }
}

fn lib_cmd(args: &[String]) {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[args.len().min(1)..];
    match sub {
        "ls" => {
            let mut req = obj(vec![("op", "lib".into()), ("sub", "ls".into())]);
            if let Some(pos) = rest.iter().position(|a| a == "--unused") {
                let days = rest
                    .get(pos + 1)
                    .filter(|s| !s.starts_with('-'))
                    .map(|s| s.trim_end_matches('d').parse::<u64>().unwrap_or_else(|_| die("--unused [DAYS]")))
                    .unwrap_or(30);
                if let JsonValue::Object(m) = &mut req {
                    m.insert("unused_days".into(), JsonValue::U64(days));
                }
            }
            let reply = Client::connect().call(req);
            if reply.get("ok") != Some(&JsonValue::Bool(true)) {
                report(&reply);
            }
            if rest.iter().any(|a| a == "-j") {
                println!("{}", reply.serialize_json());
                return;
            }
            if let Some(arr) = reply.get("pieces").and_then(|v| v.as_array()) {
                for item in arr {
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let desc = item.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    let age = match item.get("age_secs") {
                        Some(JsonValue::U64(n)) => Some(*n),
                        Some(JsonValue::I64(n)) if *n >= 0 => Some(*n as u64),
                        Some(JsonValue::F64(f)) if *f >= 0.0 => Some(*f as u64),
                        _ => None,
                    };
                    if desc.is_empty() {
                        println!("{name}\t({})", format_age(age));
                    } else {
                        println!("{name}\t{desc}\t({})", format_age(age));
                    }
                }
            }
        }
        "cat" => {
            let name = rest.first().unwrap_or_else(|| die("lib cat NAME"));
            let reply = Client::connect().call(obj(vec![("op", "lib".into()), ("sub", "cat".into()), ("name", name.as_str().into())]));
            if reply.get("ok") != Some(&JsonValue::Bool(true)) {
                eprintln!("{}", reply.serialize_json());
                exit(1);
            }
            let body = reply.get("body").and_then(|v| v.as_str()).unwrap_or("");
            print!("{body}");
            if !body.ends_with('\n') {
                println!();
            }
        }
        "put" => {
            let name = rest.first().unwrap_or_else(|| die("lib put NAME [-f FILE | -]"));
            let mut body = String::new();
            match rest.iter().position(|a| a == "-f") {
                Some(i) => {
                    let file = rest.get(i + 1).unwrap_or_else(|| die("-f FILE"));
                    body = std::fs::read_to_string(file).unwrap_or_else(|e| die(&format!("{file}: {e}")));
                }
                None => {
                    std::io::stdin().read_to_string(&mut body).unwrap_or_else(|e| die(&format!("stdin: {e}")));
                }
            }
            let reply = Client::connect().call(obj(vec![
                ("op", "lib".into()),
                ("sub", "put".into()),
                ("name", name.as_str().into()),
                ("body", body.into()),
            ]));
            report(&reply);
        }
        "rm" => {
            let name = rest.iter().find(|a| !a.starts_with('-')).unwrap_or_else(|| die("lib rm NAME [--force]"));
            let force = rest.iter().any(|a| a == "--force");
            let reply = Client::connect().call(obj(vec![
                ("op", "lib".into()),
                ("sub", "rm".into()),
                ("name", name.as_str().into()),
                ("force", JsonValue::Bool(force)),
            ]));
            report(&reply);
        }
        _ => die("lib ls [--unused [DAYS]] | lib cat NAME | lib put NAME [-f FILE | -] | lib rm NAME [--force]"),
    }
}

fn screen_socket_path() -> Option<String> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let Some(p) = var("TB_SCREEN_SOCK") {
        return Some(p);
    }
    if let Some(dir) = var("XDG_RUNTIME_DIR") {
        let p = format!("{dir}/tb-screen.sock");
        if std::path::Path::new(&p).exists() {
            return Some(p);
        }
    }
    if let Some(uid) = var("SUDO_UID") {
        let p = format!("/run/user/{uid}/tb-screen.sock");
        if std::path::Path::new(&p).exists() {
            return Some(p);
        }
    }
    let found: Vec<String> = std::fs::read_dir("/run/user")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("tb-screen.sock"))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    if found.len() == 1 {
        Some(found[0].clone())
    } else {
        var("XDG_RUNTIME_DIR").map(|d| format!("{dir}/tb-screen.sock", dir = d))
    }
}

fn screen_cmd(args: &[String]) -> ! {
    let sub = args.first().map(String::as_str).unwrap_or("status");
    let wire_line = match sub {
        "on" | "off" | "toggle" | "status" => format!("{sub}\n"),
        "inhibit" => {
            let arg = args.get(1).map(String::as_str).unwrap_or("60");
            format!("inhibit {arg}\n")
        }
        _ => die("screen on | off | toggle | status | inhibit [SECS|off]"),
    };
    if let Some(path) = screen_socket_path() {
        if let Ok(mut sock) = UnixStream::connect(&path) {
            let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
            if sock.write_all(wire_line.as_bytes()).is_ok() {
                let mut line = String::new();
                if BufReader::new(sock).read_line(&mut line).is_ok() {
                    let reply = line.trim();
                    if let Some(err) = reply.strip_prefix("ERR:") {
                        die(err.trim());
                    }
                    if !reply.is_empty() {
                        println!("{reply}");
                        exit(0);
                    }
                }
            }
        }
    }
    if sub == "status" {
        let panel_file = std::env::var("TB_SHELL_PANEL_FILE")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/sys/kernel/tinybot_panel/state".into());
        if let Ok(s) = std::fs::read_to_string(&panel_file) {
            let st = if s.trim() == "off" { "off" } else { "on" };
            println!("{st}");
            exit(0);
        }
    }
    die("cannot reach tb-screen-power socket (start tb-screen-power.service or set TB_SCREEN_SOCK)");
}

fn stage_cmd(args: &[String]) {
    let sub = args.first().map(String::as_str).unwrap_or("get");
    let rest = &args[args.len().min(1)..];
    let req = match sub {
        "get" => obj(vec![("op", "stage".into()), ("sub", "get".into())]),
        "stack" | "grid" | "deck" => {
            if rest.is_empty() {
                die(&format!("stage {sub} SPEC..."));
            }
            let specs: Vec<JsonValue> = rest.iter().map(|s| JsonValue::String(s.clone())).collect();
            obj(vec![("op", "stage".into()), ("sub", sub.into()), ("specs", JsonValue::Array(specs))])
        }
        "add" | "pin" => {
            let mut surf: Option<String> = None;
            let mut mode: Option<String> = None;
            let mut px: Option<f64> = None;
            let mut ephemeral = false;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--mode" => mode = Some(it.next().unwrap_or_else(|| die("--mode compact|fill|header")).clone()),
                    "--px" => px = Some(it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| die("--px N"))),
                    "--ephemeral" => ephemeral = true,
                    s if !s.starts_with('-') && surf.is_none() => surf = Some(s.to_string()),
                    _ => die(&format!("stage {sub} NAME [--mode compact|fill|header] [--px N] [--ephemeral]")),
                }
            }
            let name = surf.unwrap_or_else(|| die(&format!("stage {sub} NAME")));
            let mut m = HashMap::new();
            m.insert("op".to_string(), "stage".into());
            m.insert("sub".to_string(), sub.into());
            m.insert("surface".to_string(), name.into());
            if let Some(md) = mode {
                m.insert("mode".to_string(), md.into());
            }
            if let Some(p) = px {
                m.insert("px".to_string(), JsonValue::F64(p));
            }
            if ephemeral {
                m.insert("ephemeral".to_string(), JsonValue::Bool(true));
            }
            JsonValue::Object(m)
        }
        "rm" => {
            let name = rest.first().unwrap_or_else(|| die("stage rm NAME"));
            obj(vec![("op", "stage".into()), ("sub", "rm".into()), ("surface", name.as_str().into())])
        }
        "unpin" => {
            let ephemeral_only = rest.iter().any(|a| a == "--ephemeral-only");
            let name = rest.iter().find(|a| !a.starts_with('-')).unwrap_or_else(|| die("stage unpin NAME [--ephemeral-only]"));
            obj(vec![
                ("op", "stage".into()),
                ("sub", "unpin".into()),
                ("surface", name.as_str().into()),
                ("ephemeral_only", JsonValue::Bool(ephemeral_only)),
            ])
        }
        "solo" | "focus" => {
            let clear_pins = rest.iter().any(|a| a == "--clear-pins");
            let name = rest.iter().find(|a| !a.starts_with('-')).unwrap_or_else(|| die(&format!("stage {sub} NAME")));
            obj(vec![
                ("op", "stage".into()),
                ("sub", sub.into()),
                ("surface", name.as_str().into()),
                ("clear_pins", JsonValue::Bool(clear_pins)),
            ])
        }
        "next" | "prev" => obj(vec![("op", "stage".into()), ("sub", sub.into())]),
        "float" => {
            const USAGE: &str = "stage float NAME [--z N] [--place top|center|bottom] [--inset L,T,R,B] [--width fill|PX|N%] \
[--max-h PX|N%] [--hit card|all|none] [--when PATH,PATH] [--hold MS] [--motion slide|slide-top|slide-bottom|none] \
[--pull top] [--dismiss outside]";
            let mut m = HashMap::new();
            m.insert("op".to_string(), JsonValue::from("stage"));
            m.insert("sub".to_string(), JsonValue::from("float"));
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                let key = match a.as_str() {
                    "--z" => "z",
                    "--place" => "place",
                    "--inset" => "inset",
                    "--width" => "width",
                    "--max-h" => "max_h",
                    "--hit" => "hit",
                    "--when" => "when",
                    "--hold" => "hold_ms",
                    "--motion" => "motion",
                    "--pull" => "pull",
                    "--dismiss" => "dismiss",
                    s if !s.starts_with('-') && !m.contains_key("surface") => {
                        m.insert("surface".to_string(), JsonValue::from(s));
                        continue;
                    }
                    _ => die(USAGE),
                };
                let v = it.next().unwrap_or_else(|| die(USAGE));
                m.insert(key.to_string(), JsonValue::from(v.as_str()));
            }
            if !m.contains_key("surface") {
                die(USAGE);
            }
            JsonValue::Object(m)
        }
        "unfloat" => {
            let name = rest.first().unwrap_or_else(|| die("stage unfloat NAME"));
            obj(vec![("op", "stage".into()), ("sub", "unfloat".into()), ("surface", name.as_str().into())])
        }
        "layer" => {
            let (Some(name), Some(act)) = (rest.first(), rest.get(1)) else { die("stage layer NAME show|hide") };
            let show = match act.as_str() {
                "show" => true,
                "hide" => false,
                _ => die("stage layer NAME show|hide"),
            };
            obj(vec![
                ("op", "stage".into()),
                ("sub", "layer".into()),
                ("surface", name.as_str().into()),
                ("show", JsonValue::Bool(show)),
            ])
        }
        "save" => {
            let name = rest.iter().find(|a| !a.starts_with('-')).unwrap_or_else(|| die("stage save NAME [--title TITLE]"));
            let mut req = obj(vec![("op", "stage".into()), ("sub", "save".into()), ("name", name.as_str().into())]);
            if let Some(i) = rest.iter().position(|a| a == "--title") {
                let title = rest.get(i + 1).unwrap_or_else(|| die("--title TITLE"));
                if let JsonValue::Object(m) = &mut req {
                    m.insert("title".into(), title.as_str().into());
                }
            }
            req
        }
        "open" | "unsave" => {
            let name = rest.first().unwrap_or_else(|| die(&format!("stage {sub} NAME")));
            obj(vec![("op", "stage".into()), ("sub", sub.into()), ("name", name.as_str().into())])
        }
        _ => die("stage [stack|grid|deck|add|rm|pin|unpin|solo|focus|next|prev|float|unfloat|layer|save|open|unsave] ..."),
    };
    report(&Client::connect().call(req));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[args.len().min(1)..];
    match cmd {
        "show" => {
            let name = rest.first().unwrap_or_else(|| die("show NAME [-f FILE] [--back]"));
            let mut body = String::new();
            match rest.iter().position(|a| a == "-f") {
                Some(i) => {
                    let file = rest.get(i + 1).unwrap_or_else(|| die("-f FILE"));
                    body = std::fs::read_to_string(file).unwrap_or_else(|e| die(&format!("{file}: {e}")));
                }
                None => {
                    std::io::stdin().read_to_string(&mut body).unwrap_or_else(|e| die(&format!("stdin: {e}")));
                }
            }
            let mut req = obj(vec![("op", "show".into()), ("surface", name.as_str().into()), ("body", body.into())]);
            if rest.iter().any(|a| a == "--back") {
                if let JsonValue::Object(m) = &mut req {
                    m.insert("front".into(), JsonValue::Bool(false));
                }
            }
            let wait = rest.iter().position(|a| a == "--wait").map(|i| {
                rest.get(i + 1).and_then(|v| v.parse::<u64>().ok()).unwrap_or_else(|| die("--wait MS"))
            });
            let mut c = Client::connect();
            if wait.is_some() {
                report_quiet(c.call(obj(vec![("op", "watch".into()), ("surfaces", list(name))])));
            }
            let reply = c.call(req);
            report(&reply);
            if let Some(ms) = wait {
                c.out.set_read_timeout(Some(Duration::from_millis(ms.max(1)))).ok();
                let mut errors = 0;
                while let Some(ev) = c.next() {
                    if ev.get("ev").and_then(|v| v.as_str()) == Some("error") {
                        println!("{}", ev.serialize_json());
                        errors += 1;
                    }
                }
                if errors > 0 {
                    exit(1);
                }
            }
        }
        "reload" => {
            let mut target_view: Option<String> = None;
            let mut wait: Option<u64> = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--all" => target_view = None,
                    "--wait" => {
                        wait = Some(it.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or_else(|| die("--wait MS")));
                    }
                    v if !v.starts_with('-') => target_view = Some(v.to_string()),
                    _ => die("reload [VIEW | --all] [--wait MS]"),
                }
            }
            let mut req = obj(vec![("op", "reload".into())]);
            if let Some(ref v) = target_view {
                if let JsonValue::Object(m) = &mut req {
                    m.insert("view".into(), v.as_str().into());
                }
            }
            let mut c = Client::connect();
            if wait.is_some() {
                let mut wreq = obj(vec![("op", "watch".into())]);
                if let Some(ref v) = target_view {
                    if let JsonValue::Object(m) = &mut wreq {
                        m.insert("surfaces".into(), list(v));
                    }
                }
                report_quiet(c.call(wreq));
            }
            let reply = c.call(req);
            report(&reply);
            if let Some(ms) = wait {
                c.out.set_read_timeout(Some(Duration::from_millis(ms.max(1)))).ok();
                let mut errors = 0;
                while let Some(ev) = c.next() {
                    if ev.get("ev").and_then(|v| v.as_str()) == Some("error") {
                        println!("{}", ev.serialize_json());
                        errors += 1;
                    }
                }
                if errors > 0 {
                    exit(1);
                }
            }
        }
        "screen" => screen_cmd(rest),
        "stage" => stage_cmd(rest),
        "termd" => termd_cmd(rest),
        "note" => {
            let mut text = String::new();
            let mut secs = 10.0f64;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                if a == "--secs" {
                    secs = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| die("--secs N"));
                } else if text.is_empty() {
                    text = a.clone();
                } else {
                    text.push(' ');
                    text.push_str(a);
                }
            }
            report(&Client::connect().call(obj(vec![
                ("op", "note".into()),
                ("text", text.as_str().into()),
                ("secs", JsonValue::F64(secs)),
            ])));
        }
        "save" => {
            let name = rest.iter().find(|a| !a.starts_with('-')).unwrap_or_else(|| die("save NAME [--title TITLE]"));
            let mut req = obj(vec![("op", "save".into()), ("surface", name.as_str().into())]);
            if let Some(i) = rest.iter().position(|a| a == "--title") {
                let title = rest.get(i + 1).unwrap_or_else(|| die("--title TITLE"));
                if let JsonValue::Object(m) = &mut req {
                    m.insert("title".into(), title.as_str().into());
                }
            }
            report(&Client::connect().call(req));
        }
        "unsave" => {
            let name = rest.first().unwrap_or_else(|| die("unsave NAME"));
            report(&Client::connect().call(obj(vec![("op", "unsave".into()), ("surface", name.as_str().into())])));
        }
        "close" => {
            let unsave = rest.iter().any(|a| a == "--unsave");
            let name = rest.iter().find(|a| !a.starts_with('-')).unwrap_or_else(|| die("close [--unsave] NAME"));
            let mut req = obj(vec![("op", "close".into()), ("surface", name.as_str().into())]);
            if unsave {
                if let JsonValue::Object(m) = &mut req {
                    m.insert("unsave".into(), JsonValue::Bool(true));
                }
            }
            report(&Client::connect().call(req));
        }
        "lib" => lib_cmd(rest),
        "push" => {
            let name = rest.first().unwrap_or_else(|| die("push NAME [--append] (values on stdin)"));
            let append = if rest.iter().any(|a| a == "--append") { ",\"append\":true" } else { "" };
            let mut clear = rest.iter().any(|a| a == "--clear");
            let mut c = Client::connect();
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let values: Vec<&str> = line.split(|ch: char| ch == ',' || ch.is_whitespace()).filter(|v| !v.is_empty()).collect();
                if values.iter().any(|v| v.parse::<f64>().is_err()) {
                    die(&format!("not numbers: {line}"));
                }
                let cl = if std::mem::take(&mut clear) { ",\"clear\":true" } else { "" };
                let req = format!(
                    "{{\"op\":\"push\",\"stream\":{},\"values\":[{}]{append}{cl}}}\n",
                    JsonValue::String(name.clone()).serialize_json(),
                    values.join(",")
                );
                c.out.write_all(req.as_bytes()).unwrap_or_else(|e| die(&format!("write: {e}")));
            }
        }
        "set" if rest.first().map(String::as_str) == Some("-") => {
            let mut c = Client::connect();
            let mut failed = false;
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                c.out.write_all(format!("{{\"op\":\"set\",\"patch\":{}}}\n", line.trim()).as_bytes()).unwrap_or_else(|e| die(&format!("write: {e}")));
                let reply = loop {
                    let msg = c.next().unwrap_or_else(|| die("the shell closed the connection"));
                    if msg.get("ok").is_some() {
                        break msg;
                    }
                };
                if reply.get("ok") != Some(&JsonValue::Bool(true)) {
                    eprintln!("{}", reply.serialize_json());
                    failed = true;
                }
            }
            exit(failed as i32)
        }
        "set" => {
            let patch = if rest.first().map(String::as_str) == Some("-j") {
                parse(rest.get(1).unwrap_or_else(|| die("set -j JSON")))
            } else {
                let mut m = HashMap::new();
                for kv in rest {
                    let (k, v) = kv.split_once('=').unwrap_or_else(|| die("set KEY=JSON..."));
                    let v = JsonValue::deserialize_json(v).unwrap_or_else(|_| v.into());
                    m.insert(k.to_string(), v);
                }
                JsonValue::Object(m)
            };
            report(&Client::connect().call(obj(vec![("op", "set".into()), ("patch", patch)])));
        }
        "get" => {
            let mut req = obj(vec![("op", "get".into())]);
            if !rest.is_empty() {
                if let JsonValue::Object(m) = &mut req {
                    m.insert("keys".into(), JsonValue::Array(rest.iter().map(|k| k.as_str().into()).collect()));
                }
            }
            report(&Client::connect().call(req));
        }
        "watch" => {
            let mut req = obj(vec![("op", "watch".into())]);
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                let key = match a.as_str() {
                    "--surfaces" => "surfaces",
                    "--keys" => "keys",
                    _ => die("watch [--surfaces A,B] [--keys K,L]"),
                };
                let v = list(it.next().unwrap_or_else(|| die("a list after the flag")));
                if let JsonValue::Object(m) = &mut req {
                    m.insert(key.into(), v);
                }
            }
            let mut c = Client::connect();
            report_quiet(c.call(req));
            let stdout = std::io::stdout();
            while let Some(ev) = c.next() {
                let mut out = stdout.lock();
                if writeln!(out, "{}", ev.serialize_json()).and_then(|_| out.flush()).is_err() {
                    break;
                }
            }
        }
        "jit" => {
            let name = rest.first().unwrap_or_else(|| die("jit NAME PACKAGE [--dir DIR] [--src STORE] [--fps N] [--back]"));
            let pkg = rest.get(1).unwrap_or_else(|| die("jit NAME PACKAGE [--dir DIR] [--src STORE] [--fps N] [--back]"));
            let mut dir: Option<String> = None;
            let mut src_store: Option<String> = None;
            let mut fps: f64 = 24.0;
            let mut front = true;
            let mut i = 2;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--dir" => {
                        let Some(v) = rest.get(i + 1) else { die("--dir needs a directory") };
                        dir = Some(v.clone());
                        i += 2;
                    }
                    "--src" => {
                        let Some(v) = rest.get(i + 1) else { die("--src needs a store path") };
                        src_store = Some(v.clone());
                        i += 2;
                    }
                    "--fps" => {
                        let Some(v) = rest.get(i + 1).and_then(|s| s.parse::<f64>().ok()) else { die("--fps needs a number") };
                        fps = v;
                        i += 2;
                    }
                    "--back" => {
                        front = false;
                        i += 1;
                    }
                    other => die(&format!("unknown jit flag: {other}")),
                }
            }
            let mut fields = vec![
                ("op", "jit".into()),
                ("surface", name.as_str().into()),
                ("package", pkg.as_str().into()),
                ("fps", JsonValue::F64(fps)),
                ("front", JsonValue::Bool(front)),
            ];
            if let Some(d) = &dir {
                fields.push(("dir", d.as_str().into()));
            }
            if let Some(s) = &src_store {
                fields.push(("src", s.as_str().into()));
            }
            report(&Client::connect().call(obj(fields)));
        }
        "ask" => ask(rest),
        "confirm" | "guard" => {
            let text = rest.first().unwrap_or_else(|| die("confirm TEXT [--yes LABEL] [--no LABEL]"));
            let mut req = obj(vec![("op", "confirm".into()), ("text", text.as_str().into())]);
            for (flag, key) in [("--yes", "yes"), ("--no", "no")] {
                if let Some(i) = rest.iter().position(|a| a == flag) {
                    let v = rest.get(i + 1).unwrap_or_else(|| die("a label after the flag"));
                    if let JsonValue::Object(m) = &mut req {
                        m.insert(key.into(), v.as_str().into());
                    }
                }
            }
            let mut c = Client::connect();
            c.send(&req);
            match c.next() {
                Some(reply) => {
                    println!("{}", reply.serialize_json());
                    match reply.get("answer").and_then(|a| a.as_str()) {
                        Some("yes") => exit(0),
                        Some(_) => exit(1),
                        None => exit(4),
                    }
                }
                None => {
                    eprintln!("tbui: confirm went away unanswered");
                    exit(3)
                }
            }
        }
        _ => die("show | jit | termd | stage | note | reload | screen | save | unsave | close | lib | set | push | get | watch | ask | confirm"),
    }
}

fn try_shell_client() -> Option<Client> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let cand = if let Some(p) = var("TB_SHELL_SOCK") {
        Some(p)
    } else if let Some(dir) = var("XDG_RUNTIME_DIR") {
        Some(format!("{dir}/tb-shell.sock"))
    } else {
        std::fs::read_dir("/run/user")
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().join("tb-shell.sock"))
            .find(|p| p.exists())
            .map(|p| p.to_string_lossy().to_string())
    }?;
    let out = UnixStream::connect(&cand).ok()?;
    let _ = out.set_read_timeout(Some(Duration::from_secs(2)));
    let lines = BufReader::new(out.try_clone().ok()?).lines();
    Some(Client { out, lines })
}

fn resolve_active_target(
    explicit_ns: Option<String>,
    explicit_id: Option<String>,
) -> (String, Option<String>) {
    if explicit_ns.is_some() && explicit_id.is_some() {
        return (explicit_ns.unwrap(), explicit_id);
    }
    if let Some(mut c) = try_shell_client() {
        let rep = c.call(obj(vec![("op", "get".into()), ("keys", list("termd"))]));
        if let Some(active) = rep
            .get("state")
            .and_then(|s| s.get("termd"))
            .and_then(|t| t.get("active"))
        {
            let sh_ns = active.get("ns").and_then(|v| v.as_str()).unwrap_or("");
            let sh_id = active.get("id").and_then(|v| v.as_str()).unwrap_or("");
            match (&explicit_ns, &explicit_id) {
                (None, None) if !sh_id.is_empty() => {
                    return (
                        if sh_ns.is_empty() { "default".into() } else { sh_ns.into() },
                        Some(sh_id.into()),
                    );
                }
                (Some(ns), None) if ns == sh_ns && !sh_id.is_empty() => {
                    return (ns.clone(), Some(sh_id.into()));
                }
                _ => {}
            }
        }
    }
    (explicit_ns.unwrap_or_else(|| "default".into()), explicit_id)
}

fn termd_cmd(args: &[String]) {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[args.len().min(1)..];
    match sub {
        "create" => {
            let mut ns = "default".to_string();
            let mut id: Option<String> = None;
            let mut cwd: Option<String> = None;
            let mut cmd: Option<String> = None;
            let mut cols: u16 = 48;
            let mut rows: u16 = 32;
            let mut focus = false;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--ns" => {
                        ns = rest.get(i + 1).cloned().unwrap_or_else(|| die("--ns NS"));
                        i += 2;
                    }
                    "--id" => {
                        id = Some(rest.get(i + 1).cloned().unwrap_or_else(|| die("--id ID")));
                        i += 2;
                    }
                    "--cwd" => {
                        cwd = Some(rest.get(i + 1).cloned().unwrap_or_else(|| die("--cwd DIR")));
                        i += 2;
                    }
                    "--cmd" => {
                        cmd = Some(rest.get(i + 1).cloned().unwrap_or_else(|| die("--cmd CMD")));
                        i += 2;
                    }
                    "--cols" => {
                        cols = rest
                            .get(i + 1)
                            .and_then(|v| v.parse().ok())
                            .unwrap_or_else(|| die("--cols N"));
                        i += 2;
                    }
                    "--rows" => {
                        rows = rest
                            .get(i + 1)
                            .and_then(|v| v.parse().ok())
                            .unwrap_or_else(|| die("--rows N"));
                        i += 2;
                    }
                    "--focus" => {
                        focus = true;
                        i += 1;
                    }
                    other => die(&format!("termd create: unknown flag {other}")),
                }
            }
            match termd_wire::create_terminal(
                &ns,
                id.as_deref(),
                cwd.as_deref(),
                cmd.as_deref(),
                cols,
                rows,
            ) {
                Ok(rep) => {
                    let created_id = rep
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if let Some(mut c) = try_shell_client() {
                        let _ = c.call(obj(vec![
                            ("op", "termd".into()),
                            ("sub", "activated".into()),
                            ("ns", ns.as_str().into()),
                            ("id", created_id.as_str().into()),
                            ("focus", JsonValue::Bool(focus)),
                        ]));
                    }
                    report(&rep);
                }
                Err(e) => die(&format!("termd create: {e}")),
            }
        }
        "paste" => {
            let mut ns_opt: Option<String> = None;
            let mut id_opt: Option<String> = None;
            let mut enter = false;
            let mut allow_unsafe = false;
            let mut positional: Vec<String> = Vec::new();
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--ns" => {
                        ns_opt = Some(rest.get(i + 1).cloned().unwrap_or_else(|| die("--ns NS")));
                        i += 2;
                    }
                    "--id" => {
                        id_opt = Some(rest.get(i + 1).cloned().unwrap_or_else(|| die("--id ID")));
                        i += 2;
                    }
                    "--enter" => {
                        enter = true;
                        i += 1;
                    }
                    "--allow-unsafe" | "--force" => {
                        allow_unsafe = true;
                        i += 1;
                    }
                    "--" => {
                        positional.extend_from_slice(&rest[i + 1..]);
                        break;
                    }
                    a if a.starts_with('-') && a != "-" => {
                        die(&format!("termd paste: unknown flag {a}"));
                    }
                    a => {
                        positional.push(a.to_string());
                        i += 1;
                    }
                }
            }
            let text = if positional.is_empty() || (positional.len() == 1 && positional[0] == "-") {
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .unwrap_or_else(|e| die(&format!("stdin: {e}")));
                buf
            } else {
                positional.join(" ")
            };
            let (ns, resolved_id_opt) = resolve_active_target(ns_opt, id_opt);
            match termd_wire::paste_terminal(
                &ns,
                resolved_id_opt.as_deref(),
                &text,
                enter,
                allow_unsafe,
            ) {
                Ok((id, termd_wire::PasteOutcome::Written { written, entered })) => {
                    report(&obj(vec![
                        ("ok", JsonValue::Bool(true)),
                        ("ns", ns.as_str().into()),
                        ("id", id.as_str().into()),
                        ("written", JsonValue::U64(written)),
                        ("entered", JsonValue::Bool(entered)),
                    ]));
                }
                Ok((id, termd_wire::PasteOutcome::UnsafePaste { lines, preview })) => {
                    let mut staged = false;
                    if let Some(mut c) = try_shell_client() {
                        let rep = c.call(obj(vec![
                            ("op", "termd".into()),
                            ("sub", "stage_paste".into()),
                            ("ns", ns.as_str().into()),
                            ("id", id.as_str().into()),
                            ("text", text.as_str().into()),
                            ("enter", JsonValue::Bool(enter)),
                        ]));
                        staged = rep.get("ok") == Some(&JsonValue::Bool(true));
                    }
                    println!(
                        "{}",
                        obj(vec![
                            ("ok", JsonValue::Bool(false)),
                            ("code", "unsafe_paste".into()),
                            ("staged", JsonValue::Bool(staged)),
                            ("ns", ns.as_str().into()),
                            ("id", id.as_str().into()),
                            ("lines", JsonValue::U64(lines as u64)),
                            ("preview", preview.as_str().into()),
                        ])
                        .serialize_json()
                    );
                    exit(2);
                }
                Err(e) => die(&format!("termd paste: {e}")),
            }
        }
        "key" => {
            let mut ns_opt: Option<String> = None;
            let mut id_opt: Option<String> = None;
            let mut ctrl = false;
            let mut alt = false;
            let mut shift = false;
            let mut key_spec: Option<String> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--ns" => {
                        ns_opt = Some(rest.get(i + 1).cloned().unwrap_or_else(|| die("--ns NS")));
                        i += 2;
                    }
                    "--id" => {
                        id_opt = Some(rest.get(i + 1).cloned().unwrap_or_else(|| die("--id ID")));
                        i += 2;
                    }
                    "--ctrl" => {
                        ctrl = true;
                        i += 1;
                    }
                    "--alt" => {
                        alt = true;
                        i += 1;
                    }
                    "--shift" => {
                        shift = true;
                        i += 1;
                    }
                    a if a.starts_with('-') => die(&format!("termd key: unknown flag {a}")),
                    a => {
                        key_spec = Some(a.to_string());
                        i += 1;
                    }
                }
            }
            let spec = key_spec.unwrap_or_else(|| {
                die("termd key <KEY_SPEC> [--ctrl] [--alt] [--shift] [--ns NS] [--id ID]")
            });
            let (ns, resolved_id_opt) = resolve_active_target(ns_opt, id_opt);
            match termd_wire::send_key_terminal(
                &ns,
                resolved_id_opt.as_deref(),
                &spec,
                ctrl,
                alt,
                shift,
            ) {
                Ok((id, written)) => {
                    report(&obj(vec![
                        ("ok", JsonValue::Bool(true)),
                        ("ns", ns.as_str().into()),
                        ("id", id.as_str().into()),
                        ("key", spec.as_str().into()),
                        ("written", JsonValue::U64(written)),
                    ]));
                }
                Err(e) => die(&format!("termd key: {e}")),
            }
        }
        "kill" => {
            let mut ns_opt: Option<String> = None;
            let mut id_opt: Option<String> = None;
            let mut sig_opt: Option<String> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--ns" => {
                        ns_opt = Some(rest.get(i + 1).cloned().unwrap_or_else(|| die("--ns NS")));
                        i += 2;
                    }
                    "--id" => {
                        id_opt = Some(rest.get(i + 1).cloned().unwrap_or_else(|| die("--id ID")));
                        i += 2;
                    }
                    "--signal" => {
                        sig_opt =
                            Some(rest.get(i + 1).cloned().unwrap_or_else(|| die("--signal SIG")));
                        i += 2;
                    }
                    a if !a.starts_with('-') && id_opt.is_none() => {
                        id_opt = Some(a.to_string());
                        i += 1;
                    }
                    other => die(&format!("termd kill: unknown arg {other}")),
                }
            }
            let (ns, resolved_id_opt) = resolve_active_target(ns_opt, id_opt);
            match termd_wire::kill_terminal(&ns, resolved_id_opt.as_deref(), sig_opt.as_deref()) {
                Ok(id) => {
                    if let Some(mut c) = try_shell_client() {
                        let _ = c.call(obj(vec![("op", "termd".into()), ("sub", "probe".into())]));
                    }
                    report(&obj(vec![
                        ("ok", JsonValue::Bool(true)),
                        ("ns", ns.as_str().into()),
                        ("killed", id.as_str().into()),
                    ]));
                }
                Err(e) => die(&format!("termd kill: {e}")),
            }
        }
        "list" | "ls" => {
            let mut ns_filter: Option<String> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--ns" => {
                        ns_filter =
                            Some(rest.get(i + 1).cloned().unwrap_or_else(|| die("--ns NS")));
                        i += 2;
                    }
                    other => die(&format!("termd list: unknown flag {other}")),
                }
            }
            let namespaces: Vec<String> = match ns_filter {
                Some(ns) => vec![ns],
                None => vec!["default".into(), "agents".into()],
            };
            let mut out = Vec::new();
            for ns in namespaces {
                if let Ok(list) = termd_wire::list_terminals(&ns) {
                    for mut item in list {
                        if let JsonValue::Object(ref mut m) = item {
                            m.insert("ns".into(), JsonValue::String(ns.clone()));
                        }
                        out.push(item);
                    }
                }
            }
            report(&obj(vec![
                ("ok", JsonValue::Bool(true)),
                ("terminals", JsonValue::Array(out)),
            ]));
        }
        "focus" => {
            let mut ns = "default".to_string();
            let mut id_opt: Option<String> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--ns" => {
                        ns = rest.get(i + 1).cloned().unwrap_or_else(|| die("--ns NS"));
                        i += 2;
                    }
                    a if !a.starts_with('-') => {
                        id_opt = Some(a.to_string());
                        i += 1;
                    }
                    other => die(&format!("termd focus: unknown flag {other}")),
                }
            }
            let id = id_opt.unwrap_or_else(|| die("termd focus [--ns NS] <ID>"));
            report(&Client::connect().call(obj(vec![
                ("op", "termd".into()),
                ("sub", "activated".into()),
                ("ns", ns.as_str().into()),
                ("id", id.as_str().into()),
                ("focus", JsonValue::Bool(true)),
            ])));
        }
        _ => die("termd <create|paste|key|kill|list|focus> ..."),
    }
}
