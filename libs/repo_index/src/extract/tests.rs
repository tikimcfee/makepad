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
fn gists_are_first_sentences_within_the_cap() {
    assert_eq!(super::gist("# Title\n\nFirst sentence. Second one."), "Title");
    assert_eq!(super::gist("First sentence. Second one.\nmore"), "First sentence.");
    assert_eq!(super::gist("- a bullet line\n- next"), "a bullet line next");
    assert_eq!(super::gist("\n\n"), "");
    let long = "word ".repeat(40);
    let g = super::gist(&long);
    assert_eq!(g.chars().count(), crate::MAX_GIST_CHARS);
    assert!(g.ends_with('…'));
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
fn lists_worktrees_with_their_heads() {
    let tmp = fixture_repo();
    let main_head = GitRepo::open(tmp.path()).unwrap().resolve(&RevSpec::Pinned("HEAD".into())).unwrap().commit;
    let linked = makepad_git::test_support::tempdir().unwrap();
    let lane = linked.path().join("lane");
    let gone = linked.path().join("gone");
    {
        let mut repo = Repository::open(tmp.path()).unwrap();
        let start = repo.head_oid().unwrap();
        repo.worktree_add(&lane, makepad_git::WorktreeBranch::NewFrom { name: "lane", start }, None).unwrap();
        repo.worktree_add(&gone, makepad_git::WorktreeBranch::NewFrom { name: "gone", start }, None).unwrap();
    }
    fs::write(lane.join("alpha/lane.rs"), "pub fn lane() {}\n").unwrap();
    commit_all(&lane, "lane: work", 1_700_000_400);
    fs::remove_dir_all(&gone).unwrap();
    let lane_head = GitRepo::open(&lane).unwrap().resolve(&RevSpec::Worktree).unwrap().commit;
    // every worktree goes by the main checkout's name
    let main_name = tmp.path().canonicalize().unwrap().file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(GitRepo::open(&lane).unwrap().name(), main_name);
    assert_eq!(GitRepo::open(tmp.path()).unwrap().name(), main_name);
    assert_eq!(super::git::repo_name(&lane).unwrap(), Some(main_name.clone()));
    assert_eq!(super::git::repo_name(tmp.path()).unwrap(), Some(main_name));
    assert_eq!(super::git::repo_name(&lane.join("alpha")).unwrap(), None, "not a worktree root");
    assert_ne!(lane_head, main_head);
    for from in [tmp.path(), lane.as_path()] {
        let mut repo = GitRepo::open(from).unwrap();
        let list = repo.worktrees().unwrap();
        let canon = |p: &Path| p.canonicalize().unwrap();
        let got: Vec<(PathBuf, crate::Oid)> = list.iter().map(|(p, h)| (canon(p), *h)).collect();
        assert_eq!(got, vec![(canon(tmp.path()), main_head), (canon(&lane), lane_head)], "the removed worktree is not listed");
    }
}

/// Every worktree of a real checkout: `MAKEPAD_INDEX_REPO=<checkout> cargo test -p
/// makepad-repo-index real_worktrees -- --ignored --nocapture`, to compare with
/// `git worktree list`.
#[test]
#[ignore]
fn real_worktrees() {
    let Some(root) = std::env::var_os("MAKEPAD_INDEX_REPO") else { panic!("set MAKEPAD_INDEX_REPO") };
    let mut repo = GitRepo::open(Path::new(&root)).unwrap();
    for (path, head) in repo.worktrees().unwrap() {
        println!("{} {}", path.display(), super::notes::hex(&head));
    }
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
    assert_eq!(alpha.fields.sig, "lib");
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
    assert_eq!(spin.gist, "Spins the gizmo by `step`, clamped to [`MAX_TURN`].");
    assert_eq!(spin.label_len, 0);
    assert_eq!(file.gist, "Spinning gizmos: a gizmo turns by a step each tick.");
    assert_eq!(alpha.gist, "Spinning gizmos for the index fixture");
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
    assert_eq!(spinning.gist, "Call `Gizmo::spin` with a step; steps above `MAX_TURN` are clamped.");
    assert!(chunks.iter().any(|c| c.kind == Kind::Doc && c.path == "README.md"));
    // every other tracked file is one path-only record; a card's own Cargo.toml is not
    let path_only: Vec<&str> = chunks.iter().filter(|c| c.kind == Kind::File && !c.path.ends_with(".rs")).map(|c| c.path.as_str()).collect();
    assert_eq!(path_only, vec!["Cargo.toml", "libs/app_registry/apps.json"]);
    let apps = chunks.iter().find(|c| c.path == "libs/app_registry/apps.json").unwrap();
    assert_eq!((apps.name.as_str(), apps.fields.path.as_str(), apps.keys.paths.clone()), ("apps.json", "libs/app_registry/apps.json", vec!["libs/app_registry/apps.json".to_string()]));
    assert!(apps.keys.blob.is_some() && apps.fields.body.is_empty() && apps.gist.is_empty() && apps.parent.is_none() && apps.keys.package.is_none());
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
    let mut ex = Extractor::citing(circle("fixture"), RevSpec::Pinned("HEAD".into()));
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
    assert_eq!(plan[1].label_len as usize, "topic-a".len());
    assert_eq!(&plan[1].path[plan[1].label_len as usize + 1..], "PLAN.md");
    assert_eq!(plan[1].gist, "`Gizmo::spin` at alpha/src/lib.rs:15 clamps the step through `clamp_turn`.");
    assert_eq!(local_label_len(&chunks), "fixture/notes/local-a".len());
    assert!(plan[1].fields.body.starts_with("## Gizmo spin"), "record text is the body");
    assert!(plan[1].keys.edges.iter().any(|e| e.key == Key::Path { path: "alpha/src/lib.rs".into(), line: Some(15) }));
    let bin = chunks.iter().find(|c| c.path == "topic-a/pixel.bin").unwrap();
    assert_eq!(bin.kind, Kind::NoteFile);
    assert!(bin.flags.binary && bin.fields.body.is_empty());
    let local_spin = chunks.iter().find(|c| c.path == "fixture/notes/local-a/spin.rs").unwrap();
    assert_eq!(local_spin.kind, Kind::NoteFile);
    assert!(local_spin.fields.body.contains("spin_note_local"));
}

fn local_label_len(chunks: &[Chunk]) -> usize {
    chunks.iter().find(|c| c.path.starts_with("fixture/notes/local-a/")).unwrap().label_len as usize
}

/// A repo that counts its commit walks (what a cite costs).
struct Counting<'a> {
    inner: &'a mut GitRepo,
    walks: usize,
}

