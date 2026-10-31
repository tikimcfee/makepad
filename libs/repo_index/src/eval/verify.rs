//! verify-gold (EVAL §3.3): every gold item re-resolved at its question's revision, from git
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
    circles, eval::pointer, AbsentScope, Anchor, BaseCircle, BaseSource, CircleBody, CircleManifest, Drift, Error, GoldDrift,
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
    if lines.get(line as usize - 1).is_some_and(|l| anchor_in(anchor, l)) {
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
                        needles.entry(rev.tree).or_default().push(Needle { q: qi, g: gi, text: format!("fn {f}"), as_fn: true })
                    }
                    Some(AbsentScope::Text(_)) => {}
                    None | Some(AbsentScope::Circle(_)) => {
                        if !q.query.trim().is_empty() && !q.query.trim().contains(char::is_whitespace) {
                            needles.entry(rev.tree).or_default().push(Needle { q: qi, g: gi, text: q.query.trim().to_string(), as_fn: false })
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
            for (n, needle) in list.iter().enumerate() {
                let found = if needle.as_fn {
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
    fn anchors_match_whole_words() {
        assert!(anchor_in(&Anchor::Def("fn cur_unit".into()), "pub(crate) fn cur_unit() -> *mut Unit {"));
        assert!(!anchor_in(&Anchor::Def("fn cur_unit".into()), "fn cur_unit_inner() {"));
        assert!(!anchor_in(&Anchor::Def("enum KeyCode".into()), "pub struct KeyCode;"));
        assert!(anchor_in(&Anchor::Heading("# x".into()), "# x"));
        assert!(check_line("a\nfn f() {}\n", 2, &Anchor::Def("fn f".into())).is_none());
        assert!(matches!(check_line("a\nb\nfn f() {}\n", 1, &Anchor::Def("fn f".into())), Some(Drift::Lines { by: 2, now: 3 })));
        assert!(matches!(check_line("a\n", 1, &Anchor::Def("fn f".into())), Some(Drift::Lost)));
    }

    #[test]
    fn drift_kinds_at_the_pinned_rev() {
        let root = std::env::temp_dir().join(format!("repo-index-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut repo = FakeRepo::new(&root);
        let rev = repo.rev(
            &["9a76551d7"],
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
        let absent = |scope| GoldTarget::Absent { scope, semantic: false };
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
        repo2.rev(&["9a76551d7"], 1, &[("x.rs", "zz_sentinel\n")]);
        let s = Question { query: "zz_sentinel".into(), subtype: "self-exclusion".into(), ..q("s", &["makepad"], vec![(2, absent(None))]) };
        let got: Vec<String> = verify(&mut repo2, &m, &[s]).unwrap().iter().map(|d| format!("{:?}", d.drift)).collect();
        assert_eq!(got, ["SentinelInert", "SentinelOutside { hits: 1 }"]);
    }
}
