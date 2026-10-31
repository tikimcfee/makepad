//! `Repository::rev_parse` and `expand_ref` on a repository laid out by
//! hand: loose and packed objects and refs, symbolic refs, annotated tags,
//! a linked worktree, abbreviated ids made to collide; and, ignored by
//! default, cross-checked against the git CLI on this checkout.
use makepad_git::commit::serialize_commit;
use makepad_git::oid::hash_object;
use makepad_git::test_support::{pack_all_loose, tempdir, write_pack, TempDir};
use makepad_git::*;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn sig() -> Signature {
    Signature {
        name: "Test".into(),
        email: "test@example.org".into(),
        timestamp: 0,
        tz_offset: "+0000".into(),
    }
}

fn commit_of(tree: ObjectId, parents: Vec<ObjectId>, message: &str) -> Commit {
    Commit { tree, parents, author: sig(), committer: sig(), message: message.to_string() }
}

fn tag_bytes(target: ObjectId, kind: &str, name: &str) -> Vec<u8> {
    format!("object {target}\ntype {kind}\ntag {name}\ntagger Test <test@example.org> 0 +0000\n\n{name}\n")
        .into_bytes()
}

struct Fixture {
    _dir: TempDir,
    main: PathBuf,
    wt: PathBuf,
    ids: HashMap<&'static str, ObjectId>,
    /// A 4-digit prefix two blobs share.
    blob_pair: String,
    /// A 4-digit prefix a commit and a blob share.
    commit_blob: String,
}

impl Fixture {
    fn id(&self, name: &str) -> ObjectId {
        self.ids[name]
    }
}

