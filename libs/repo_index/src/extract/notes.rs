//! Note chunks from every text file of a notes circle's sources, read from disk: one
//! [`crate::Kind::Note`] per Markdown section, one [`crate::Kind::NoteFile`] per other text file
//! (code, diffs, logs), and a path-only `NoteFile` per binary file (a NUL in the first 8000
//! bytes), so pointer counts see every file (EVAL §1.3). Markdown splits as
//! [`crate::PointerUnit::Blobs`] says. Paths are `<label>/<path in dir>`.
//! Each chunk carries its blob key (copies collapse) and outward edges: tracked repo paths,
//! `path:line` and commit hexes that resolve.
//!
//! A record's text is its chunk's `fields.body`, exactly (the heading line included; empty for
//! a binary file): what [`crate::SegmentRead::record_text`] returns.

use super::markdown;
use crate::{CircleName, Edge, Error, Key, Oid, Repo, Rev, Span};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// git's binary heuristic: a NUL in the first 8000 bytes.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8000)].contains(&0)
}

/// One record of a notes file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteRecord {
    /// The section heading; the file name for a whole file or the text before a first heading.
    pub name: String,
    pub text: String,
    pub span: Span,
    pub lines: crate::Lines,
    /// One record per Markdown section (`Note`), or the whole file (`NoteFile`).
    pub section: bool,
    pub binary: bool,
}

/// The records of one notes file; `rel` is its path within its source.
pub fn records(rel: &str, bytes: &[u8]) -> Vec<NoteRecord> {
    let file_name = rel.rsplit('/').next().unwrap_or(rel).to_string();
    if is_binary(bytes) {
        return vec![NoteRecord { name: file_name, text: String::new(), span: Span::default(), lines: crate::Lines::default(), section: false, binary: true }];
    }
    let text = String::from_utf8_lossy(bytes);
    if rel.ends_with(".md") {
        return markdown::note_records(&text)
            .into_iter()
            .map(|s| NoteRecord {
                name: if s.heading.is_empty() { file_name.clone() } else { s.heading },
                text: s.text,
                span: s.span,
                lines: s.lines,
                section: true,
                binary: false,
            })
            .collect();
    }
    let lines = text.lines().count().max(1) as u32;
    vec![NoteRecord {
        name: file_name,
        text: text.into_owned(),
        span: Span { lo: 0, hi: bytes.len() as u32 },
        lines: crate::Lines { start: 1, end: lines },
        section: false,
        binary: false,
    }]
}

