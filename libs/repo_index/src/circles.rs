//! The circle registry: the built-in circles of the repo the caller stands in, plus
//! `~/.makepad/index/circles.conf`, as a [`crate::CircleManifest`]; the default scope; `--in`.
//!
//! Built in: `makepad` (the tree; excludes [`crate::INDEX_OWN_DATA`]), `makepad:history`, and
//! `commercial` / `commercial:history` when `apps/commercial` is present. Machine-local:
//! `notes:local`, the repo's untracked `local/` when present, minus the directories other circles
//! claim ([`crate::Provenance::ClaimedBy`]), and every `circles.conf` circle. A circle that is not
//! configured is [`crate::Error::UnknownCircle`], never an empty result. Two sources of one circle
//! with the same label are a `circles.conf` error. Owns
//! [`crate::Circles::def_hash`]: SHA-1 over a canonical encoding of families, exclusions, ref
//! names and notes labels, never machine paths.
//!
//! `circles.conf`, one definition per line, `#` comments; a name defined twice adds sources:
//! ```text
//! notes:<topic> = notes ~/notes/<topic>              # a notes directory, label `<topic>`
//! notes:<topic> = notes local/agent_state/<dir>      # a repo-local dir it claims from notes:local,
//!                                                    # label `<repo dir>/local/agent_state/<dir>`
//! notes:<topic> = notes ~/x/y as <label>             # an explicit label
//! <repo> = tree ~/src/<repo>                         # another repo, a consumer, never a member
//! <repo>:history = history ~/src/<repo> refs/remotes/origin/main refs/heads/main
//! <name> = select <circle> package <pkg>             # derived: kind <k>,.. | path <prefix> | package <p>
//! <name> = overlap <circle> <circle> on path         # derived: on path | commit | blob | ident | package
//! <name> = union <circle> <circle> ..                # derived: blocks (default) or `fuse rank`
//! eval = ~/notes/<topic>/questions.jsonl             # machine-local eval questions
//! ```
//! A relative directory is relative to the repo root. Only notes circles may be defined twice;
//! built-in names cannot be redefined. A derived circle may name circles defined after it.

use crate::{
    query, BaseCircle, BaseSource, CircleBody, CircleDef, CircleExpr, CircleManifest, CircleName, Error, Exclusion, Family,
    Fuse, KeyKind, NotesSource, Origin, Provenance, Selector, INDEX_OWN_DATA,
};
use std::path::{Component, Path, PathBuf};

pub const MAKEPAD: &str = "makepad";
pub const MAKEPAD_HISTORY: &str = "makepad:history";
pub const COMMERCIAL: &str = "commercial";
pub const COMMERCIAL_HISTORY: &str = "commercial:history";
pub const NOTES_LOCAL: &str = "notes:local";
/// What `makepad:history` walks: upstream `work`, plus the local `work` branch's commits.
pub const MAKEPAD_UPSTREAM: &str = "refs/remotes/upstream/work";
pub const MAKEPAD_LOCAL: &[&str] = &["refs/heads/work"];
/// What `commercial:history` walks: its own repo's `main`.
pub const COMMERCIAL_UPSTREAM: &str = "refs/remotes/origin/main";
pub const COMMERCIAL_LOCAL: &[&str] = &["refs/heads/main"];

/// The families a tree, a history and a notes circle admit.
pub const TREE_FAMILIES: &[Family] = &[Family::Crate, Family::File, Family::Item, Family::Doc, Family::Splash];
pub const HISTORY_FAMILIES: &[Family] = &[Family::Commit];
pub const NOTES_FAMILIES: &[Family] = &[Family::Note];

/// The user's home directory, for `~` in `circles.conf`.
pub fn user_home() -> Result<PathBuf, Error> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Error::Io {
            context: "the index home".into(),
            error: std::io::Error::new(std::io::ErrorKind::NotFound, "neither MAKEPAD_HOME nor the user's home is set"),
        })
}

