//! `makepad index <verb>`: verb dispatch. Plain text on stdout, `--json` for records; errors on
//! stderr. Exit 0 on an answer (absence included), 1 on an error, 2 on a usage error or a verb
//! that is not built yet.
//!
//! A verb runs over [`Parts`]: the circle registry and opener, ranking, rendering, and git reads
//! in the repo the caller stands in. Every query names its circles with `--in`, or takes the
//! default scope of the directory it runs in; a circle that is not configured is an error, never
//! an empty answer (PLAN §2). Handles are what hits print (PLAN §5): `path:line[-end]`,
//! `path::Item`, `crate:<package>`, a commit's hex, any of them prefixed by `<circle>/` and
//! suffixed by `@<rev>`.

use crate::{
    circles, eval, ChunkAt, CircleExpr, CircleManifest, CircleName, CircleStatus, Circles, CrateView, Detail, Error, EvalMode,
    EvalOpts, Handle, Harness, Key, KindFilter, Lines, Located, Mode, Oid, OpenCircle, Opened, Output, Query, QuestionSet,
    Render, Repo, RepoId, RevSpec, Search, SegmentKey, ShowOutput, SourceStamp, StatusOutput, Store, Target, DEFAULT_LIMIT,
    INDEXER_VERSION, SHOW_MAX_LINES,
};
use crate::extract::{git::GitRepo, notes::Cite, Extractor};
use crate::open::{Opener, RepoOpener};
use crate::output::Renderer;
use crate::query::Engine;
use crate::store::{index_root, FsStore};
use crate::terms::Terms;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Every verb, with its one-line usage and summary (the runner's help page prints these too).
pub const VERBS: &[(&str, &str, &str)] = &[
    ("find", "find <words..> [--in <circle>,..] [--kind k,..] [--crate p] [--path p] [-n 10] [--rev r] [--json]", "Ranked hits per circle, one line each; --kind takes kinds (fn, struct, method, ..) or families (item, doc, commit, note, ..)"),
    ("show", "show <handle> [--in <circle>] [--rev r] [--context N] [--all] [--json]", "An item, section or commit, bounded; a handle ending in @<rev> resolves at that rev"),
    ("crate", "crate <package> [--rev r] [--json]", "A crate's card: description, deps, dependents, apps"),
    ("def", "def <identifier> [--in <circle>,..] [--rev r] [--json]", "Exact definitions of an identifier"),
    ("refs", "refs <identifier> [--uses] [--rev r] [--json]", "Definitions, re-exports, then uses by name"),
    ("history", "history <path|words..> [--rev r] [--json]", "Commits in makepad:history"),
    ("stale", "stale [--rev r] [--json]", "Docs and headers naming things that are gone"),
    ("circles", "circles [--json]", "Configured circles, their sources and index state; an `also:` pointer to a notes circle counts files by substring, so it can differ from that circle's ranked totals"),
    ("build", "build [--in <circle>,..] [--rev r]", "Build or refresh the circles' indexes"),
    ("status", "status [--json]", "What is indexed, at which revision, and what the overlay holds"),
    ("gc", "gc", "Remove index data nothing uses any more"),
    ("eval", "eval [run|verify-gold|diff] [--mode retrieval|agent] [--set shipped|local|heldout|<file>] [--in c,..] [--k 10] [--json] [--save runs/<name>]", "The eval harness; each question is indexed at its own pinned rev"),
];

pub fn usage() -> String {
    let mut out = String::from("makepad index <verb> ...\n\n");
    for (_, usage, summary) in VERBS {
        out.push_str(&format!("  {usage}\n      {summary}\n"));
    }
    out
}

/// What a verb runs over.
pub struct Parts {
    pub circles: Box<dyn Circles>,
    pub search: Box<dyn Search>,
    pub render: Box<dyn Render>,
    /// Git reads in the repo the caller stands in (`show` text, `eval verify-gold`, `gc`).
    pub repo: Box<dyn Repo>,
    /// The index store (`gc`).
    pub store: Box<dyn Store>,
    pub cwd: PathBuf,
}

/// Builds [`Parts`] over the concrete parts: makepad-git for the repo the caller stands in, the
/// file store under [`index_root`], the extractor (notes cite the `makepad` circle at HEAD), the
/// tokenizer, the ranking engine and the renderer.
fn wire() -> Result<Parts, Error> {
    let cwd = std::env::current_dir().map_err(|error| Error::Io { context: "the current directory".into(), error })?;
    let mut repo = GitRepo::open(&cwd)?;
    let head = repo.resolve(&RevSpec::Pinned("HEAD".into()))?;
    let cite = Cite::from_repo(CircleName(circles::MAKEPAD.into()), &mut repo, &head)?;
    let index = index_root()?;
    let open_repo: RepoOpener = Box::new(|root: &Path| Ok(Box::new(GitRepo::open(root)?) as Box<dyn Repo>));
    let opener = Opener::new(
        Box::new(GitRepo::open(&cwd)?),
        open_repo,
        Box::new(FsStore::new(index.clone())),
        Box::new(Extractor { cite: Some(cite) }),
        Box::new(Terms),
        circles::conf_path()?,
        circles::user_home()?,
    );
    Ok(Parts {
        circles: Box::new(opener),
        search: Box::new(Engine::default()),
        render: Box::new(Renderer),
        repo: Box::new(repo),
        store: Box::new(FsStore::new(index)),
        cwd,
    })
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
        Some(verb) if VERBS.iter().any(|(v, _, _)| *v == verb) => match wire() {
            Ok(mut parts) => run(&mut parts, args, &mut std::io::stdout(), &mut std::io::stderr()),
            Err(e) => {
                eprintln!("makepad index {verb}: {e}");
                1
            }
        },
        Some(other) => {
            eprintln!("makepad index: unknown verb `{other}`\n");
            eprint!("{}", usage());
            2
        }
    }
}

