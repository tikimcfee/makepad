//! Commit chunks for a history circle: the message as a commit chunk plus one bullet chunk per
//! message bullet; touched paths in the low-weight path field. Which commits are local-only, and
//! which are visible at a pinned revision, is computed when the circle is opened, never stored.
//! Commits whose touched paths all fall under an exclusion are left out, and excluded paths
//! are dropped from every commit's list.
