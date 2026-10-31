//! The query-time overlay of a worktree: everything between its base and the files on disk
//! (committed, staged, unstaged, deleted, and untracked files the ignore rules admit, from
//! [`crate::Repo::changes`]; ignored files never) re-extracted in memory through [`crate::Extract::extract_file`] (which applies the circle's
//! exclusions) into one [`crate::OverlayFile`] per file; changed and deleted
//! paths shadow their base chunks. Ranking keeps the base's statistics.
