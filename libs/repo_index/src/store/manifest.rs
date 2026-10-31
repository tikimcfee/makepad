//! Numbered manifests of append circles: commit by `hard_link` put-if-absent of
//! `manifest-<N>`, re-apply and retry on a lost race, read the newest valid one, tiered merges.
