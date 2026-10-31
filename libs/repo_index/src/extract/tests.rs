//! Extraction over the fixture, committed into a scratch repository with makepad-git.

use super::git::GitRepo;
use super::notes::Cite;
use super::Extractor;
use crate::{
    BaseCircle, BaseSource, Change, Chunk, CircleName, Detail, Exclusion, Extract, ExtractReport, Family, Key, Kind, NotesSource,
    Provenance, Repo, Rev, RevSpec,
};
use makepad_git::{Repository, Signature};
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixture")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let target = to.join(e.file_name());
        if e.path().is_dir() {
            copy_tree(&e.path(), &target);
        } else {
            fs::copy(e.path(), &target).unwrap();
        }
    }
}

fn files_under(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        if e.path().is_dir() {
            files_under(&e.path(), &rel, out);
        } else {
            out.push(rel);
        }
    }
}

fn sig(t: i64) -> Signature {
    Signature { name: "Test".into(), email: "test@example.com".into(), timestamp: t, tz_offset: "+0000".into() }
}

/// `git add -A && git commit`.
fn commit_all(dir: &Path, message: &str, t: i64) {
    let mut repo = Repository::open(dir).unwrap();
    let mut files = Vec::new();
    files_under(dir, "", &mut files);
    for f in &files {
        repo.stage_file(f).unwrap();
    }
    for entry in repo.read_index().unwrap().entries {
        if !files.contains(&entry.path) {
            repo.unstage_file(&entry.path).unwrap();
        }
    }
    repo.commit(&format!("{message}\n"), sig(t)).unwrap();
}

/// The fixture as a fresh repository with one commit on `main`.
fn fixture_repo() -> makepad_git::test_support::TempDir {
    let tmp = makepad_git::test_support::tempdir().unwrap();
    let dir = tmp.path();
    let git = dir.join(".git");
    fs::create_dir_all(git.join("objects")).unwrap();
    fs::create_dir_all(git.join("refs/heads")).unwrap();
    fs::create_dir_all(git.join("refs/tags")).unwrap();
    fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    copy_tree(&fixture_dir(), dir);
    commit_all(dir, "fixture: first", 1_700_000_000);
    tmp
}

fn circle(name: &str) -> CircleName {
    CircleName(name.into())
}

fn tree_def(root: &Path, families: Vec<Family>) -> BaseCircle {
    BaseCircle {
        source: BaseSource::Tree { root: root.to_path_buf() },
        families,
        exclude: vec![
            Exclusion { prefix: "excluded/".into(), provenance: Provenance::IndexOwnData },
            Exclusion { prefix: "notes/".into(), provenance: Provenance::ClaimedBy(circle("notes:topic-a")) },
        ],
    }
}

const CODE: [Family; 5] = [Family::Crate, Family::File, Family::Item, Family::Doc, Family::Splash];

fn extract(def: &BaseCircle, name: &str, rev: Option<&Rev>, repo: &mut dyn Repo, ex: &mut Extractor) -> (Vec<Chunk>, ExtractReport) {
    let mut chunks = Vec::new();
    let report = ex
        .extract(&circle(name), def, rev, repo, &mut |c| {
            chunks.push(c);
            Ok(())
        })
        .unwrap();
    (chunks, report)
}

fn mentions(c: &Chunk, term: &str) -> bool {
    [&c.path, &c.name, &c.fields.name, &c.fields.path, &c.fields.sig, &c.fields.doc, &c.fields.body].iter().any(|s| s.contains(term))
}

fn by_name<'a>(chunks: &'a [Chunk], name: &str) -> (usize, &'a Chunk) {
    chunks.iter().enumerate().find(|(_, c)| c.name == name).unwrap_or_else(|| panic!("no chunk {name}"))
}

