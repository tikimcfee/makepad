//! The query-time overlay of a worktree: everything between its base and the files on disk
//! (committed, staged, unstaged, deleted, and untracked files the ignore rules admit, from
//! [`crate::Repo::changes`]; ignored files never) re-extracted in memory through
//! [`crate::Extract::extract_file`] into one [`crate::OverlayFile`] per file; changed and deleted
//! paths shadow their base chunks. Ranking keeps the base's statistics.
//!
//! The circle's own definition applies: a path its exclusions cover (the index's eval data, a
//! claimed notes directory) is neither read, re-extracted nor shadowed, exactly as the base
//! build skipped it, so a lane editing `libs/repo_index/eval/` never reaches the `makepad` circle.

use crate::{circles, BaseCircle, Change, CircleName, Error, Extract, OverlayFile, Repo, Rev, Tokenize};
use std::path::Path;

/// A worktree's overlay over one tree circle.
#[derive(Debug, Default)]
pub struct Overlay {
    /// Changed and added files, re-extracted, by path.
    pub files: Vec<OverlayFile>,
    /// Base paths the overlay replaces or deletes, sorted.
    pub shadowed: Vec<String>,
    /// Paths an exclusion kept out, for `status`.
    pub excluded: u64,
}

/// What an overlay is taken over.
pub struct Over<'a> {
    pub circle: &'a CircleName,
    pub def: &'a BaseCircle,
    pub base: &'a Rev,
    /// The worktree root the changed files are read from.
    pub root: &'a Path,
    /// A path's package in the base (a lone file's chunks carry none: no tree is around them).
    pub package: &'a dyn Fn(&str) -> Option<String>,
}

/// The overlay of the worktree at `over.root` over `over.base`.
pub fn overlay(repo: &mut dyn Repo, extract: &mut dyn Extract, tokenize: &dyn Tokenize, over: &Over) -> Result<Overlay, Error> {
    let Over { circle, def, base, root, package } = *over;
    let mut out = Overlay::default();
    for change in repo.changes(base)? {
        let (path, shadows) = match &change {
            Change::Added(p) => (p, false),
            Change::Modified(p) | Change::Deleted(p) => (p, true),
        };
        if circles::excluded(def, path).is_some() {
            out.excluded += 1;
            continue;
        }
        if shadows {
            out.shadowed.push(path.clone());
        }
        if matches!(change, Change::Deleted(_)) {
            continue;
        }
        let bytes = match std::fs::read(root.join(path)) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // deleted between the change scan and this read: it shadows, and adds nothing
                eprintln!("makepad index: overlay: {path} changed and is now gone ({e}); treated as deleted");
                if !shadows {
                    out.shadowed.push(path.clone());
                }
                continue;
            }
            Err(error) => return Err(Error::Io { context: format!("overlay: reading {path}"), error }),
        };
        let pkg = package(path);
        let chunks: Vec<_> = extract
            .extract_file(circle, def, path, &bytes)?
            .into_iter()
            .map(|mut c| {
                if c.keys.package.is_none() {
                    c.keys.package = pkg.clone();
                }
                let t = tokenize.chunk_terms(&c);
                (c, t)
            })
            .collect();
        if !chunks.is_empty() {
            out.files.push(OverlayFile { path: path.clone(), chunks });
        }
    }
    out.files.sort_by(|a, b| a.path.cmp(&b.path));
    out.shadowed.sort();
    out.shadowed.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open::fakes::{FakeExtract, FakeRepo, FakeTokenize};
    use crate::{circles, Change, Oid, RevSpec};

    #[test]
    fn overlay_reextracts_shadows_and_keeps_exclusions_out() {
        let root = std::env::temp_dir().join(format!("repo-index-delta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (p, text) in [
            ("a/edited.rs", "fn edited_now() {}\n"),
            ("a/new.rs", "fn brand_new() {}\n"),
            ("libs/repo_index/eval/questions.jsonl", "fn never_indexed() {}\n"),
        ] {
            let f = root.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, text).unwrap();
        }
        let m = circles::manifest_for(&root, None, &root).unwrap();
        let def = match &circles::find_def(&m, &CircleName("makepad".into())).unwrap().body {
            crate::CircleBody::Base(b) => b.clone(),
            _ => unreachable!(),
        };
        let mut repo = FakeRepo::new(&root);
        repo.changes = vec![
            Change::Modified("a/edited.rs".into()),
            Change::Added("a/new.rs".into()), // committed since the base, or untracked and not ignored
            Change::Deleted("a/gone.rs".into()),
            Change::Added("a/raced.rs".into()), // listed, then deleted before the read
            Change::Added("libs/repo_index/eval/questions.jsonl".into()),
            Change::Modified("libs/repo_index/eval/old.jsonl".into()),
        ];
        let base = Rev { spec: RevSpec::Worktree, commit: Oid([1; 20]), tree: Oid([2; 20]) };
        let pkg = |p: &str| p.starts_with("a/").then(|| "pkg-a".to_string());
        let name = CircleName("makepad".into());
        let over = Over { circle: &name, def: &def, base: &base, root: &root, package: &pkg };
        let o = overlay(&mut repo, &mut FakeExtract, &FakeTokenize, &over).unwrap();
        let paths: Vec<&str> = o.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a/edited.rs", "a/new.rs"]);
        assert_eq!(o.shadowed, ["a/edited.rs", "a/gone.rs", "a/raced.rs"]);
        assert_eq!(o.excluded, 2);
        let names: Vec<&str> = o.files.iter().flat_map(|f| f.chunks.iter().map(|(c, _)| c.name.as_str())).collect();
        assert!(names.contains(&"edited_now") && names.contains(&"brand_new"), "{names:?}");
        assert!(!names.contains(&"never_indexed"));
        assert!(o.files.iter().all(|f| f.chunks.iter().all(|(_, t)| !t.terms.is_empty())));
        assert!(o.files.iter().all(|f| f.chunks.iter().all(|(c, _)| c.keys.package.as_deref() == Some("pkg-a"))));
    }
}