/// A verb's words and flags.
#[derive(Debug, Default)]
pub struct Args {
    pub words: Vec<String>,
    pub values: HashMap<&'static str, Vec<String>>,
    pub flags: Vec<&'static str>,
}

impl Args {
    pub fn value(&self, f: &str) -> Option<&str> {
        self.values.get(f).and_then(|v| v.last()).map(String::as_str)
    }
    pub fn has(&self, f: &str) -> bool {
        self.flags.contains(&f)
    }
}

/// Parses `args` against the flags a verb takes (`--x v` or `--x=v`); anything else that starts
/// with `-` is a usage error.
pub fn parse_args(args: &[String], valued: &[&'static str], boolean: &[&'static str]) -> Result<Args, String> {
    let mut out = Args::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if !a.starts_with('-') || a == "-" {
            out.words.push(a.clone());
            continue;
        }
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (a.as_str(), None),
        };
        if let Some(f) = valued.iter().find(|f| **f == flag) {
            let v = match inline {
                Some(v) => v,
                None => it.next().cloned().ok_or_else(|| format!("{flag} needs a value"))?,
            };
            out.values.entry(f).or_default().push(v);
        } else if let Some(f) = boolean.iter().find(|f| **f == flag) {
            if inline.is_some() {
                return Err(format!("{flag} takes no value"));
            }
            out.flags.push(f);
        } else {
            return Err(format!("unknown flag {flag}"));
        }
    }
    Ok(out)
}

/// Parses a handle as hits print it; `circles` are the configured names (for the prefix).
pub fn parse_handle(text: &str, manifest: &CircleManifest) -> Result<Handle, String> {
    let bad = || format!("`{text}` is not a handle (path:line[-end], path::Item, crate:<package> or a commit hex)");
    let (body, rev) = match text.rsplit_once('@') {
        Some((b, r)) if !r.is_empty() && !r.contains('/') => (b, Some(r.to_string())),
        _ => (text, None),
    };
    let mut circle = None;
    let mut rest = body;
    let mut best = 0;
    for d in &manifest.defs {
        if let Some(r) = body.strip_prefix(&format!("{}/", d.name.0)) {
            if d.name.0.len() > best {
                best = d.name.0.len();
                circle = Some(d.name.clone());
                rest = r;
            }
        }
    }
    if rest.is_empty() {
        return Err(bad());
    }
    let target = if let Some(pkg) = rest.strip_prefix("crate:") {
        Target::Crate(pkg.to_string())
    } else if rest.len() >= 7 && rest.len() <= 40 && rest.bytes().all(|b| b.is_ascii_hexdigit()) {
        Target::Commit(rest.to_ascii_lowercase())
    } else if let Some((path, item)) = rest.split_once("::") {
        if path.is_empty() || item.is_empty() {
            return Err(bad());
        }
        Target::Item { path: path.to_string(), item: item.to_string() }
    } else {
        let (path, range) = rest.rsplit_once(':').ok_or_else(bad)?;
        let num = |s: &str| s.parse::<u32>().ok().filter(|n| *n > 0);
        let lines = match range.split_once('-') {
            Some((a, b)) => Lines { start: num(a).ok_or_else(bad)?, end: num(b).ok_or_else(bad)? },
            None => {
                let n = num(range).ok_or_else(bad)?;
                Lines { start: n, end: n }
            }
        };
        if path.is_empty() || lines.end < lines.start {
            return Err(bad());
        }
        Target::Lines { path: path.to_string(), lines }
    };
    Ok(Handle { circle, target, rev })
}

/// Runs one verb (`args[0]`), writing its answer to `out` and errors to `err`.
pub fn run(parts: &mut Parts, args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let verb = args.first().map(String::as_str).unwrap_or("");
    match dispatch(parts, verb, &args[1.min(args.len())..], out) {
        Ok(code) => code,
        Err(Failure::Usage(m)) => {
            let line = VERBS.iter().find(|(v, _, _)| *v == verb).map(|(_, u, _)| *u).unwrap_or("");
            let _ = writeln!(err, "makepad index {verb}: {m}\nusage: makepad index {line}");
            2
        }
        Err(Failure::NotBuilt(m)) => {
            let _ = writeln!(err, "makepad index {verb}: not built yet: {m}");
            2
        }
        Err(Failure::Error(e)) => {
            let _ = writeln!(err, "makepad index {verb}: {e}");
            1
        }
    }
}

enum Failure {
    Usage(String),
    NotBuilt(String),
    Error(Error),
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure::Error(e)
    }
}

fn usage_err(m: impl Into<String>) -> Failure {
    Failure::Usage(m.into())
}

fn io(e: std::io::Error) -> Failure {
    Failure::Error(Error::Io { context: "writing the answer".into(), error: e })
}

fn emit(parts: &Parts, out: &mut dyn Write, answer: &Output, json: bool) -> Result<i32, Failure> {
    let text = if json { parts.render.json_lines(&parts.render.records(answer)) } else { parts.render.text(answer) };
    out.write_all(text.as_bytes()).map_err(io)?;
    Ok(0)
}

fn rev_of(a: &Args) -> RevSpec {
    a.value("--rev").map_or(RevSpec::Worktree, |r| RevSpec::Pinned(r.to_string()))
}

