//! Ranked, per-circle results ([`crate::Search`]): BM25F over name, path, sig, doc and body;
//! definitions above uses; query-term coverage; the exact definition short-circuit; filters;
//! package grouping; absence answers with the nearest hits; pointer counts for circles not
//! opened. Scores never cross circles.
