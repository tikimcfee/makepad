//! tbui: the client of tb-shell. One request per line, one reply per line.
//!
//!   tbui show NAME [-f FILE] [--back] [--wait MS]
//!        body from FILE or stdin; --back keeps the front surface; --wait prints the
//!        surface's script errors (its first render, taps, timers) for MS ms, exit 1 if any
//!   tbui set -j JSON | tbui set KEY=JSON...
//!   tbui set -        one connection; each line of stdin is a patch (for a service: no
//!                     process per update); exit 1 if any was refused
//!   tbui push NAME    one connection; each line of stdin is the values of stream NAME
//!                     (numbers, comma or space separated), for StreamView; no replies
//!   tbui get [KEY...]
//!   tbui watch [--surfaces A,B] [--keys K,L]
//!   tbui close NAME
//!   tbui ask "QUESTION" CHOICE... [--from TEXT] [--timeout SECS]
//!        prints the choice; exit 0 for the first choice, 1 for another, 2 on timeout.
//!        Run as root it is the shell's GUARD (two choices: the first is yes, the second no):
//!        drawn above everything, input to the views gated, the asker named by the kernel;
//!        exit 3 when it went away unanswered. From any other user it is an ordinary view.
//!   tbui guard "TEXT" [--yes LABEL] [--no LABEL]
//!        the shell's own guard (a uid it allows, root by default); waits for a person's tap;
//!        exit 0 yes, 1 no, 3 when the guard went away unanswered (the shell ended)

use makepad_micro_serde::{DeJson, JsonValue, SerJson};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::exit;
use std::time::Duration;

fn socket_path() -> String {
    if let Ok(p) = std::env::var("TB_SHELL_SOCK") {
        return p;
    }
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    format!("{dir}/tb-shell.sock")
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

    /// The next line, or None at end of stream or on a read timeout.
    fn next(&mut self) -> Option<JsonValue> {
        let line = self.lines.next()?.ok()?;
        Some(parse(&line))
    }

    /// Sends a request and returns its reply (events before it are skipped).
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

/// Prints a reply; a refusal exits 1.
fn report(reply: &JsonValue) {
    println!("{}", reply.serialize_json());
    if reply.get("ok") != Some(&JsonValue::Bool(true)) {
        exit(1);
    }
}

/// A Splash string literal holding `s`.
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
    // Root asks through the guard; TBUI_ASK_GUARD=1 does it for a test (the shell still
    // refuses a guard from a uid it does not allow).
    extern "C" {
        fn geteuid() -> u32;
    }
    if unsafe { geteuid() } == 0 || std::env::var("TBUI_ASK_GUARD").map_or(false, |v| v == "1") {
        if choices.len() != 2 {
            die("a root ask is the guard: exactly two choices, yes then no");
        }
        let mut c = Client::connect();
        c.send(&obj(vec![("op", "guard".into()), ("text", question.as_str().into()), ("yes", choices[0].as_str().into()), ("no", choices[1].as_str().into())]));
        if timeout > 0 {
            c.out.set_read_timeout(Some(Duration::from_secs(timeout))).ok();
        }
        // On a timeout the connection closes with this process, and the guard goes with it.
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
    // Watch first, so the answer cannot arrive before we listen for it.
    report_quiet(c.call(obj(vec![("op", "watch".into()), ("surfaces", list(&id))])));
    let body = ask_body(&id, &question, &from, &choices);
    // Owned: if this process ends without closing it, the shell closes it.
    report_quiet(c.call(obj(vec![("op", "show".into()), ("surface", id.as_str().into()), ("body", body.into()), ("owned", JsonValue::Bool(true))])));
    if timeout > 0 {
        c.out.set_read_timeout(Some(Duration::from_secs(timeout))).ok();
    }
    let choice = loop {
        let Some(ev) = c.next() else { break None };
        if ev.get("ev").and_then(|v| v.as_str()) == Some("emit")
            && ev.get("name").and_then(|v| v.as_str()) == Some("answer")
        {
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
                // Errors after the reply: the first render runs after it.
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
        "push" => {
            let name = rest.first().unwrap_or_else(|| die("push NAME (values on stdin)"));
            let mut c = Client::connect();
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let values: Vec<&str> = line.split(|ch: char| ch == ',' || ch.is_whitespace()).filter(|v| !v.is_empty()).collect();
                if values.iter().any(|v| v.parse::<f64>().is_err()) {
                    die(&format!("not numbers: {line}"));
                }
                let req = format!("{{\"op\":\"push\",\"stream\":{},\"values\":[{}]}}\n", JsonValue::String(name.clone()).serialize_json(), values.join(","));
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
                // The shell parses and checks the patch; the client only frames it.
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
                    // A value that is not JSON is taken as a string.
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
        "close" => {
            let name = rest.first().unwrap_or_else(|| die("close NAME"));
            report(&Client::connect().call(obj(vec![("op", "close".into()), ("surface", name.as_str().into())])));
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
                        None => exit(4), // refused
                    }
                }
                None => {
                    eprintln!("tbui: the guard went away unanswered");
                    exit(3)
                }
            }
        }
        _ => die("show | set | get | watch | close | ask   (see the top of src/bin/tbui.rs)"),
    }
}
