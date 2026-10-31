//! verify-gold: every gold item re-resolved at its question's revision, from git
//! objects (notes from disk); only what changed is returned.
//! - a path that is gone is `Gone`; a line whose symbol moved is `Lines`, or `Lost`;
//! - a package no `Cargo.toml` names is `PackageGone`;
//! - a commit (or `alt`) that is not an ancestor of the revision is `NotAncestor`; a
//!   `via_pointer` commit need only resolve, else `Unreachable`;
//! - a non-semantic absence with hits in its scope is `NoLongerAbsent` (the index's own data is
//!   never searched: it quotes every absent term);
//! - a pointer count that differs from the count recomputed from `circles.conf` is
//!   `PointerCount`;
//! - the self-exclusion sentinel needs a raw hit inside the eval directory (`SentinelInert`
//!   otherwise) and none outside it (`SentinelOutside`); a commit between upstream and the
//!   revision that mixes eval data with other paths is `MixedEvalCommit`.
//!
//! A question whose revision does not resolve reports `Unreachable` on its first item. Items on
//! circles that are not configured here are not checked.

use crate::{
    circles, eval::pointer, AbsentScope, Anchor, BaseCircle, BaseSource, CircleBody, CircleManifest, CircleName, Drift, Error, GoldDrift,
    GoldTarget, Oid, Question, Repo, RevSpec, TreeFile, INDEX_OWN_DATA,
};
use std::collections::HashMap;
use std::path::PathBuf;

/// The eval directory the sentinel lives in.
pub const EVAL_DIR: &str = "libs/repo_index/eval/";

fn own_data(path: &str) -> bool {
    INDEX_OWN_DATA.iter().any(|p| path.starts_with(p))
}

/// Whether `line` holds the anchor.
pub fn anchor_in(anchor: &Anchor, line: &str) -> bool {
    match anchor {
        Anchor::Def(d) => {
            let mut words = d.split_whitespace();
            let (kind, name) = (words.next().unwrap_or(""), words.last().unwrap_or(""));
            !name.is_empty() && contains_word(line, name) && (kind == name || contains_word(line, kind))
        }
        Anchor::Heading(h) | Anchor::Text(h) => line.trim().contains(h.trim()),
    }
}

fn contains_word(hay: &str, word: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    hay.match_indices(word).any(|(i, _)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + word.len()..].chars().next();
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

/// Checks a line-anchored item against `text`: `None` when it holds, else the drift.
pub fn check_line(text: &str, line: u32, anchor: &Anchor) -> Option<Drift> {
    let lines: Vec<&str> = text.lines().collect();
    if (line as usize).checked_sub(1).and_then(|i| lines.get(i)).is_some_and(|l| anchor_in(anchor, l)) {
        return None;
    }
    // the nearest line holding it
    let found = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| anchor_in(anchor, l))
        .map(|(i, _)| i as u32 + 1)
        .min_by_key(|n| n.abs_diff(line));
    Some(match found {
        Some(now) => Drift::Lines { by: i64::from(now) - i64::from(line), now },
        None => Drift::Lost,
    })
}

/// A tree's files and lazily read texts.
struct Tree {
    files: Vec<TreeFile>,
    by_path: HashMap<String, usize>,
    texts: HashMap<usize, String>,
    cargo_names: Option<Vec<String>>,
}