/// `circles.conf` under the index root ([`crate::store::index_root`]).
pub fn conf_path() -> Result<PathBuf, Error> {
    crate::store::index_root().map(|h| h.join("circles.conf"))
}

fn name(s: &str) -> CircleName {
    CircleName(s.to_string())
}

fn base_def(n: &str, source: BaseSource, families: &[Family], exclude: Vec<Exclusion>) -> CircleDef {
    CircleDef {
        name: name(n),
        body: CircleBody::Base(BaseCircle { source, families: families.to_vec(), exclude }),
        origin: Origin::BuiltIn,
    }
}

/// The registry for the repo at `root`: its built-in circles, plus `conf` (the file's path and
/// its text) when there is one. `home` expands `~`.
pub fn manifest_for(root: &Path, conf: Option<(&Path, &str)>, home: &Path) -> Result<CircleManifest, Error> {
    let own_data = INDEX_OWN_DATA
        .iter()
        .map(|p| Exclusion { prefix: p.to_string(), provenance: Provenance::IndexOwnData })
        .collect();
    let history = |root: &Path, upstream: &str, local: &[&str]| BaseSource::History {
        root: root.to_path_buf(),
        upstream: upstream.into(),
        local: local.iter().map(|s| s.to_string()).collect(),
    };
    let mut defs = vec![
        base_def(MAKEPAD, BaseSource::Tree { root: root.to_path_buf() }, TREE_FAMILIES, own_data),
        base_def(MAKEPAD_HISTORY, history(root, MAKEPAD_UPSTREAM, MAKEPAD_LOCAL), HISTORY_FAMILIES, Vec::new()),
    ];
    let commercial = root.join("apps").join("commercial");
    if commercial.is_dir() {
        defs.push(base_def(COMMERCIAL, BaseSource::Tree { root: commercial.clone() }, TREE_FAMILIES, Vec::new()));
        defs.push(base_def(
            COMMERCIAL_HISTORY,
            history(&commercial, COMMERCIAL_UPSTREAM, COMMERCIAL_LOCAL),
            HISTORY_FAMILIES,
            Vec::new(),
        ));
    }
    let local = root.join("local");
    if local.is_dir() {
        let sources = vec![NotesSource { dir: local.clone(), label: repo_label(root, &local) }];
        defs.push(base_def(NOTES_LOCAL, BaseSource::Notes { sources }, NOTES_FAMILIES, Vec::new()));
    }
    let mut eval = Vec::new();
    if let Some((file, text)) = conf {
        parse_conf(&mut defs, &mut eval, root, home, file, text)?;
    }
    Ok(CircleManifest { defs, conf: conf.map(|(f, _)| f.to_path_buf()), eval })
}

/// The label of a directory inside a repo: the checkout directory's name and the path from the
/// root (`makepad/local/plans`).
fn repo_label(root: &Path, dir: &Path) -> String {
    let checkout = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let rel = dir.strip_prefix(root).map(slash_path).unwrap_or_default();
    if rel.is_empty() {
        checkout
    } else {
        format!("{checkout}/{rel}")
    }
}

