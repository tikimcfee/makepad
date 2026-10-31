//! Removes bases no live worktree's merge-base names (keeping the newest for upstream) and
//! segments no recent manifest references, after a grace period; a delete that fails is
//! retried later and logged. A notes circle removed from `circles.conf` loses its segments (and
//! the record text they keep) with it.
