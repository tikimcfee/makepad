//! Pointer counts for registered circles the query did not open ([`crate::Opened::others`]):
//! one line each, never hits. Units ([`crate::PointerUnit`]):
//! - notes circles: distinct blobs with a record holding every query term as a substring of
//!   its lowercased text or its path within its source (EVAL §1.3's rule, exactly);
//! - tree circles: chunks matching every query word by token;
//! - history circles: commits matching every query word by token, visible at the revision;
//!   plus a branch pointer ([`PointerUnit::Files`]): the files touched by the matching
//!   local-only commits, reachable or not, and the branch's changed files
//!   ([`crate::Visibility::local_files`]) whose text holds every absent term.

use std::collections::HashSet;

use crate::query::rank::{chunks_with, keys, rank, view, Filter, Plan, Ranked};
use crate::query::short;
use crate::terms::Terms;
use crate::Tokenize;
use crate::{ChunkId, Key, Kind, Oid, OpenCircle, Opened, Pointer, PointerUnit, Visibility};

pub fn count(opened: &Opened, words: &[String], plan: &Plan, absent: &[String]) -> Vec<Pointer> {
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
        // One ranking at any revision serves both: the commits the revision reaches, and the
        // local-only ones (a pinned upstream revision reaches none of them).
        let any_rev = Filter { any_rev: true, ..Filter::default() };
        let matching = matching_commits(c, &rank(c, ci, plan, &any_rev, usize::MAX), plan);
        let reached = matching.iter().filter(|o| vis.reachable.contains(o)).count() as u64;
        if reached > 0 {
            out.push(pointer(c, reached, PointerUnit::Commits));
        }
        // The branch's commits matter for what this circle lacks: with absent terms, a local
        // commit matches on those alone.
        let branch_matching = if absent.is_empty() {
            matching
        } else {
            let plan = Plan::new(absent, &Terms.query_terms(absent));
            matching_commits(c, &rank(c, ci, &plan, &any_rev, usize::MAX), &plan)
        };
        if let Some(branch) = branch_pointer(opened, c, vis, &branch_matching, absent) {
            out.push(branch);
        }
    }
    out
}

/// Distinct commits among the chunks matching every content word, best first.
fn matching_commits(c: &OpenCircle, ranked: &Ranked, plan: &Plan) -> Vec<Oid> {
    let mut commits: Vec<Oid> = Vec::new();
    for e in ranked.entries.iter().filter(|e| e.covered >= plan.words) {
        if let Some(oid) = view(c, e.at).commit {
            if !commits.contains(&oid) {
                commits.push(oid);
            }
        }
    }
    commits
}

/// Branch work not upstream: the files touched by the local-only commits that match, whether
/// or not the revision reaches them (a pinned upstream revision never does), plus the files the
/// branch changed whose text holds the absent terms (read only when there are any).
fn branch_pointer(opened: &Opened, c: &OpenCircle, vis: &Visibility, matching: &[Oid], absent: &[String]) -> Option<Pointer> {
    if vis.local.is_empty() && vis.local_files.is_empty() {
        return None;
    }
    let mut local: Vec<Oid> = matching.iter().copied().filter(|o| vis.local.contains(o)).collect();
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
    if !absent.is_empty() {
        if let Some(read) = &opened.blobs {
            let terms: Vec<String> = absent.iter().map(|t| t.to_ascii_lowercase()).collect();
            for (path, blob) in &vis.local_files {
                match read(*blob) {
                    Ok(mut bytes) => {
                        // Lower-cased in place: the terms are identifiers and paths, ASCII in
                        // practice, so ASCII folding finds them without copying the text.
                        bytes.make_ascii_lowercase();
                        let text = String::from_utf8_lossy(&bytes);
                        if terms.iter().all(|t| text.contains(t.as_str())) {
                            files.insert(path.clone());
                        }
                    }
                    Err(e) => eprintln!("makepad index: branch pointer skips {path}: {e}"),
                }
            }
        } else if !vis.local_files.is_empty() {
            eprintln!("makepad index: {}: no blob reader, so the branch's changed files are not searched", c.name.0);
        }
    }
    // The local commits that touched a matched file count too, newest first after the ones
    // that matched by text; the sample names the best.
    let mut touching: Vec<(i64, Oid)> = Vec::new();
    for path in &files {
        for at in chunks_with(c, &Key::Path { path: path.clone(), line: None }) {
            let v = view(c, at);
            if let (Kind::Commit, Some(oid)) = (v.kind, v.commit) {
                if vis.local.contains(&oid) && !local.contains(&oid) && !touching.iter().any(|(_, o)| *o == oid) {
                    let time = match &at {
                        crate::ChunkAt::Segment { segment, chunk } => match c.segments[*segment].detail(*chunk) {
                            crate::Detail::Commit(info) => info.time,
                            _ => 0,
                        },
                        crate::ChunkAt::Overlay { .. } => 0,
                    };
                    touching.push((time, oid));
                }
            }
        }
    }
    touching.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    local.extend(touching.into_iter().map(|(_, o)| o));
    if sample.is_empty() {
        if let Some(oid) = local.first() {
            if let Some(at) = chunks_with(c, &Key::Commit(*oid)).into_iter().find(|at| view(c, *at).kind == Kind::Commit) {
                sample = format!("{} {}", short(oid), view(c, at).name);
            }
        }
    }
    if files.is_empty() && local.is_empty() {
        return None;
    }
    let branch = if vis.local_refs.is_empty() {
        eprintln!("makepad index: {} has local-only commits but no local ref names; the branch pointer is named after the circle", c.name.0);
        c.name.0.clone()
    } else {
        vis.local_refs.join(",")
    };
    Some(Pointer { circle: None, branch: Some(branch), count: files.len() as u64, unit: PointerUnit::Files, commits: local.len() as u64, sample })
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