/// History `c0 <- c1 <- c2 <- m`, `c0 <- s1 <- m` (m merges s1), packed;
/// refs loose and packed; tags; a linked worktree `wt` detached at `c1`.
fn fixture() -> Fixture {
    let dir = tempdir().unwrap();
    let main = dir.path().join("main");
    let git = main.join(".git");
    fs::create_dir_all(git.join("objects")).unwrap();
    fs::create_dir_all(git.join("refs/heads")).unwrap();
    fs::create_dir_all(git.join("refs/tags")).unwrap();
    fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let mut repo = Repository::open(&main).unwrap();
    let mut ids = HashMap::new();

    let tree_of = |repo: &mut Repository, content: &str| {
        let blob = repo.write_blob(content.as_bytes()).unwrap();
        repo.write_tree(&Tree { entries: vec![TreeEntry { mode: 0o100644, name: "f".into(), oid: blob }] }).unwrap()
    };
    let t0 = tree_of(&mut repo, "0");
    let c0 = repo.write_commit(&commit_of(t0, vec![], "c0")).unwrap();
    let t1 = tree_of(&mut repo, "1");
    let c1 = repo.write_commit(&commit_of(t1, vec![c0], "c1")).unwrap();
    let t2 = tree_of(&mut repo, "2");
    let c2 = repo.write_commit(&commit_of(t2, vec![c1], "c2")).unwrap();
    let ts = tree_of(&mut repo, "s");
    let s1 = repo.write_commit(&commit_of(ts, vec![c0], "s1")).unwrap();
    let tm = tree_of(&mut repo, "m");
    let m = repo.write_commit(&commit_of(tm, vec![c2, s1], "m")).unwrap();
    let blob = repo.write_blob(b"tagged blob").unwrap();
    let v1 = repo.write_object(ObjectKind::Tag, &tag_bytes(m, "commit", "v1")).unwrap();
    let outer = repo.write_object(ObjectKind::Tag, &tag_bytes(v1, "tag", "outer")).unwrap();
    let blobtag = repo.write_object(ObjectKind::Tag, &tag_bytes(blob, "blob", "blobtag")).unwrap();
    pack_all_loose(&git).unwrap();
    for (name, oid) in [("c0", c0), ("c1", c1), ("c2", c2), ("s1", s1), ("m", m), ("tm", tm), ("blob", blob), ("v1", v1), ("outer", outer), ("blobtag", blobtag)] {
        ids.insert(name, oid);
    }

    // Abbreviated-id collisions: two blobs, and a commit with a blob. One
    // of each pair is packed, the other loose.
    let mut seen: HashMap<String, (ObjectKind, Vec<u8>)> = HashMap::new();
    let (mut blob_pair, mut commit_blob) = (None, None);
    let mut n = 0;
    while blob_pair.is_none() || commit_blob.is_none() {
        let probe_blob = format!("probe {n}").into_bytes();
        let probe_commit = serialize_commit(&commit_of(t0, vec![c0], &format!("probe {n}")));
        for (kind, data) in [(ObjectKind::Blob, probe_blob), (ObjectKind::Commit, probe_commit)] {
            let hex = hash_object(kind.as_str(), &data).to_hex();
            let key = hex[..4].to_string();
            if blob_pair.as_ref() == Some(&key) {
                continue;
            }
            match seen.get(&key) {
                Some((ObjectKind::Blob, other)) if kind == ObjectKind::Blob && blob_pair.is_none() => {
                    write_pack(&git, &[(ObjectKind::Blob, other.clone())]).unwrap();
                    repo.write_object(kind, &data).unwrap();
                    blob_pair = Some(key);
                }
                Some((other_kind, other)) if *other_kind != kind && commit_blob.is_none() => {
                    let (packed, loose) = if kind == ObjectKind::Blob {
                        ((kind, data.clone()), (*other_kind, other.clone()))
                    } else {
                        ((*other_kind, other.clone()), (kind, data.clone()))
                    };
                    write_pack(&git, &[packed]).unwrap();
                    repo.write_object(loose.0, &loose.1).unwrap();
                    commit_blob = Some(key);
                }
                Some(_) => {}
                None => {
                    seen.insert(key, (kind, data));
                }
            }
        }
        n += 1;
    }
    let commit_blob = commit_blob.unwrap();
    // The commit of the pair, for the disambiguation checks.
    let mut pair = Repository::open(&main).unwrap();
    let candidates = pair.objects_with_prefix(&ObjectIdPrefix::from_hex(&commit_blob).unwrap()).unwrap();
    for oid in candidates {
        if pair.read_object(&oid).unwrap().kind == ObjectKind::Commit {
            ids.insert("probe", oid);
        }
    }

    // Refs: loose and packed, symbolic, tags, remotes, FETCH_HEAD.
    repo.create_branch("main", &m).unwrap();
    repo.create_branch("feature", &s1).unwrap();
    repo.create_branch("dup", &c0).unwrap();
    fs::write(git.join("refs/tags/v1"), format!("{v1}\n")).unwrap();
    fs::write(git.join("refs/tags/light"), format!("{c1}\n")).unwrap();
    fs::write(git.join("refs/tags/dup"), format!("{c2}\n")).unwrap();
    fs::create_dir_all(git.join("refs/remotes/upstream")).unwrap();
    fs::write(git.join("refs/remotes/upstream/HEAD"), "ref: refs/remotes/upstream/work\n").unwrap();
    fs::create_dir_all(git.join("refs/remotes/gone")).unwrap();
    fs::write(git.join("refs/remotes/gone/HEAD"), "ref: refs/remotes/gone/main\n").unwrap();
    fs::write(
        git.join("packed-refs"),
        format!(
            "# pack-refs with: peeled fully-peeled sorted \n\
             {c2} refs/heads/packed\n\
             {s1} refs/remotes/upstream/work\n\
             {outer} refs/tags/outer\n^{m}\n\
             {blobtag} refs/tags/blobtag\n^{blob}\n\
             {c0} refs/tags/v1\n"
        ),
    )
    .unwrap();
    fs::write(git.join("FETCH_HEAD"), format!("{c1}\t\tbranch 'work' of host:repo\n{c0}\tnot-for-merge\tbranch 'x'\n")).unwrap();

    // A linked worktree, detached at c1, with a private bisect ref.
    let wt = dir.path().join("wt");
    let private = git.join("worktrees").join("wt");
    fs::create_dir_all(private.join("refs/bisect")).unwrap();
    fs::write(private.join("HEAD"), format!("{c1}\n")).unwrap();
    fs::write(private.join("commondir"), "../..\n").unwrap();
    fs::write(private.join("gitdir"), format!("{}\n", wt.join(".git").display())).unwrap();
    fs::write(private.join("refs/bisect/bad"), format!("{c2}\n")).unwrap();
    fs::create_dir_all(&wt).unwrap();
    fs::write(wt.join(".git"), format!("gitdir: {}\n", private.display())).unwrap();

    Fixture { _dir: dir, main, wt, ids, blob_pair: blob_pair.unwrap(), commit_blob }
}