#[test]
fn resolves_revisions() {
    let tmp = fixture_repo();
    let mut repo = GitRepo::open(tmp.path()).unwrap();
    let head = repo.resolve(&RevSpec::Pinned("HEAD".into())).unwrap();
    assert_eq!(repo.resolve(&RevSpec::Worktree).unwrap().commit, head.commit);
    assert_eq!(repo.resolve(&RevSpec::Pinned("main".into())).unwrap().commit, head.commit);
    assert_eq!(repo.resolve(&RevSpec::Pinned("refs/heads/main".into())).unwrap().commit, head.commit);
    let hex = super::notes::hex(&head.commit);
    assert_eq!(repo.resolve(&RevSpec::Pinned(hex[..9].to_string())).unwrap().commit, head.commit);
    assert_eq!(repo.resolve(&RevSpec::Pinned(hex.clone())).unwrap().tree, head.tree);
    assert!(repo.resolve(&RevSpec::Pinned("no-such-branch".into())).is_err());
    assert!(repo.resolve(&RevSpec::Pinned("0000000".into())).is_err());
    let again = GitRepo::open(&tmp.path().join("alpha")).unwrap();
    assert_eq!(again.id(), repo.id(), "the id is the common dir's");
}

#[test]
fn tree_circle_from_the_fixture() {
    let tmp = fixture_repo();
    let mut repo = GitRepo::open(tmp.path()).unwrap();
    let rev = repo.resolve(&RevSpec::Pinned("HEAD".into())).unwrap();
    let def = tree_def(tmp.path(), CODE.to_vec());
    let (chunks, report) = extract(&def, "fixture", Some(&rev), &mut repo, &mut Extractor::new());
    assert_eq!(report.chunks as usize, chunks.len());

    // exclusions by provenance: nothing from excluded/ or notes/, in any field
    assert!(chunks.iter().all(|c| !c.path.starts_with("excluded/") && !c.path.starts_with("notes/")));
    // the fixture's README names the sentinel to document it; nothing else may
    assert!(chunks.iter().filter(|c| c.path != "README.md").all(|c| !mentions(c, "fixture_sentinel_excluded") && !mentions(c, "spin_note")));
    let mut excluded = report.excluded.clone();
    excluded.sort();
    assert_eq!(excluded, vec![("excluded/".to_string(), 1), ("notes/".to_string(), 5)]);

    // parents point backwards, at a chunk of the same file or the file's crate
    for (i, c) in chunks.iter().enumerate() {
        if let Some(p) = c.parent {
            assert!((p as usize) < i, "{} parent {p} not before {i}", c.name);
        }
    }

    // crate cards
    let (ai, alpha) = by_name(&chunks, "fixture-alpha");
    let Detail::Crate(card) = &alpha.detail else { panic!("no card") };
    assert_eq!(alpha.path, "alpha/Cargo.toml");
    assert_eq!(card.deps, vec![("fixture-beta".to_string(), String::new())]);
    assert_eq!(card.modules, vec!["tables".to_string()]);
    assert_eq!(card.apps, vec!["spinner".to_string()]);
    assert_eq!(card.rs_files, 2);
    let (_, beta) = by_name(&chunks, "fixture-beta");
    let Detail::Crate(bcard) = &beta.detail else { panic!("no card") };
    assert_eq!(bcard.dependents, vec!["alpha".to_string()]);

    // files and items
    let (fi, file) = chunks.iter().enumerate().find(|(_, c)| c.kind == Kind::File && c.path == "alpha/src/lib.rs").unwrap();
    assert_eq!(file.parent, Some(ai as u32));
    assert_eq!(file.fields.doc, "Spinning gizmos: a gizmo turns by a step each tick.");
    assert_eq!(file.keys.package.as_deref(), Some("fixture-alpha"));
    let (ii, imp) = by_name(&chunks, "impl Gizmo");
    assert_eq!(imp.parent, Some(fi as u32));
    let (_, spin) = by_name(&chunks, "Gizmo::spin");
    assert_eq!(spin.parent, Some(ii as u32));
    assert_eq!((spin.lines.start, spin.keys.idents.clone()), (15, vec!["spin".to_string()]));
    assert!(spin.flags.definition && spin.flags.public && spin.flags.documented);
    assert_eq!(spin.keys.blob, file.keys.blob);

    // generated: skeleton; unpairable: a bag
    let tables = chunks.iter().find(|c| c.kind == Kind::File && c.path == "alpha/src/tables.rs").unwrap();
    assert!(tables.flags.generated);
    let spin_table = by_name(&chunks, "SPIN_TABLE").1;
    assert!(spin_table.flags.generated && spin_table.fields.body.is_empty() && spin_table.fields.sig.starts_with("pub static SPIN_TABLE"));
    let broken = chunks.iter().find(|c| c.path == "beta/src/broken.rs").unwrap();
    assert!(broken.flags.fallback && broken.fields.body.contains("unfinished_turn"));
    assert_eq!(report.fallbacks, vec![("beta/src/broken.rs".to_string(), "delimiters do not pair".to_string())]);

    // docs and splash, owned or not
    let spinning = by_name(&chunks, "Spinning").1;
    assert_eq!((spinning.kind, spinning.parent, spinning.lines.start), (Kind::Doc, None, 5));
    assert!(spinning.fields.doc.contains("steps above `MAX_TURN` are clamped"));
    assert!(chunks.iter().any(|c| c.kind == Kind::Doc && c.path == "README.md"));
    let view = by_name(&chunks, "GizmoView").1;
    assert_eq!((view.kind, view.lines.start, view.keys.idents.clone()), (Kind::Splash, 3, vec!["GizmoView".to_string()]));
}