impl Repo for Counting<'_> {
    fn id(&self) -> crate::RepoId {
        self.inner.id()
    }
    fn root(&self) -> PathBuf {
        self.inner.root()
    }
    fn resolve(&mut self, spec: &RevSpec) -> Result<Rev, crate::Error> {
        self.inner.resolve(spec)
    }
    fn files(&mut self, tree: crate::Oid) -> Result<Vec<crate::TreeFile>, crate::Error> {
        self.inner.files(tree)
    }
    fn read_blob(&mut self, blob: crate::Oid) -> Result<Vec<u8>, crate::Error> {
        self.inner.read_blob(blob)
    }
    fn changes(&mut self, base: &Rev) -> Result<Vec<Change>, crate::Error> {
        self.inner.changes(base)
    }
    fn commits(&mut self, tips: &[crate::Oid], hide: &[crate::Oid]) -> Result<Vec<crate::CommitRecord>, crate::Error> {
        self.walks += 1;
        self.inner.commits(tips, hide)
    }
    fn worktrees(&mut self) -> Result<Vec<(PathBuf, crate::Oid)>, crate::Error> {
        self.inner.worktrees()
    }
    fn commit_ids(&mut self, tips: &[crate::Oid]) -> Result<Vec<crate::Oid>, crate::Error> {
        self.walks += 1;
        self.inner.commit_ids(tips)
    }
    fn merge_base(&mut self, a: crate::Oid, b: crate::Oid) -> Result<Option<crate::Oid>, crate::Error> {
        self.inner.merge_base(a, b)
    }
}