impl Tree {
    fn text(&mut self, repo: &mut dyn Repo, i: usize) -> Result<&str, Error> {
        if !self.texts.contains_key(&i) {
            let bytes = repo.read_blob(self.files[i].blob)?;
            self.texts.insert(i, String::from_utf8_lossy(&bytes).into_owned());
        }
        Ok(&self.texts[&i])
    }
    fn has_path(&self, p: &str) -> bool {
        let dir = format!("{}/", p.trim_end_matches('/'));
        self.by_path.contains_key(p) || self.files.iter().any(|f| f.path.starts_with(&dir))
    }
    fn cargo_names<'s>(&'s mut self, repo: &mut dyn Repo) -> Result<&'s [String], Error> {
        if self.cargo_names.is_none() {
            let mut names = Vec::new();
            let idx: Vec<usize> = (0..self.files.len()).filter(|i| self.files[*i].path.ends_with("Cargo.toml")).collect();
            for i in idx {
                let mut in_package = false;
                for line in self.text(repo, i)?.lines() {
                    let t = line.trim();
                    if t.starts_with('[') {
                        in_package = t == "[package]";
                    } else if in_package {
                        if let Some(v) = t.strip_prefix("name").map(str::trim_start).and_then(|r| r.strip_prefix('=')) {
                            names.push(v.trim().trim_matches('"').to_string());
                            in_package = false;
                        }
                    }
                }
            }
            self.cargo_names = Some(names);
        }
        Ok(self.cargo_names.as_deref().unwrap_or_default())
    }
}

/// A notes path (`<label>/<path>`) on disk, through the configured circles' labels.
fn note_file<'m>(manifest: &'m CircleManifest, path: &str) -> Option<(PathBuf, String, &'m BaseCircle)> {
    let mut best: Option<(PathBuf, String, &'m BaseCircle, usize)> = None;
    for d in &manifest.defs {
        if let CircleBody::Base(b @ BaseCircle { source: BaseSource::Notes { sources }, .. }) = &d.body {
            for s in sources {
                if let Some(rest) = path.strip_prefix(&format!("{}/", s.label)) {
                    if best.as_ref().is_none_or(|x| s.label.len() > x.3) {
                        best = Some((s.dir.join(rest), rest.to_string(), b, s.label.len()));
                    }
                }
            }
        }
    }
    best.map(|(p, rest, b, _)| (p, rest, b))
}

fn is_history(manifest: &CircleManifest, name: &str) -> bool {
    manifest.defs.iter().any(|d| d.name.0 == name && matches!(&d.body, CircleBody::Base(BaseCircle { source: BaseSource::History { .. }, .. })))
}

fn notes_def<'a>(manifest: &'a CircleManifest, name: &str) -> Option<&'a BaseCircle> {
    manifest.defs.iter().find(|d| d.name.0 == name).and_then(|d| match &d.body {
        CircleBody::Base(b @ BaseCircle { source: BaseSource::Notes { .. }, .. }) => Some(b),
        _ => None,
    })
}

/// One lexical check over a tree's files.
struct Needle {
    q: usize,
    g: usize,
    text: String,
    /// `fn <name>` must be followed by a non-identifier character.
    as_fn: bool,
    /// Matched case-insensitively (`text` is lowercase): an absence's `term`.
    fold: bool,
}

/// A block's claim that a term occurs nowhere in its circle at a revision.
#[derive(Clone, Debug)]
pub struct Claim {
    pub id: String,
    pub circle: CircleName,
    pub rev: RevSpec,
    pub term: String,
}

/// Whether `hay` (lowercase) holds `word` (lowercase) as a whole word: not inside a longer
/// identifier (letters, digits and `_` around it), so `overlay` is not in `run_overlay_used`.
pub fn has_word(hay: &str, word: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    !word.is_empty()
        && hay.match_indices(word).any(|(i, _)| {
            !hay[..i].chars().next_back().is_some_and(ident) && !hay[i + word.len()..].chars().next().is_some_and(ident)
        })
}

/// A path-shaped term without a `:line` or `:start-end` suffix.
fn path_term(t: &str) -> &str {
    match t.rsplit_once(':') {
        Some((p, l)) if !l.is_empty() && l.chars().all(|c| c.is_ascii_digit() || c == '-') => p,
        _ => t,
    }
}

