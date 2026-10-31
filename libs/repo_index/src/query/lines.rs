//! Line-level evidence: a large item matches a query on one line of its thousand, and the
//! chunk's BM25F, normalized by its length, hides that. For the files behind the best-ranked
//! chunks, read their text at query time (tree circles through [`crate::Opened::blobs`], notes
//! circles from their stored record text), score every line by the query words it holds
//! (weighted by rarity, named words double), and lift the innermost chunk holding the best line,
//! reported at that line (`path:line`). A line between items lifts the file chunk. No format
//! change: only the top files are read, within a byte budget. Overlay chunks (changed files)
//! keep their chunk-level ranking.

use std::collections::HashMap;

use crate::query::rank::{at_key, view, Plan, Scored};
use crate::terms::{split_parts, words};
use crate::{BlobReader, ChunkAt, ChunkId, Key, Kind, Lines, OpenCircle, TermForm};

/// Files read per circle and query.
pub const LINE_FILES: usize = 6;
/// Bytes read per circle and query, at most.
pub const LINE_BYTES: usize = 2 << 20;
/// Entries whose files are read: the best this many.
pub const LINE_FROM: usize = 30;
/// Best lines kept per file.
pub const LINES_PER_FILE: usize = 2;


/// What one query word matches on a line: its folded whole or an inflection of it (never an
/// inner run of it).
struct LineWord {
    word: u16,
    texts: Vec<String>,
    weight: f32,
}

fn line_words(c: &OpenCircle, plan: &Plan) -> Vec<LineWord> {
    let n = c.stats.chunks.max(1) as f32;
    let df = |t: &str| -> f32 { c.segments.iter().map(|s| s.df(TermForm::Folded, t) + s.df(TermForm::Part, t)).sum::<u32>() as f32 };
    let idf = |d: f32| (1.0 + (n - d + 0.5) / (d + 0.5)).ln().max(0.01);
    let max_idf = idf(0.0);
    let mut out: Vec<LineWord> = Vec::new();
    for &w in &plan.content {
        let mut texts: Vec<String> = plan
            .groups
            .iter()
            .filter(|g| g.word == w && matches!(g.role, crate::query::rank::Role::Primary | crate::query::rank::Role::Variant))
            .map(|g| g.text.clone())
            .collect();
        for p in plan.paths.iter().filter(|p| p.word == w) {
            texts.extend(p.parts.iter().cloned());
        }
        if texts.is_empty() {
            continue;
        }
        let rarity = texts.iter().map(|t| idf(df(t).min(n))).fold(0.0, f32::max) / max_idf;
        let named = if plan.named.contains(&w) { crate::query::rank::NAMED_WEIGHT } else { 1.0 };
        out.push(LineWord { word: w, texts, weight: rarity * named });
    }
    out
}

/// A line's score: the summed weight of the query words it holds, whole or as a part of an
/// identifier. A path word needs all its parts on the line.
fn line_score(line: &str, qw: &[LineWord], paths: &[u16]) -> f32 {
    let mut toks: Vec<String> = Vec::new();
    for w in words(line) {
        let f = makepad_search::fold(w);
        if f.contains('_') || w.chars().skip(1).any(char::is_uppercase) {
            toks.extend(split_parts(w));
        }
        toks.push(f);
    }
    let mut score = 0.0;
    for q in qw {
        let hit = if paths.contains(&q.word) {
            q.texts.iter().all(|t| toks.iter().any(|x| x == t))
        } else {
            q.texts.iter().any(|t| toks.iter().any(|x| x == t))
        };
        if hit {
            score += q.weight;
        }
    }
    score
}

/// The file text and the line its first byte sits on.
fn source(c: &OpenCircle, at: ChunkAt, blobs: Option<&BlobReader>, cache: &mut HashMap<crate::Oid, Option<String>>, budget: &mut usize) -> Option<(String, u32)> {
    let ChunkAt::Segment { segment, chunk } = at else { return None };
    let seg = &c.segments[segment];
    if let Some(text) = seg.record_text(chunk) {
        return Some((text.to_string(), view(c, at).lines.start.max(1)));
    }
    let blob = view(c, at).blob?;
    let read = blobs?;
    let text = cache.entry(blob).or_insert_with(|| {
        if *budget == 0 {
            return None;
        }
        match read(blob) {
            Ok(bytes) => {
                *budget = budget.saturating_sub(bytes.len());
                Some(String::from_utf8_lossy(&bytes).into_owned())
            }
            Err(e) => {
                eprintln!("makepad index: line scoring skips {}: {e}", view(c, at).path);
                None
            }
        }
    });
    text.clone().map(|t| (t, 1))
}