#[test]
fn the_cite_is_computed_only_for_notes_and_reused() {
    let tmp = fixture_repo();
    let mut git = GitRepo::open(tmp.path()).unwrap();
    let rev = git.resolve(&RevSpec::Pinned("HEAD".into())).unwrap();
    let mut repo = Counting { inner: &mut git, walks: 0 };
    let mut ex = Extractor::citing(circle("fixture"), RevSpec::Pinned("HEAD".into()));
    extract(&tree_def(tmp.path(), CODE.to_vec()), "fixture", Some(&rev), &mut repo, &mut ex);
    assert_eq!(repo.walks, 0, "a tree circle computes no cite");
    let notes = BaseCircle {
        source: BaseSource::Notes { sources: vec![NotesSource { dir: fixture_dir().join("notes/topic-a"), label: "topic-a".into() }] },
        families: vec![Family::Note],
        exclude: vec![],
    };
    let (chunks, _) = extract(&notes, "notes:topic-a", None, &mut repo, &mut ex);
    assert_eq!(repo.walks, 1, "the first notes circle computes it");
    assert!(chunks.iter().any(|c| !c.keys.edges.is_empty()));
    extract(&notes, "notes:topic-a", None, &mut repo, &mut ex);
    assert_eq!(repo.walks, 1, "the same revision reuses it");
    let mut plain = Extractor::new();
    let (chunks, _) = extract(&notes, "notes:topic-a", None, &mut repo, &mut plain);
    assert_eq!(repo.walks, 1);
    assert!(chunks.iter().all(|c| c.keys.edges.is_empty()), "no cite, no edges");
}

