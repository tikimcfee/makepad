//! tb-surface: the agent's side of tinybot's live surface. One call per turn, nothing held open.
//!
//!   tb-surface set FILE [--surface ID] [--rev N]   show a surface (FILE: a JSON object, - for stdin)
//!   tb-surface wait [--timeout S]                  print the waiting taps, or block for the next one
//!   tb-surface clear                               back to the idle line
//!   tb-surface status                              what is shown, how many taps wait
//!
//! This is `tb-surface.py` as a binary, for an image without python3: the same commands,
//! options, output lines and exit codes. Standard library only: nothing of the package's
//! Makepad dependency is named here, so none of it is linked in. The protocol is in the README;
//! the app's side of it is `wire.rs` and `socket.rs`.

#[cfg(unix)]
fn main() {
    std::process::exit(unix::main());
}

#[cfg(not(unix))]
fn main() {
    eprintln!("tb-surface: the surface is reached over a Unix socket");
    std::process::exit(3);
}

#[cfg(unix)]
mod unix {
    use std::io::{self, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// The protocol version this tool speaks (`v` in the app's `hello`).
    const VERSION: u64 = 1;

    const USAGE: &str = "usage: tb-surface [-h] [--socket PATH] {set,wait,clear,status} ...\n";
    const HELP: &str = "\
usage: tb-surface [-h] [--socket PATH] {set,wait,clear,status} ...

tinybot's live surface, one call per turn

  tb-surface set FILE [--surface ID] [--rev N]   show a surface (FILE: a JSON object, - for stdin)
  tb-surface wait [--timeout S]                  print the waiting taps, or block for the next one
  tb-surface clear                               back to the idle line
  tb-surface status                              what is shown, how many taps wait

options:
  -h, --help     this text
  --socket PATH  the surface's socket (default: $TINYBOT_SURFACE_SOCK, then
                 $XDG_RUNTIME_DIR/tinybot/surface.sock)
  --surface ID   set: the surface id, over the one in FILE
  --rev N        set: the rev, over the one in FILE
  --timeout S    wait: give up after S seconds (0: only what waits now; default: no limit)

Each command prints the app's answer as JSON lines on stdout. `wait` prints one `tap` line per
press and nothing else; the app keeps a tap until this command has printed it.

Exit status: 0 done; 1 the app refused (its `err` line is on stdout); 2 `wait` ended without a
tap; 3 the app cannot be reached or did not speak the protocol; 64 FILE cannot be read.
";

    // ---- JSON, as far as this tool needs it ----

    /// A JSON object as its members in order: the key, decoded, and the value's text without
    /// the whitespace between its tokens. This tool only looks at top-level members and a
    /// surface description goes to the app as it was written, so no tree is built.
    type Members = Vec<(String, String)>;