fn bad_revision(result: Result<ObjectId, GitError>) -> String {
    match result {
        Err(GitError::BadRevision { reason, .. }) => reason,
        other => panic!("expected BadRevision, got {other:?}"),
    }
}

#[test]
fn names_resolve_by_git_rules_through_loose_and_packed_refs() {
    let f = fixture();
    let mut repo = Repository::open(&f.main).unwrap();
    for (rev, want) in [
        ("HEAD", "m"), ("@", "m"), ("main", "m"), ("heads/main", "m"), ("refs/heads/main", "m"),
        ("packed", "c2"), ("feature", "s1"),
        ("upstream/work", "s1"), ("remotes/upstream/work", "s1"), ("upstream", "s1"),
        ("light", "c1"), ("v1", "v1"), ("outer", "outer"), ("blobtag", "blobtag"),
        // A tag and a branch named alike: refs/tags comes first.
        ("dup", "c2"), ("heads/dup", "c0"),
        // FETCH_HEAD's first id, with the rest of the line ignored.
        ("FETCH_HEAD", "c1"),
    ] {
        assert_eq!(repo.rev_parse(rev).unwrap(), f.id(want), "{rev}");
    }
    // A loose ref shadows its packed copy.
    assert_eq!(repo.rev_parse("refs/tags/v1").unwrap(), f.id("v1"));
}

#[test]
fn suffixes_walk_parents_and_peel() {
    let f = fixture();
    let mut repo = Repository::open(&f.main).unwrap();
    for (rev, want) in [
        ("main~1", "c2"), ("main~", "c2"), ("main^", "c2"), ("main^1", "c2"), ("main^2", "s1"),
        ("main~3", "c0"), ("main^^", "c1"), ("main^2~1", "c0"), ("main~0", "m"), ("main^0", "m"),
        ("main^{commit}", "m"), ("main^{tree}", "tm"), ("main^{}", "m"), ("main^{object}", "m"),
        ("v1^{}", "m"), ("v1^{commit}", "m"), ("v1^{tree}", "tm"), ("v1~1", "c2"), ("v1^0", "m"),
        ("v1^{tag}", "v1"), ("outer^{}", "m"), ("outer^{tag}", "outer"), ("outer~2", "c1"),
        ("blobtag^{}", "blob"), ("blobtag^{blob}", "blob"),
    ] {
        assert_eq!(repo.rev_parse(rev).unwrap(), f.id(want), "{rev}");
    }
    let reason = bad_revision(repo.rev_parse("main~4"));
    assert!(reason.contains("main~3") && reason.contains(&f.id("c0").to_hex()) && reason.contains("no parent"), "{reason}");
    let reason = bad_revision(repo.rev_parse("main^3"));
    assert!(reason.contains("has 2 parent(s)"), "{reason}");
    assert!(bad_revision(repo.rev_parse("main~1^2")).contains("has 1 parent(s)"));
    let reason = bad_revision(repo.rev_parse("blobtag^{commit}"));
    assert!(reason.contains("blob") && reason.contains("does not peel to a commit"), "{reason}");
    assert!(bad_revision(repo.rev_parse("blobtag~1")).contains("does not peel to a commit"));
    assert!(bad_revision(repo.rev_parse("main^{tree}^{commit}")).contains("tree"));
    assert!(bad_revision(repo.rev_parse("main^{tag}")).contains("not a tag"));
    assert!(bad_revision(repo.rev_parse("main^{blob}")).contains("does not peel to a blob"));
}

