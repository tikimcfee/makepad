//! [`crate::Repo`] over makepad-git: revision resolution (makepad-git's `rev_parse`, peeled to
//! a commit), tree walks, blob reads, worktree changes against a base, commit walks and merge
//! bases. No shell-outs.

use crate::{Change, CommitInfo, CommitRecord, Error, Oid, Repo, RepoId, Rev, RevSpec, TreeFile};
use makepad_git::{ObjectId, Repository, TreeChange};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Regular files (`100644`, `100755`); symlinks and gitlinks are not content.
const REGULAR_FILE_MODES: [u32; 2] = [0o100644, 0o100755];

pub struct GitRepo {
    repo: Repository,
    id: RepoId,
    /// The canonical git common dir.
    common: PathBuf,
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

/// The name a repo goes by in every worktree: the main checkout's directory name, from the
/// git common dir (`<checkout>/.git` gives `<checkout>`; a bare `<name>.git` gives `<name>`).
fn checkout_name(common: &Path) -> String {
    let dir = if common.file_name().is_some_and(|n| n == ".git") { common.parent().unwrap_or(common) } else { common };
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    name.strip_suffix(".git").map(str::to_string).unwrap_or(name)
}

/// [`GitRepo::name`] of the repository whose worktree root is `root`; `None` when `root` is no
/// worktree root (it has no `.git` entry).
pub fn repo_name(root: &Path) -> Result<Option<String>, Error> {
    let Some(paths) = makepad_git::repository_paths(root).map_err(|e| git_err(&format!("{}", root.display()), e))? else {
        return Ok(None);
    };
    let common = paths.common_dir.canonicalize().map_err(|error| Error::Io { context: format!("canonicalize {}", paths.common_dir.display()), error })?;
    Ok(Some(checkout_name(&common)))
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
        Ok(GitRepo { repo, id, common })
    }

    /// The repo's name, the same in every worktree: `makepad` for each worktree of a clone at
    /// `.../makepad` (notes labels use it).
    pub fn name(&self) -> String {
        checkout_name(&self.common)
    }

