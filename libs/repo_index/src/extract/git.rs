//! [`crate::Repo`] over makepad-git: revision resolution, tree walks, blob reads, worktree
//! changes against a base, commit walks and merge bases. No shell-outs.
//!
//! Abbreviated hexes resolve among the commits reachable from any ref (makepad-git has no
//! object-prefix lookup yet); a prefix that matches no such commit is an error, never a guess.

use crate::{Change, CommitInfo, CommitRecord, Error, Oid, Repo, RepoId, Rev, RevSpec, TreeFile};
use makepad_git::{ObjectId, ObjectKind, Repository, TreeChange};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Regular files (`100644`, `100755`); symlinks and gitlinks are not content.
const REGULAR_FILE_MODES: [u32; 2] = [0o100644, 0o100755];

pub struct GitRepo {
    repo: Repository,
    id: RepoId,
    /// Commits reachable from any ref, for abbreviated hexes; built on first use.
    reachable: Option<Vec<ObjectId>>,
}

fn git_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Git(format!("{context}: {e}"))
}

pub fn oid(o: &ObjectId) -> Oid {
    Oid(*o.as_bytes())
}

fn object_id(o: &Oid) -> ObjectId {
    ObjectId::from_bytes(o.0)
}

/// The first 8 bytes, little-endian, of the SHA-1 of `bytes` (the crate's one hash).
pub fn hash64(bytes: &[u8]) -> u64 {
    let full = makepad_git::oid::hash_object("blob", bytes);
    let b = full.as_bytes();
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// The git blob id of `bytes`.
pub fn blob_oid(bytes: &[u8]) -> Oid {
    oid(&makepad_git::oid::hash_object("blob", bytes))
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn change_path(c: TreeChange) -> String {
    match c {
        TreeChange::Added { path, .. } | TreeChange::Deleted { path, .. } | TreeChange::Modified { path, .. } => path,
    }
}

impl GitRepo {
    /// Opens the repository that contains `path` (a worktree root or any path inside it).
    pub fn open(path: &Path) -> Result<GitRepo, Error> {
        let repo = Repository::open(path).map_err(|e| git_err(&format!("open {}", path.display()), e))?;
        let common = repo.common_dir.canonicalize().map_err(|error| Error::Io {
            context: format!("canonicalize {}", repo.common_dir.display()),
            error,
        })?;
        let id = RepoId(hash64(common.to_string_lossy().as_bytes()));
        Ok(GitRepo { repo, id, reachable: None })
    }

    fn commit_tree(&mut self, commit: ObjectId) -> Result<ObjectId, Error> {
        Ok(self.repo.read_commit(&commit).map_err(|e| git_err(&format!("read commit {}", commit.to_hex()), e))?.tree)
    }

    /// A ref, tag or commit id peeled to the commit it names.
    fn peel(&mut self, id: ObjectId) -> Result<ObjectId, Error> {
        self.peel_commit(id)?.ok_or_else(|| Error::Rev(format!("{} names no commit", id.to_hex())))
    }

    /// `None` when the object (or the tag's target) is a tree or blob.
    fn peel_commit(&mut self, mut id: ObjectId) -> Result<Option<ObjectId>, Error> {
        loop {
            let obj = self.repo.read_object(&id).map_err(|e| git_err(&format!("read {}", id.to_hex()), e))?;
            match obj.kind {
                ObjectKind::Commit => return Ok(Some(id)),
                ObjectKind::Tag => {
                    let text = String::from_utf8_lossy(&obj.data);
                    let target = text
                        .lines()
                        .find_map(|l| l.strip_prefix("object "))
                        .ok_or_else(|| Error::Rev(format!("tag {} names no object", id.to_hex())))?;
                    id = ObjectId::from_hex(target.trim()).map_err(|e| git_err("tag target", e))?;
                }
                ObjectKind::Tree | ObjectKind::Blob => return Ok(None),
            }
        }
    }

    fn all_ref_tips(&mut self) -> Result<Vec<ObjectId>, Error> {
        let refs = makepad_git::refs::list_refs_in(&self.repo.git_dir, &self.repo.common_dir, "refs/")
            .map_err(|e| git_err("list refs", e))?;
        let mut tips = Vec::new();
        if let Ok(head) = self.repo.head_oid() {
            tips.push(head);
        }
        for r in refs {
            let id = makepad_git::refs::resolve_ref_in(&self.repo.git_dir, &self.repo.common_dir, &r.name)
                .map_err(|e| git_err(&format!("resolve {}", r.name), e))?;
            // a ref to a tree or blob names no commit: not a tip
            if let Some(commit) = self.peel_commit(id)? {
                tips.push(commit);
            }
        }
        tips.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        tips.dedup();
        Ok(tips)
    }

    /// Every commit reachable from `tips`.
    fn reachable_from(&mut self, tips: &[ObjectId]) -> Result<Vec<(ObjectId, makepad_git::Commit)>, Error> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for tip in tips {
            if seen.contains(tip) {
                continue;
            }
            let walk = self.repo.log(tip, usize::MAX).map_err(|e| git_err(&format!("log {}", tip.to_hex()), e))?;
            for (id, commit) in walk {
                if seen.insert(id) {
                    out.push((id, commit));
                }
            }
        }
        Ok(out)
    }

    fn resolve_abbrev(&mut self, prefix: &str) -> Result<ObjectId, Error> {
        if self.reachable.is_none() {
            let tips = self.all_ref_tips()?;
            let all = self.reachable_from(&tips)?;
            self.reachable = Some(all.into_iter().map(|(id, _)| id).collect());
        }
        let lower = prefix.to_ascii_lowercase();
        let found: Vec<ObjectId> =
            self.reachable.iter().flatten().filter(|id| id.to_hex().starts_with(&lower)).copied().collect();
        match found.len() {
            1 => Ok(found[0]),
            0 => Err(Error::Rev(format!("no commit reachable from a ref starts with {prefix}"))),
            n => Err(Error::Rev(format!("{prefix} is ambiguous: {n} commits start with it"))),
        }
    }

    fn resolve_name(&mut self, name: &str) -> Result<ObjectId, Error> {
        if name == "HEAD" {
            return self.repo.head_oid().map_err(|e| git_err("HEAD", e));
        }
        if name.len() == 40 && is_hex(name) {
            return ObjectId::from_hex(name).map_err(|e| git_err(name, e));
        }
        // gitrevisions order
        let candidates = [
            name.to_string(),
            format!("refs/{name}"),
            format!("refs/tags/{name}"),
            format!("refs/heads/{name}"),
            format!("refs/remotes/{name}"),
            format!("refs/remotes/{name}/HEAD"),
        ];
        for c in &candidates {
            if let Ok(id) = makepad_git::refs::resolve_ref_in(&self.repo.git_dir, &self.repo.common_dir, c) {
                return Ok(id);
            }
        }
        if name.len() >= 4 && is_hex(name) {
            return self.resolve_abbrev(name);
        }
        Err(Error::Rev(format!("unknown revision {name}")))
    }

    fn worktree_blob(&self, path: &str) -> Result<Option<Oid>, Error> {
        let full = self.repo.workdir.join(path);
        if !full.is_file() {
            return Ok(None);
        }
        let id = makepad_git::hash_file_blob(&full).map_err(|e| git_err(&format!("hash {path}"), e))?;
        Ok(Some(oid(&id)))
    }
}

impl Repo for GitRepo {
    fn id(&self) -> RepoId {
        self.id
    }

    fn root(&self) -> PathBuf {
        self.repo.workdir.clone()
    }

    fn resolve(&mut self, spec: &RevSpec) -> Result<Rev, Error> {
        let named = match spec {
            RevSpec::Worktree => self.repo.head_oid().map_err(|e| git_err("HEAD", e))?,
            RevSpec::Pinned(name) => self.resolve_name(name)?,
        };
        let commit = self.peel(named)?;
        let tree = self.commit_tree(commit)?;
        Ok(Rev { spec: spec.clone(), commit: oid(&commit), tree: oid(&tree) })
    }

    fn files(&mut self, tree: Oid) -> Result<Vec<TreeFile>, Error> {
        let flat = self.repo.flatten_tree_with_mode(&object_id(&tree)).map_err(|e| git_err("flatten tree", e))?;
        let mut files: Vec<TreeFile> = flat
            .into_iter()
            .filter(|(_, (_, mode))| REGULAR_FILE_MODES.contains(mode))
            .map(|(path, (blob, mode))| TreeFile { path, blob: oid(&blob), mode })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    fn read_blob(&mut self, blob: Oid) -> Result<Vec<u8>, Error> {
        let id = object_id(&blob);
        self.repo.read_blob(&id).map_err(|e| git_err(&format!("read blob {}", id.to_hex()), e))
    }

    fn changes(&mut self, base: &Rev) -> Result<Vec<Change>, Error> {
        let head = self.repo.head_oid().map_err(|e| git_err("HEAD", e))?;
        let head_tree = self.commit_tree(head)?;
        let base_tree = object_id(&base.tree);
        let mut candidates: Vec<String> = Vec::new();
        if head_tree != base_tree {
            let diff = self.repo.diff_trees(&base_tree, &head_tree).map_err(|e| git_err("diff base..HEAD", e))?;
            candidates.extend(diff.into_iter().map(change_path));
        }
        // staged, unstaged, deleted, and untracked files the ignore rules admit
        let status = self
            .repo
            .status_with_options(makepad_git::StatusOptions { skip_target_dirs: true, ..Default::default() })
            .map_err(|e| git_err("status", e))?;
        candidates.extend(status.entries.into_iter().map(|e| e.path));
        candidates.sort();
        candidates.dedup();
        let base_files: HashMap<String, Oid> = self.files(base.tree)?.into_iter().map(|f| (f.path, f.blob)).collect();
        let mut out = Vec::new();
        for path in candidates {
            match (base_files.get(&path), self.worktree_blob(&path)?) {
                (None, Some(_)) => out.push(Change::Added(path)),
                (Some(old), Some(new)) if *old != new => out.push(Change::Modified(path)),
                (Some(_), None) => out.push(Change::Deleted(path)),
                _ => {}
            }
        }
        Ok(out)
    }

    fn commits(&mut self, tips: &[Oid], hide: &[Oid]) -> Result<Vec<CommitRecord>, Error> {
        let hide_ids: Vec<ObjectId> = hide.iter().map(object_id).collect();
        let hidden: HashSet<ObjectId> = self.reachable_from(&hide_ids)?.into_iter().map(|(id, _)| id).collect();
        let tip_ids: Vec<ObjectId> = tips.iter().map(object_id).collect();
        let mut shown: Vec<(ObjectId, makepad_git::Commit)> =
            self.reachable_from(&tip_ids)?.into_iter().filter(|(id, _)| !hidden.contains(id)).collect();
        shown.sort_by(|a, b| b.1.committer.timestamp.cmp(&a.1.committer.timestamp).then(a.0.as_bytes().cmp(b.0.as_bytes())));
        let mut out = Vec::with_capacity(shown.len());
        for (id, commit) in shown {
            let touched = match commit.parents.first() {
                Some(parent) => {
                    let parent_tree = self.commit_tree(*parent)?;
                    let diff = self
                        .repo
                        .diff_trees(&parent_tree, &commit.tree)
                        .map_err(|e| git_err(&format!("diff {}", id.to_hex()), e))?;
                    let mut paths: Vec<String> = diff.into_iter().map(change_path).collect();
                    paths.sort();
                    paths
                }
                None => self.files(oid(&commit.tree))?.into_iter().map(|f| f.path).collect(),
            };
            out.push(CommitRecord {
                oid: oid(&id),
                info: CommitInfo { time: commit.committer.timestamp, parents: commit.parents.iter().map(oid).collect() },
                message: commit.message,
                touched,
            });
        }
        Ok(out)
    }

    fn merge_base(&mut self, a: Oid, b: Oid) -> Result<Option<Oid>, Error> {
        let m = self.repo.merge_base(&object_id(&a), &object_id(&b)).map_err(|e| git_err("merge-base", e))?;
        Ok(m.map(|id| oid(&id)))
    }
}
