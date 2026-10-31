//! The query-time overlay of a worktree: everything between its base and the files on disk
//! (committed or not, from [`crate::Repo::changes`]) re-extracted in memory; changed and deleted
//! paths shadow their base chunks. Ranking keeps the base's statistics.