#[test]
fn abbreviated_ids_resolve_only_when_unique() {
    let f = fixture();
    let mut repo = Repository::open(&f.main).unwrap();
    let m = f.id("m").to_hex();
    for len in [4, 7, 12, 39, 40] {
        let prefix = &m[..len];
        let candidates = repo.objects_with_prefix(&ObjectIdPrefix::from_hex(prefix).unwrap()).unwrap();
        if candidates.len() == 1 {
            assert_eq!(repo.rev_parse(prefix).unwrap(), f.id("m"), "{prefix}");
        }
    }
    assert_eq!(repo.rev_parse(&m.to_ascii_uppercase()[..10]).unwrap(), f.id("m"));
    assert_eq!(repo.rev_parse(&format!("{}~2", &m[..10])).unwrap(), f.id("c1"));
    // Packed and loose objects are both found (the pairs straddle the two).
    let blob = f.id("blob").to_hex();
    assert_eq!(repo.rev_parse(&blob[..10]).unwrap(), f.id("blob"));

    // Two blobs: ambiguous whatever follows, every candidate listed.
    match repo.rev_parse(&f.blob_pair) {
        Err(GitError::AmbiguousObjectId { prefix, candidates }) => {
            assert_eq!(prefix, f.blob_pair);
            assert_eq!(candidates.len(), 2);
            assert!(candidates.iter().all(|(oid, kind)| oid.to_hex().starts_with(&f.blob_pair) && *kind == ObjectKind::Blob));
            let message = GitError::AmbiguousObjectId { prefix, candidates: candidates.clone() }.to_string();
            assert!(candidates.iter().all(|(oid, _)| message.contains(&oid.to_hex())), "{message}");
        }
        other => panic!("expected ambiguity, got {other:?}"),
    }
    assert!(matches!(repo.rev_parse(&format!("{}^{{commit}}", f.blob_pair)), Err(GitError::AmbiguousObjectId { .. })));

    // A commit and a blob: ambiguous alone, the commit where a commit (or
    // tree-ish) is needed.
    match repo.rev_parse(&f.commit_blob) {
        Err(GitError::AmbiguousObjectId { candidates, .. }) => {
            let mut kinds: Vec<_> = candidates.iter().map(|(_, kind)| kind.as_str()).collect();
            kinds.sort();
            assert_eq!(kinds, ["blob", "commit"]);
        }
        other => panic!("expected ambiguity, got {other:?}"),
    }
    let probe = f.id("probe");
    assert_eq!(repo.rev_parse(&format!("{}^{{commit}}", f.commit_blob)).unwrap(), probe);
    assert_eq!(repo.rev_parse(&format!("{}~0", f.commit_blob)).unwrap(), probe);
    assert_eq!(repo.rev_parse(&format!("{}~1", f.commit_blob)).unwrap(), f.id("c0"));
    let probe_tree = repo.read_commit(&probe).unwrap().tree;
    assert_eq!(repo.rev_parse(&format!("{}^{{tree}}", f.commit_blob)).unwrap(), probe_tree);

    // Not found, too short, and a full id of a missing object.
    let absent = (0..=0xffffu32).map(|n| format!("{n:04x}")).find(|p| {
        repo.objects_with_prefix(&ObjectIdPrefix::from_hex(p).unwrap()).unwrap().is_empty()
    }).unwrap();
    assert!(bad_revision(repo.rev_parse(&absent)).contains("no object id starting with it"));
    assert!(bad_revision(repo.rev_parse(&m[..3])).contains("at least 4 hex digits"));
    let missing = "0123456789012345678901234567890123456789";
    assert!(bad_revision(repo.rev_parse(missing)).contains("does not exist"));

    // A ref named like an abbreviated id wins, as in git.
    repo.create_branch(&f.blob_pair, &f.id("c0")).unwrap();
    assert_eq!(repo.rev_parse(&f.blob_pair).unwrap(), f.id("c0"));
}

#[test]
fn linked_worktree_has_its_own_head_and_shares_refs() {
    let f = fixture();
    let mut wt = Repository::open(&f.wt).unwrap();
    assert_eq!(wt.rev_parse("HEAD").unwrap(), f.id("c1"));
    assert_eq!(wt.rev_parse("HEAD~1").unwrap(), f.id("c0"));
    assert_eq!(wt.rev_parse("main").unwrap(), f.id("m"));
    assert_eq!(wt.rev_parse("upstream/work").unwrap(), f.id("s1"));
    assert_eq!(wt.rev_parse("v1^{tree}").unwrap(), f.id("tm"));
    assert_eq!(wt.rev_parse("refs/bisect/bad").unwrap(), f.id("c2"));
    assert_eq!(wt.rev_parse("main-worktree/HEAD").unwrap(), f.id("m"));
    assert_eq!(wt.expand_ref("HEAD").unwrap(), Some(("HEAD".to_string(), f.id("c1"))));
    assert_eq!(wt.expand_ref("main-worktree/HEAD").unwrap(), Some(("refs/heads/main".to_string(), f.id("m"))));

    let mut main = Repository::open(&f.main).unwrap();
    assert_eq!(main.rev_parse("worktrees/wt/HEAD").unwrap(), f.id("c1"));
    assert_eq!(main.rev_parse("worktrees/wt/refs/bisect/bad").unwrap(), f.id("c2"));
    assert!(bad_revision(main.rev_parse("refs/bisect/bad")).contains("no ref named refs/bisect/bad"));
    assert!(bad_revision(main.rev_parse("worktrees/nope/HEAD")).contains("no ref named"));
}