/// The circles `--in` names, else the default scope of `cwd` (else `fallback`, a verb's own).
fn scope(parts: &Parts, manifest: &CircleManifest, a: &Args, fallback: Option<&str>) -> Result<Vec<CircleExpr>, Failure> {
    if let Some(arg) = a.value("--in") {
        return Ok(parts.circles.parse_in(manifest, arg)?);
    }
    if let Some(f) = fallback {
        return Ok(parts.circles.parse_in(manifest, f)?);
    }
    let names = parts.circles.default_scope(manifest, &parts.cwd);
    if names.is_empty() {
        return Err(usage_err(format!("{} is in no circle's sources; name circles with --in", parts.cwd.display())));
    }
    Ok(names.into_iter().map(CircleExpr::Circle).collect())
}

fn bases(manifest: &CircleManifest, exprs: &[CircleExpr]) -> Result<Vec<CircleName>, Failure> {
    let mut out: Vec<CircleName> = Vec::new();
    for e in exprs {
        for b in circles::base_names(manifest, e)? {
            if !out.contains(&b) {
                out.push(b);
            }
        }
    }
    Ok(out)
}

fn short(rev: &Option<crate::Rev>) -> String {
    rev.as_ref().map(|r| crate::open::hex(&r.commit)[..9].to_string()).unwrap_or_default()
}

fn dispatch(parts: &mut Parts, verb: &str, args: &[String], out: &mut dyn Write) -> Result<i32, Failure> {
    match verb {
        "find" | "def" | "history" => {
            let a = parse_args(args, &["--in", "--kind", "--crate", "--path", "-n", "--rev"], &["--json"]).map_err(usage_err)?;
            if a.words.is_empty() {
                return Err(usage_err("no words given"));
            }
            let (mode, fallback) = match verb {
                "def" => (Mode::Def, None),
                "history" => (Mode::History, Some(circles::MAKEPAD_HISTORY)),
                _ => (Mode::Find, None),
            };
            let kinds: Vec<KindFilter> = match a.value("--kind") {
                Some(k) => circles::parse_kinds(k).map_err(usage_err)?,
                None => Vec::new(),
            };
            let limit = match a.value("-n") {
                Some(n) => n.parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(|| usage_err("-n takes a positive number"))?,
                None => DEFAULT_LIMIT,
            };
            let manifest = parts.circles.manifest()?;
            let exprs = scope(parts, &manifest, &a, fallback)?;
            let rev = rev_of(&a);
            let opened = parts.circles.open(&manifest, &bases(&manifest, &exprs)?, &rev)?;
            let query = Query {
                mode,
                words: a.words.clone(),
                circles: exprs,
                rev,
                kinds,
                packages: a.values.get("--crate").cloned().unwrap_or_default(),
                paths: a.values.get("--path").cloned().unwrap_or_default(),
                limit,
                pointers: true,
            };
            let answer = parts.search.find(&opened, &query)?;
            emit(parts, out, &Output::Find(answer), a.has("--json"))
        }
        "show" => {
            let a = parse_args(args, &["--in", "--rev", "--context"], &["--all", "--json"]).map_err(usage_err)?;
            let [handle_text] = a.words.as_slice() else { return Err(usage_err("show takes one handle")) };
            let context: u32 = match a.value("--context") {
                Some(n) => n.parse().map_err(|_| usage_err("--context takes a number"))?,
                None => 0,
            };
            let manifest = parts.circles.manifest()?;
            let handle = parse_handle(handle_text, &manifest).map_err(usage_err)?;
            let exprs = match &handle.circle {
                Some(c) => vec![CircleExpr::Circle(c.clone())],
                None => scope(parts, &manifest, &a, None)?,
            };
            let rev = handle.rev.clone().map(RevSpec::Pinned).unwrap_or_else(|| rev_of(&a));
            let opened = parts.circles.open(&manifest, &bases(&manifest, &exprs)?, &rev)?;
            let found = parts.search.resolve(&opened, &handle)?;
            let Some(at) = found.first() else {
                return Err(Failure::Error(Error::Rev(format!("no record at {handle_text}"))));
            };
            let shown = show(parts.repo.as_mut(), &manifest, &opened, *at, handle, context, a.has("--all"))?;
            emit(parts, out, &Output::Show(Box::new(shown)), a.has("--json"))
        }
        "crate" => {
            let a = parse_args(args, &["--rev"], &["--json"]).map_err(usage_err)?;
            let [pkg] = a.words.as_slice() else { return Err(usage_err("crate takes one package name")) };
            let manifest = parts.circles.manifest()?;
            let exprs = scope(parts, &manifest, &a, None)?;
            let opened = parts.circles.open(&manifest, &bases(&manifest, &exprs)?, &rev_of(&a))?;
            let handle = Handle { circle: None, target: Target::Crate(pkg.clone()), rev: None };
            let found = parts.search.resolve(&opened, &handle)?;
            let card = found.iter().find_map(|l| {
                let c = &opened.open[l.circle];
                let detail = match l.at {
                    ChunkAt::Segment { segment, chunk } => c.segments[segment].detail(chunk),
                    ChunkAt::Overlay { file, chunk } => c.overlay[file].chunks[chunk].0.detail.clone(),
                };
                match detail {
                    Detail::Crate(card) => Some((c, card)),
                    _ => None,
                }
            });
            let Some((c, card)) = card else {
                return Err(Failure::Error(Error::Rev(format!("no crate `{pkg}` in {}", names(&opened)))));
            };
            let view = CrateView { circle: c.name.clone(), rev: short(&c.rev), name: pkg.clone(), card };
            emit(parts, out, &Output::Crate(Box::new(view)), a.has("--json"))
        }
        "refs" => {
            let a = parse_args(args, &["--rev"], &["--uses", "--json"]).map_err(usage_err)?;
            let [ident] = a.words.as_slice() else { return Err(usage_err("refs takes one identifier")) };
            let manifest = parts.circles.manifest()?;
            let exprs = scope(parts, &manifest, &a, None)?;
            let opened = parts.circles.open(&manifest, &bases(&manifest, &exprs)?, &rev_of(&a))?;
            let answer = parts.search.refs(&opened, ident, a.has("--uses"))?;
            emit(parts, out, &Output::Refs(Box::new(answer)), a.has("--json"))
        }
        "circles" | "status" | "build" => {
            let a = if verb == "build" {
                parse_args(args, &["--in", "--rev"], &[])
            } else {
                parse_args(args, &[], &["--json"])
            }
            .map_err(usage_err)?;
            if !a.words.is_empty() {
                return Err(usage_err(format!("{verb} takes no words")));
            }
            let manifest = parts.circles.manifest()?;
            let named = match verb {
                "circles" => Vec::new(),
                _ => bases(&manifest, &scope(parts, &manifest, &a, None)?)?,
            };
            let opened = parts.circles.open(&manifest, &named, &rev_of(&a))?;
            emit(parts, out, &Output::Status(status(&manifest, &opened)), a.has("--json"))
        }
        "stale" => Err(Failure::NotBuilt("`stale` arrives in phase 4 (PLAN §10)".into())),
        "gc" => {
            let a = parse_args(args, &[], &[]).map_err(usage_err)?;
            if !a.words.is_empty() {
                return Err(usage_err("gc takes no words"));
            }
            let manifest = parts.circles.manifest()?;
            let (bases, circles_live) = live(&manifest, parts.repo.as_mut(), parts.circles.as_ref())?;
            let report = parts.store.gc(parts.repo.id(), &bases, &circles_live)?;
            for p in &report.removed {
                writeln!(out, "removed {}", p.display()).map_err(io)?;
            }
            for (p, why) in &report.deferred {
                writeln!(out, "kept {}: {why}", p.display()).map_err(io)?;
            }
            writeln!(out, "gc: {} live bases, {} removed, {} kept for now", bases.len(), report.removed.len(), report.deferred.len()).map_err(io)?;
            Ok(0)
        }
        "eval" => eval_verb(parts, args, out),
        other => Err(usage_err(format!("unknown verb `{other}`"))),
    }
}