/// A relative path as `/`-separated text.
pub fn slash_path(p: &Path) -> String {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn expand(word: &str, root: &Path, home: &Path) -> PathBuf {
    if word == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = word.strip_prefix("~/") {
        return home.join(rest);
    }
    let p = Path::new(word);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

fn parse_err(file: &Path, line: u32, message: impl Into<String>) -> Error {
    Error::Parse { file: file.display().to_string(), line, message: message.into() }
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.'))
        && !n.starts_with(':')
        && !n.ends_with(':')
}

fn conf_line(origin: &Origin) -> u32 {
    match origin {
        Origin::Conf { line } => *line,
        Origin::BuiltIn => 0,
    }
}

fn parse_conf(
    defs: &mut Vec<CircleDef>,
    eval: &mut Vec<PathBuf>,
    root: &Path,
    home: &Path,
    file: &Path,
    text: &str,
) -> Result<(), Error> {
    let builtin: Vec<CircleName> = defs.iter().map(|d| d.name.clone()).collect();
    let first_conf = defs.len();
    for (i, raw) in text.lines().enumerate() {
        let line = (i + 1) as u32;
        let content = raw.split('#').next().unwrap_or("").trim();
        if content.is_empty() {
            continue;
        }
        let Some((lhs, rhs)) = content.split_once('=') else {
            return Err(parse_err(file, line, "expected `<name> = <definition>`"));
        };
        let lhs = lhs.trim();
        let words: Vec<&str> = rhs.split_whitespace().collect();
        if lhs == "eval" {
            match words.as_slice() {
                [p] => eval.push(expand(p, root, home)),
                _ => return Err(parse_err(file, line, "expected `eval = <file.jsonl>`")),
            }
            continue;
        }
        if !valid_name(lhs) {
            return Err(parse_err(file, line, format!("`{lhs}` is not a circle name")));
        }
        let cname = name(lhs);
        if builtin.contains(&cname) {
            return Err(parse_err(file, line, format!("`{lhs}` is a built-in circle and cannot be redefined")));
        }
        let Some((&verb, args)) = words.split_first() else {
            return Err(parse_err(file, line, "a definition is empty"));
        };
        let origin = Origin::Conf { line };
        let existing = defs[first_conf..].iter().position(|d| d.name == cname).map(|p| p + first_conf);
        let body = match verb {
            "notes" => {
                add_notes(defs, existing, &cname, args, &At { root, home, file, line })?;
                continue;
            }
            _ if existing.is_some() => return Err(parse_err(file, line, format!("`{lhs}` is defined twice"))),
            "tree" => {
                let [dir] = args else {
                    return Err(parse_err(file, line, "expected `tree <dir>`"));
                };
                CircleBody::Base(BaseCircle {
                    source: BaseSource::Tree { root: expand(dir, root, home) },
                    families: TREE_FAMILIES.to_vec(),
                    exclude: Vec::new(),
                })
            }
            "history" => {
                let [dir, upstream, local @ ..] = args else {
                    return Err(parse_err(file, line, "expected `history <dir> <upstream ref> [<local ref>..]`"));
                };
                CircleBody::Base(BaseCircle {
                    source: BaseSource::History {
                        root: expand(dir, root, home),
                        upstream: upstream.to_string(),
                        local: local.iter().map(|s| s.to_string()).collect(),
                    },
                    families: HISTORY_FAMILIES.to_vec(),
                    exclude: Vec::new(),
                })
            }
            "select" => {
                let by = match args {
                    [_, "kind", kinds] => Selector::Kinds(
                        kinds
                            .split(',')
                            .map(|w| query::kind_filter(w.trim()).ok_or_else(|| parse_err(file, line, format!("unknown kind `{w}`"))))
                            .collect::<Result<_, _>>()?,
                    ),
                    [_, "path", prefix] => Selector::PathPrefix(prefix.to_string()),
                    [_, "package", pkg] => Selector::Package(pkg.to_string()),
                    _ => {
                        return Err(parse_err(file, line, "expected `select <circle> kind <k>,.. | path <prefix> | package <p>`"))
                    }
                };
                CircleBody::Derived(CircleExpr::Select { of: Box::new(CircleExpr::Circle(name(args[0]))), by })
            }
            "overlap" => {
                let [left, right, "on", key] = args else {
                    return Err(parse_err(file, line, "expected `overlap <circle> <circle> on <key>`"));
                };
                let on = match *key {
                    "path" => KeyKind::Path,
                    "commit" => KeyKind::Commit,
                    "blob" => KeyKind::Blob,
                    "ident" => KeyKind::Ident,
                    "package" => KeyKind::Package,
                    other => return Err(parse_err(file, line, format!("unknown key `{other}`"))),
                };
                CircleBody::Derived(CircleExpr::Overlap {
                    left: Box::new(CircleExpr::Circle(name(left))),
                    right: Box::new(CircleExpr::Circle(name(right))),
                    on,
                })
            }
            "union" => {
                let (of, fuse) = match args {
                    [of @ .., "fuse", "rank"] => (of, Fuse::Rank),
                    [of @ .., "fuse", "blocks"] => (of, Fuse::Blocks),
                    of => (of, Fuse::Blocks),
                };
                if of.len() < 2 {
                    return Err(parse_err(file, line, "a union names at least two circles"));
                }
                CircleBody::Derived(CircleExpr::Union { of: of.iter().map(|c| CircleExpr::Circle(name(c))).collect(), fuse })
            }
            other => return Err(parse_err(file, line, format!("unknown definition `{other}`"))),
        };
        defs.push(CircleDef { name: cname, body, origin });
    }
    // a derived circle names configured circles only
    for d in &defs[first_conf..] {
        if let CircleBody::Derived(expr) = &d.body {
            if let Some(n) = expr_names(expr).into_iter().find(|n| !defs.iter().any(|x| &x.name == n)) {
                return Err(parse_err(file, conf_line(&d.origin), format!("`{}` names unknown circle `{}`", d.name.0, n.0)));
            }
        }
    }
    Ok(())
}

/// Where a `circles.conf` line is read: the repo root, `~`, the file and the line.
struct At<'a> {
    root: &'a Path,
    home: &'a Path,
    file: &'a Path,
    line: u32,
}

/// A `notes <dir> [as <label>]` line: a new notes circle or one more source of it. A directory
/// under the repo's `local/` is claimed: it leaves `notes:local`.
fn add_notes(defs: &mut Vec<CircleDef>, existing: Option<usize>, cname: &CircleName, args: &[&str], at: &At) -> Result<(), Error> {
    let At { root, home, file, line } = *at;
    let (dir_word, label) = match args {
        [d] => (*d, None),
        [d, "as", l] => (*d, Some(l.to_string())),
        _ => return Err(parse_err(file, line, "expected `notes <dir> [as <label>]`")),
    };
    let dir = expand(dir_word, root, home);
    let label = label.unwrap_or_else(|| {
        if dir.starts_with(root) {
            repo_label(root, &dir)
        } else {
            dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        }
    });
    if label.is_empty() {
        return Err(parse_err(file, line, format!("`{dir_word}` gives an empty label; add `as <label>`")));
    }
    let source = NotesSource { dir: dir.clone(), label };
    match existing {
        Some(at) => match &mut defs[at].body {
            CircleBody::Base(BaseCircle { source: BaseSource::Notes { sources }, .. }) => {
                if sources.iter().any(|s| s.label == source.label) {
                    return Err(parse_err(file, line, format!("`{}` already has a source labelled `{}`", cname.0, source.label)));
                }
                sources.push(source);
            }
            _ => return Err(parse_err(file, line, format!("`{}` is defined twice", cname.0))),
        },
        None => defs.push(CircleDef {
            name: cname.clone(),
            body: CircleBody::Base(BaseCircle {
                source: BaseSource::Notes { sources: vec![source] },
                families: NOTES_FAMILIES.to_vec(),
                exclude: Vec::new(),
            }),
            origin: Origin::Conf { line },
        }),
    }
    if let Ok(rel) = dir.strip_prefix(root.join("local")) {
        let prefix = format!("{}/", slash_path(rel));
        if let Some(CircleBody::Base(b)) = defs.iter_mut().find(|d| d.name.0 == NOTES_LOCAL).map(|d| &mut d.body) {
            b.exclude.push(Exclusion { prefix, provenance: Provenance::ClaimedBy(cname.clone()) });
        }
    }
    Ok(())
}

/// The circle names an expression mentions, each once, in order.
pub fn expr_names(expr: &CircleExpr) -> Vec<CircleName> {
    fn walk(e: &CircleExpr, out: &mut Vec<CircleName>) {
        match e {
            CircleExpr::Circle(n) => {
                if !out.contains(n) {
                    out.push(n.clone())
                }
            }
            CircleExpr::Union { of, .. } => of.iter().for_each(|x| walk(x, out)),
            CircleExpr::Select { of, .. } => walk(of, out),
            CircleExpr::Overlap { left, right, .. } => {
                walk(left, out);
                walk(right, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(expr, &mut out);
    out
}

pub fn find_def<'a>(manifest: &'a CircleManifest, n: &CircleName) -> Option<&'a CircleDef> {
    manifest.defs.iter().find(|d| &d.name == n)
}

/// The base circles an expression rests on, derived circles expanded. An unknown name is
/// [`Error::UnknownCircle`]; a derived circle that names itself is a parse error.
pub fn base_names(manifest: &CircleManifest, expr: &CircleExpr) -> Result<Vec<CircleName>, Error> {
    fn walk(m: &CircleManifest, e: &CircleExpr, out: &mut Vec<CircleName>, stack: &mut Vec<CircleName>) -> Result<(), Error> {
        for n in expr_names(e) {
            let def = find_def(m, &n).ok_or_else(|| Error::UnknownCircle(n.0.clone()))?;
            match &def.body {
                CircleBody::Base(_) => {
                    if !out.contains(&n) {
                        out.push(n)
                    }
                }
                CircleBody::Derived(inner) => {
                    if stack.contains(&n) {
                        return Err(Error::Parse {
                            file: m.conf.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                            line: conf_line(&def.origin),
                            message: format!("derived circle `{}` names itself", n.0),
                        });
                    }
                    stack.push(n);
                    walk(m, inner, out, stack)?;
                    stack.pop();
                }
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(manifest, expr, &mut out, &mut Vec::new())?;
    Ok(out)
}

/// The source roots a circle reads, for the default scope.
fn roots(def: &CircleDef) -> Vec<&Path> {
    match &def.body {
        CircleBody::Base(BaseCircle { source: BaseSource::Tree { root }, .. }) => vec![root.as_path()],
        CircleBody::Base(BaseCircle { source: BaseSource::Notes { sources }, .. }) => {
            sources.iter().map(|s| s.dir.as_path()).collect()
        }
        _ => Vec::new(),
    }
}

/// The circle whose source root holds `cwd` most closely: `makepad` in a makepad worktree,
/// `notes:<topic>` in a notes directory, `notes:local` under `local/`. Empty when `cwd` is in
/// none of them (the caller then needs `--in`).
pub fn default_scope(manifest: &CircleManifest, cwd: &Path) -> Vec<CircleName> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let cwd = canon(cwd);
    let mut best: Option<(usize, &CircleName)> = None;
    for def in &manifest.defs {
        for root in roots(def) {
            let root = canon(root);
            let depth = root.components().count();
            if cwd.starts_with(&root) && best.is_none_or(|(d, _)| depth > d) {
                best = Some((depth, &def.name));
            }
        }
    }
    best.map(|(_, n)| vec![n.clone()]).unwrap_or_default()
}

/// `--in a,b`: each name is a configured circle (a derived one becomes its expression); a name
/// ending in `*` takes every circle with that prefix (`notes:*`) and must match at least one.
pub fn parse_in(manifest: &CircleManifest, arg: &str) -> Result<Vec<CircleExpr>, Error> {
    let mut out = Vec::new();
    for w in arg.split(',').map(str::trim) {
        let picked: Vec<&CircleDef> = match w.strip_suffix('*') {
            Some(prefix) => manifest.defs.iter().filter(|d| d.name.0.starts_with(prefix)).collect(),
            None => manifest.defs.iter().filter(|d| d.name.0 == w).collect(),
        };
        if w.is_empty() || picked.is_empty() {
            return Err(Error::UnknownCircle(if w.is_empty() { arg.to_string() } else { w.to_string() }));
        }
        for d in picked {
            out.push(match &d.body {
                CircleBody::Base(_) => CircleExpr::Circle(d.name.clone()),
                CircleBody::Derived(e) => e.clone(),
            });
        }
    }
    Ok(out)
}

/// The exclusion that covers `path` (source-root-relative, `/`-separated), if any.
pub fn excluded<'a>(def: &'a BaseCircle, path: &str) -> Option<&'a Exclusion> {
    def.exclude.iter().find(|e| path.starts_with(e.prefix.as_str()) || e.prefix.strip_suffix('/') == Some(path))
}

/// [`crate::Circles::def_hash`]: the SHA-1 of a canonical text of what decides a circle's records
/// (kind of source, families, exclusions, ref names, notes labels), its first 8 bytes
/// little-endian. Machine paths never enter it, so every worktree of a repo shares its bases.
pub fn def_hash(def: &BaseCircle) -> u64 {
    let mut text = String::from("circle-def v1\n");
    match &def.source {
        BaseSource::Tree { .. } => text.push_str("source tree\n"),
        BaseSource::History { upstream, local, .. } => {
            text.push_str(&format!("source history {upstream} {}\n", local.join(" ")));
        }
        BaseSource::Notes { sources } => {
            let mut labels: Vec<&str> = sources.iter().map(|s| s.label.as_str()).collect();
            labels.sort_unstable();
            text.push_str(&format!("source notes {}\n", labels.join(" ")));
        }
    }
    let mut fams: Vec<&str> = def.families.iter().map(|f| f.as_str()).collect();
    fams.sort_unstable();
    text.push_str(&format!("families {}\n", fams.join(",")));
    let mut ex: Vec<String> = def
        .exclude
        .iter()
        .map(|e| match &e.provenance {
            Provenance::IndexOwnData => format!("exclude {} index-own-data\n", e.prefix),
            Provenance::ClaimedBy(c) => format!("exclude {} claimed-by {}\n", e.prefix, c.0),
        })
        .collect();
    ex.sort_unstable();
    text.extend(ex);
    let oid = makepad_git::oid::hash_object("blob", text.as_bytes());
    let b = oid.as_bytes();
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("repo-index-circles-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn base<'a>(m: &'a CircleManifest, n: &str) -> &'a BaseCircle {
        match &find_def(m, &name(n)).unwrap().body {
            CircleBody::Base(b) => b,
            CircleBody::Derived(_) => panic!("{n} is derived"),
        }
    }

    #[test]
    fn built_ins_follow_the_tree() {
        let root = scratch("builtin").join("makepad");
        std::fs::create_dir_all(&root).unwrap();
        let m = manifest_for(&root, None, &root).unwrap();
        let names: Vec<&str> = m.defs.iter().map(|d| d.name.0.as_str()).collect();
        assert_eq!(names, [MAKEPAD, MAKEPAD_HISTORY]);
        let mk = base(&m, MAKEPAD);
        assert_eq!(mk.exclude.len(), INDEX_OWN_DATA.len());
        assert!(mk.exclude.iter().all(|e| e.provenance == Provenance::IndexOwnData));
        std::fs::create_dir_all(root.join("apps/commercial")).unwrap();
        std::fs::create_dir_all(root.join("local/plans")).unwrap();
        let m = manifest_for(&root, None, &root).unwrap();
        let names: Vec<&str> = m.defs.iter().map(|d| d.name.0.as_str()).collect();
        assert_eq!(names, [MAKEPAD, MAKEPAD_HISTORY, COMMERCIAL, COMMERCIAL_HISTORY, NOTES_LOCAL]);
        match &base(&m, NOTES_LOCAL).source {
            BaseSource::Notes { sources } => assert_eq!(sources[0].label, "makepad/local"),
            _ => panic!("notes:local is a notes circle"),
        }
    }

    #[test]
    fn conf_maps_claims_and_labels() {
        let top = scratch("conf");
        let root = top.join("makepad");
        std::fs::create_dir_all(root.join("local/agent_state/port")).unwrap();
        let home = top.join("home");
        let conf = "\
# notes
notes:topic = notes ~/mail/topic
notes:topic = notes local/agent_state/port   # claimed from notes:local
notes:other = notes ~/x/y as why
eval = ~/mail/topic/q.jsonl
pick = select makepad kind fn,doc
both = union makepad notes:topic fuse rank
same = overlap notes:topic makepad on path
";
        let m = manifest_for(&root, Some((Path::new("circles.conf"), conf)), &home).unwrap();
        match &base(&m, "notes:topic").source {
            BaseSource::Notes { sources } => {
                let labels: Vec<&str> = sources.iter().map(|s| s.label.as_str()).collect();
                assert_eq!(labels, ["topic", "makepad/local/agent_state/port"]);
                assert_eq!(sources[0].dir, home.join("mail/topic"));
            }
            _ => panic!("notes:topic is a notes circle"),
        }
        match &base(&m, "notes:other").source {
            BaseSource::Notes { sources } => assert_eq!(sources[0].label, "why"),
            _ => panic!("notes:other is a notes circle"),
        }
        assert_eq!(
            base(&m, NOTES_LOCAL).exclude,
            [Exclusion { prefix: "agent_state/port/".into(), provenance: Provenance::ClaimedBy(name("notes:topic")) }]
        );
        assert_eq!(m.eval, [home.join("mail/topic/q.jsonl")]);
        assert!(matches!(&find_def(&m, &name("pick")).unwrap().body, CircleBody::Derived(CircleExpr::Select { .. })));
        assert!(matches!(
            &find_def(&m, &name("both")).unwrap().body,
            CircleBody::Derived(CircleExpr::Union { fuse: Fuse::Rank, .. })
        ));
        assert_eq!(base_names(&m, &CircleExpr::Circle(name("same"))).unwrap(), [name("notes:topic"), name(MAKEPAD)]);
        assert_eq!(find_def(&m, &name("notes:topic")).unwrap().origin, Origin::Conf { line: 2 });
    }

    #[test]
    fn conf_errors_name_the_line() {
        let root = scratch("conferr").join("makepad");
        std::fs::create_dir_all(&root).unwrap();
        let bad = [
            ("makepad = tree /x\n", 1, "built-in"),
            ("a = tree /x\na = tree /y\n", 2, "defined twice"),
            ("notes:t = notes /a/t\nnotes:t = notes /b/t\n", 2, "already has a source labelled `t`"),
            ("x = bogus\n", 1, "unknown definition"),
            ("\nx = union makepad nope\n", 2, "unknown circle `nope`"),
            ("x = select makepad kind fnn\n", 1, "unknown kind"),
            ("no equals here\n", 1, "expected"),
            ("x = union makepad\n", 1, "at least two"),
        ];
        for (text, line, needle) in bad {
            match manifest_for(&root, Some((Path::new("c.conf"), text)), &root) {
                Err(Error::Parse { line: l, message, .. }) => {
                    assert_eq!(l, line, "{text}");
                    assert!(message.contains(needle), "{text}: {message}");
                }
                other => panic!("{text}: {other:?}"),
            }
        }
    }

    #[test]
    fn in_names_and_globs_or_fails() {
        let root = scratch("in").join("makepad");
        std::fs::create_dir_all(&root).unwrap();
        let conf = "notes:a = notes /n/a\nnotes:b = notes /n/b\nboth = union makepad notes:a\nloop = union loop makepad\n";
        let m = manifest_for(&root, Some((Path::new("c"), conf)), &root).unwrap();
        let got = parse_in(&m, "makepad, notes:*").unwrap();
        let names: Vec<Vec<CircleName>> = got.iter().map(expr_names).collect();
        assert_eq!(names, [vec![name(MAKEPAD)], vec![name("notes:a")], vec![name("notes:b")]]);
        assert!(matches!(&parse_in(&m, "both").unwrap()[0], CircleExpr::Union { .. }));
        for bad in ["nope", "makepad,nope", "zz:*", "", "makepad,"] {
            assert!(matches!(parse_in(&m, bad), Err(Error::UnknownCircle(_))), "{bad}");
        }
        assert!(matches!(base_names(&m, &CircleExpr::Circle(name("loop"))), Err(Error::Parse { .. })));
    }

    #[test]
    fn default_scope_is_the_closest_root() {
        let top = scratch("scope");
        let root = top.join("makepad");
        std::fs::create_dir_all(root.join("local/plans")).unwrap();
        std::fs::create_dir_all(root.join("platform/src")).unwrap();
        std::fs::create_dir_all(top.join("mail/topic/sub")).unwrap();
        let conf = format!("notes:topic = notes {}\n", top.join("mail/topic").display());
        let m = manifest_for(&root, Some((Path::new("c"), &conf)), &top).unwrap();
        assert_eq!(default_scope(&m, &root.join("platform/src")), [name(MAKEPAD)]);
        assert_eq!(default_scope(&m, &root.join("local/plans")), [name(NOTES_LOCAL)]);
        assert_eq!(default_scope(&m, &top.join("mail/topic/sub")), [name("notes:topic")]);
        assert!(default_scope(&m, &top).is_empty());
    }

    #[test]
    fn def_hash_ignores_machine_paths_but_not_exclusions() {
        let a = BaseCircle {
            source: BaseSource::Tree { root: PathBuf::from("/one") },
            families: TREE_FAMILIES.to_vec(),
            exclude: vec![Exclusion { prefix: "x/".into(), provenance: Provenance::IndexOwnData }],
        };
        let mut b = a.clone();
        b.source = BaseSource::Tree { root: PathBuf::from("/two") };
        assert_eq!(def_hash(&a), def_hash(&b));
        b.exclude.push(Exclusion { prefix: "y/".into(), provenance: Provenance::IndexOwnData });
        assert_ne!(def_hash(&a), def_hash(&b));
        let n1 = BaseCircle {
            source: BaseSource::Notes { sources: vec![NotesSource { dir: "/m/a".into(), label: "a".into() }] },
            families: NOTES_FAMILIES.to_vec(),
            exclude: Vec::new(),
        };
        let mut n2 = n1.clone();
        n2.source = BaseSource::Notes { sources: vec![NotesSource { dir: "/elsewhere/a".into(), label: "a".into() }] };
        assert_eq!(def_hash(&n1), def_hash(&n2));
        n2.source = BaseSource::Notes { sources: vec![NotesSource { dir: "/m/a".into(), label: "b".into() }] };
        assert_ne!(def_hash(&n1), def_hash(&n2));
    }

    #[test]
    fn exclusions_cover_prefixes_only() {
        let root = scratch("excl").join("makepad");
        std::fs::create_dir_all(&root).unwrap();
        let m = manifest_for(&root, None, &root).unwrap();
        let def = base(&m, MAKEPAD);
        assert!(excluded(def, "libs/repo_index/eval/questions.jsonl").is_some());
        assert!(excluded(def, "libs/repo_index/eval").is_some());
        assert!(excluded(def, "libs/repo_index/fixture/alpha/src/lib.rs").is_some());
        assert!(excluded(def, "libs/repo_index/evaluate.rs").is_none());
        assert!(excluded(def, "libs/repo_index/src/eval/mod.rs").is_none());
    }
}