    struct Scan<'a> {
        text: &'a str,
        at: usize,
    }

    impl<'a> Scan<'a> {
        fn fail<T>(&self, what: &str) -> Result<T, String> {
            let before = &self.text.as_bytes()[..self.at.min(self.text.len())];
            let line = before.iter().filter(|b| **b == b'\n').count() + 1;
            let column = before.iter().rev().take_while(|b| **b != b'\n').count() + 1;
            Err(format!("{what} (line {line} column {column})"))
        }

        fn peek(&self) -> Option<u8> {
            self.text.as_bytes().get(self.at).copied()
        }

        fn eat(&mut self, byte: u8) -> bool {
            let found = self.peek() == Some(byte);
            self.at += found as usize;
            found
        }

        fn word(&mut self, word: &str) -> bool {
            let found = self.text.as_bytes()[self.at..].starts_with(word.as_bytes());
            self.at += if found { word.len() } else { 0 };
            found
        }

        fn space(&mut self) {
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                self.at += 1;
            }
        }

        fn digits(&mut self) -> bool {
            let from = self.at;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.at += 1;
            }
            self.at > from
        }

        /// A string token, its quotes included.
        fn string(&mut self) -> Result<&'a str, String> {
            let start = self.at;
            if !self.eat(b'"') {
                return self.fail("a string is expected");
            }
            loop {
                match self.peek() {
                    Some(b'"') => {
                        self.at += 1;
                        return Ok(&self.text[start..self.at]);
                    }
                    Some(b'\\') => {
                        self.at += 1;
                        match self.peek() {
                            Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => self.at += 1,
                            Some(b'u') => {
                                let hex = self.text.as_bytes().get(self.at + 1..self.at + 5);
                                if !hex.is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) {
                                    return self.fail("\\u takes four hex digits");
                                }
                                self.at += 5;
                            }
                            _ => return self.fail("an unknown escape in a string"),
                        }
                    }
                    Some(byte) if byte >= 0x20 => self.at += 1,
                    Some(_) => return self.fail("a control character in a string"),
                    None => return self.fail("a string does not end"),
                }
            }
        }

        fn number(&mut self) -> Result<&'a str, String> {
            let start = self.at;
            let minus = self.eat(b'-');
            // Python's json.loads takes these three and json.dumps writes them back, so the
            // Python tool sends them on; the app then refuses the line as `bad_json`
            if self.word("Infinity") || (!minus && self.word("NaN")) {
                return Ok(&self.text[start..self.at]);
            }
            if !self.eat(b'0') && !self.digits() {
                return self.fail("a value is expected");
            }
            if self.eat(b'.') && !self.digits() {
                return self.fail("digits are expected after the point");
            }
            if self.eat(b'e') || self.eat(b'E') {
                let _ = self.eat(b'+') || self.eat(b'-');
                if !self.digits() {
                    return self.fail("digits are expected in the exponent");
                }
            }
            Ok(&self.text[start..self.at])
        }

        /// The key of an object member and its colon.
        fn key(&mut self) -> Result<&'a str, String> {
            self.space();
            let key = self.string()?;
            self.space();
            if !self.eat(b':') {
                return self.fail("':' is expected");
            }
            Ok(key)
        }

        /// One value of any depth, appended to `out` without whitespace. `open` holds the
        /// closing bracket of every array and object still open, so deep nesting costs heap,
        /// not stack.
        fn value(&mut self, out: &mut String) -> Result<(), String> {
            let mut open: Vec<u8> = Vec::new();
            loop {
                self.space();
                match self.peek() {
                    Some(b'{') => {
                        self.at += 1;
                        out.push('{');
                        self.space();
                        if self.eat(b'}') {
                            out.push('}');
                        } else {
                            open.push(b'}');
                            out.push_str(self.key()?);
                            out.push(':');
                            continue;
                        }
                    }
                    Some(b'[') => {
                        self.at += 1;
                        out.push('[');
                        self.space();
                        if self.eat(b']') {
                            out.push(']');
                        } else {
                            open.push(b']');
                            continue;
                        }
                    }
                    Some(b'"') => out.push_str(self.string()?),
                    Some(b't') if self.word("true") => out.push_str("true"),
                    Some(b'f') if self.word("false") => out.push_str("false"),
                    Some(b'n') if self.word("null") => out.push_str("null"),
                    _ => out.push_str(self.number()?),
                }
                // a value has ended: what closes with it, up to the comma before the next one
                loop {
                    let Some(&close) = open.last() else { return Ok(()) };
                    self.space();
                    if self.eat(b',') {
                        out.push(',');
                        if close == b'}' {
                            out.push_str(self.key()?);
                            out.push(':');
                        }
                        break;
                    }
                    if !self.eat(close) {
                        return self.fail(if close == b'}' { "',' or '}' is expected" } else { "',' or ']' is expected" });
                    }
                    out.push(close as char);
                    open.pop();
                }
            }
        }
    }

    /// Checks that `text` is one JSON object and nothing else.
    fn object(text: &str) -> Result<Members, String> {
        let mut scan = Scan { text, at: 0 };
        let mut members = Members::new();
        scan.space();
        if !scan.eat(b'{') {
            return scan.fail("one JSON object is expected");
        }
        scan.space();
        if !scan.eat(b'}') {
            loop {
                let key = unescape(scan.key()?);
                let mut value = String::new();
                scan.value(&mut value)?;
                members.push((key, value));
                scan.space();
                if scan.eat(b'}') {
                    break;
                }
                if !scan.eat(b',') {
                    return scan.fail("',' or '}' is expected");
                }
            }
        }
        scan.space();
        if scan.peek().is_some() {
            return scan.fail("text after the object");
        }
        Ok(members)
    }

    /// The text of a string token that `Scan::string` accepted.
    fn unescape(token: &str) -> String {
        fn hex4(chars: &mut std::str::Chars) -> u32 {
            (0..4).fold(0, |n, _| n * 16 + chars.next().and_then(|c| c.to_digit(16)).unwrap_or(0))
        }
        let mut out = String::new();
        let mut chars = token[1..token.len() - 1].chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('b') => out.push('\u{8}'),
                Some('f') => out.push('\u{c}'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    let mut code = hex4(&mut chars);
                    if (0xD800..0xDC00).contains(&code) {
                        let mut ahead = chars.clone();
                        if ahead.next() == Some('\\') && ahead.next() == Some('u') {
                            let low = hex4(&mut ahead);
                            if (0xDC00..0xE000).contains(&low) {
                                code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                                chars = ahead;
                            }
                        }
                    }
                    out.push(char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER));
                }
                Some(other) => out.push(other),
                None => {}
            }
        }
        out
    }

    /// A string token for `text`, with the escapes of Python's `json.dumps(ensure_ascii=False)`.
    fn quote(text: &str) -> String {
        let mut out = String::from("\"");
        for c in text.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{8}' => out.push_str("\\b"),
                '\u{c}' => out.push_str("\\f"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    // ---- the connection ----

    enum Fail {
        /// The app cannot be reached or does not speak the protocol: exit 3.
        Unreachable(String),
        /// No line from the app within the time given.
        TimedOut,
        /// stdout is gone. The Python tool dies of the same with status 1; what matters is
        /// that no `done` follows a tap that could not be printed.
        Output(io::Error),
    }

    fn gone(e: io::Error) -> Fail {
        Fail::Unreachable(format!("the surface went away ({e})"))
    }

    /// A line from the app.
    struct Reply {
        text: String,
        members: Members,
        /// its `type`
        kind: String,
    }

    impl Reply {
        /// A top-level member's value as JSON text; of a repeated key the last, as a JSON
        /// reader that fills a map has it.
        fn get(&self, key: &str) -> Option<&str> {
            self.members.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
        }

        fn seq(&self) -> u64 {
            self.get("seq").and_then(|v| v.parse().ok()).unwrap_or(0)
        }
    }

    /// A connection: the hello is read on connect, then requests and answers are lines.
    struct Surface {
        sock: UnixStream,
        buf: Vec<u8>,
    }

    impl Surface {
        fn connect(path: &PathBuf) -> Result<Surface, Fail> {
            let sock = UnixStream::connect(path).map_err(|e| Fail::Unreachable(format!("no surface at {} ({e})", path.display())))?;
            let mut conn = Surface { sock, buf: Vec::new() };
            let hello = conn.read(Some(5.0))?;
            if hello.kind != "hello" || hello.get("v").and_then(|v| v.parse().ok()) != Some(VERSION) {
                return Err(Fail::Unreachable(format!("unexpected greeting: {}", hello.text)));
            }
            Ok(conn)
        }

        fn send(&mut self, line: &str) -> Result<(), Fail> {
            let mut bytes = Vec::with_capacity(line.len() + 1);
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
            self.sock.write_all(&bytes).map_err(gone)
        }

        /// The next line. `timeout` in seconds; `None` (or more than a clock holds) waits for
        /// ever.
        fn read(&mut self, timeout: Option<f64>) -> Result<Reply, Fail> {
            let deadline = timeout.and_then(|t| Duration::try_from_secs_f64(t).ok()).and_then(|d| Instant::now().checked_add(d));
            let end = loop {
                if let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
                    break end;
                }
                let left = match deadline {
                    None => None,
                    Some(deadline) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            return Err(Fail::TimedOut);
                        }
                        Some(left)
                    }
                };
                self.sock.set_read_timeout(left).map_err(gone)?;
                let mut chunk = [0u8; 65536];
                match self.sock.read(&mut chunk) {
                    Ok(0) => return Err(Fail::Unreachable("the surface closed the connection".into())),
                    Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Err(Fail::TimedOut),
                    Err(e) => return Err(gone(e)),
                }
            };
            let rest = self.buf.split_off(end + 1);
            let mut line = std::mem::replace(&mut self.buf, rest);
            line.pop();
            let text = String::from_utf8_lossy(&line).into_owned();
            let Ok(members) = object(&text) else {
                return Err(Fail::Unreachable(format!("not a JSON object from the surface: {text:?}")));
            };
            let kind = match members.iter().rev().find(|(k, _)| k == "type") {
                Some((_, v)) if v.starts_with('"') => unescape(v),
                _ => String::new(),
            };
            Ok(Reply { text, members, kind })
        }
    }

    // ---- the commands ----

    fn emit(line: &str) -> Result<(), Fail> {
        let mut out = io::stdout().lock();
        out.write_all(line.as_bytes()).and_then(|_| out.write_all(b"\n")).and_then(|_| out.flush()).map_err(Fail::Output)
    }

    /// Prints a one-line answer; the exit status says whether it was a refusal.
    fn answer(reply: Reply) -> Result<i32, Fail> {
        emit(&reply.text)?;
        Ok(if reply.kind == "err" { 1 } else { 0 })
    }

    fn set(conn: &mut Surface, file: &str, surface: &Option<String>, rev: Option<i128>) -> Result<i32, Fail> {
        let text = if file == "-" { io::read_to_string(io::stdin()) } else { std::fs::read_to_string(file) };
        let members = match text.map_err(|e| e.to_string()).and_then(|text| object(&text)) {
            Ok(members) => members,
            Err(why) => {
                eprintln!("tb-surface: {file}: {why}");
                return Ok(64);
            }
        };
        // FILE's members as written, less the ones this call gives. A key FILE repeats goes
        // out repeated: the app keeps the last, which is what Python's reader keeps.
        let mut line = String::from("{");
        for (key, value) in &members {
            if key == "type" || (key == "surface" && surface.is_some()) || (key == "rev" && rev.is_some()) {
                continue;
            }
            line.push_str(&quote(key));
            line.push(':');
            line.push_str(value);
            line.push(',');
        }
        line.push_str("\"type\":\"set\"");
        if let Some(surface) = surface {
            line.push_str(",\"surface\":");
            line.push_str(&quote(surface));
        }
        if let Some(rev) = rev {
            line.push_str(&format!(",\"rev\":{rev}"));
        }
        line.push('}');
        conn.send(&line)?;
        answer(conn.read(Some(10.0))?)
    }

    fn wait(conn: &mut Surface, timeout: Option<f64>) -> Result<i32, Fail> {
        let block = timeout.map_or(true, |t| t > 0.0);
        conn.send(&format!("{{\"type\":\"taps\",\"wait\":{block}}}"))?;
        let mut reply = match conn.read(if block { timeout } else { Some(10.0) }) {
            Err(Fail::TimedOut) => {
                eprintln!("tb-surface: no tap within {} s", timeout.unwrap_or(0.0));
                return Ok(2);
            }
            other => other?,
        };
        let mut last = 0;
        while reply.kind == "tap" {
            emit(&reply.text)?;
            last = reply.seq().max(last);
            reply = conn.read(Some(10.0))?;
        }
        if reply.kind == "err" {
            return answer(reply);
        }
        if reply.kind != "taps" {
            return Err(Fail::Unreachable(format!("unexpected answer: {}", reply.text)));
        }
        if last == 0 {
            return Ok(2);
        }
        // only now, with the taps printed, may the app forget them
        conn.send(&format!("{{\"type\":\"done\",\"seq\":{last}}}"))?;
        conn.read(Some(10.0))?;
        Ok(0)
    }

    fn ask(conn: &mut Surface, kind: &str) -> Result<i32, Fail> {
        conn.send(&format!("{{\"type\":\"{kind}\"}}"))?;
        answer(conn.read(Some(10.0))?)
    }

    // ---- the command line ----

    enum Command {
        Set { file: String, surface: Option<String>, rev: Option<i128> },
        Wait { timeout: Option<f64> },
        Clear,
        Status,
    }

    /// Why no command runs: `None` when help was asked for, else what is wrong.
    type Stop = Option<String>;

    fn is_help(arg: &str) -> bool {
        arg == "-h" || arg == "--help"
    }

    /// The value of `--name VALUE` or `--name=VALUE` when `arg` is that option. Like
    /// argparse, a VALUE that looks like another option is refused.
    fn option(arg: &str, name: &str, rest: &mut impl Iterator<Item = String>) -> Option<Result<String, Stop>> {
        if arg == name {
            return Some(match rest.next() {
                Some(value) if !value.starts_with('-') || value == "-" || value.parse::<f64>().is_ok() => Ok(value),
                _ => Err(Some(format!("argument {name}: expected one argument"))),
            });
        }
        let value = arg.strip_prefix(name)?.strip_prefix('=')?;
        Some(Ok(value.to_string()))
    }

    fn unrecognized<T>(arg: &str) -> Result<T, Stop> {
        Err(Some(format!("unrecognized arguments: {arg}")))
    }

    fn parse(mut rest: impl Iterator<Item = String>) -> Result<(Option<String>, Command), Stop> {
        let mut socket = None;
        let command = loop {
            let Some(arg) = rest.next() else {
                return Err(Some("the following arguments are required: command".into()));
            };
            if is_help(&arg) {
                return Err(None);
            }
            match option(&arg, "--socket", &mut rest) {
                Some(value) => socket = Some(value?),
                None => break arg,
            }
        };
        let command = match command.as_str() {
            "set" => {
                let (mut file, mut surface, mut rev, mut plain) = (None, None, None, false);
                while let Some(arg) = rest.next() {
                    if plain || arg == "-" || !arg.starts_with('-') {
                        if file.is_some() {
                            return unrecognized(&arg);
                        }
                        file = Some(arg);
                    } else if arg == "--" {
                        plain = true;
                    } else if is_help(&arg) {
                        return Err(None);
                    } else if let Some(value) = option(&arg, "--surface", &mut rest) {
                        surface = Some(value?);
                    } else if let Some(value) = option(&arg, "--rev", &mut rest) {
                        let value = value?;
                        let Ok(n) = value.trim().parse::<i128>() else {
                            return Err(Some(format!("argument --rev: invalid int value: '{value}'")));
                        };
                        rev = Some(n);
                    } else {
                        return unrecognized(&arg);
                    }
                }
                let Some(file) = file else {
                    return Err(Some("the following arguments are required: FILE".into()));
                };
                Command::Set { file, surface, rev }
            }
            "wait" => {
                let mut timeout = None;
                while let Some(arg) = rest.next() {
                    if is_help(&arg) {
                        return Err(None);
                    } else if let Some(value) = option(&arg, "--timeout", &mut rest) {
                        let value = value?;
                        let Ok(seconds) = value.trim().parse::<f64>() else {
                            return Err(Some(format!("argument --timeout: invalid float value: '{value}'")));
                        };
                        timeout = Some(seconds);
                    } else {
                        return unrecognized(&arg);
                    }
                }
                Command::Wait { timeout }
            }
            "clear" | "status" => {
                if let Some(arg) = rest.next() {
                    return if is_help(&arg) { Err(None) } else { unrecognized(&arg) };
                }
                if command == "clear" {
                    Command::Clear
                } else {
                    Command::Status
                }
            }
            other => {
                return Err(Some(format!("argument command: invalid choice: '{other}' (choose from set, wait, clear, status)")));
            }
        };
        Ok((socket, command))
    }

    /// `--socket PATH`, else `$TINYBOT_SURFACE_SOCK`, else `$XDG_RUNTIME_DIR/tinybot/surface.sock`
    /// (`/run/user/UID` without that variable): the app's rule.
    fn socket_path(arg: Option<String>) -> PathBuf {
        extern "C" {
            fn getuid() -> u32;
        }
        if let Some(path) = arg.filter(|p| !p.is_empty()) {
            return PathBuf::from(path);
        }
        if let Some(path) = std::env::var_os("TINYBOT_SURFACE_SOCK").filter(|p| !p.is_empty()) {
            return PathBuf::from(path);
        }
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { getuid() })));
        runtime.join("tinybot").join("surface.sock")
    }

    pub fn main() -> i32 {
        let args = std::env::args_os().skip(1).map(|arg| arg.to_string_lossy().into_owned());
        let (socket, command) = match parse(args) {
            Ok(parsed) => parsed,
            Err(None) => {
                let _ = io::stdout().write_all(HELP.as_bytes());
                return 0;
            }
            Err(Some(wrong)) => {
                eprint!("{USAGE}");
                eprintln!("tb-surface: error: {wrong}");
                return 2;
            }
        };
        // the app is reached before FILE is read, so a missing app is status 3 whatever FILE is
        let done = Surface::connect(&socket_path(socket)).and_then(|mut conn| match &command {
            Command::Set { file, surface, rev } => set(&mut conn, file, surface, *rev),
            Command::Wait { timeout } => wait(&mut conn, *timeout),
            Command::Clear => ask(&mut conn, "clear"),
            Command::Status => ask(&mut conn, "status"),
        });
        match done {
            Ok(status) => status,
            Err(Fail::Unreachable(why)) => {
                eprintln!("tb-surface: {why}");
                3
            }
            Err(Fail::TimedOut) => {
                eprintln!("tb-surface: the surface did not answer");
                3
            }
            Err(Fail::Output(e)) => {
                eprintln!("tb-surface: cannot write to stdout ({e})");
                1
            }
        }
    }
}
