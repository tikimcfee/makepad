//! The query-time overlay of a worktree: everything between its base and the files on disk
//! (committed, staged, unstaged, deleted, and untracked files the ignore rules admit, from
//! [`crate::Repo::changes`]; ignored files never) re-extracted in memory through
//! [`crate::Extract::extract_file`] into one [`crate::OverlayFile`] per file; changed and deleted
//! paths shadow their base chunks. Ranking keeps the base's statistics.
//!
//! The circle's own definition applies: a path its exclusions cover (the index's eval data, a
//! claimed notes directory) is neither read, re-extracted nor shadowed, exactly as the base
//! build skipped it, so a lane editing `libs/repo_index/eval/` never reaches the `makepad` circle.

use crate::{circles, BaseCircle, Change, Chunk, CircleName, Error, Extract, OverlayFile, Repo, Rev, Tokenize};
use std::collections::HashSet;
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
    /// The package of the base's manifest at a path (`<dir>/Cargo.toml`), when it is a card.
    /// The overlay's own manifests come first ([`package_of`]).
    pub package: &'a dyn Fn(&str) -> Option<String>,
}

/// The overlay of the worktree at `over.root` over `over.base`. The changed files are read,
/// re-extracted together ([`Extract::extract_files`], on the crate's workers), and their chunks
/// tokenized together ([`crate::open::tokenize_all`], the same workers), so one large file does
/// not hold the rest up. The crate cards the changes alter ([`Extract::extract_cards`]: full
/// cards, as a base build of the worktree gives them) are each their manifest path's whole
/// overlay file, in place of the manifest-only card `extract_files` gives, and shadow that path
/// even when the manifest itself is unchanged (a card whose dependents changed).
pub fn overlay(repo: &mut dyn Repo, extract: &mut dyn Extract, tokenize: &(dyn Tokenize + Sync), over: &Over) -> Result<Overlay, Error> {
    let Over { circle, def, base, root, package } = *over;
    let mut out = Overlay::default();
    let mut read: Vec<(String, Vec<u8>)> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let changes = repo.changes(base)?;
    for change in &changes {
        let (path, shadows) = match change {
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
            deleted.push(path.clone());
            continue;
        }
        match std::fs::read(root.join(path)) {
            Ok(b) => read.push((path.clone(), b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // deleted between the change scan and this read: it shadows, and adds nothing
                eprintln!("makepad index: overlay: {path} changed and is now gone ({e}); treated as deleted");
                if !shadows {
                    out.shadowed.push(path.clone());
                }
                deleted.push(path.clone());
            }
            Err(error) => return Err(Error::Io { context: format!("overlay: reading {path}"), error }),
        }
    }
    let extracted = extract.extract_files(circle, def, &read)?;
    let full = extract.extract_cards(circle, def, base, root, &changes, repo)?;
    // the overlay's manifests: the package of its full card, or none (a changed manifest with no
    // card: virtual now, or not parsing; its own chunks stand)
    let mut cards: Vec<(&str, Option<String>)> = read
        .iter()
        .map(|(p, _)| p.as_str())
        .filter(|p| *p == "Cargo.toml" || p.ends_with("/Cargo.toml"))
        .map(|p| (p, full.iter().find(|c| c.path == p).and_then(|c| c.keys.package.clone())))
        .collect();
    for c in &full {
        if !cards.iter().any(|(p, _)| *p == c.path) {
            cards.push((c.path.as_str(), c.keys.package.clone()));
        }
    }
    let deleted: HashSet<&str> = deleted.iter().map(String::as_str).collect();
    // every file's chunks in one run, with each file's (path, chunk count)
    let mut all: Vec<Chunk> = Vec::new();
    let mut files: Vec<(String, usize)> = Vec::new();
    for ((path, _), mut chunks) in read.iter().zip(extracted) {
        if full.iter().any(|c| c.path == *path) {
            continue; // its full card replaces the manifest-only one
        }
        let pkg = package_of(path, &cards, &deleted, package);
        for c in &mut chunks {
            if c.keys.package.is_none() {
                c.keys.package = pkg.clone();
            }
        }
        if !chunks.is_empty() {
            files.push((path.clone(), chunks.len()));
            all.extend(chunks);
        }
    }
    for c in &full {
        if !out.shadowed.contains(&c.path) {
            out.shadowed.push(c.path.clone());
        }
        files.push((c.path.clone(), 1));
    }
    all.extend(full.iter().cloned());
    let terms = crate::open::tokenize_all(tokenize, &all);
    let mut pairs = all.into_iter().zip(terms);
    for (path, n) in files {
        out.files.push(OverlayFile { path, chunks: pairs.by_ref().take(n).collect() });
    }
    out.files.sort_by(|a, b| a.path.cmp(&b.path));
    out.shadowed.sort();
    out.shadowed.dedup();
    Ok(out)
}

/// The package of `path` in the worktree: that of the nearest enclosing manifest that is a card,
/// the overlay's own first (a crate new since the base, an edited manifest), then the base's,
/// skipping a manifest the worktree deleted and one that is no card (a virtual workspace).
pub fn package_of(path: &str, cards: &[(&str, Option<String>)], deleted: &HashSet<&str>, base: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut dirs: Vec<&str> = Vec::new();
    let mut dir = path;
    while let Some((up, _)) = dir.rsplit_once('/') {
        dirs.push(up);
        dir = up;
    }
    dirs.push("");
    for d in dirs {
        let manifest = if d.is_empty() { "Cargo.toml".to_string() } else { format!("{d}/Cargo.toml") };
        if let Some((_, card)) = cards.iter().find(|(p, _)| *p == manifest) {
            match card {
                Some(pkg) => return Some(pkg.clone()),
                None => continue,
            }
        }
        if deleted.contains(manifest.as_str()) {
            continue;
        }
        if let Some(pkg) = base(&manifest) {
            return Some(pkg);
        }
    }
    None
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
        assert!(o.files.iter().all(|f| f.chunks.iter().all(|(_, t)| !t.is_empty())));
        assert!(o.files.iter().all(|f| f.chunks.iter().all(|(c, _)| c.keys.package.as_deref() == Some("pkg-a"))));
    }

    #[test]
    fn packages_resolve_against_the_overlay_cards_then_the_base() {
        let root = std::env::temp_dir().join(format!("repo-index-delta-pkg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (p, text) in [
            ("Cargo.toml", "[workspace]\nmembers = [\"libs/new\"]\n"),
            ("libs/new/Cargo.toml", "[package]\nname = \"new-crate\"\nversion = \"0.1.0\"\n"),
            ("libs/new/src/lib.rs", "pub fn fresh() {}\n"),
            ("libs/old/src/a.rs", "pub fn edited() {}\n"),
            ("libs/gone/src/b.rs", "pub fn orphan() {}\n"),
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
        let base = repo.rev(
            &["base"],
            1,
            &[
                ("Cargo.toml", "[workspace]\nmembers = [\"libs/old\", \"libs/gone\"]\n"),
                ("libs/old/Cargo.toml", "[package]\nname = \"old-crate\"\nversion = \"0.1.0\"\n"),
                ("libs/old/src/a.rs", "pub fn before() {}\n"),
                ("libs/gone/Cargo.toml", "[package]\nname = \"gone-crate\"\nversion = \"0.1.0\"\n"),
                ("libs/gone/src/b.rs", "pub fn orphan() {}\n"),
            ],
        );
        std::fs::write(root.join("libs/old/Cargo.toml"), "[package]\nname = \"old-crate\"\nversion = \"0.1.0\"\n").unwrap();
        repo.changes = vec![
            Change::Modified("Cargo.toml".into()),
            Change::Added("libs/new/Cargo.toml".into()), // a crate new since the base
            Change::Added("libs/new/src/lib.rs".into()),
            Change::Modified("libs/old/src/a.rs".into()),
            Change::Deleted("libs/gone/Cargo.toml".into()), // its manifest went away
            Change::Modified("libs/gone/src/b.rs".into()),
        ];
        let cards = |p: &str| match p {
            "libs/old/Cargo.toml" => Some("old-crate".to_string()),
            "libs/gone/Cargo.toml" => Some("gone-crate".to_string()),
            _ => None,
        };
        let name = CircleName("makepad".into());
        let over = Over { circle: &name, def: &def, base: &base, root: &root, package: &cards };
        let o = overlay(&mut repo, &mut crate::extract::Extractor::new(), &FakeTokenize, &over).unwrap();
        let pkg = |path: &str| {
            let f = o.files.iter().find(|f| f.path == path).unwrap_or_else(|| panic!("{path} not in the overlay"));
            f.chunks[0].0.keys.package.clone()
        };
        assert_eq!(pkg("libs/new/src/lib.rs").as_deref(), Some("new-crate"));
        assert_eq!(pkg("libs/old/src/a.rs").as_deref(), Some("old-crate"));
        assert_eq!(pkg("libs/gone/src/b.rs"), None, "a deleted manifest is no card");
        // the new crate's manifest is its full card alone, and shadows its path
        let card = o.files.iter().find(|f| f.path == "libs/new/Cargo.toml").unwrap();
        assert_eq!(card.chunks.len(), 1);
        assert!(o.shadowed.contains(&"libs/new/Cargo.toml".to_string()), "a card shadows its path: {:?}", o.shadowed);
        assert_eq!((card.chunks[0].0.kind, card.chunks[0].0.keys.package.as_deref()), (crate::Kind::Crate, Some("new-crate")));
        match &card.chunks[0].0.detail {
            crate::Detail::Crate(c) => assert_eq!(c.rs_files, 1, "a full card counts the crate's files"),
            d => panic!("{d:?}"),
        }
        // the virtual root manifest, changed, is its path chunk
        assert!(o.files.iter().any(|f| f.path == "Cargo.toml"));
        // the virtual root manifest is no card: nothing falls through to it
        let none: HashSet<&str> = HashSet::new();
        assert_eq!(package_of("x/y.rs", &[("Cargo.toml", None)], &none, &|p: &str| (p == "Cargo.toml").then(|| "root-pkg".into())), None);
        assert_eq!(package_of("x/y.rs", &[], &none, &|p: &str| (p == "Cargo.toml").then(|| "root-pkg".into())).as_deref(), Some("root-pkg"));
    }
}