/// Every file under `dir`, sorted by its `/`-separated path within `dir`.
pub fn walk(dir: &Path) -> Result<Vec<(String, PathBuf)>, Error> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), String::new())];
    while let Some((path, rel)) = stack.pop() {
        let entries = std::fs::read_dir(&path).map_err(|error| Error::Io { context: format!("read dir {}", path.display()), error })?;
        for entry in entries {
            let entry = entry.map_err(|error| Error::Io { context: format!("read dir {}", path.display()), error })?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let p = entry.path();
            if p.is_dir() {
                stack.push((p, child));
            } else if p.is_file() {
                out.push((child, p));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// What notes may cite: a repo circle's tracked paths and commits.
#[derive(Clone, Debug)]
pub struct Cite {
    pub circle: CircleName,
    pub paths: HashSet<String>,
    /// Lowercase hexes, sorted.
    pub commits: Vec<(String, Oid)>,
}

impl Cite {
    /// The paths of `rev`'s tree and the commits reachable from it.
    pub fn from_repo(circle: CircleName, repo: &mut dyn Repo, rev: &Rev) -> Result<Cite, Error> {
        let paths = repo.files(rev.tree)?.into_iter().map(|f| f.path).collect();
        let mut commits: Vec<(String, Oid)> = repo.commits(&[rev.commit], &[])?.into_iter().map(|c| (hex(&c.oid), c.oid)).collect();
        commits.sort();
        Ok(Cite { circle, paths, commits })
    }

    fn commit(&self, prefix: &str) -> Option<Oid> {
        let at = self.commits.partition_point(|(h, _)| h.as_str() < prefix);
        let mut it = self.commits[at..].iter().take_while(|(h, _)| h.starts_with(prefix));
        let first = it.next()?;
        // an ambiguous prefix cites nothing
        it.next().is_none().then_some(first.1)
    }

    /// The edges a text's tokens make: tracked paths (optionally `:line` or `:a-b`) and unique
    /// commit prefixes of 7 or more hex digits with a letter among them.
    pub fn edges(&self, text: &str) -> Vec<Edge> {
        let mut out: Vec<Edge> = Vec::new();
        let mut add = |key: Key| {
            let e = Edge { to: self.circle.clone(), key };
            if !out.contains(&e) {
                out.push(e);
            }
        };
        for raw in text.split(|c: char| c.is_whitespace() || "()[]{}<>\"'`,;|*".contains(c)) {
            let tok = raw.trim_end_matches(['.', ':', '!', '?']).trim_start_matches("./");
            if tok.len() < 7 {
                continue;
            }
            let (path, line) = match tok.rsplit_once(':') {
                Some((p, l)) if !l.is_empty() && l.chars().next().is_some_and(|c| c.is_ascii_digit()) => {
                    (p, l.split('-').next().and_then(|n| n.parse::<u32>().ok()))
                }
                _ => (tok, None),
            };
            if self.paths.contains(path) {
                add(Key::Path { path: path.to_string(), line });
                continue;
            }
            let lower = tok.to_ascii_lowercase();
            if lower.len() <= 40 && lower.bytes().all(|b| b.is_ascii_hexdigit()) && lower.bytes().any(|b| b.is_ascii_alphabetic()) {
                if let Some(oid) = self.commit(&lower) {
                    add(Key::Commit(oid));
                }
            }
        }
        out
    }
}

pub fn hex(oid: &Oid) -> String {
    oid.0.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_by_kind() {
        let plan = include_bytes!("../../fixture/notes/topic-a/PLAN.md");
        let r = records("topic-a/PLAN.md", plan);
        assert_eq!(r.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["Topic A plan", "Gizmo spin"]);
        assert!(r.iter().all(|r| r.section && !r.binary));
        let code = records("spin.rs", include_bytes!("../../fixture/notes/topic-a/spin.rs"));
        assert_eq!(code.len(), 1);
        assert!(!code[0].section && code[0].text.contains("spin_note_draft"));
        let bin = records("pixel.bin", include_bytes!("../../fixture/notes/topic-a/pixel.bin"));
        assert_eq!(bin.len(), 1);
        assert!(bin[0].binary && bin[0].text.is_empty());
        // invalid UTF-8 without a NUL is text, decoded lossily
        let lossy = records("x.txt", b"caf\xe9 ok");
        assert!(!lossy[0].binary && lossy[0].text.contains("caf\u{fffd} ok"));
    }

    #[test]
    fn edges_from_paths_and_commits() {
        let a = Oid([0x9a; 20]);
        let b = Oid([0x9b; 20]);
        let cite = Cite {
            circle: CircleName("makepad".into()),
            paths: ["alpha/src/lib.rs".to_string()].into_iter().collect(),
            commits: vec![(hex(&a), a), (hex(&b), b)],
        };
        let e = cite.edges("`Gizmo::spin` at alpha/src/lib.rs:15 (see ./alpha/src/lib.rs.) landed in 9a9a9a9a; 9b9b9b9 too; 12345678 is a number");
        assert_eq!(
            e.iter().map(|e| e.key.clone()).collect::<Vec<_>>(),
            vec![
                Key::Path { path: "alpha/src/lib.rs".into(), line: Some(15) },
                Key::Path { path: "alpha/src/lib.rs".into(), line: None },
                Key::Commit(a),
                Key::Commit(b),
            ]
        );
    }
}