    fn commit_tree(&mut self, commit: ObjectId) -> Result<ObjectId, Error> {
        Ok(self.repo.read_commit(&commit).map_err(|e| git_err(&format!("read commit {}", commit.to_hex()), e))?.tree)
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
        let name = match spec {
            RevSpec::Worktree => "HEAD",
            RevSpec::Pinned(name) => name.as_str(),
        };
        let commit = self.repo.rev_parse(&format!("{name}^{{commit}}")).map_err(|e| Error::Rev(e.to_string()))?;
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

    /// The index's blob ids are compared with the base tree directly (committed and staged
    /// changes alike, no HEAD tree needed), and a worktree-only status gives what differs from
    /// the index (unstaged edits, deletions, untracked files the ignore rules admit). Only files
    /// that status names are hashed; any other path's content is its index blob.
    fn changes(&mut self, base: &Rev) -> Result<Vec<Change>, Error> {
        let base_files: HashMap<String, Oid> = self.files(base.tree)?.into_iter().map(|f| (f.path, f.blob)).collect();
        let index = self.repo.read_index().map_err(|e| git_err("read index", e))?;
        let opts = makepad_git::StatusOptions { skip_target_dirs: true, ..Default::default() };
        let status = makepad_git::compute_status_worktree_only_with_options(&index, &self.repo.workdir, opts)
            .map_err(|e| git_err("worktree status", e))?;
        let off_index: HashSet<String> = status.entries.into_iter().map(|e| e.path).collect();
        let mut current: HashMap<&str, Option<Oid>> = HashMap::new();
        // regular files only, as the base side ([`Repo::files`])
        for e in index.entries.iter().filter(|e| REGULAR_FILE_MODES.contains(&e.mode)) {
            if !off_index.contains(&e.path) {
                current.insert(&e.path, Some(oid(&e.oid)));
            }
        }
        let mut candidates: Vec<&str> = current.iter().filter(|(p, o)| base_files.get(**p) != o.as_ref()).map(|(p, _)| *p).collect();
        candidates.extend(base_files.keys().filter(|p| !current.contains_key(p.as_str())).map(String::as_str));
        candidates.extend(off_index.iter().map(String::as_str));
        candidates.sort_unstable();
        candidates.dedup();
        let mut out = Vec::new();
        for path in candidates {
            let now = match current.get(path) {
                Some(index_blob) => *index_blob,
                None => self.worktree_blob(path)?,
            };
            match (base_files.get(path), now) {
                (None, Some(_)) => out.push(Change::Added(path.to_string())),
                (Some(old), Some(new)) if *old != new => out.push(Change::Modified(path.to_string())),
                (Some(_), None) => out.push(Change::Deleted(path.to_string())),
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

    /// makepad-git's `worktree_list`: the main checkout, then each registration under
    /// `<common>/worktrees/`. A registration whose directory is gone (prunable) or whose HEAD is
    /// unborn has no tree to keep, so it is not listed.
    fn worktrees(&mut self) -> Result<Vec<(PathBuf, Oid)>, Error> {
        let list = self.repo.worktree_list().map_err(|e| git_err("worktree list", e))?;
        Ok(list
            .into_iter()
            .filter(|w| w.prunable.is_none())
            .filter_map(|w| w.head.map(|h| (w.path, oid(&h))))
            .collect())
    }

    fn merge_base(&mut self, a: Oid, b: Oid) -> Result<Option<Oid>, Error> {
        let m = self.repo.merge_base(&object_id(&a), &object_id(&b)).map_err(|e| git_err("merge-base", e))?;
        Ok(m.map(|id| oid(&id)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parts of `changes` on a real worktree, timed with `time` around each:
    /// `MAKEPAD_INDEX_REPO=<worktree> MAKEPAD_INDEX_REV=<base> MAKEPAD_INDEX_PART=diff|status|flatten|changes
    /// <test binary> real_changes_parts --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_changes_parts() {
        let Some(root) = std::env::var_os("MAKEPAD_INDEX_REPO") else { panic!("set MAKEPAD_INDEX_REPO") };
        let base_name = std::env::var("MAKEPAD_INDEX_REV").unwrap_or_else(|_| "HEAD".into());
        let part = std::env::var("MAKEPAD_INDEX_PART").unwrap_or_default();
        let mut repo = GitRepo::open(Path::new(&root)).unwrap();
        let base = repo.resolve(&RevSpec::Pinned(base_name)).unwrap();
        let head = repo.resolve(&RevSpec::Worktree).unwrap();
        match part.as_str() {
            "diff" => println!("diff {}", repo.repo.diff_trees(&object_id(&base.tree), &object_id(&head.tree)).unwrap().len()),
            "status" => println!("status {}", repo.repo.status_with_options(makepad_git::StatusOptions { skip_target_dirs: true, ..Default::default() }).unwrap().entries.len()),
            "status-nocompare" => println!(
                "status {}",
                repo.repo
                    .status_with_options(makepad_git::StatusOptions { skip_target_dirs: true, skip_worktree_content_compare: true, ..Default::default() })
                    .unwrap()
                    .entries
                    .len()
            ),
            "flatten" => println!("flatten {}", repo.files(base.tree).unwrap().len()),
            "index" => println!("index {}", repo.repo.read_index().unwrap().entries.len()),
            "wtonly" => {
                let index = repo.repo.read_index().unwrap();
                let opts = makepad_git::StatusOptions { skip_target_dirs: true, ..Default::default() };
                println!("wtonly {}", makepad_git::compute_status_worktree_only_with_options(&index, &repo.repo.workdir, opts).unwrap().entries.len())
            }
            "changes" => {
                let mut c: Vec<String> = repo.changes(&base).unwrap().iter().map(|c| format!("{c:?}")).collect();
                c.sort();
                println!("changes {}", c.len());
                if let Some(out) = std::env::var_os("MAKEPAD_INDEX_OUT") {
                    std::fs::write(out, c.join("\n")).unwrap();
                }
            }
            _ => println!("opened"),
        }
    }
}