#[test]
fn families_filter_and_reparent() {
    let tmp = fixture_repo();
    let mut repo = GitRepo::open(tmp.path()).unwrap();
    let rev = repo.resolve(&RevSpec::Pinned("HEAD".into())).unwrap();
    let def = tree_def(tmp.path(), vec![Family::Item]);
    let (chunks, _) = extract(&def, "fixture", Some(&rev), &mut repo, &mut Extractor::new());
    assert!(chunks.iter().all(|c| c.kind.family() == Family::Item));
    let (ii, _) = by_name(&chunks, "impl Gizmo");
    assert_eq!(by_name(&chunks, "Gizmo::spin").1.parent, Some(ii as u32));
    assert_eq!(by_name(&chunks, "Gizmo").1.parent, None, "its file and crate are not emitted");
}

#[test]
fn notes_circle_labels_copies_binaries_and_edges() {
    let tmp = fixture_repo();
    let mut repo = GitRepo::open(tmp.path()).unwrap();
    let rev = repo.resolve(&RevSpec::Pinned("HEAD".into())).unwrap();
    let mut cite = Cite::from_repo(circle("fixture"), &mut repo, &rev).unwrap();
    // notes/ is part of the scratch tree here; the real makepad circle excludes it
    cite.paths.retain(|p| !p.starts_with("notes/"));
    let notes = fixture_dir().join("notes");
    let def = BaseCircle {
        source: BaseSource::Notes {
            sources: vec![
                NotesSource { dir: notes.join("topic-a"), label: "topic-a".into() },
                NotesSource { dir: notes.join("local-a"), label: "fixture/notes/local-a".into() },
            ],
        },
        families: vec![Family::Note],
        exclude: vec![],
    };
    let mut ex = Extractor { cite: Some(cite) };
    let (chunks, report) = extract(&def, "notes:topic-a", None, &mut repo, &mut ex);
    assert_eq!(report.files, 5);
    let paths: Vec<&str> = chunks.iter().map(|c| c.path.as_str()).collect();
    assert!(paths.contains(&"topic-a/spin.rs") && paths.contains(&"fixture/notes/local-a/spin.rs"));
    let plan: Vec<&Chunk> = chunks.iter().filter(|c| c.path == "topic-a/PLAN.md").collect();
    let copy: Vec<&Chunk> = chunks.iter().filter(|c| c.path == "topic-a/COPY.md").collect();
    assert_eq!(plan.len(), 2);
    assert_eq!(plan[0].keys.blob, copy[0].keys.blob, "copies share a blob");
    assert_eq!(plan[1].kind, Kind::Note);
    assert_eq!(plan[1].fields.path, "PLAN.md", "the label is not in the path field");
    assert!(plan[1].fields.body.starts_with("## Gizmo spin"), "record text is the body");
    assert!(plan[1].keys.edges.iter().any(|e| e.key == Key::Path { path: "alpha/src/lib.rs".into(), line: Some(15) }));
    let bin = chunks.iter().find(|c| c.path == "topic-a/pixel.bin").unwrap();
    assert_eq!(bin.kind, Kind::NoteFile);
    assert!(bin.flags.binary && bin.fields.body.is_empty());
    let local_spin = chunks.iter().find(|c| c.path == "fixture/notes/local-a/spin.rs").unwrap();
    assert_eq!(local_spin.kind, Kind::NoteFile);
    assert!(local_spin.fields.body.contains("spin_note_local"));
}