#[test]
fn expand_ref_names_the_ref_a_symbolic_chain_ends_at() {
    let f = fixture();
    let repo = Repository::open(&f.main).unwrap();
    assert_eq!(repo.expand_ref("HEAD").unwrap(), Some(("refs/heads/main".to_string(), f.id("m"))));
    assert_eq!(repo.expand_ref("upstream").unwrap(), Some(("refs/remotes/upstream/work".to_string(), f.id("s1"))));
    assert_eq!(repo.expand_ref("dup").unwrap(), Some(("refs/tags/dup".to_string(), f.id("c2"))));
    assert_eq!(repo.expand_ref("packed").unwrap(), Some(("refs/heads/packed".to_string(), f.id("c2"))));
    assert_eq!(repo.expand_ref("nope").unwrap(), None);
    // `main` is no root ref: `.git/main` is never read.
    fs::write(f.main.join(".git/main"), format!("{}\n", f.id("c0"))).unwrap();
    assert_eq!(repo.expand_ref("main").unwrap(), Some(("refs/heads/main".to_string(), f.id("m"))));
}

#[test]
fn failures_say_what_failed() {
    let f = fixture();
    let git = f.main.join(".git");
    let mut repo = Repository::open(&f.main).unwrap();
    // A remote HEAD pointing at a missing branch is an error, not a miss.
    let reason = bad_revision(repo.rev_parse("gone"));
    assert!(reason.contains("refs/remotes/gone/main") && reason.contains("refs/remotes/gone/HEAD"), "{reason}");
    let reason = bad_revision(repo.rev_parse("nope~2"));
    assert!(reason.contains("no ref named nope") && reason.contains("refs/remotes/nope/HEAD"), "{reason}");
    for rev in ["", "HEAD@{1}", "@{-1}", "main..feature", "HEAD:f", ":/c0", "HEAD^{/c0}", "a//b", "-n", "main~x", "^main"] {
        assert!(matches!(repo.rev_parse(rev), Err(GitError::BadRevision { .. })), "{rev:?}");
    }
    // A symbolic ref may not lead outside the ref store.
    fs::write(git.join("refs/heads/escape"), "ref: ../../../outside\n").unwrap();
    assert!(bad_revision(repo.rev_parse("escape")).contains("ref escape"));
    // A symbolic loop stops.
    fs::write(git.join("refs/heads/loop"), "ref: refs/heads/loop\n").unwrap();
    assert!(bad_revision(repo.rev_parse("loop")).contains("levels of symbolic refs"));
    // A ref to a missing object fails at the step that reads it.
    fs::write(git.join("refs/heads/broken"), "0123456789012345678901234567890123456789\n").unwrap();
    let reason = bad_revision(repo.rev_parse("broken~1"));
    assert!(reason.contains("broken") && reason.contains("missing"), "{reason}");
    assert!(bad_revision(repo.rev_parse("broken")).contains("does not exist"));
    // HEAD on an unborn branch.
    fs::write(git.join("HEAD"), "ref: refs/heads/unborn\n").unwrap();
    assert!(bad_revision(repo.rev_parse("HEAD")).contains("refs/heads/unborn"));
}

// --- The git CLI as an oracle, read-only, on this checkout ---

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .output()
        .expect("git must be available for the ignored oracle");
    output.status.success().then(|| String::from_utf8(output.stdout).unwrap().trim().to_string())
}

/// This checkout, the main worktree and every linked worktree that still
/// exists on disk.
fn checkouts(here: &Repository) -> Vec<PathBuf> {
    let mut dirs = vec![here.workdir.clone(), here.common_dir.parent().unwrap().to_path_buf()];
    if let Ok(entries) = fs::read_dir(here.common_dir.join("worktrees")) {
        for entry in entries.flatten() {
            let Ok(gitdir) = fs::read_to_string(entry.path().join("gitdir")) else { continue };
            let dot_git = PathBuf::from(gitdir.trim());
            if dot_git.is_file() {
                dirs.push(dot_git.parent().unwrap().to_path_buf());
            }
        }
    }
    dirs.sort();
    dirs.dedup_by(|a, b| a.canonicalize().ok() == b.canonicalize().ok());
    dirs
}