/// A manifest circle GC keeps: its repo (`None` for a machine's notes circle) and name.
pub type LiveCircle = (Option<RepoId>, CircleName);

/// What GC keeps (PLAN §6): for each tree circle of this repo, the base of every worktree's
/// merge-base with the circle's upstream, and the upstream tip's own base; and every configured
/// circle that keeps a manifest (history, notes). Tree circles of other repos are not this
/// repo's: their bases are not kept here and rebuild on first use.
pub fn live(
    manifest: &CircleManifest,
    repo: &mut dyn Repo,
    circles_reg: &dyn Circles,
) -> Result<(Vec<SegmentKey>, Vec<LiveCircle>), Error> {
    use crate::{BaseCircle, BaseSource, CircleBody};
    let root = repo.root();
    let mut keys: Vec<SegmentKey> = Vec::new();
    let mut circles_live = Vec::new();
    let worktrees = repo.worktrees()?;
    for d in &manifest.defs {
        let CircleBody::Base(def @ BaseCircle { source, .. }) = &d.body else { continue };
        match source {
            BaseSource::Tree { root: r } if r == &root => {
                let upstream = manifest.defs.iter().find_map(|h| match &h.body {
                    CircleBody::Base(BaseCircle { source: BaseSource::History { root: hr, upstream, .. }, .. }) if hr == r => Some(upstream.clone()),
                    _ => None,
                });
                let up = match &upstream {
                    Some(u) => match repo.resolve(&RevSpec::Pinned(u.clone())) {
                        Ok(x) => Some(x.commit),
                        Err(e) => {
                            eprintln!("makepad index gc: {u} does not resolve ({e}); each worktree keeps the base of its HEAD");
                            None
                        }
                    },
                    None => None,
                };
                let mut commits: Vec<Oid> = up.into_iter().collect();
                for (_, head) in &worktrees {
                    let base = match up {
                        Some(u) => repo.merge_base(*head, u)?.unwrap_or(*head),
                        None => *head,
                    };
                    commits.push(base);
                }
                for c in commits {
                    let tree = repo.resolve(&RevSpec::Pinned(crate::open::hex(&c)))?.tree;
                    let key = SegmentKey {
                        repo: Some(repo.id()),
                        circle: d.name.clone(),
                        source: SourceStamp::Tree { tree },
                        def_hash: circles_reg.def_hash(def),
                        indexer: INDEXER_VERSION,
                    };
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
            }
            BaseSource::Tree { .. } => {}
            BaseSource::History { .. } => circles_live.push((Some(repo.id()), d.name.clone())),
            BaseSource::Notes { .. } => {
                let repo_key = (d.name.0 == circles::NOTES_LOCAL).then(|| repo.id());
                circles_live.push((repo_key, d.name.clone()));
            }
        }
    }
    Ok((keys, circles_live))
}

fn names(opened: &Opened) -> String {
    opened.open.iter().map(|c| c.name.0.as_str()).collect::<Vec<_>>().join(", ")
}

/// One line describing a circle's source.
fn source_line(def: &crate::CircleDef) -> String {
    use crate::{BaseSource, CircleBody};
    match &def.body {
        CircleBody::Base(b) => match &b.source {
            BaseSource::Tree { root } => format!("tree {}", root.display()),
            BaseSource::History { upstream, local, .. } => format!("history {upstream} {}", local.join(" ")),
            BaseSource::Notes { sources } => {
                format!("notes {}", sources.iter().map(|s| s.label.as_str()).collect::<Vec<_>>().join(" "))
            }
        },
        CircleBody::Derived(e) => {
            format!("derived over {}", circles::expr_names(e).iter().map(|n| n.0.as_str()).collect::<Vec<_>>().join(" "))
        }
    }
}

fn status(manifest: &CircleManifest, opened: &Opened) -> StatusOutput {
    let find = |n: &CircleName| opened.open.iter().chain(&opened.others).find(|c| &c.name == n);
    let circles = manifest
        .defs
        .iter()
        .map(|d| {
            let open: Option<&OpenCircle> = find(&d.name);
            let derived = matches!(d.body, crate::CircleBody::Derived(_));
            CircleStatus {
                name: d.name.clone(),
                origin: d.origin.clone(),
                source: source_line(d),
                rev: open.map(|c| short(&c.rev)).unwrap_or_default(),
                segments: open.map(|c| c.segments.iter().map(|s| s.id()).collect()).unwrap_or_default(),
                chunks: open.map_or(0, |c| c.stats.chunks),
                overlay_files: open.map_or(0, |c| c.overlay.len() as u64),
                built: open.is_some(),
                note: if derived {
                    "derived: a view, nothing to build".into()
                } else if open.is_none() {
                    format!("not built: `makepad index build --in {}`", d.name.0)
                } else {
                    String::new()
                },
            }
        })
        .collect();
    StatusOutput { circles }
}

/// The record a [`Located`] names, as `show` prints it.
fn show(
    repo: &mut dyn Repo,
    manifest: &CircleManifest,
    opened: &Opened,
    at: Located,
    handle: Handle,
    context: u32,
    all: bool,
) -> Result<ShowOutput, Failure> {
    let c = &opened.open[at.circle];
    let (kind, path, lines, name, package, blob, commit, detail) = match at.at {
        ChunkAt::Segment { segment, chunk } => {
            let v = c.segments[segment].chunk(chunk);
            (v.kind, v.path.to_string(), v.lines, v.name.to_string(), v.package.to_string(), v.blob, v.commit, c.segments[segment].detail(chunk))
        }
        ChunkAt::Overlay { file, chunk } => {
            let ch = &c.overlay[file].chunks[chunk].0;
            (ch.kind, ch.path.clone(), ch.lines, ch.name.clone(), ch.keys.package.clone().unwrap_or_default(), ch.keys.blob, ch.keys.commit, ch.detail.clone())
        }
    };
    let overlay = matches!(at.at, ChunkAt::Overlay { .. });
    let source = source_text(repo, manifest, c, &Src { path: &path, blob, commit, detail: &detail, overlay })?;
    let all_lines: Vec<&str> = source.lines().collect();
    let (from, to) = if kind.family() == crate::Family::Commit || lines.start == 0 {
        (1, all_lines.len() as u32)
    } else {
        (lines.start.saturating_sub(context).max(1), (lines.end + context).min(all_lines.len() as u32))
    };
    let span: Vec<String> = all_lines.iter().skip(from as usize - 1).take((to + 1).saturating_sub(from) as usize).map(|s| s.to_string()).collect();
    let cap = if all { span.len() } else { SHOW_MAX_LINES.min(span.len()) };
    let more = (span.len() - cap) as u32;
    let text = span.into_iter().take(cap).collect();
    let mut cited_by = Vec::new();
    for other in opened.open.iter().chain(&opened.others).filter(|o| o.name != c.name) {
        let n: usize = other.segments.iter().map(|s| s.cited_by(&Key::Path { path: path.clone(), line: None }).len()).sum();
        if n > 0 {
            cited_by.push((other.name.clone(), n as u64));
        }
    }
    let mut also_at = Vec::new();
    if let Some(b) = blob {
        for s in &c.segments {
            for id in s.chunks_with(&Key::Blob(b)) {
                let p = s.chunk(id).path.to_string();
                if p != path && !also_at.contains(&p) {
                    also_at.push(p);
                }
            }
        }
    }
    Ok(ShowOutput {
        circle: c.name.clone(),
        rev: short(&c.rev),
        handle,
        kind,
        package,
        path,
        lines: Lines { start: from, end: to },
        name,
        text,
        more,
        uses: None,
        cited_by,
        also_at,
    })
}

/// Where a record's text comes from.
struct Src<'a> {
    path: &'a str,
    blob: Option<Oid>,
    commit: Option<Oid>,
    detail: &'a Detail,
    /// From the worktree overlay.
    overlay: bool,
}

