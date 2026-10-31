//! Pointer counts for registered circles the query did not open ([`crate::Opened::others`]):
//! one line each, never hits. Units ([`crate::PointerUnit`]):
//! - notes circles: distinct blobs with a record holding every query term as a substring of
//!   its lowercased text or its path within its source (EVAL §1.3's rule, exactly);
//! - tree circles: chunks matching every query word by token;
//! - history circles: commits matching every query word by token, visible at the revision;
//!   plus a branch pointer, the files touched by the matching local-only commits.

use std::collections::HashSet;

use crate::query::rank::{chunks_with, keys, rank, view, Filter, Plan};
use crate::query::short;
use crate::{ChunkId, Key, Kind, OpenCircle, Opened, Pointer, PointerUnit};

pub fn count(opened: &Opened, words: &[String], plan: &Plan) -> Vec<Pointer> {
    let mut out = Vec::new();
    if plan.words == 0 {
        return out;
    }
    for (ci, c) in opened.others.iter().enumerate() {
        if c.rev.is_none() && c.visible.is_none() {
            let count = notes_blobs(c, words);
            if count > 0 {
                out.push(pointer(c, count, PointerUnit::Blobs));
            }
            continue;
        }
        let Some(vis) = &c.visible else {
            let ranked = rank(c, ci, plan, &Filter::default(), 0);
            if ranked.all_words > 0 {
                out.push(pointer(c, ranked.all_words, PointerUnit::Chunks));
            }
            continue;
        };
        let ranked = rank(c, ci, plan, &Filter::default(), usize::MAX);
        let all: Vec<_> = ranked.entries.iter().filter(|e| e.covered >= plan.words).collect();
        let mut commits: Vec<crate::Oid> = Vec::new();
        for e in &all {
            if let Some(oid) = view(c, e.at).commit {
                if !commits.contains(&oid) {
                    commits.push(oid);
                }
            }
        }
        if !commits.is_empty() {
            out.push(pointer(c, commits.len() as u64, PointerUnit::Commits));
        }
        let local: Vec<crate::Oid> = commits.iter().copied().filter(|o| vis.local.contains(o)).collect();
        if local.is_empty() {
            continue;
        }
        let mut files: HashSet<String> = HashSet::new();
        let mut sample = String::new();
        for oid in &local {
            for at in chunks_with(c, &Key::Commit(*oid)) {
                files.extend(keys(c, at).paths);
                let v = view(c, at);
                if sample.is_empty() && v.kind == Kind::Commit {
                    sample = format!("{} {}", short(oid), v.name);
                }
            }
        }
        let branch = if vis.local_refs.is_empty() {
            eprintln!("makepad index: {} has local-only commits but no local ref names; the branch pointer is named after the circle", c.name.0);
            c.name.0.clone()
        } else {
            vis.local_refs.join(",")
        };
        out.push(Pointer {
            circle: None,
            branch: Some(branch),
            count: files.len() as u64,
            unit: PointerUnit::Files,
            commits: local.len() as u64,
            sample,
        });
    }
    out
}

fn pointer(c: &OpenCircle, count: u64, unit: PointerUnit) -> Pointer {
    Pointer { circle: Some(c.name.clone()), branch: None, count, unit, commits: 0, sample: String::new() }
}

/// Distinct blobs (or files, when a record has no blob) with a matching record.
fn notes_blobs(c: &OpenCircle, words: &[String]) -> u64 {
    let terms: Vec<String> = words.join(" ").split_whitespace().map(str::to_lowercase).collect();
    if terms.is_empty() {
        return 0;
    }
    let mut seen: HashSet<String> = HashSet::new();
    for seg in &c.segments {
        for id in 0..seg.chunk_count() {
            let Some(text) = seg.record_text(ChunkId(id)) else { continue };
            let v = seg.chunk(ChunkId(id));
            let hay = format!("{}\n{}", text.to_lowercase(), v.path_in_source().to_lowercase());
            if terms.iter().all(|t| hay.contains(t.as_str())) {
                seen.insert(v.blob.map(|b| super::hex(&b)).unwrap_or_else(|| v.path.to_string()));
            }
        }
    }
    seen.len() as u64
}
