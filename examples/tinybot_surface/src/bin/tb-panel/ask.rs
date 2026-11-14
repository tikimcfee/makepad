//! Questions from agents. `tb-panel ask FILE` hands a question to the running runner over
//! its own socket (`ask.sock` in the state directory). Any user's process may ask, so the
//! socket takes nothing but a question and a wait for its answer, and the runner records
//! who asked from the socket itself (uid and process name), beside what the asker says of
//! itself. The runner hashes the question's text and the files it covers itself, so an
//! answer is tied to exact bytes.

use crate::json::Json;
use crate::sha256;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

/// The longest question text. With its covers and choices a question is one page, and a
/// page is one line of at most 64 KiB to the app.
const TEXT_BYTES: usize = 8 * 1024;

#[derive(Clone)]
pub struct Question {
    pub qid: String,
    pub from: String,
    pub text: String,
    pub text_sha256: String,
    /// path and sha256 of each file the answer is about
    pub covers: Vec<(String, String)>,
    pub choices: Vec<String>,
    pub t_asked: f64,
    /// who asked, as the socket says: the uid, and the name the process gives itself
    pub peer: Option<(u32, String)>,
}

impl Question {
    pub fn covers_json(&self) -> Json {
        Json::Arr(self.covers.iter().map(|(path, sha)| Json::obj([("path", path.as_str().into()), ("sha256", sha.as_str().into())])).collect())
    }

    /// As it is kept in the runner's state file while it waits.
    pub fn json(&self) -> Json {
        Json::obj([
            ("qid", self.qid.as_str().into()),
            ("from", self.from.as_str().into()),
            ("text_sha256", self.text_sha256.as_str().into()),
            ("text", self.text.as_str().into()),
            ("covers", self.covers_json()),
            ("choices", self.choices.clone().into()),
            ("t_asked", self.t_asked.into()),
            (
                "peer",
                match &self.peer {
                    Some((uid, comm)) => Json::obj([("uid", (*uid as u64).into()), ("comm", comm.as_str().into())]),
                    None => Json::Null,
                },
            ),
        ])
    }

    /// "comm (uid N)" of the asking process.
    pub fn asked_by(&self) -> Option<String> {
        self.peer.as_ref().map(|(uid, comm)| format!("{comm} (uid {uid})"))
    }

    /// From the state file of a runner that ran before. The hashes are the ones taken then.
    pub fn from_state(state: &Json) -> Option<Question> {
        let text = |name: &str| state.get(name).and_then(|v| v.as_str()).map(str::to_string);
        let mut covers = Vec::new();
        for cover in state.get("covers")?.as_arr()? {
            covers.push((cover.get("path")?.as_str()?.to_string(), cover.get("sha256")?.as_str()?.to_string()));
        }
        let choices = state.get("choices")?.as_arr()?.iter().filter_map(|c| c.as_str().map(str::to_string)).collect();
        Some(Question {
            qid: text("qid")?,
            from: text("from")?,
            text: text("text")?,
            text_sha256: text("text_sha256")?,
            covers,
            choices,
            t_asked: state.get("t_asked")?.as_f64()?,
            peer: state.get("peer").and_then(|peer| Some((peer.get("uid")?.as_u64()? as u32, peer.get("comm")?.as_str()?.to_string()))),
        })
    }

    /// The covered files that no longer have the hash the question was asked with.
    pub fn changed(&self) -> Vec<String> {
        self.covers.iter().filter(|(path, sha)| sha256::of_file(path).ok().as_ref() != Some(sha)).map(|(path, _)| path.clone()).collect()
    }
}

