//! [`crate::Repo`] over makepad-git: revision resolution (abbreviated hex and short ref names,
//! which makepad-git itself does not resolve), tree walks, blob reads, worktree changes against
//! a base via the git index, commit walks and merge bases. No shell-outs.