#[test]
#[ignore = "read-only external git oracle on this repository and its linked worktrees"]
fn rev_parse_matches_git_on_this_repository() {
    let here = Repository::open(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
    let worktree_ids: Vec<String> = fs::read_dir(here.common_dir.join("worktrees"))
        .map(|entries| entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    let mut names: Vec<String> = ["HEAD", "@", "FETCH_HEAD", "ORIG_HEAD", "main-worktree/HEAD", "work", "upstream/work", "upstream", "origin", "no-such-branch"]
        .map(String::from)
        .to_vec();
    names.extend(worktree_ids.iter().map(|id| format!("worktrees/{id}/HEAD")));
    let refs = git(&here.workdir, &["for-each-ref", "--format=%(refname) %(refname:short)"]).unwrap();
    for line in refs.lines() {
        names.extend(line.split(' ').map(String::from));
    }
    names.sort();
    names.dedup();

    let mut revs: Vec<String> = Vec::new();
    for name in &names {
        for suffix in ["", "~1", "^", "^2", "^0", "^{}", "^{tree}", "^{commit}", "^{tag}", "~3^{tree}"] {
            revs.push(format!("{name}{suffix}"));
        }
    }
    // Abbreviations of commits, trees, blobs and tag objects at several
    // lengths, and of every 4-digit prefix in a sample (most ambiguous or
    // absent), alone and where a commit or tree is wanted.
    let mut ids: Vec<String> = Vec::new();
    for rev in ["HEAD", "HEAD~1", "HEAD~7", "HEAD^{tree}", "HEAD:Cargo.toml", "HEAD~3:libs"] {
        ids.extend(git(&here.workdir, &["rev-parse", rev]));
    }
    ids.extend(refs.lines().filter_map(|l| git(&here.workdir, &["rev-parse", l.split(' ').next().unwrap()])));
    let mut prefixes: Vec<String> = Vec::new();
    for id in &ids {
        for len in [4, 5, 6, 7, 12, 40] {
            prefixes.push(id[..len].to_string());
        }
        prefixes.push(id[..9].to_ascii_uppercase());
    }
    prefixes.extend((0..0x10000u32).step_by(0x400 + 7).map(|n| format!("{n:04x}")));
    prefixes.sort();
    prefixes.dedup();
    for prefix in &prefixes {
        for suffix in ["", "^{commit}", "~0", "^{tree}", "~1"] {
            revs.push(format!("{prefix}{suffix}"));
        }
    }

    let mut compared = 0;
    let mut resolved = 0;
    let mut mismatches = Vec::new();
    let mut few: Vec<String> = ["HEAD", "@", "HEAD~1", "HEAD^{tree}", "HEAD~2^{commit}", "main-worktree/HEAD", "work", "upstream/work"]
        .map(String::from)
        .to_vec();
    few.extend(worktree_ids.iter().map(|id| format!("worktrees/{id}/HEAD~1")));
    for dir in checkouts(&here) {
        let mut repo = Repository::open(&dir).unwrap();
        let is_here = dir.canonicalize().ok() == here.workdir.canonicalize().ok();
        for rev in if is_here { &revs } else { &few } {
            let expected = git(&dir, &["rev-parse", "--verify", "--quiet", rev]);
            let actual = repo.rev_parse(rev);
            compared += 1;
            resolved += expected.is_some() as usize;
            match (&expected, &actual) {
                (Some(e), Ok(a)) if *e == a.to_hex() => {}
                (None, Err(_)) => {}
                _ => mismatches.push(format!("{}: {rev}: git {expected:?}, ours {actual:?}", dir.display())),
            }
        }
        for name in if is_here { &names } else { &few } {
            let Some(expected) = git(&dir, &["rev-parse", "--symbolic-full-name", "--verify", "--quiet", name]) else { continue };
            if expected.is_empty() {
                continue;
            }
            let actual = repo.expand_ref(name);
            compared += 1;
            if !matches!(&actual, Ok(Some((full, _))) if *full == expected) {
                mismatches.push(format!("{}: --symbolic-full-name {name}: git {expected}, ours {actual:?}", dir.display()));
            }
        }
    }
    eprintln!("rev-parse git oracle: {compared} comparisons, {resolved} revisions git resolved, {} mismatches", mismatches.len());
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
