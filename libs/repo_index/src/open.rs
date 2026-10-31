//! [`crate::Circles`]: opening circles at a revision. Resolve the revision, take the base for
//! its merge-base with upstream (open it, or build it once through extract, tokenize and the
//! segment sink), build the worktree overlay (`delta.rs`), and compute a history circle's
//! visibility. cli and eval both open circles here and nowhere else.
