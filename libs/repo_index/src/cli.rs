//! `makepad index <verb>`: verb dispatch. Plain text on stdout, `--json` for records; errors on
//! stderr. Exit 0 on an answer (absence included), 1 on an error, 2 on a usage error or a verb
//! that is not built yet.

/// Every verb, with its one-line usage and summary (the runner's help page prints these too).
pub const VERBS: &[(&str, &str, &str)] = &[
    ("find", "find <words..> [--in <circle>,..] [--kind k,..] [--crate p] [--path p] [-n 10] [--rev r] [--json]", "Ranked hits per circle, one line each; --kind takes kinds (fn, struct, method, ..) or families (item, doc, commit, note, ..)"),
    ("show", "show <handle> [--in <circle>] [--rev r] [--context N] [--all] [--json]", "An item, section or commit, bounded; a handle ending in @<rev> resolves at that rev"),
    ("crate", "crate <package> [--rev r] [--json]", "A crate's card: description, deps, dependents, apps"),
    ("def", "def <identifier> [--in <circle>,..] [--rev r] [--json]", "Exact definitions of an identifier"),
    ("refs", "refs <identifier> [--uses] [--rev r] [--json]", "Definitions, re-exports, then uses by name"),
    ("history", "history <path|words..> [--rev r] [--json]", "Commits in makepad:history"),
    ("stale", "stale [--rev r] [--json]", "Docs and headers naming things that are gone"),
    ("circles", "circles [--json]", "Configured circles, their sources and index state"),
    ("build", "build [--in <circle>,..] [--rev r]", "Build or refresh the circles' indexes"),
    ("status", "status [--json]", "What is indexed, at which revision, and what the overlay holds"),
    ("gc", "gc", "Remove index data nothing uses any more"),
    ("eval", "eval [run|verify-gold|diff] [--rev r] ..", "The eval harness; each question is indexed at its own pinned rev unless --rev overrides"),
];

pub fn usage() -> String {
    let mut out = String::from("makepad index <verb> ...\n\n");
    for (_, usage, summary) in VERBS {
        out.push_str(&format!("  {usage}\n      {summary}\n"));
    }
    out
}

/// Runs `makepad index` with the words after `index`; the exit code.
pub fn main(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None => {
            eprint!("{}", usage());
            2
        }
        Some("help" | "-h" | "--help") => {
            print!("{}", usage());
            0
        }
        Some(verb) if VERBS.iter().any(|(v, _, _)| *v == verb) => {
            eprintln!("makepad index {verb}: not built yet");
            2
        }
        Some(other) => {
            eprintln!("makepad index: unknown verb `{other}`\n");
            eprint!("{}", usage());
            2
        }
    }
}
