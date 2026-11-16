//! tbui: the client of tb-shell. One request per line, one reply per line.
//!
//!   tbui show NAME [-f FILE] [--back] [--wait MS]
//!        body from FILE or stdin; --back keeps the front surface; --wait prints the
//!        surface's errors (first render, taps, timers, frame budget) for MS ms, exit 1 if any
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
    extern "C" {
        fn geteuid() -> u32;
    }
    if unsafe { geteuid() } == 0 || std::env::var("TBUI_ASK_GUARD").map_or(false, |v| v == "1") {
        if choices.len() != 2 {
            die("a root ask is the guard: exactly two choices, yes then no");
        }
        let mut c = Client::connect();
        c.send(&obj(vec![
            ("op", "guard".into()),
            ("text", question.as_str().into()),
            ("yes", choices[0].as_str().into()),
            ("no", choices[1].as_str().into()),
        ]));
        if timeout > 0 {
            c.out.set_read_timeout(Some(Duration::from_secs(timeout))).ok();
        }
        match c.next() {
            Some(reply) if reply.get("ok") == Some(&JsonValue::Bool(true)) => {
                let choice = reply.get("choice").and_then(|v| v.as_str()).unwrap_or("").to_string();
                println!("{choice}");
                exit(if reply.get("answer").and_then(|v| v.as_str()) == Some("yes") { 0 } else { 1 })
            }
            Some(reply) => {
                eprintln!("{}", reply.serialize_json());
                exit(4)
            }
            None if timeout > 0 => exit(2),
            None => {
                eprintln!("tbui: the guard went away unanswered");
                exit(3)
            }
        }
    }
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
        "ask" => ask(rest),
        "guard" => {
            let text = rest.first().unwrap_or_else(|| die("guard TEXT [--yes LABEL] [--no LABEL]"));
            let mut req = obj(vec![("op", "guard".into()), ("text", text.as_str().into())]);
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
                    eprintln!("tbui: the guard went away unanswered");
                    exit(3)
                }
            }
        }
        _ => die("show | save | unsave | close | lib | set | push | get | watch | ask | guard"),
    }
}