/// Lifts the chunks holding the best lines of the top entries' files. `entries` are ordered,
/// best first; the caller re-orders after.
pub fn refine(c: &OpenCircle, circle: usize, plan: &Plan, entries: &mut Vec<Scored>, blobs: Option<&BlobReader>) {
    if plan.words == 0 || entries.is_empty() || c.visible.is_some() {
        return;
    }
    if blobs.is_none() && c.rev.is_some() {
        eprintln!("makepad index: {}: no blob reader, so lines are not scored", c.name.0);
        return;
    }
    let qw = line_words(c, plan);
    let all: f32 = qw.iter().map(|q| q.weight).sum();
    if all <= 0.0 {
        return;
    }
    let paths: Vec<u16> = plan.paths.iter().map(|p| p.word).collect();
    let top = entries.iter().filter(|e| e.pin == 0).map(|e| e.score).fold(0.0, f32::max).max(f32::MIN_POSITIVE);
    let mut cache: HashMap<crate::Oid, Option<String>> = HashMap::new();
    let mut budget = LINE_BYTES;
    let mut files: Vec<(String, ChunkAt)> = Vec::new();
    for e in entries.iter().take(LINE_FROM) {
        let v = view(c, e.at);
        let key = if v.label_len > 0 { format!("{}#{}", v.path, v.lines.start) } else { v.path.to_string() };
        if v.kind != Kind::Crate && !files.iter().any(|(p, _)| *p == key) && files.len() < LINE_FILES {
            files.push((key, e.at));
        }
    }
    let index: HashMap<_, usize> = entries.iter().enumerate().map(|(i, e)| (at_key(e.at), i)).collect();
    let mut lifts: Vec<(ChunkAt, u32, f32)> = Vec::new();
    for (_, at) in files {
        let Some((text, first)) = source(c, at, blobs, &mut cache, &mut budget) else { continue };
        // Most lines hold no query word: a substring test on the lower-cased text skips them
        // before any tokenizing (ASCII lower-casing keeps every byte offset).
        let lower = text.to_ascii_lowercase();
        let mut best: Vec<(f32, u32)> = text
            .lines()
            .zip(lower.lines())
            .enumerate()
            .filter(|(_, (_, low))| qw.iter().any(|q| q.texts.iter().any(|t| low.contains(t.as_str()))))
            .map(|(i, (l, _))| (line_score(l, &qw, &paths), first + i as u32))
            .filter(|(s, _)| *s > 0.0)
            .collect();
        best.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        let v = view(c, at);
        for (s, line) in best.into_iter().take(LINES_PER_FILE) {
            let frac = s / all;
            let holder = if v.label_len > 0 { Some(at) } else { innermost(c, v.path, line) };
            if let Some(h) = holder {
                lifts.push((h, line, frac));
            }
        }
    }
    let proximity = qw.len() > 1;
    for (at, line, frac) in lifts {
        // A one-word query has no proximity to reward: its lines only say where in a hit the
        // word is, and lift a chunk that was not a hit (a line between items).
        let lifted = top * frac * frac;
        match index.get(&at_key(at)) {
            Some(&i) => {
                let e = &mut entries[i];
                if e.line.is_none() {
                    e.line = Some(line);
                }
                if proximity {
                    e.score = e.score.max(lifted);
                }
            }
            None => entries.push(Scored {
                circle,
                at,
                score: lifted,
                covered: 0,
                primary: true,
                definition: false,
                pin: 0,
                why: Vec::new(),
                also_at: Vec::new(),
                line: Some(line),
            }),
        }
    }
}

/// The smallest chunk of `path` whose lines hold `line` (a crate card never).
fn innermost(c: &OpenCircle, path: &str, line: u32) -> Option<ChunkAt> {
    let mut best: Option<(u32, ChunkAt)> = None;
    for (si, seg) in c.segments.iter().enumerate() {
        for id in seg.chunks_with(&Key::Path { path: path.to_string(), line: Some(line) }) {
            let v = seg.chunk(id);
            if v.kind == Kind::Crate || c.shadowed.iter().any(|s| s == v.path) {
                continue;
            }
            let Lines { start, end } = v.lines;
            let width = end.saturating_sub(start);
            if start <= line && line <= end.max(start) && best.is_none_or(|(w, _)| width < w) {
                best = Some((width, ChunkAt::Segment { segment: si, chunk: ChunkId(id.0) }));
            }
        }
    }
    best.map(|(_, at)| at)
}