#[test]
fn extract_files_is_extract_file_per_file_in_order() {
    let mut files = Vec::new();
    files_under(&fixture_dir(), "", &mut files);
    let inputs: Vec<(String, Vec<u8>)> = files.iter().map(|f| (f.clone(), fs::read(fixture_dir().join(f)).unwrap())).collect();
    let def = tree_def(&fixture_dir(), CODE.to_vec());
    let mut ex = Extractor::new();
    let c = circle("fixture");
    let together = ex.extract_files(&c, &def, &inputs).unwrap();
    assert_eq!(together.len(), inputs.len());
    let debug = |v: &Vec<Chunk>| format!("{v:?}");
    for ((path, bytes), got) in inputs.iter().zip(&together) {
        assert_eq!(debug(got), debug(&ex.extract_file(&c, &def, path, bytes).unwrap()), "{path}");
    }
    assert!(together[inputs.iter().position(|(p, _)| p == "excluded/sentinel.rs").unwrap()].is_empty());
    assert!(together.iter().any(|v| v.iter().any(|c| c.name == "Gizmo::spin")));
    // a history circle has no files: the error comes back
    let history = BaseCircle { source: BaseSource::History { root: fixture_dir(), upstream: "x".into(), local: vec![] }, families: vec![Family::Commit], exclude: vec![] };
    assert!(ex.extract_files(&c, &history, &inputs[..1]).is_err());
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

/// Every tracked file outside the exclusions has a chunk, in the base build and in
/// [`Extract::extract_file`] alike (an overlay that re-extracts a file never deletes it), and a
/// file the extractors find nothing in is its path chunk in both.
#[test]
fn every_file_has_a_chunk_in_the_base_and_the_overlay() {
    let tmp = fixture_repo();
    let dir = tmp.path();
    fs::create_dir_all(dir.join("scripts")).unwrap();
    fs::write(dir.join("scripts/deploy.sh"), "#!/bin/sh\necho deploy\n").unwrap();
    fs::write(dir.join("ui/empty.splash"), "// no definitions here\n").unwrap();
    fs::write(dir.join("docs/empty.md"), "").unwrap();
    commit_all(dir, "files without items", 1_700_000_050);
    let mut repo = GitRepo::open(dir).unwrap();
    let rev = repo.resolve(&RevSpec::Pinned("HEAD".into())).unwrap();
    let def = tree_def(dir, CODE.to_vec());
    let (base, _) = extract(&def, "fixture", Some(&rev), &mut repo, &mut Extractor::new());
    let mut files = Vec::new();
    files_under(dir, "", &mut files);
    let mut ex = Extractor::new();
    let c = circle("fixture");
    for path in files.iter().filter(|p| !p.starts_with(".git/") && !p.starts_with("excluded/") && !p.starts_with("notes/")) {
        let bytes = fs::read(dir.join(path)).unwrap();
        let overlay = ex.extract_file(&c, &def, path, &bytes).unwrap();
        assert!(!overlay.is_empty(), "the overlay drops {path}");
        let card = overlay.iter().any(|c| c.kind == Kind::Crate);
        assert!(card || base.iter().any(|b| &b.path == path), "the base drops {path}");
    }
    for path in ["Cargo.toml", "scripts/deploy.sh", "ui/empty.splash", "docs/empty.md", "libs/app_registry/apps.json"] {
        let bytes = fs::read(dir.join(path)).unwrap();
        let overlay = ex.extract_file(&c, &def, path, &bytes).unwrap();
        let in_base: Vec<&Chunk> = base.iter().filter(|b| b.path == path).collect();
        assert_eq!((overlay.len(), in_base.len()), (1, 1), "{path}");
        let (o, b) = (&overlay[0], in_base[0]);
        assert_eq!((o.kind, o.name.as_str(), &o.fields.path, &o.keys.paths, o.keys.blob), (Kind::File, b.name.as_str(), &b.fields.path, &b.keys.paths, b.keys.blob), "{path}");
    }
}

fn cards_by_dir(chunks: &[Chunk]) -> std::collections::BTreeMap<String, String> {
    chunks
        .iter()
        .filter_map(|c| match &c.detail {
            Detail::Crate(card) => Some((card.dir.clone(), format!("{} {:?} {:?} {}", c.name, c.fields.sig, card, c.flags.generated))),
            _ => None,
        })
        .collect()
}

/// The overlay's crate cards are the cards a base build of the worktree's tree gives: a changed
/// manifest keeps its dependents, apps, targets, file count and modules; a crate new since the
/// base gets its full card; a crate whose dependents changed gets its card again; the rest are
/// not returned, and are unchanged.
#[test]
fn overlay_cards_are_the_base_build_of_the_worktree() {
    let tmp = fixture_repo();
    let dir = tmp.path();
    let mut repo = GitRepo::open(dir).unwrap();
    let base = repo.resolve(&RevSpec::Pinned("HEAD".into())).unwrap();
    let def = tree_def(dir, CODE.to_vec());
    let ex = Extractor::new();
    let c = circle("fixture");
    let (before, _) = extract(&def, "fixture", Some(&base), &mut repo, &mut Extractor::new());

    // no change the graph sees: nothing
    fs::write(dir.join("alpha/src/lib.rs"), fs::read_to_string(dir.join("alpha/src/lib.rs")).unwrap() + "\npub fn more() {}\n").unwrap();
    let changes = repo.changes(&base).unwrap();
    assert!(ex.cards(&c, &def, &base, dir, &changes, &mut repo).unwrap().is_empty());

    // a manifest edited, a crate added that depends on beta, a module file deleted
    let alpha = fs::read_to_string(dir.join("alpha/Cargo.toml")).unwrap();
    fs::write(dir.join("alpha/Cargo.toml"), alpha.replace("Spinning gizmos for the index fixture", "Spinning gizmos, edited")).unwrap();
    fs::create_dir_all(dir.join("gamma/src")).unwrap();
    fs::write(dir.join("gamma/Cargo.toml"), "[package]\nname = \"fixture-gamma\"\nversion = \"0.1.0\"\n\n[dependencies]\nfixture-beta = { path = \"../beta\" }\n").unwrap();
    fs::write(dir.join("gamma/src/lib.rs"), "mod wheel;\n").unwrap();
    fs::write(dir.join("gamma/src/wheel.rs"), "pub fn roll() {}\n").unwrap();
    fs::remove_file(dir.join("beta/src/broken.rs")).unwrap();
    let changes = repo.changes(&base).unwrap();
    let overlay = ex.cards(&c, &def, &base, dir, &changes, &mut repo).unwrap();
    assert!(overlay.iter().all(|c| c.kind == Kind::Crate && c.parent.is_none()));

    commit_all(dir, "the worktree, committed", 1_700_000_400);
    let mut repo = GitRepo::open(dir).unwrap();
    let head = repo.resolve(&RevSpec::Pinned("HEAD".into())).unwrap();
    let (after, _) = extract(&def, "fixture", Some(&head), &mut repo, &mut Extractor::new());
    let (before, after, overlay) = (cards_by_dir(&before), cards_by_dir(&after), cards_by_dir(&overlay));
    assert_eq!(overlay.keys().collect::<Vec<_>>(), ["alpha", "beta", "gamma"]);
    for (d, card) in &overlay {
        assert_eq!(card, &after[d], "{d}");
    }
    for (d, card) in &after {
        if !overlay.contains_key(d) {
            assert_eq!(card, &before[d], "{d}");
        }
    }
    assert!(overlay["alpha"].contains("rs_files: 2") && overlay["alpha"].contains("\"spinner\"") && overlay["alpha"].contains("\"tables\""), "{}", overlay["alpha"]);
    assert!(overlay["beta"].contains("dependents: [\"alpha\", \"gamma\"]"), "{}", overlay["beta"]);
    assert!(overlay["gamma"].contains("modules: [\"wheel\"]"), "{}", overlay["gamma"]);
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
    // staged, not committed
    fs::write(dir.join("docs/guide.md"), "# Staged\n").unwrap();
    Repository::open(dir).unwrap().stage_file("docs/guide.md").unwrap();
    // staged, then edited back to the base content: no change
    let splash = fs::read(dir.join("ui/demo.splash")).unwrap();
    fs::write(dir.join("ui/demo.splash"), "let Other = View{}\n").unwrap();
    Repository::open(dir).unwrap().stage_file("ui/demo.splash").unwrap();
    fs::write(dir.join("ui/demo.splash"), &splash).unwrap();
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
            Change::Modified("docs/guide.md".into()),
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
    let mut generated: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut bytes = 0u64;
    let report = Extractor::new()
        .extract(&circle("makepad"), &def, Some(&rev), &mut repo, &mut |c| {
            *kinds.entry(c.kind.as_str()).or_default() += 1;
            if c.kind == Kind::Crate && std::env::var("MAKEPAD_INDEX_CARD").is_ok_and(|n| n == c.name) {
                println!("card {} sig: {}", c.name, c.fields.sig);
            }
            if c.flags.generated {
                *generated.entry(c.path.clone()).or_default() += 1;
            }
            bytes += (c.fields.name.len() + c.fields.path.len() + c.fields.sig.len() + c.fields.doc.len() + c.fields.body.len()) as u64;
            Ok(())
        })
        .unwrap();
    println!("rev {rev_name}: extracted {} files into {} chunks", report.files, report.chunks);
    println!("field text {:.1} MB; chunks by kind {kinds:?}", bytes as f64 / 1e6);
    println!("fallbacks {}: {:?}", report.fallbacks.len(), report.fallbacks);
    let mut big: Vec<(u64, String)> = generated.iter().map(|(p, n)| (*n, p.clone())).collect();
    big.sort_by(|a, b| b.cmp(a));
    println!("generated: {} files, {} chunks; largest {:?}", generated.len(), generated.values().sum::<u64>(), &big[..big.len().min(6)]);
    println!("skipped {}: {:?}; excluded {:?}", report.skipped.len(), report.skipped, report.excluded);
}

/// The cost parts of a Cite over a real checkout, timed with `time` around each:
/// `MAKEPAD_INDEX_REPO=<checkout> MAKEPAD_INDEX_PART=files|commits|cite <test binary>
/// real_cite_parts --ignored --nocapture`.
#[test]
#[ignore]
fn real_cite_parts() {
    let Some(root) = std::env::var_os("MAKEPAD_INDEX_REPO") else { panic!("set MAKEPAD_INDEX_REPO") };
    let part = std::env::var("MAKEPAD_INDEX_PART").unwrap_or_default();
    let mut repo = GitRepo::open(Path::new(&root)).unwrap();
    let head = repo.resolve(&RevSpec::Pinned("HEAD".into())).unwrap();
    match part.as_str() {
        "files" => println!("files {}", repo.files(head.tree).unwrap().len()),
        "commits" => println!("commits {}", repo.commits(&[head.commit], &[]).unwrap().len()),
        "ids" => println!("ids {}", repo.commit_ids(&[head.commit]).unwrap().len()),
        "cite" => println!("cite {}", Cite::from_repo(circle("makepad"), &mut repo, &head).unwrap().commits.len()),
        _ => println!("resolved"),
    }
}

/// The overlay's extraction cost on a real lane, timed with `time` around each part:
/// `MAKEPAD_INDEX_REPO=<worktree> MAKEPAD_INDEX_REV=<base> MAKEPAD_INDEX_PART=changes|read|extract
/// <test binary> real_overlay_parts --ignored --nocapture`.
#[test]
#[ignore]
fn real_overlay_parts() {
    let Some(root) = std::env::var_os("MAKEPAD_INDEX_REPO") else { panic!("set MAKEPAD_INDEX_REPO") };
    let base_name = std::env::var("MAKEPAD_INDEX_REV").unwrap_or_else(|_| "HEAD".into());
    let part = std::env::var("MAKEPAD_INDEX_PART").unwrap_or_default();
    let root = PathBuf::from(root);
    let mut repo = GitRepo::open(&root).unwrap();
    let base = repo.resolve(&RevSpec::Pinned(base_name)).unwrap();
    if part.is_empty() {
        return println!("opened");
    }
    let changes = repo.changes(&base).unwrap();
    if part == "changes" {
        return println!("changes {}", changes.len());
    }
    let mut files = Vec::new();
    for c in &changes {
        if let Change::Added(p) | Change::Modified(p) = c {
            files.push((p.clone(), fs::read(root.join(p)).unwrap()));
        }
    }
    let bytes: usize = files.iter().map(|(_, b)| b.len()).sum();
    if part == "read" {
        return println!("read {} files, {bytes} bytes", files.len());
    }
    if part == "hash" {
        let n = files.iter().filter(|(_, b)| super::git::blob_oid(b).0[0] < 255).count();
        return println!("hash {n}");
    }
    if part == "lex" {
        let mut toks = Vec::new();
        let opts = relax_lex::lexer::LexOptions { edition: 2021, mode: relax_lex::lexer::Mode::Rust, trivia: true };
        let mut n = 0;
        for (_, b) in files.iter().filter(|(p, _)| p.ends_with(".rs")) {
            toks.clear();
            let _ = relax_lex::lexer::lex_with(b, &opts, &mut toks);
            n += toks.len();
        }
        return println!("lex {n}");
    }
    if part == "scan" {
        let n: usize = files.iter().filter(|(p, _)| p.ends_with(".rs")).map(|(_, b)| super::rust::scan(b, false).items.len()).sum();
        return println!("scan {n}");
    }
    if part == "sizes" {
        let mut v: Vec<(usize, &str)> = files.iter().map(|(p, b)| (b.len(), p.as_str())).collect();
        v.sort();
        v.reverse();
        return println!("sizes {:?}", &v[..8]);
    }
    let def = BaseCircle {
        source: BaseSource::Tree { root: root.clone() },
        families: CODE.to_vec(),
        exclude: crate::INDEX_OWN_DATA.iter().map(|p| Exclusion { prefix: p.to_string(), provenance: Provenance::IndexOwnData }).collect(),
    };
    let mut ex = Extractor::new();
    if part == "cards-files" || part == "cards-read" || part == "cards-graph" {
        let files = repo.files(base.tree).unwrap();
        let mut manifests = Vec::new();
        if part != "cards-files" {
            for f in files.iter().filter(|f| f.path.ends_with("Cargo.toml")) {
                manifests.push((f.path.clone(), String::from_utf8_lossy(&repo.read_blob(f.blob).unwrap()).into_owned()));
            }
        }
        if part == "cards-graph" {
            let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
            let g = super::cargo::Graph::build(&manifests, &paths, None);
            return println!("graph {}", g.packages.len());
        }
        return println!("{} files {} manifests", files.len(), manifests.len());
    }
    if part == "cards" {
        let cards = ex.cards(&circle("makepad"), &def, &base, &root, &changes, &mut repo).unwrap();
        let names: Vec<&str> = cards.iter().map(|c| c.name.as_str()).collect();
        return println!("cards {}: {names:?}", cards.len());
    }
    let mut chunks = 0;
    if part == "extract-files" {
        chunks = ex.extract_files(&circle("makepad"), &def, &files).unwrap().iter().map(Vec::len).sum();
    } else {
        for (p, b) in &files {
            chunks += ex.extract_file(&circle("makepad"), &def, p, b).unwrap().len();
        }
    }
    println!("extract {} files, {bytes} bytes, {chunks} chunks", files.len());
}