/// For each claim, how many of its circle's records at its revision hold the term: in a tree
/// circle, a path-shaped term (query's rule) counts the files whose path is it, lies under it
/// or ends with it, and any other term the files whose text or path holds it as a whole word,
/// case-insensitively (the circle's exclusions apply; one pass per tree for all its claims); in
/// a history circle, the reachable commits whose message or touched paths hold it as a word; in
/// a notes circle, the files whose text or path does. A circle of another repo is not checked
/// here (0, said on stderr).
pub fn occurrences(repo: &mut dyn Repo, manifest: &CircleManifest, claims: &[Claim]) -> Result<Vec<u64>, Error> {
    let mut out = vec![0u64; claims.len()];
    let root = repo.root();
    // (circle, tree) -> claim indices, for one pass per tree
    let mut by_tree: HashMap<(String, Oid), Vec<usize>> = HashMap::new();
    let mut by_history: HashMap<Oid, Vec<usize>> = HashMap::new();
    let mut by_notes: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, c) in claims.iter().enumerate() {
        let Some(CircleBody::Base(def)) = manifest.defs.iter().find(|d| d.name == c.circle).map(|d| &d.body) else { continue };
        match &def.source {
            BaseSource::Tree { root: r } if std::fs::canonicalize(r).ok() == std::fs::canonicalize(&root).ok() => {
                let rev = repo.resolve(&c.rev)?;
                by_tree.entry((c.circle.0.clone(), rev.tree)).or_default().push(i);
            }
            BaseSource::History { root: r, .. } if std::fs::canonicalize(r).ok() == std::fs::canonicalize(&root).ok() => {
                let rev = repo.resolve(&c.rev)?;
                by_history.entry(rev.commit).or_default().push(i);
            }
            BaseSource::Notes { .. } => by_notes.entry(c.circle.0.clone()).or_default().push(i),
            _ => eprintln!("makepad index: eval: {}'s absence of `{}` is not checked (another repo)", c.circle.0, c.term),
        }
    }
    let def_of = |name: &str| match manifest.defs.iter().find(|d| d.name.0 == name).map(|d| &d.body) {
        Some(CircleBody::Base(b)) => Some(b),
        _ => None,
    };
    for ((circle, tree), idx) in by_tree {
        let Some(def) = def_of(&circle) else { continue };
        // a path-shaped term (query's own rule) is absent when no file's path is it, lies under
        // it or ends with it; any other term when no file's text or path holds it as a word
        let (paths, texts): (Vec<usize>, Vec<usize>) = idx.iter().partition(|i| crate::query::rank::path_shaped(path_term(&claims[**i].term)));
        let words: Vec<String> = texts.iter().map(|i| claims[*i].term.to_lowercase()).collect();
        for f in repo.files(tree)? {
            if circles::excluded(def, &f.path).is_some() {
                continue;
            }
            for i in &paths {
                let p = path_term(&claims[*i].term).trim_end_matches('/');
                if f.path == p || f.path.starts_with(&format!("{p}/")) || f.path.ends_with(&format!("/{p}")) {
                    out[*i] += 1;
                }
            }
            if words.is_empty() {
                continue;
            }
            let text = String::from_utf8_lossy(&repo.read_blob(f.blob)?).to_lowercase();
            let path = f.path.to_lowercase();
            for (k, w) in words.iter().enumerate() {
                if has_word(&text, w) || has_word(&path, w) {
                    out[texts[k]] += 1;
                }
            }
        }
    }
    for (commit, idx) in by_history {
        let words: Vec<String> = idx.iter().map(|i| claims[*i].term.to_lowercase()).collect();
        for c in repo.commits(&[commit], &[])? {
            let hay = format!("{}\n{}", c.message, c.touched.join("\n")).to_lowercase();
            for (k, w) in words.iter().enumerate() {
                if has_word(&hay, w) {
                    out[idx[k]] += 1;
                }
            }
        }
    }
    for (circle, idx) in by_notes {
        let Some(def @ BaseCircle { source: BaseSource::Notes { sources }, .. }) = def_of(&circle) else { continue };
        let words: Vec<String> = idx.iter().map(|i| claims[*i].term.to_lowercase()).collect();
        for s in sources {
            for (rel, full) in crate::open::walk_files(&s.dir)? {
                if circles::excluded(def, &rel).is_some() {
                    continue;
                }
                let bytes = std::fs::read(&full).map_err(|error| Error::Io { context: format!("absence check {}", full.display()), error })?;
                let text = String::from_utf8_lossy(&bytes).to_lowercase();
                let path = rel.to_lowercase();
                for (k, w) in words.iter().enumerate() {
                    if has_word(&text, w) || has_word(&path, w) {
                        out[idx[k]] += 1;
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Re-resolves every gold item.
pub fn verify(repo: &mut dyn Repo, manifest: &CircleManifest, questions: &[Question]) -> Result<Vec<GoldDrift>, Error> {
    let mut out = Vec::new();
    let mut trees: HashMap<Oid, Tree> = HashMap::new();
    let mut needles: HashMap<Oid, Vec<Needle>> = HashMap::new();
    let configured: Vec<&str> = manifest.defs.iter().map(|d| d.name.0.as_str()).collect();
    for (qi, q) in questions.iter().enumerate() {
        let rev = match repo.resolve(&q.rev) {
            Ok(r) => r,
            Err(e) => {
                let RevSpec::Pinned(spec) = &q.rev else { return Err(e) };
                out.push(GoldDrift { id: q.id.clone(), gold: 0, drift: Drift::Unreachable { commit: spec.clone() } });
                continue;
            }
        };
        let tree = match trees.entry(rev.tree) {
            std::collections::hash_map::Entry::Occupied(t) => t.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let files = repo.files(rev.tree)?;
                let by_path = files.iter().enumerate().map(|(i, f)| (f.path.clone(), i)).collect();
                v.insert(Tree { files, by_path, texts: HashMap::new(), cargo_names: None })
            }
        };
        let drift = |g: usize, drift: Drift| GoldDrift { id: q.id.clone(), gold: g, drift };
        for (gi, g) in q.gold.iter().enumerate() {
            if g.requires.as_ref().is_some_and(|c| !configured.contains(&c.0.as_str())) {
                continue;
            }
            match &g.target {
                GoldTarget::Path { path, line, .. } => match tree.by_path.get(path).copied() {
                    None if tree.has_path(path) => {}
                    None => out.push(drift(gi, Drift::Gone)),
                    Some(i) => {
                        if let (Some(line), Some(anchor)) = (line, &g.symbol) {
                            if let Some(d) = check_line(tree.text(repo, i)?, *line, anchor) {
                                out.push(drift(gi, d));
                            }
                        }
                    }
                },
                GoldTarget::Note { path, line, .. } => {
                    let Some((file, _, _)) = note_file(manifest, path) else { continue };
                    match std::fs::read(&file) {
                        Err(_) => out.push(drift(gi, Drift::Gone)),
                        Ok(bytes) => {
                            if let (Some(line), Some(anchor)) = (line, &g.symbol) {
                                if let Some(d) = check_line(&String::from_utf8_lossy(&bytes), *line, anchor) {
                                    out.push(drift(gi, d));
                                }
                            }
                        }
                    }
                }
                GoldTarget::Package(p) => {
                    if !tree.cargo_names(repo)?.iter().any(|n| n == p) {
                        out.push(drift(gi, Drift::PackageGone));
                    }
                }
                GoldTarget::Commit { hex, alt, via_pointer } => {
                    for c in std::iter::once(hex).chain(alt) {
                        match repo.resolve(&RevSpec::Pinned(c.clone())) {
                            Err(_) if *via_pointer => out.push(drift(gi, Drift::Unreachable { commit: c.clone() })),
                            Err(_) => out.push(drift(gi, Drift::NotAncestor { commit: c.clone() })),
                            Ok(r) if !*via_pointer && repo.merge_base(r.commit, rev.commit)? != Some(r.commit) => {
                                out.push(drift(gi, Drift::NotAncestor { commit: c.clone() }))
                            }
                            Ok(_) => {}
                        }
                    }
                }
                GoldTarget::Absent { semantic: true, .. } | GoldTarget::Dependents { .. } | GoldTarget::Error(_) => {}
                GoldTarget::Absent { scope, term: Some(t), .. } if q.subtype != "self-exclusion" => {
                    let history = match scope {
                        Some(AbsentScope::Circle(c)) => is_history(manifest, &c.0),
                        _ => false,
                    };
                    if history {
                        let t = t.to_lowercase();
                        let hits = repo
                            .commits(&[rev.commit], &[])?
                            .iter()
                            .filter(|c| c.message.to_lowercase().contains(&t) || c.touched.iter().any(|p| p.to_lowercase().contains(&t)))
                            .count();
                        if hits > 0 {
                            out.push(drift(gi, Drift::NoLongerAbsent { hits: hits as u64 }));
                        }
                    } else {
                        needles.entry(rev.tree).or_default().push(Needle { q: qi, g: gi, text: t.to_lowercase(), as_fn: false, fold: true });
                    }
                }
                GoldTarget::Absent { scope, .. } if q.subtype != "self-exclusion" => match scope {
                    Some(AbsentScope::Path(p)) => {
                        let hits = tree.files.iter().filter(|f| f.path == *p || f.path.starts_with(&format!("{p}/")) || f.path.ends_with(&format!("/{p}"))).count();
                        if hits > 0 {
                            out.push(drift(gi, Drift::NoLongerAbsent { hits: hits as u64 }));
                        }
                    }
                    Some(AbsentScope::Package(n)) => {
                        if tree.cargo_names(repo)?.iter().any(|x| x == n) {
                            out.push(drift(gi, Drift::NoLongerAbsent { hits: 1 }));
                        }
                    }
                    Some(AbsentScope::Fn(f)) => {
                        needles.entry(rev.tree).or_default().push(Needle { q: qi, g: gi, text: format!("fn {f}"), as_fn: true, fold: false })
                    }
                    Some(AbsentScope::Text(_)) => {}
                    None | Some(AbsentScope::Circle(_)) => {
                        if !q.query.trim().is_empty() && !q.query.trim().contains(char::is_whitespace) {
                            needles.entry(rev.tree).or_default().push(Needle { q: qi, g: gi, text: q.query.trim().to_string(), as_fn: false, fold: false })
                        }
                    }
                },
                GoldTarget::Absent { .. } => {}
                GoldTarget::Pointer { circle, count: Some(c), .. } => {
                    let Some(def) = notes_def(manifest, &circle.0) else { continue };
                    let BaseSource::Notes { sources } = &def.source else { continue };
                    let now = pointer::count_blobs(sources, def, &q.query)?;
                    if now != *c {
                        out.push(drift(gi, Drift::PointerCount { gold: *c, now }));
                    }
                }
                GoldTarget::Pointer { .. } => {}
            }
        }
        if q.subtype == "self-exclusion" {
            let sentinel = q.query.trim();
            let (mut inside, mut outside) = (0u64, 0u64);
            for i in 0..tree.files.len() {
                let path = tree.files[i].path.clone();
                if tree.text(repo, i)?.contains(sentinel) {
                    if path.starts_with(EVAL_DIR) {
                        inside += 1;
                    } else {
                        outside += 1;
                    }
                }
            }
            if inside == 0 {
                out.push(drift(0, Drift::SentinelInert));
            }
            if outside > 0 {
                out.push(drift(0, Drift::SentinelOutside { hits: outside }));
            }
            match repo.resolve(&RevSpec::Pinned(circles::MAKEPAD_UPSTREAM.into())) {
                Ok(up) => {
                    for c in repo.commits(&[rev.commit], &[up.commit])? {
                        let eval = c.touched.iter().any(|p| p.starts_with(EVAL_DIR));
                        let other = c.touched.iter().any(|p| !p.starts_with(EVAL_DIR));
                        if eval && other {
                            out.push(drift(0, Drift::MixedEvalCommit { commit: crate::open::hex(&c.oid) }));
                        }
                    }
                }
                Err(e) => eprintln!("makepad index: verify-gold: mixed eval commits not checked: {} does not resolve ({e})", circles::MAKEPAD_UPSTREAM),
            }
        }
    }
    // one pass per tree over every lexical needle
    for (tree_oid, list) in needles {
        let tree = trees.get_mut(&tree_oid).expect("tree read above");
        let mut hits = vec![0u64; list.len()];
        for i in 0..tree.files.len() {
            if own_data(&tree.files[i].path) {
                continue;
            }
            let text = tree.text(repo, i)?;
            let folded = list.iter().any(|n| n.fold).then(|| text.to_lowercase());
            for (n, needle) in list.iter().enumerate() {
                let found = if needle.fold {
                    folded.as_deref().is_some_and(|f| f.contains(&needle.text))
                } else if needle.as_fn {
                    text.match_indices(&needle.text).any(|(at, _)| {
                        !text[at + needle.text.len()..].chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_')
                    })
                } else {
                    text.contains(&needle.text)
                };
                if found {
                    hits[n] += 1;
                }
            }
        }
        for (n, needle) in list.iter().enumerate() {
            if hits[n] > 0 {
                out.push(GoldDrift { id: questions[needle.q].id.clone(), gold: needle.g, drift: Drift::NoLongerAbsent { hits: hits[n] } });
            }
        }
    }
    out.sort_by(|a, b| (a.id.as_str(), a.gold).cmp(&(b.id.as_str(), b.gold)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::score::tests::q;
    use crate::open::fakes::{oid, FakeRepo};
    use crate::{CommitInfo, CommitRecord};

    fn anchored(mut question: Question, gi: usize, anchor: Anchor) -> Question {
        question.gold[gi].symbol = Some(anchor);
        question
    }

    #[test]
    fn an_absent_term_is_checked_in_files_or_in_history() {
        let root = std::env::temp_dir().join(format!("repo-index-verify-term-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut repo = FakeRepo::new(&root);
        let rev = repo.rev(&["c0ffee123"], 1, &[("a.rs", "// Mentions GlyphX here\n"), ("libs/repo_index/eval/questions.jsonl", "consumerword\n")]);
        repo.commits = vec![CommitRecord { oid: rev.commit, info: CommitInfo { time: 0, parents: vec![] }, message: "relax: fix for ConsumerWord".into(), touched: vec!["b.rs".into()] }];
        let m = circles::manifest_for(&root, None, &root).unwrap();
        let term = |t: &str, scope| GoldTarget::Absent { scope, semantic: false, term: Some(t.into()) };
        let history = Some(AbsentScope::Circle(crate::CircleName("makepad:history".into())));
        let questions = vec![
            q("files", &["makepad"], vec![(2, term("glyphx", Some(AbsentScope::Text("tracked files".into())))), (2, term("consumerword", None))]),
            q("hist", &["makepad:history"], vec![(2, term("consumerword", history.clone())), (2, term("glyphx", history))]),
        ];
        let got: Vec<String> = verify(&mut repo, &m, &questions).unwrap().iter().map(|d| format!("{}#{} {:?}", d.id, d.gold, d.drift)).collect();
        // case-insensitive; the eval data is never searched; history looks at messages, not files
        assert_eq!(got, ["files#0 NoLongerAbsent { hits: 1 }", "hist#0 NoLongerAbsent { hits: 1 }"]);
    }

    #[test]
    fn a_claimed_absence_is_checked_against_the_circle_at_its_revision() {
        let root = std::env::temp_dir().join(format!("repo-index-occurs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut repo = FakeRepo::new(&root);
        let rev = repo.rev(
            &["c0ffee123"],
            1,
            &[
                ("a.rs", "// Uses FOO_ here, and run_overlay_used\n"),
                ("libs/repo_index/eval/questions.jsonl", "bar_term\n"),
            ],
        );
        repo.commits = vec![CommitRecord { oid: rev.commit, info: CommitInfo { time: 0, parents: vec![] }, message: "fix Baz handling".into(), touched: vec!["q.rs".into()] }];
        let m = circles::manifest_for(&root, None, &root).unwrap();
        let claim = |circle: &str, term: &str| Claim { id: "q".into(), circle: CircleName(circle.into()), rev: RevSpec::Pinned("c0ffee123".into()), term: term.into() };
        let claims = [
            claim("makepad", "foo_"),           // occurs (case-insensitive): a false absence
            claim("makepad", "overlay"),        // only inside an identifier: truly absent
            claim("makepad", "bar_term"),       // only in the index's own eval data: truly absent
            claim("makepad:history", "baz"),    // in a reachable commit message
            claim("makepad:history", "q.rs"),   // a touched path
            claim("makepad:history", "nowhere"),
        ];
        assert_eq!(occurrences(&mut repo, &m, &claims).unwrap(), [1, 0, 0, 1, 1, 0]);
        // a path-shaped term is judged on paths: mentioned in a text, but no such file, is absent
        let mut repo2 = FakeRepo::new(&root);
        repo2.rev(&["c0ffee123"], 1, &[("README.md", "see examples/demo/REVIEW.md and push-tree.sh\n"), ("tools/run/push.rs", "x\n"), ("libs/a/b.rs", "x\n")]);
        let paths = [
            claim("makepad", "examples/demo/REVIEW.md"), // only mentioned: truly absent
            claim("makepad", "push-tree.sh"),            // only mentioned: truly absent
            claim("makepad", "push.rs"),                 // a file ends with it
            claim("makepad", "libs/a"),                  // a directory holds files
            claim("makepad", "a/b.rs:12"),               // with a line suffix
        ];
        assert_eq!(occurrences(&mut repo2, &m, &paths).unwrap(), [0, 0, 1, 1, 1]);
        assert!(has_word("a b_c d", "b_c") && !has_word("ab_c", "b_c") && !has_word("x", ""));
    }

    #[test]
    fn anchors_match_whole_words() {
        assert!(anchor_in(&Anchor::Def("fn cur_unit".into()), "pub(crate) fn cur_unit() -> *mut Unit {"));
        assert!(!anchor_in(&Anchor::Def("fn cur_unit".into()), "fn cur_unit_inner() {"));
        assert!(!anchor_in(&Anchor::Def("enum KeyCode".into()), "pub struct KeyCode;"));
        assert!(anchor_in(&Anchor::Heading("# x".into()), "# x"));
        assert!(check_line("a\nfn f() {}\n", 2, &Anchor::Def("fn f".into())).is_none());
        assert!(matches!(check_line("a\nb\nfn f() {}\n", 1, &Anchor::Def("fn f".into())), Some(Drift::Lines { by: 2, now: 3 })));
        assert!(matches!(check_line("a\n", 1, &Anchor::Def("fn f".into())), Some(Drift::Lost)));
        // line 0 never underflows: it holds no line, so the anchor is looked for
        assert!(matches!(check_line("fn f() {}\n", 0, &Anchor::Def("fn f".into())), Some(Drift::Lines { by: 1, now: 1 })));
    }

    #[test]
    fn drift_kinds_at_the_pinned_rev() {
        let root = std::env::temp_dir().join(format!("repo-index-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut repo = FakeRepo::new(&root);
        let rev = repo.rev(
            &["c0ffee123"],
            1,
            &[
                ("a/lib.rs", "// moved down\nfn keep() {}\n"),
                ("a/Cargo.toml", "[package]\nname = \"pkg-a\"\n"),
                ("b/still.rs", "fn unit_here() {}\n"),
                ("libs/repo_index/eval/questions.jsonl", "zz_sentinel fn ghost_fn reverse_term\n"),
            ],
        );
        let up = repo.rev(&["refs/remotes/upstream/work"], 2, &[]);
        repo.rev(&["c0ffee0c0ffee"], 3, &[]);
        repo.commits = vec![CommitRecord {
            oid: rev.commit,
            info: CommitInfo { time: 0, parents: vec![up.commit] },
            message: "mixed".into(),
            touched: vec!["libs/repo_index/eval/questions.jsonl".into(), "a/lib.rs".into()],
        }];
        repo.merge_bases.insert((oid(3), rev.commit), oid(9));
        let p = |path: &str, line: Option<u32>| GoldTarget::Path { path: path.into(), line, end: None, alt_lines: vec![] };
        let absent = |scope| GoldTarget::Absent { scope, semantic: false, term: None };
        let questions = vec![
            anchored(q("p", &["makepad"], vec![(2, p("a/lib.rs", Some(1))), (2, p("gone.rs", None)), (1, p("a", None))]), 0, Anchor::Def("fn keep".into())),
            q("k", &["makepad"], vec![(2, GoldTarget::Package("pkg-a".into())), (2, GoldTarget::Package("pkg-gone".into()))]),
            q(
                "c",
                &["makepad"],
                vec![
                    (2, GoldTarget::Commit { hex: "c0ffee0".into(), alt: vec![], via_pointer: false }),
                    (1, GoldTarget::Commit { hex: "dead123".into(), alt: vec![], via_pointer: true }),
                ],
            ),
            q(
                "f",
                &["makepad"],
                vec![
                    (1, absent(Some(AbsentScope::Fn("unit_here".into())))),
                    (1, absent(Some(AbsentScope::Fn("ghost_fn".into())))),
                    (1, absent(Some(AbsentScope::Path("still.rs".into())))),
                ],
            ),
            Question { query: "reverse_term".into(), ..q("a", &["makepad"], vec![(2, absent(None))]) },
            Question { query: "zz_sentinel".into(), subtype: "self-exclusion".into(), ..q("s", &["makepad"], vec![(2, absent(None))]) },
            Question { rev: RevSpec::Pinned("fffffff".into()), ..q("u", &["makepad"], vec![(2, p("a/lib.rs", None))]) },
            q("r", &["makepad"], vec![(2, GoldTarget::Note { path: "unconfigured/x.md".into(), line: Some(1), end: None, alt_lines: vec![] })]),
        ];
        let m = circles::manifest_for(&root, None, &root).unwrap();
        let got: Vec<String> = verify(&mut repo, &m, &questions).unwrap().iter().map(|d| format!("{}#{} {:?}", d.id, d.gold, d.drift)).collect();
        let mixed = format!("s#0 MixedEvalCommit {{ commit: \"{}\" }}", crate::open::hex(&rev.commit));
        assert_eq!(
            got,
            [
                "c#0 NotAncestor { commit: \"c0ffee0\" }",
                "c#1 Unreachable { commit: \"dead123\" }",
                "f#0 NoLongerAbsent { hits: 1 }",
                "f#2 NoLongerAbsent { hits: 1 }",
                "k#1 PackageGone",
                "p#0 Lines { by: 1, now: 2 }",
                "p#1 Gone",
                mixed.as_str(),
                "u#0 Unreachable { commit: \"fffffff\" }",
            ]
        );
        // the sentinel: dead when the eval file lacks it, live elsewhere is a leak
        let mut repo2 = FakeRepo::new(&root);
        repo2.rev(&["c0ffee123"], 1, &[("x.rs", "zz_sentinel\n")]);
        let s = Question { query: "zz_sentinel".into(), subtype: "self-exclusion".into(), ..q("s", &["makepad"], vec![(2, absent(None))]) };
        let got: Vec<String> = verify(&mut repo2, &m, &[s]).unwrap().iter().map(|d| format!("{:?}", d.drift)).collect();
        assert_eq!(got, ["SentinelInert", "SentinelOutside { hits: 1 }"]);
    }
}
