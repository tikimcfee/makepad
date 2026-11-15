//! tb-panel: the panel runner for tinybot's live surface. Hardware tries become pages the
//! user taps: a panel file (JSON, root's) describes a page and what its buttons run, and
//! this one runner (root) shows Home, the panels and agents' questions on the surface, runs
//! each try as a transient systemd unit and writes what happened to state files and records.
//!
//!   tb-panel run [--dirs DIR:DIR…] [--home] [--key DEVICE:CODE]   serve the panels
//!   tb-panel check FILE                       is FILE a panel the runner would take? 0 or 64
//!   tb-panel show [NAME]                      a panel's state file, or the runner's own
//!   tb-panel ask FILE                         queue a question; prints its qid
//!   tb-panel ask --wait QID [--timeout S]     wait for the answer; prints it
//!
//! Standard library only, and nothing of Makepad: the runner knows the surface by its socket
//! (the protocol is in the README). It knows no device: every limit that protects hardware
//! is the tool's that a panel runs; the runner passes values on as they were typed, each as
//! one argument, never through a shell.

#[cfg(unix)]
mod app;
#[cfg(unix)]
mod ask;
#[cfg(unix)]
mod exec;
#[cfg(unix)]
mod json;
#[cfg(unix)]
mod feed;
#[cfg(unix)]
mod panel;
#[cfg(unix)]
mod records;
#[cfg(unix)]
mod run;
#[cfg(unix)]
mod sha256;

#[cfg(not(unix))]
fn main() {
    eprintln!("tb-panel: the runner needs Unix sockets and systemd");
    std::process::exit(3);
}

#[cfg(unix)]
fn main() {
    std::process::exit(cli::main());
}

