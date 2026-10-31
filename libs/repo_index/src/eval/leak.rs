//! The leak checks.
//! - **Containment:** no chunk of an opened circle, in a segment or the overlay, lies under one of
//!   its circle's exclusions (the index's own data, a claimed notes directory).
//! - **The crate's files:** nothing under `libs/repo_index/` (code, fixture, docs, the shipped
//!   questions) names a consumer's terms, a machine's notes paths, a fork-only commit, a home
//!   path, a host or a person (EVAL R8; makepad is public). The terms are assembled at run time,
//!   so this file does not hold them itself.

use crate::{circles, BaseCircle, CircleBody, CircleManifest, ChunkId, OpenCircle, Opened};
use std::path::Path;

/// Every chunk of `c` an exclusion of its definition covers, as `path (prefix)`.
pub fn containment_of(c: &OpenCircle, def: &BaseCircle) -> Vec<String> {
    let mut out = Vec::new();
    // exclusions are written against the path within the source (a note loses its label)
    let mut check = |path: &str, in_source: &str, where_: &str| {
        if let Some(e) = circles::excluded(def, in_source) {
            out.push(format!("{}: {where_} chunk {path} is under the exclusion {}", c.name.0, e.prefix));
        }
    };
    for s in &c.segments {
        for i in 0..s.chunk_count() {
            let v = s.chunk(ChunkId(i));
            check(v.path, v.path_in_source(), "segment");
        }
    }
    for f in &c.overlay {
        for (ch, _) in &f.chunks {
            check(&ch.path, &ch.path[ch.label_len as usize + usize::from(ch.label_len > 0)..], "overlay");
        }
    }
    out
}

/// [`containment_of`] over every circle opened, named or not.
pub fn containment(opened: &Opened, manifest: &CircleManifest) -> Vec<String> {
    let mut out = Vec::new();
    for c in opened.open.iter().chain(&opened.others) {
        if let Some(CircleBody::Base(def)) = circles::find_def(manifest, &c.name).map(|d| &d.body) {
            out.extend(containment_of(c, def));
        }
    }
    out
}

/// What must never appear in the crate's tracked files: (term, case-sensitive, whole word).
fn banned() -> Vec<(String, bool, bool)> {
    let j = |parts: &[&str]| parts.concat();
    let mut v = vec![
        (j(&["gly", "ph3d"]), false, false),
        (j(&["G3", "Rend", "erer"]), true, false),
        (j(&["rever", "se_z"]), true, false),
        (j(&["drop", "_frame"]), true, false),
        (j(&["MAKEPAD_RUN", "_OVER", "LAY"]), true, false),
        (j(&["RELAX", "_PROVE"]), true, false),
        (j(&["agent", "-mail"]), false, false),
        (j(&["agent", "_state/", "gly"]), false, false),
        (j(&["/ho", "me/"]), true, false),
        (j(&["/Us", "ers/"]), true, false),
        (j(&["mono", "lith"]), false, false),
        (j(&["Iv", "an"]), true, true),
    ];
    // commits that exist only on a fork's branch
    for h in [["5e34", "823f6"], ["f3f2", "f350e"], ["f5b4", "ad401"], ["921b", "2f362"], ["4236", "e3a0c"], ["a9e2", "df5db"], ["6bd6", "2a67f"]] {
        v.push((j(&h), false, false));
    }
    v
}

fn holds(line: &str, term: &str, case: bool, word: bool) -> bool {
    let (hay, needle) = if case { (line.to_string(), term.to_string()) } else { (line.to_lowercase(), term.to_lowercase()) };
    hay.match_indices(&needle).any(|(i, _)| {
        !word || {
            let ident = |c: char| c.is_alphanumeric() || c == '_';
            !hay[..i].chars().next_back().is_some_and(ident) && !hay[i + needle.len()..].chars().next().is_some_and(ident)
        }
    })
}

/// Every banned term in the files under `root` (build output skipped), as `path:line: term`.
pub fn scan_files(root: &Path) -> Result<Vec<String>, crate::Error> {
    let terms = banned();
    let mut out = Vec::new();
    for (rel, full) in crate::open::walk_files(root)? {
        if rel.starts_with("target/") {
            continue;
        }
        let bytes = std::fs::read(&full).map_err(|error| crate::Error::Io { context: format!("leak scan {}", full.display()), error })?;
        let text = String::from_utf8_lossy(&bytes);
        for (n, line) in text.lines().enumerate() {
            for (t, case, word) in &terms {
                if holds(line, t, *case, *word) {
                    out.push(format!("{rel}:{}: `{t}`", n + 1));
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open::fakes::{FakeSegment, FakeExtract};
    use crate::{CircleName, Extract, FieldTerms, RepoId, SegmentKey, SourceStamp};

    #[test]
    fn the_crate_holds_no_leak() {
        let hits = scan_files(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        assert!(hits.is_empty(), "leaks:\n{}", hits.join("\n"));
    }

    #[test]
    fn the_scan_finds_what_it_bans() {
        let d = std::env::temp_dir().join(format!("repo-index-leak-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("target")).unwrap();
        let j = |p: &[&str]| p.concat();
        std::fs::write(d.join("a.md"), format!("fine\nsee {}\nderivative is fine\n", j(&["GLY", "PH3D"]))).unwrap();
        std::fs::write(d.join("b.rs"), format!("// by {}\n", j(&["Iv", "an"]))).unwrap();
        std::fs::write(d.join("target/x"), j(&["gly", "ph3d"])).unwrap();
        assert_eq!(scan_files(&d).unwrap().len(), 2);
    }

    #[test]
    fn containment_flags_excluded_chunks_in_segments_and_overlay() {
        let root = std::env::temp_dir().join(format!("repo-index-contain-{}", std::process::id()));
        let m = circles::manifest_for(&root, None, &root).unwrap();
        let def = match &circles::find_def(&m, &CircleName("makepad".into())).unwrap().body {
            CircleBody::Base(b) => b.clone(),
            _ => unreachable!(),
        };
        let name = CircleName("makepad".into());
        // a chunk the extractor never makes for an excluded path, forced in to prove the check
        let mut leaked = FakeExtract.extract_file(&name, &def, "a.rs", b"fn a() {}\n").unwrap().remove(0);
        leaked.path = "libs/repo_index/eval/questions.jsonl".into();
        let key = SegmentKey { repo: Some(RepoId(1)), circle: name.clone(), source: SourceStamp::Tree { tree: crate::Oid([0; 20]) }, def_hash: 0, indexer: 1 };
        let ok = FakeExtract.extract_file(&name, &def, "a.rs", b"fn a() {}\n").unwrap();
        let mut c = OpenCircle {
            name: name.clone(),
            rev: None,
            segments: vec![Box::new(FakeSegment { key, chunks: ok.into_iter().map(|c| (c, FieldTerms::default())).collect() })],
            stats: Default::default(),
            overlay: Vec::new(),
            shadowed: Vec::new(),
            visible: None,
        };
        assert!(containment_of(&c, &def).is_empty());
        c.overlay.push(crate::OverlayFile { path: leaked.path.clone(), chunks: vec![(leaked, FieldTerms::default())] });
        assert_eq!(containment_of(&c, &def).len(), 1);
    }
}