#[test]
fn history_drops_eval_only_commits() {
    let tmp = fixture_repo();
    let dir = tmp.path();
    fs::write(dir.join("excluded/sentinel.rs"), "pub fn fixture_sentinel_excluded() { /* more */ }\n").unwrap();
    commit_all(dir, "eval: add fixture_sentinel_excluded question", 1_700_000_100);
    fs::write(dir.join("beta/src/lib.rs"), "pub fn clamp_turn(step: u32, max: u32) -> u32 {\n    step.min(max).max(1)\n}\n").unwrap();
    fs::write(dir.join("excluded/sentinel.rs"), "pub fn fixture_sentinel_excluded() {}\n").unwrap();
    commit_all(dir, "beta: clamp turns\n\n- beta: never turn by zero,\n  even when asked\n- excluded: tidy", 1_700_000_200);
    let mut repo = GitRepo::open(dir).unwrap();
    let def = BaseCircle {
        source: BaseSource::History { root: dir.to_path_buf(), upstream: "refs/heads/main".into(), local: vec![] },
        families: vec![Family::Commit],
        exclude: vec![Exclusion { prefix: "excluded/".into(), provenance: Provenance::IndexOwnData }],
    };
    let (chunks, report) = extract(&def, "fixture:history", None, &mut repo, &mut Extractor::new());
    let commits: Vec<&Chunk> = chunks.iter().filter(|c| c.kind == Kind::Commit).collect();
    assert_eq!(commits.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["beta: clamp turns", "fixture: first"]);
    assert!(chunks.iter().all(|c| !mentions(c, "fixture_sentinel_excluded") && !mentions(c, "excluded/")));
    assert_eq!(commits[0].keys.paths, vec!["beta/src/lib.rs".to_string()]);
    assert!(matches!(commits[0].detail, Detail::Commit(ref i) if i.time == 1_700_000_200));
    let bullets: Vec<&Chunk> = chunks.iter().filter(|c| c.kind == Kind::Bullet).collect();
    assert_eq!(bullets.len(), 2);
    assert_eq!(bullets[0].fields.body, "beta: never turn by zero,\neven when asked");
    assert_eq!(bullets[0].parent, Some(0));
    assert!(report.excluded.iter().any(|(p, n)| p == "excluded/" && *n >= 2));
}