/// Checks a question as an agent wrote it and hashes what it names. The qid is given later.
pub fn question(file: &Json) -> Result<Question, String> {
    let members = file.as_obj().ok_or("a question is one JSON object")?;
    if let Some((key, _)) = members.iter().find(|(key, _)| !["type", "from", "text", "covers", "choices"].contains(&key.as_str())) {
        return Err(format!("unknown field {key:?} (known: from, text, covers, choices)"));
    }
    let text_of = |name: &str| match file.get(name).and_then(|v| v.as_str()) {
        Some(text) if !text.trim().is_empty() => Ok(text.to_string()),
        _ => Err(format!("{name} is missing or not a non-empty string")),
    };
    let from = text_of("from")?;
    let text = text_of("text")?;
    if text.len() > TEXT_BYTES {
        return Err(format!("text is {} bytes, more than {TEXT_BYTES}", text.len()));
    }
    let mut choices = Vec::new();
    for choice in file.get("choices").and_then(|c| c.as_arr()).ok_or("choices is missing or not an array")? {
        match choice.as_str() {
            Some(word) if !word.is_empty() && !choices.iter().any(|c| c == word) => choices.push(word.to_string()),
            _ => return Err("choices holds something that is not a word, or one word twice".into()),
        }
    }
    if choices.is_empty() {
        return Err("choices is empty".into());
    }
    let mut covers = Vec::new();
    if let Some(list) = file.get("covers") {
        for cover in list.as_arr().ok_or("covers is not an array")? {
            let field = |name: &str| cover.get(name).and_then(|v| v.as_str()).ok_or(format!("covers: {name} is missing or not a string"));
            let (path, given) = (field("path")?, field("sha256")?);
            if !path.starts_with('/') {
                return Err(format!("covers: {path:?} is not an absolute path"));
            }
            // the runner's own reading of the file decides
            let now = sha256::of_file(path).map_err(|e| format!("covers: {path}: {e}"))?;
            if !now.eq_ignore_ascii_case(given) {
                return Err(format!("file changed: {path} is {now}, the question says {given}"));
            }
            covers.push((path.to_string(), now));
        }
    }
    Ok(Question { qid: String::new(), from, text_sha256: sha256::of_bytes(text.as_bytes()), text, covers, choices, t_asked: crate::records::now(), peer: None })
}

/// What arrives on the runner's socket, for the runner's main thread to answer.
pub enum Asked {
    /// a checked and hashed question
    Question(Question, UnixStream),
    /// a client that waits for the answer to a qid
    Wait(String, UnixStream),
}

pub fn reply(mut to: &UnixStream, line: &Json) {
    let _ = to.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = to.write_all(format!("{line}\n").as_bytes());
}

pub fn refuse(to: &UnixStream, msg: &str) {
    reply(to, &Json::obj([("type", "err".into()), ("msg", msg.into())]));
}

/// The uid and the process name of the other end of a connection.
fn peer_of(stream: &UnixStream) -> Option<(u32, String)> {
    use std::os::unix::io::AsRawFd;
    extern "C" {
        fn getsockopt(fd: i32, level: i32, name: i32, val: *mut u8, len: *mut u32) -> i32;
    }
    // SOL_SOCKET 1, SO_PEERCRED 17: struct ucred { pid, uid, gid }
    let mut cred = [0u32; 3];
    let mut len = 12u32;
    if unsafe { getsockopt(stream.as_raw_fd(), 1, 17, cred.as_mut_ptr() as *mut u8, &mut len) } != 0 {
        return None;
    }
    let comm = std::fs::read_to_string(format!("/proc/{}/comm", cred[0])).unwrap_or_default();
    Some((cred[1], if comm.trim().is_empty() { "?".to_string() } else { comm.trim().to_string() }))
}

/// Binds the runner's socket for questions: open to every user, since all it takes is a
/// question and a wait.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // a file left by a runner that is gone; a second runner is kept out by the unit
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

/// Serves the runner's socket: one request line per connection. A question is checked and
/// hashed here, so a large covered file never holds up the taps.
pub fn serve(listener: UnixListener, hand_on: impl Fn(Asked)) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            continue;
        }
        let _ = stream.set_read_timeout(None);
        let request = match Json::parse(&line) {
            Ok(request) => request,
            Err(e) => {
                refuse(&stream, &format!("not JSON: {e}"));
                continue;
            }
        };
        match request.get("type").and_then(|t| t.as_str()) {
            Some("ask") => match question(&request) {
                Ok(mut question) => {
                    question.peer = peer_of(&stream);
                    hand_on(Asked::Question(question, stream))
                }
                Err(why) => refuse(&stream, &why),
            },
            Some("wait") => match request.get("qid").and_then(|q| q.as_str()) {
                Some(qid) => hand_on(Asked::Wait(qid.to_string(), stream)),
                None => refuse(&stream, "wait: qid is missing"),
            },
            _ => refuse(&stream, "type is ask or wait"),
        }
    }
}

/// One request to the runner and its answer line. `timeout`: how long to wait for the
/// answer (`None`: for ever). `Ok(None)`: it did not come in time.
pub fn call(socket: &Path, request: &Json, timeout: Option<Duration>) -> std::io::Result<Option<Json>> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(format!("{request}\n").as_bytes())?;
    stream.set_read_timeout(timeout)?;
    let mut line = String::new();
    match BufReader::new(&stream).read_line(&mut line) {
        Ok(0) => Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the runner closed the connection")),
        Ok(_) => Json::parse(&line).map(Some).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(None),
        Err(e) => Err(e),
    }
}