#[cfg(unix)]
mod cli {
    use crate::json::Json;
    use crate::{ask, panel, run};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    const USAGE: &str = "\
usage: tb-panel run [--dirs DIR:DIR…] [--home] [--socket PATH] [--key DEVICE:CODE[,CODE…]]
       tb-panel check FILE
       tb-panel show [NAME]
       tb-panel ask FILE
       tb-panel ask --wait QID [--timeout S]

run    serves the panels of the directories (default /usr/share/tinybot/panels:/etc/tinybot/panels;
       a later directory wins for the same name) with a Home page; --home says so and changes nothing
       --agent-direct=no: an agent's new page never comes up by itself, only from Home
       --key /dev/input/event0:523,115,114 reads those keys: each runs the action a panel binds to it
check  reads FILE as the runner would: exit 0 when it is a valid panel, 64 with the reason when not
show   prints the state file of the panel NAME, or the runner's own without a name
ask    FILE is {\"from\":…,\"text\":…,\"covers\":[{\"path\":…,\"sha256\":…}],\"choices\":[…]}; prints the qid.
       exit 0 queued, 1 refused (the reason is printed), 3 no runner, 64 FILE unreadable
ask --wait  prints the answer to QID. exit 0 the first choice, 1 any other answer, 2 no answer
       within S seconds, 3 no runner or no such question

for other places: --state-dir DIR (default /run/tinybot/panel), --tries-dir DIR (default
/var/lib/tinybot/tries), --panel-state FILE (default /sys/kernel/tinybot_panel/state)
";

    struct Args {
        words: Vec<String>,
        dirs: Vec<PathBuf>,
        socket: Option<String>,
        agent_direct: bool,
        state_dir: PathBuf,
        tries_dir: PathBuf,
        panel_state: PathBuf,
        key: Option<(PathBuf, Vec<u16>)>,
        wait: Option<String>,
        timeout: Option<f64>,
    }

    fn parse(mut rest: impl Iterator<Item = String>) -> Result<Args, String> {
        let mut args = Args {
            words: Vec::new(),
            dirs: vec!["/usr/share/tinybot/panels".into(), "/etc/tinybot/panels".into()],
            socket: None,
            agent_direct: true,
            state_dir: "/run/tinybot/panel".into(),
            tries_dir: "/var/lib/tinybot/tries".into(),
            panel_state: "/sys/kernel/tinybot_panel/state".into(),
            key: None,
            wait: None,
            timeout: None,
        };
        while let Some(arg) = rest.next() {
            let (name, mut value) = match arg.split_once('=') {
                Some((name, value)) if name.starts_with("--") => (name.to_string(), Some(value.to_string())),
                _ => (arg.clone(), None),
            };
            let mut take = || value.take().or_else(|| rest.next()).ok_or(format!("{name} takes a value"));
            match name.as_str() {
                "--dirs" => args.dirs = take()?.split(':').filter(|d| !d.is_empty()).map(PathBuf::from).collect(),
                "--socket" => args.socket = Some(take()?),
                "--agent-direct" => {
                    args.agent_direct = match take()?.as_str() {
                        "yes" => true,
                        "no" => false,
                        other => return Err(format!("--agent-direct {other:?} is not yes or no")),
                    }
                }
                "--state-dir" => args.state_dir = take()?.into(),
                "--tries-dir" => args.tries_dir = take()?.into(),
                "--panel-state" => args.panel_state = take()?.into(),
                "--key" => {
                    let value = take()?;
                    let key = value.rsplit_once(':').and_then(|(device, codes)| {
                        let codes: Option<Vec<u16>> = codes.split(',').map(|code| code.parse().ok()).collect();
                        Some((PathBuf::from(device), codes?))
                    });
                    args.key = Some(key.ok_or(format!("--key {value:?} is not DEVICE:CODE[,CODE…], an input device and key codes"))?);
                }
                "--wait" => args.wait = Some(take()?),
                "--timeout" => {
                    let value = take()?;
                    args.timeout = Some(value.trim().parse().map_err(|_| format!("--timeout {value:?} is not a number of seconds"))?);
                }
                "--home" => {}
                "-h" | "--help" => return Err(String::new()),
                option if option.starts_with("--") => return Err(format!("unknown option {option}")),
                _ => args.words.push(arg),
            }
        }
        Ok(args)
    }

    fn check(file: &str) -> i32 {
        match panel::Panel::load(Path::new(file)) {
            Ok(panel) => {
                println!("{file}: panel {} ({} items)", panel.name, panel.items.len());
                0
            }
            Err(why) => {
                eprintln!("tb-panel: {file}: {why}");
                64
            }
        }
    }

    fn show(args: &Args, name: Option<&str>) -> i32 {
        let file = args.state_dir.join(format!("{}.json", name.unwrap_or("runner")));
        match std::fs::read_to_string(&file) {
            Ok(state) => {
                println!("{}", state.trim_end());
                0
            }
            Err(e) => {
                eprintln!("tb-panel: {}: {e}", file.display());
                1
            }
        }
    }

    fn ask_file(args: &Args, file: &str) -> i32 {
        let question = std::fs::read_to_string(file).map_err(|e| e.to_string()).and_then(|text| Json::parse(&text));
        let mut question = match question {
            Ok(question @ Json::Obj(_)) => question,
            Ok(_) => {
                eprintln!("tb-panel: {file}: a question is one JSON object");
                return 64;
            }
            Err(why) => {
                eprintln!("tb-panel: {file}: {why}");
                return 64;
            }
        };
        question.set("type", "ask".into());
        match ask::call(&args.state_dir.join("ask.sock"), &question, Some(Duration::from_secs(60))) {
            Ok(Some(answer)) => match answer.get("qid").and_then(|q| q.as_str()) {
                Some(qid) => {
                    println!("{qid}");
                    0
                }
                None => {
                    println!("{}", answer.get("msg").and_then(|m| m.as_str()).unwrap_or("refused"));
                    1
                }
            },
            Ok(None) => {
                eprintln!("tb-panel: the runner did not answer");
                3
            }
            Err(e) => {
                eprintln!("tb-panel: no runner at {}: {e}", args.state_dir.join("ask.sock").display());
                3
            }
        }
    }

    /// The answer of a question that has its record already: the record is the state, and
    /// it is there after the runner that took the answer is gone.
    fn recorded(args: &Args, qid: &str) -> Option<Json> {
        for name in ["_questions.jsonl", "_questions.jsonl.1"] {
            let text = std::fs::read_to_string(args.tries_dir.join(name)).unwrap_or_default();
            for line in text.lines().rev() {
                if let Ok(record) = Json::parse(line) {
                    if record.get("qid").and_then(|q| q.as_str()) == Some(qid) {
                        return Some(record);
                    }
                }
            }
        }
        None
    }

    fn ask_wait(args: &Args, qid: &str) -> i32 {
        // 0 for the first choice, 1 for any other answer and for an answer that is void
        let said = |word: Option<&str>, first: bool| {
            println!("{}", word.unwrap_or("void"));
            if word.is_some() && first {
                0
            } else {
                1
            }
        };
        if let Some(record) = recorded(args, qid) {
            let word = record.get("answer").and_then(|a| a.as_str());
            let first = record.get("choices").and_then(|c| c.as_arr()).and_then(|c| c.first()).and_then(|c| c.as_str());
            return said(word, word.is_some() && word == first);
        }
        let request = Json::obj([("type", "wait".into()), ("qid", qid.into())]);
        let timeout = args.timeout.and_then(|s| Duration::try_from_secs_f64(s.max(0.001)).ok());
        match ask::call(&args.state_dir.join("ask.sock"), &request, timeout) {
            Ok(Some(answer)) if answer.get("type").and_then(|t| t.as_str()) == Some("answer") => {
                said(answer.get("answer").and_then(|a| a.as_str()), answer.get("index").and_then(|i| i.as_u64()) == Some(0))
            }
            Ok(Some(answer)) => {
                eprintln!("tb-panel: {}", answer.get("msg").and_then(|m| m.as_str()).unwrap_or("refused"));
                3
            }
            Ok(None) => {
                eprintln!("tb-panel: no answer to {qid} within {} s", args.timeout.unwrap_or(0.0));
                2
            }
            Err(e) => {
                eprintln!("tb-panel: no runner at {}: {e}", args.state_dir.join("ask.sock").display());
                3
            }
        }
    }

    pub fn main() -> i32 {
        let args = match parse(std::env::args_os().skip(1).map(|arg| arg.to_string_lossy().into_owned())) {
            Ok(args) => args,
            Err(wrong) if wrong.is_empty() => {
                print!("{USAGE}");
                return 0;
            }
            Err(wrong) => {
                eprint!("{USAGE}");
                eprintln!("tb-panel: {wrong}");
                return 64;
            }
        };
        let words: Vec<&str> = args.words.iter().map(String::as_str).collect();
        match (words.as_slice(), &args.wait) {
            (["run"], None) => run::run(run::Options {
                dirs: args.dirs,
                socket: args.socket,
                agent_direct: args.agent_direct,
                state_dir: args.state_dir,
                tries_dir: args.tries_dir,
                panel_state: args.panel_state,
                key: args.key,
            }),
            (["check", file], None) => check(file),
            (["show"], None) => show(&args, None),
            (["show", name], None) => show(&args, Some(name)),
            (["ask", file], None) => ask_file(&args, file),
            (["ask"], Some(qid)) => ask_wait(&args, qid),
            _ => {
                eprint!("{USAGE}");
                64
            }
        }
    }
}