#[test]
fn worktree_changes_and_file_overlay() {
    let tmp = fixture_repo();
    let dir = tmp.path();
    let mut repo = GitRepo::open(dir).unwrap();
    let base = repo.resolve(&RevSpec::Pinned("HEAD".into())).unwrap();
    fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
    commit_all(dir, "ignore", 1_700_000_300);
    fs::write(dir.join("alpha/src/lib.rs"), "//! Changed.\npub fn changed() {}\n").unwrap();
    fs::remove_file(dir.join("beta/src/lib.rs")).unwrap();
    fs::write(dir.join("alpha/src/new_file.rs"), "pub fn brand_new() {}\n").unwrap();
    fs::write(dir.join("ignored.txt"), "never a change\n").unwrap();
    let mut repo = GitRepo::open(dir).unwrap();
    let mut changes = repo.changes(&base).unwrap();
    changes.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert_eq!(
        changes,
        vec![
            Change::Added(".gitignore".into()),
            Change::Added("alpha/src/new_file.rs".into()),
            Change::Deleted("beta/src/lib.rs".into()),
            Change::Modified("alpha/src/lib.rs".into()),
        ]
    );

    let def = tree_def(dir, CODE.to_vec());
    let mut ex = Extractor::new();
    let c = circle("fixture");
    assert!(ex.extract_file(&c, &def, "excluded/sentinel.rs", b"pub fn fixture_sentinel_excluded() {}").unwrap().is_empty());
    let rs = ex.extract_file(&c, &def, "alpha/src/new_file.rs", b"/// New.\npub fn brand_new() {}\n").unwrap();
    assert_eq!(rs.iter().map(|c| (c.kind, c.name.as_str(), c.parent)).collect::<Vec<_>>(), vec![(Kind::File, "new_file.rs", None), (Kind::Fn, "brand_new", Some(0))]);
    let card = ex.extract_file(&c, &def, "alpha/Cargo.toml", include_bytes!("../../fixture/alpha/Cargo.toml")).unwrap();
    let Detail::Crate(card) = &card[0].detail else { panic!("no card") };
    assert_eq!(card.deps, vec![("fixture-beta".to_string(), String::new())]);
    assert!(card.dependents.is_empty());
}

/// Extraction over a whole repository at a revision, from git objects only; time it by running
/// the test binary under `time`: `MAKEPAD_INDEX_REPO=<checkout> [MAKEPAD_INDEX_REV=<rev>]
/// cargo test --release -p makepad-repo-index whole_repo_extraction -- --ignored --nocapture`.
#[test]
#[ignore]
fn whole_repo_extraction() {
    let Some(root) = std::env::var_os("MAKEPAD_INDEX_REPO") else { panic!("set MAKEPAD_INDEX_REPO") };
    let rev_name = std::env::var("MAKEPAD_INDEX_REV").unwrap_or_else(|_| "HEAD".into());
    let root = PathBuf::from(root);
    let mut repo = GitRepo::open(&root).unwrap();
    let rev = repo.resolve(&RevSpec::Pinned(rev_name.clone())).unwrap();
    let def = BaseCircle {
        source: BaseSource::Tree { root: root.clone() },
        families: CODE.to_vec(),
        exclude: crate::INDEX_OWN_DATA.iter().map(|p| Exclusion { prefix: p.to_string(), provenance: Provenance::IndexOwnData }).collect(),
    };
    let mut kinds: std::collections::BTreeMap<&'static str, u64> = std::collections::BTreeMap::new();
    let mut bytes = 0u64;
    let report = Extractor::new()
        .extract(&circle("makepad"), &def, Some(&rev), &mut repo, &mut |c| {
            *kinds.entry(c.kind.as_str()).or_default() += 1;
            bytes += (c.fields.name.len() + c.fields.path.len() + c.fields.sig.len() + c.fields.doc.len() + c.fields.body.len()) as u64;
            Ok(())
        })
        .unwrap();
    println!("rev {rev_name}: extracted {} files into {} chunks", report.files, report.chunks);
    println!("field text {:.1} MB; chunks by kind {kinds:?}", bytes as f64 / 1e6);
    println!("fallbacks {}: {:?}", report.fallbacks.len(), report.fallbacks);
    println!("skipped {}: {:?}; excluded {:?}", report.skipped.len(), report.skipped, report.excluded);
}