/// The text a record's lines index into: a commit's message, a notes file, the worktree file
/// (overlay hits, shadowed paths), else the blob at the circle's revision.
fn source_text(repo: &mut dyn Repo, manifest: &CircleManifest, c: &OpenCircle, src: &Src) -> Result<String, Failure> {
    let Src { path, blob, commit, detail, overlay } = *src;
    if let (Some(oid), Detail::Commit(info)) = (commit, detail) {
        let found = repo.commits(&[oid], &info.parents)?;
        return found
            .into_iter()
            .find(|r| r.oid == oid)
            .map(|r| r.message)
            .ok_or_else(|| Failure::Error(Error::Git(format!("commit {} not found", crate::open::hex(&oid)))));
    }
    if c.rev.is_none() {
        // a notes record: `<label>/<path in dir>` on disk
        let file = notes_file(manifest, &c.name, path).ok_or_else(|| Failure::Error(Error::Rev(format!("{path}: no source of {} has its label", c.name.0))))?;
        let bytes = std::fs::read(&file).map_err(|error| Error::Io { context: format!("reading {}", file.display()), error })?;
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    if overlay || c.shadowed.iter().any(|s| s == path) {
        let file = repo.root().join(path);
        let bytes = std::fs::read(&file).map_err(|error| Error::Io { context: format!("reading {}", file.display()), error })?;
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    let blob = match blob {
        Some(b) => b,
        None => {
            let tree = c.rev.as_ref().map(|r| r.tree).expect("a tree circle has a revision");
            repo.files(tree)?.into_iter().find(|f| f.path == path).map(|f| f.blob).ok_or_else(|| Failure::Error(Error::Rev(format!("{path} is not in the tree"))))?
        }
    };
    Ok(String::from_utf8_lossy(&repo.read_blob(blob)?).into_owned())
}

fn notes_file(manifest: &CircleManifest, circle: &CircleName, path: &str) -> Option<PathBuf> {
    use crate::{BaseCircle, BaseSource, CircleBody};
    let def = circles::find_def(manifest, circle)?;
    let CircleBody::Base(BaseCircle { source: BaseSource::Notes { sources }, .. }) = &def.body else { return None };
    sources
        .iter()
        .filter_map(|s| path.strip_prefix(&format!("{}/", s.label)).map(|rest| (s.label.len(), s.dir.join(rest))))
        .max_by_key(|(n, _)| *n)
        .map(|(_, p)| p)
}

/// The sets `--set` names (default: shipped, plus every machine-local `eval =` file).
fn sets(a: &Args, manifest: &CircleManifest) -> Result<Vec<QuestionSet>, Failure> {
    let mut out = Vec::new();
    let names = a.values.get("--set").cloned().unwrap_or_else(|| vec!["shipped".into(), "local".into()]);
    for s in names {
        match s.as_str() {
            "shipped" => out.push(QuestionSet::Shipped),
            "local" => out.extend(manifest.eval.iter().cloned().map(QuestionSet::Local)),
            "heldout" => return Err(usage_err("--set heldout:<file>: the held-out file is passed, never kept in a tree")),
            other => match other.strip_prefix("heldout:") {
                Some(p) => out.push(QuestionSet::Heldout(PathBuf::from(p))),
                None => out.push(QuestionSet::File(PathBuf::from(other))),
            },
        }
    }
    Ok(out)
}

fn eval_verb(parts: &mut Parts, args: &[String], out: &mut dyn Write) -> Result<i32, Failure> {
    let a = parse_args(args, &["--mode", "--set", "--in", "--k", "--save"], &["--json"]).map_err(usage_err)?;
    let sub = a.words.first().map(String::as_str).unwrap_or("run");
    if sub == "diff" {
        let [_, x, y] = a.words.as_slice() else { return Err(usage_err("eval diff <run a> <run b>")) };
        let d = eval::diff_runs(Path::new(x), Path::new(y))?;
        writeln!(out, "{d}").map_err(io)?;
        return Ok(0);
    }
    if !matches!(sub, "run" | "verify-gold") || a.words.len() > 1 {
        return Err(usage_err(format!("unknown eval command `{}`", a.words.join(" "))));
    }
    let mode = match a.value("--mode") {
        None | Some("retrieval") => EvalMode::Retrieval,
        Some("agent") => EvalMode::Agent,
        Some(m) => return Err(usage_err(format!("unknown mode `{m}`"))),
    };
    let k = match a.value("--k") {
        Some(k) => k.parse().ok().filter(|k| *k > 0).ok_or_else(|| usage_err("--k takes a positive number"))?,
        None => DEFAULT_LIMIT,
    };
    let manifest = parts.circles.manifest()?;
    let circles_in: Vec<CircleName> = match a.value("--in") {
        Some(arg) => bases(&manifest, &parts.circles.parse_in(&manifest, arg)?)?,
        None => Vec::new(),
    };
    let sets = sets(&a, &manifest)?;
    let root = parts.repo.root();
    let Parts { circles, search, render, repo, .. } = parts;
    let mut harness = eval::Eval { circles: circles.as_mut(), search: search.as_ref(), render: render.as_ref(), repo: repo.as_mut(), root };
    let questions = harness.load(&sets)?;
    if sub == "verify-gold" {
        let drifts = harness.verify_gold(&questions)?;
        for d in &drifts {
            writeln!(out, "{}#{} {:?}", d.id, d.gold, d.drift).map_err(io)?;
        }
        writeln!(out, "verify-gold: {} questions, {} changes", questions.len(), drifts.len()).map_err(io)?;
        return Ok(if drifts.is_empty() { 0 } else { 1 });
    }
    let opts = EvalOpts { mode, sets, circles: circles_in, k, json: a.has("--json"), save: a.value("--save").map(PathBuf::from) };
    let report = harness.run(&questions, &opts)?;
    let types: HashMap<&str, &str> = questions.iter().map(|q| (q.id.as_str(), q.qtype.as_str())).collect();
    let text = if opts.json { eval::report_json(&report) } else { eval::report_text(&report, &|id| types.get(id).unwrap_or(&"?").to_string()) };
    out.write_all(text.as_bytes()).map_err(io)?;
    Ok(if report.failures.is_empty() { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open::fakes::{opener, FakeRepo};
    use crate::{FindOutput, Kind, Record, RefsOutput};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Seen {
        queries: Vec<Query>,
        opened: Vec<Vec<String>>,
    }

    /// Answers with one block per opened circle; resolves `Lines` handles to the first chunk of
    /// the file; records what it was asked.
    struct EchoSearch(Rc<RefCell<Seen>>);

    impl Search for EchoSearch {
        fn find(&self, opened: &Opened, query: &Query) -> Result<FindOutput, Error> {
            let mut s = self.0.borrow_mut();
            s.queries.push(query.clone());
            s.opened.push(opened.open.iter().map(|c| c.name.0.clone()).collect());
            Ok(FindOutput { blocks: opened.open.iter().map(|c| crate::eval::score::tests::block(&c.name.0, vec![], true)).collect(), pointers: vec![] })
        }
        fn resolve(&self, opened: &Opened, handle: &Handle) -> Result<Vec<Located>, Error> {
            let Target::Lines { path, .. } = &handle.target else { return Ok(Vec::new()) };
            for (ci, c) in opened.open.iter().enumerate() {
                for (si, s) in c.segments.iter().enumerate() {
                    for i in 0..s.chunk_count() {
                        if s.chunk(crate::ChunkId(i)).path == path && s.chunk(crate::ChunkId(i)).kind == Kind::Fn {
                            return Ok(vec![Located { circle: ci, at: ChunkAt::Segment { segment: si, chunk: crate::ChunkId(i) } }]);
                        }
                    }
                }
            }
            Ok(Vec::new())
        }
        fn refs(&self, _: &Opened, ident: &str, _: bool) -> Result<RefsOutput, Error> {
            Err(Error::Git(format!("refs {ident}: not in this test")))
        }
    }

    struct DebugRender;
    impl Render for DebugRender {
        fn text(&self, out: &Output) -> String {
            match out {
                Output::Show(s) => format!("{}:{}-{}\n{}\n", s.path, s.lines.start, s.lines.end, s.text.join("\n")),
                Output::Status(s) => s.circles.iter().map(|c| format!("{} built={} {}\n", c.name.0, c.built, c.note)).collect(),
                Output::Find(f) => f.blocks.iter().map(|b| format!("{} absent={}\n", b.circle.0, b.absent)).collect(),
                _ => String::new(),
            }
        }
        fn records(&self, _: &Output) -> Vec<Record> {
            Vec::new()
        }
        fn json_lines(&self, _: &[Record]) -> String {
            "{}\n".into()
        }
    }

    fn parts(tag: &str) -> (Parts, Rc<RefCell<Seen>>, PathBuf) {
        let (p, seen, root, _) = parts_and_store(tag);
        (p, seen, root)
    }

    fn parts_and_store(tag: &str) -> (Parts, Rc<RefCell<Seen>>, PathBuf, crate::open::fakes::FakeStore) {
        let root = std::env::temp_dir().join(format!("repo-index-cli-{tag}-{}", std::process::id())).join("makepad");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
        std::fs::create_dir_all(root.join("src")).unwrap();
        let files: &[(&str, &str)] = &[("src/a.rs", "// a\n// b\nfn alpha() {}\n// d\n// e\n")];
        let mut repo = FakeRepo::new(&root);
        repo.rev(&["HEAD", "refs/remotes/upstream/work", "9a76551d7"], 1, files);
        let mut show_repo = FakeRepo::new(&root);
        show_repo.rev(&["HEAD", "refs/remotes/upstream/work", "9a76551d7"], 1, files);
        let (circles, store) = opener(repo, &root.join("none.conf"));
        let seen = Rc::new(RefCell::new(Seen::default()));
        let p = Parts {
            circles: Box::new(circles),
            search: Box::new(EchoSearch(seen.clone())),
            render: Box::new(DebugRender),
            repo: Box::new(show_repo),
            store: Box::new(store.clone()),
            cwd: root.join("src"),
        };
        (p, seen, root, store)
    }

    fn call(p: &mut Parts, args: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(p, &args, &mut out, &mut err);
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn args_and_handles_parse() {
        let a = parse_args(&["x".into(), "--in=a,b".into(), "-n".into(), "3".into(), "--json".into(), "y".into()], &["--in", "-n"], &["--json"]).unwrap();
        assert_eq!(a.words, ["x", "y"]);
        assert_eq!((a.value("--in"), a.value("-n"), a.has("--json")), (Some("a,b"), Some("3"), true));
        assert!(parse_args(&["--bogus".into()], &[], &[]).is_err());
        assert!(parse_args(&["--in".into()], &["--in"], &[]).is_err());
        let m = circles::manifest_for(Path::new("/r/makepad"), Some((Path::new("c"), "notes:t = notes /n/t\n")), Path::new("/h")).unwrap();
        let h = |s: &str| parse_handle(s, &m);
        assert_eq!(h("src/a.rs:3-9").unwrap().target, Target::Lines { path: "src/a.rs".into(), lines: Lines { start: 3, end: 9 } });
        assert_eq!(h("src/a.rs::Gizmo::spin").unwrap().target, Target::Item { path: "src/a.rs".into(), item: "Gizmo::spin".into() });
        assert_eq!(h("crate:makepad-hal").unwrap().target, Target::Crate("makepad-hal".into()));
        assert_eq!(h("2271C0EB3").unwrap().target, Target::Commit("2271c0eb3".into()));
        let n = h("notes:t/t/PLAN.md:64@9a76551d7").unwrap();
        assert_eq!((n.circle.unwrap().0, n.rev.unwrap()), ("notes:t".to_string(), "9a76551d7".to_string()));
        assert!(matches!(n.target, Target::Lines { ref path, .. } if path == "t/PLAN.md"));
        assert_eq!(h("makepad:history/abc1234").unwrap().circle, Some(CircleName("makepad:history".into())));
        for bad in ["src/a.rs", "a.rs:0", "a.rs:9-3", "a.rs:x", "::x", "notes:t/"] {
            assert!(h(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn find_takes_the_default_scope_or_in_and_fails_on_unknown_circles() {
        let (mut p, seen, _) = parts("find");
        let (code, out, _) = call(&mut p, &["find", "alpha", "--kind", "fn", "-n", "3"]);
        assert_eq!(code, 0);
        assert_eq!(out, "makepad absent=true\n", "absence is an answer, exit 0");
        let q = seen.borrow().queries[0].clone();
        assert_eq!((q.limit, q.kinds.len(), q.mode), (3, 1, Mode::Find));
        assert_eq!(seen.borrow().opened[0], ["makepad"]);
        let (code, _, err) = call(&mut p, &["find", "alpha", "--in", "notes:nope"]);
        assert_eq!(code, 1);
        assert!(err.contains("unknown circle `notes:nope`"), "{err}");
        assert_eq!(call(&mut p, &["find"]).0, 2);
        assert_eq!(call(&mut p, &["find", "x", "--kind", "fnn"]).0, 2);
        assert_eq!(call(&mut p, &["find", "x", "--json"]).1, "{}\n");
        let (code, _, err) = call(&mut p, &["stale"]);
        assert!(code == 2 && err.contains("not built yet"), "{err}");
    }

    #[test]
    fn show_prints_the_item_bounded_with_context() {
        let (mut p, _, _) = parts("show");
        let (code, out, err) = call(&mut p, &["show", "src/a.rs:3", "--context", "1"]);
        assert_eq!(code, 0, "{err}");
        assert_eq!(out, "src/a.rs:2-4\n// b\nfn alpha() {}\n// d\n");
        let (code, _, err) = call(&mut p, &["show", "src/nothing.rs:1"]);
        assert!(code == 1 && err.contains("no record at"), "{err}");
        assert_eq!(call(&mut p, &["show", "not-a-handle"]).0, 2);
    }

    #[test]
    fn status_lists_every_circle_and_what_is_built() {
        let (mut p, _, _) = parts("status");
        let (code, out, _) = call(&mut p, &["circles"]);
        assert_eq!(code, 0);
        assert!(out.contains("makepad built=false not built: `makepad index build --in makepad`"), "{out}");
        assert_eq!(call(&mut p, &["build"]).0, 0);
        let (_, out, _) = call(&mut p, &["status"]);
        assert!(out.contains("makepad built=true"), "{out}");
        let (code, out, _) = call(&mut p, &["gc"]);
        assert!(code == 0 && out.contains("gc: 1 live bases"), "{out}");
    }

    #[test]
    fn gc_keeps_every_worktrees_base_and_the_manifest_circles() {
        let root = std::env::temp_dir().join(format!("repo-index-cli-gc-{}", std::process::id())).join("makepad");
        std::fs::create_dir_all(root.join("local")).unwrap();
        let mut repo = FakeRepo::new(&root);
        let up = repo.rev(&["refs/remotes/upstream/work"], 1, &[]);
        let a = repo.rev(&["a"], 2, &[]);
        let b = repo.rev(&["b"], 3, &[]);
        let mb = repo.rev(&["mb"], 4, &[]);
        repo.merge_bases.insert((a.commit, up.commit), mb.commit);
        repo.merge_bases.insert((b.commit, up.commit), up.commit);
        repo.worktrees = vec![(root.clone(), a.commit), (root.join("../wt"), b.commit)];
        let m = circles::manifest_for(&root, None, &root).unwrap();
        let (reg, _) = opener(FakeRepo::new(&root), &root.join("none.conf"));
        let (keys, live_circles) = live(&m, &mut repo, &reg).unwrap();
        let trees: Vec<Oid> = keys.iter().map(|k| match k.source { SourceStamp::Tree { tree } => tree, _ => unreachable!() }).collect();
        assert_eq!(trees, [up.tree, mb.tree], "the upstream tip, and each worktree's merge-base once");
        let names: Vec<&str> = live_circles.iter().map(|(_, n)| n.0.as_str()).collect();
        assert_eq!(names, ["makepad:history", "notes:local"]);
        assert!(live_circles.iter().all(|(r, _)| r.is_some()), "both belong to the repo");
    }

    #[test]
    fn gc_runs_for_this_repo_only() {
        let (mut p, _, _, store) = parts_and_store("gcscope");
        assert_eq!(call(&mut p, &["build"]).0, 0);
        assert_eq!(call(&mut p, &["gc"]).0, 0);
        let st = store.state.borrow();
        let [(repo, keys, live_circles)] = st.gc_calls.as_slice() else { panic!("one gc call, got {}", st.gc_calls.len()) };
        assert_eq!(*repo, p.repo.id(), "the sweep is scoped to the repo gc runs from");
        assert!(!keys.is_empty() && keys.iter().all(|k| k.repo == Some(*repo)), "only this repo's bases are named live");
        assert!(live_circles.iter().all(|(r, _)| r.is_none() || *r == Some(*repo)));
    }

    #[test]
    fn eval_sets_and_commands() {
        let (mut p, _, _) = parts("eval");
        let (code, _, err) = call(&mut p, &["eval", "--set", "heldout"]);
        assert!(code == 2 && err.contains("held-out file is passed"), "{err}");
        assert_eq!(call(&mut p, &["eval", "bogus"]).0, 2);
        assert_eq!(call(&mut p, &["eval", "--mode", "x"]).0, 2);
        let (code, _, err) = call(&mut p, &["eval", "--set", "/no/such/file.jsonl"]);
        assert!(code == 1 && err.contains("questions /no/such/file.jsonl"), "{err}");
    }
}
