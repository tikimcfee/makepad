//! Numbered manifests of append circles: commit by `hard_link` put-if-absent of
//! `manifest-<N>`, re-apply and retry on a lost race, read the newest valid one, tiered merges.
//!
//! A manifest is text, one fact per line, ending in the SHA-1 of the lines before it:
//! ```text
//! makepad-index manifest 1
//! circle makepad:history
//! version 3
//! writer pid 4242 in <cwd>
//! stats <chunks> <name> <path> <sig> <doc> <body>
//! segment seg-<sha1>.mpx <chunks> <5 field lengths> history:<tip>,<tip>
//! sha1 <hex>
//! ```
//! Exactly one writer wins version N+1 (the Delta Lake log protocol; Lucene's `segments_N`);
//! a loser reads N+1, re-applies its change and tries N+2. A reader takes the newest manifest
//! whose checksum holds and never blocks. After an `Add`, a tier of [`MERGE_FACTOR`] history
//! segments of like size is merged into one (Lucene's TieredMergePolicy), committed the same
//! way; a merge whose inputs are gone by then is abandoned.

use super::read::Segment;
use super::write::{put_if_absent, Builder, Published};
use super::{io_err, log, FsStore};
use crate::{CircleName, CircleStats, Error, ManifestChange, Oid, RepoId, SegmentId, SegmentManifest, SegmentRead, SourceStamp};
use makepad_git::oid::hash_object;
use makepad_git::ObjectId;
use std::fs;
use std::path::{Path, PathBuf};

const HEAD: &str = "makepad-index manifest 1";
/// Segments of one tier merged at once; a tier holds sizes within a factor of 8.
pub const MERGE_FACTOR: usize = 8;

/// One segment of a manifest, with what summing and merging need.
#[derive(Clone, Debug, PartialEq)]
pub struct SegmentEntry {
    pub id: SegmentId,
    pub stats: CircleStats,
    pub source: SourceStamp,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub manifest: SegmentManifest,
    pub segments: Vec<SegmentEntry>,
}

pub fn path_of(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("manifest-{n}"))
}

/// Manifest numbers present in `dir`, highest first.
pub fn numbers(dir: &Path) -> Result<Vec<u64>, Error> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(dir, "list", e)),
    };
    let mut out = Vec::new();
    for e in rd {
        let e = e.map_err(|e| io_err(dir, "list", e))?;
        if let Some(n) = e.file_name().to_str().and_then(|n| n.strip_prefix("manifest-")).and_then(|n| n.parse::<u64>().ok()) {
            out.push(n);
        }
    }
    out.sort_unstable_by(|a, b| b.cmp(a));
    Ok(out)
}

/// The highest manifest number present (0 for none) and the newest manifest that reads back
/// whole; invalid ones above it are logged and skipped.
pub fn newest(dir: &Path, circle: &CircleName) -> Result<(u64, Option<Entry>), Error> {
    let ns = numbers(dir)?;
    let top = ns.first().copied().unwrap_or(0);
    for n in ns {
        let path = path_of(dir, n);
        match read(&path, n, circle) {
            Ok(e) => return Ok((top, Some(e))),
            Err(Error::Io { error, .. }) if error.kind() == std::io::ErrorKind::NotFound => continue, // gc took it
            Err(e @ Error::Corrupt { .. }) => log(&format!("{e}; reading an older manifest")),
            Err(e) => return Err(e),
        }
    }
    Ok((top, None))
}

pub fn read(path: &Path, n: u64, circle: &CircleName) -> Result<Entry, Error> {
    let text = fs::read_to_string(path).map_err(|e| io_err(path, "read", e))?;
    let entry = decode(&text).map_err(|m| Error::Corrupt { path: path.to_path_buf(), message: m })?;
    if entry.manifest.version != n {
        return Err(Error::Corrupt { path: path.to_path_buf(), message: format!("holds version {}", entry.manifest.version) });
    }
    if entry.manifest.circle != *circle {
        return Err(Error::Corrupt { path: path.to_path_buf(), message: format!("belongs to circle `{}`, not `{}`", entry.manifest.circle.0, circle.0) });
    }
    Ok(entry)
}

fn stats_text(s: &CircleStats) -> String {
    let l = s.field_lengths;
    format!("{} {} {} {} {} {}", s.chunks, l[0], l[1], l[2], l[3], l[4])
}

fn parse_stats(words: &[&str]) -> Option<CircleStats> {
    if words.len() != 6 {
        return None;
    }
    let mut s = CircleStats { chunks: words[0].parse().ok()?, ..CircleStats::default() };
    for (l, w) in s.field_lengths.iter_mut().zip(&words[1..]) {
        *l = w.parse().ok()?;
    }
    Some(s)
}

fn hex(o: &Oid) -> String {
    ObjectId::from_bytes(o.0).to_hex()
}

fn unhex(s: &str) -> Option<Oid> {
    ObjectId::from_hex(s).ok().map(|o| Oid(*o.as_bytes()))
}

pub fn source_text(s: &SourceStamp) -> String {
    match s {
        SourceStamp::Tree { tree } => format!("tree:{}", hex(tree)),
        SourceStamp::History { tips } => format!("history:{}", tips.iter().map(hex).collect::<Vec<_>>().join(",")),
        SourceStamp::Notes { content } => format!("notes:{content:016x}"),
    }
}

fn parse_source(s: &str) -> Option<SourceStamp> {
    let (kind, rest) = s.split_once(':')?;
    Some(match kind {
        "tree" => SourceStamp::Tree { tree: unhex(rest)? },
        "history" if rest.is_empty() => SourceStamp::History { tips: Vec::new() },
        "history" => SourceStamp::History { tips: rest.split(',').map(unhex).collect::<Option<_>>()? },
        "notes" => SourceStamp::Notes { content: u64::from_str_radix(rest, 16).ok()? },
        _ => return None,
    })
}

pub fn encode(e: &Entry) -> String {
    let m = &e.manifest;
    let mut t = format!("{HEAD}\ncircle {}\nversion {}\nwriter {}\nstats {}\n", m.circle.0, m.version, m.writer.replace(['\n', '\r'], " "), stats_text(&m.stats));
    for s in &e.segments {
        t.push_str(&format!("segment {} {} {}\n", s.id.0, stats_text(&s.stats), source_text(&s.source)));
    }
    let sum = hash_object("blob", t.as_bytes()).to_hex();
    t.push_str(&format!("sha1 {sum}\n"));
    t
}

pub fn decode(text: &str) -> Result<Entry, String> {
    let body_end = text.rfind("sha1 ").ok_or("no checksum line")?;
    let (body, sum) = text.split_at(body_end);
    if sum.trim_end().strip_prefix("sha1 ") != Some(hash_object("blob", body.as_bytes()).to_hex().as_str()) {
        return Err("fails its checksum".into());
    }
    let mut lines = body.lines();
    if lines.next() != Some(HEAD) {
        return Err("not a manifest of this format".into());
    }
    let mut field = |name: &str| -> Result<String, String> {
        let l = lines.next().ok_or(format!("no {name} line"))?;
        l.strip_prefix(name).and_then(|r| r.strip_prefix(' ')).map(str::to_string).ok_or(format!("expected {name}, got `{l}`"))
    };
    let circle = CircleName(field("circle")?);
    let version = field("version")?.parse().map_err(|_| "bad version")?;
    let writer = field("writer")?;
    let stats = parse_stats(&field("stats")?.split(' ').collect::<Vec<_>>()).ok_or("bad stats")?;
    let mut segments = Vec::new();
    for l in lines {
        let w: Vec<&str> = l.split(' ').collect();
        if w.len() != 9 || w[0] != "segment" {
            return Err(format!("bad segment line `{l}`"));
        }
        segments.push(SegmentEntry {
            id: SegmentId(w[1].to_string()),
            stats: parse_stats(&w[2..8]).ok_or("bad segment stats")?,
            source: parse_source(w[8]).ok_or("bad segment source")?,
        });
    }
    let manifest = SegmentManifest {
        circle,
        version,
        segments: segments.iter().map(|s| s.id.clone()).collect(),
        stats,
        sources: sources_of(&segments),
        writer,
    };
    Ok(Entry { manifest, segments })
}

fn sources_of(segments: &[SegmentEntry]) -> Vec<SourceStamp> {
    let mut out: Vec<SourceStamp> = Vec::new();
    for s in segments {
        if !out.contains(&s.source) {
            out.push(s.source.clone());
        }
    }
    out
}

fn sum(segments: &[SegmentEntry]) -> CircleStats {
    let mut t = CircleStats::default();
    for s in segments {
        t.chunks += s.stats.chunks;
        for (a, b) in t.field_lengths.iter_mut().zip(s.stats.field_lengths) {
            *a += b;
        }
    }
    t
}

fn writer() -> String {
    let cwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_else(|e| format!("<cwd: {e}>"));
    format!("pid {} in {cwd}", std::process::id())
}

/// Opens a segment being committed, checking every byte once.
fn admit(store: &FsStore, repo: Option<RepoId>, circle: &CircleName, id: &SegmentId) -> Result<(Segment, SegmentEntry), Error> {
    let seg = store.segment(repo, circle, id)?;
    seg.verify()?;
    let entry = SegmentEntry { id: id.clone(), stats: seg.stats(), source: seg.key().source.clone() };
    Ok((seg, entry))
}

enum Applied {
    New(Vec<SegmentEntry>),
    Unchanged,
}

fn apply(store: &FsStore, repo: Option<RepoId>, circle: &CircleName, cur: &[SegmentEntry], change: &ManifestChange) -> Result<Applied, Error> {
    let has = |id: &SegmentId| cur.iter().any(|s| s.id == *id);
    Ok(match change {
        ManifestChange::Add(id) => {
            if has(id) {
                return Ok(Applied::Unchanged);
            }
            let (seg, entry) = admit(store, repo, circle, id)?;
            if let Some(first) = cur.first() {
                let k0 = store.segment(repo, circle, &first.id)?.key().clone();
                let k = seg.key();
                if (k.def_hash, k.indexer) != (k0.def_hash, k0.indexer) {
                    return Err(Error::Corrupt {
                        path: seg.path().to_path_buf(),
                        message: format!(
                            "built for circle definition {:016x} by indexer {}, but the circle holds {:016x} by {}: replace the circle instead of adding",
                            k.def_hash, k.indexer, k0.def_hash, k0.indexer
                        ),
                    });
                }
            }
            let mut next = cur.to_vec();
            next.push(entry);
            Applied::New(next)
        }
        ManifestChange::Merge { inputs, output } => {
            if let Some(gone) = inputs.iter().find(|i| !has(i)) {
                log(&format!("merge into {} of {} abandoned: input {} is no longer live", output.0, circle.0, gone.0));
                return Ok(Applied::Unchanged);
            }
            let (_, entry) = admit(store, repo, circle, output)?;
            let at = cur.iter().position(|s| inputs.contains(&s.id)).unwrap_or(cur.len());
            let mut next: Vec<SegmentEntry> = Vec::with_capacity(cur.len());
            for (i, s) in cur.iter().enumerate() {
                if i == at {
                    next.push(entry.clone());
                }
                if !inputs.contains(&s.id) {
                    next.push(s.clone());
                }
            }
            Applied::New(next)
        }
        ManifestChange::Replace(id) => {
            if cur.len() == 1 && cur[0].id == *id {
                return Ok(Applied::Unchanged);
            }
            Applied::New(vec![admit(store, repo, circle, id)?.1])
        }
    })
}

fn empty(circle: &CircleName) -> SegmentManifest {
    SegmentManifest { circle: circle.clone(), version: 0, segments: Vec::new(), stats: CircleStats::default(), sources: Vec::new(), writer: String::new() }
}

/// Commits one change, retrying on lost races until it lands or is abandoned; after an `Add`,
/// runs the tiered merges it makes due. Returns the newest manifest this writer saw.
pub fn commit(store: &FsStore, repo: Option<RepoId>, circle: &CircleName, change: ManifestChange) -> Result<SegmentManifest, Error> {
    let landed = commit_one(store, repo, circle, &change)?;
    if !matches!(change, ManifestChange::Add(_)) {
        return Ok(landed.manifest);
    }
    let mut cur = landed;
    loop {
        match merge_due(store, repo, circle, &cur) {
            Ok(Some(next)) => cur = next,
            Ok(None) => return Ok(cur.manifest),
            Err(e) => {
                log(&format!("tiered merge of {} failed ({e}); its segments stay unmerged", circle.0));
                return Ok(cur.manifest);
            }
        }
    }
}

fn commit_one(store: &FsStore, repo: Option<RepoId>, circle: &CircleName, change: &ManifestChange) -> Result<Entry, Error> {
    let dir = store.circle_dir(repo, circle)?;
    fs::create_dir_all(&dir).map_err(|e| io_err(&dir, "create", e))?;
    loop {
        let (top, cur) = newest(&dir, circle)?;
        let segments = match apply(store, repo, circle, cur.as_ref().map_or(&[][..], |e| &e.segments), change)? {
            Applied::New(s) => s,
            Applied::Unchanged => return Ok(cur.unwrap_or_else(|| Entry { manifest: empty(circle), segments: Vec::new() })),
        };
        let version = top + 1;
        let manifest = SegmentManifest {
            circle: circle.clone(),
            version,
            segments: segments.iter().map(|s| s.id.clone()).collect(),
            stats: sum(&segments),
            sources: sources_of(&segments),
            writer: writer(),
        };
        let entry = Entry { manifest, segments };
        match put_if_absent(&dir, &path_of(&dir, version), encode(&entry).as_bytes())? {
            Published::Linked => return Ok(entry),
            Published::Exists => continue, // another writer took this version: re-read, re-apply
        }
    }
}

/// The tier of a segment: sizes within a factor of 8 share one.
fn tier(chunks: u64) -> u32 {
    (63 - chunks.max(1).leading_zeros()) / 3
}

/// The first full tier's oldest [`MERGE_FACTOR`] history segments, if any tier is full.
pub fn plan(segments: &[SegmentEntry]) -> Option<Vec<SegmentId>> {
    let mut tiers: Vec<(u32, Vec<&SegmentEntry>)> = Vec::new();
    for s in segments.iter().filter(|s| matches!(s.source, SourceStamp::History { .. })) {
        let t = tier(s.stats.chunks);
        match tiers.iter_mut().find(|(k, _)| *k == t) {
            Some((_, v)) => v.push(s),
            None => tiers.push((t, vec![s])),
        }
    }
    tiers.sort_by_key(|(t, _)| *t);
    tiers.into_iter().find(|(_, v)| v.len() >= MERGE_FACTOR).map(|(_, v)| v[..MERGE_FACTOR].iter().map(|s| s.id.clone()).collect())
}

/// Runs one due merge: writes the merged segment and commits it; `None` when no tier is full.
fn merge_due(store: &FsStore, repo: Option<RepoId>, circle: &CircleName, cur: &Entry) -> Result<Option<Entry>, Error> {
    let Some(inputs) = plan(&cur.segments) else { return Ok(None) };
    let segs = inputs.iter().map(|id| store.segment(repo, circle, id)).collect::<Result<Vec<_>, _>>()?;
    let mut tips: Vec<Oid> = Vec::new();
    for s in &segs {
        if let SourceStamp::History { tips: t } = &s.key().source {
            for tip in t {
                if !tips.contains(tip) {
                    tips.push(*tip);
                }
            }
        }
    }
    let k0 = segs[0].key();
    let key = crate::SegmentKey { repo, circle: circle.clone(), source: SourceStamp::History { tips }, def_hash: k0.def_hash, indexer: k0.indexer };
    let mut b = Builder::new(key);
    for s in &segs {
        b.append(s)?;
    }
    let output = b.write(&store.circle_dir(repo, circle)?)?;
    // landed, or abandoned because another writer merged first: either way re-plan on the result
    Ok(Some(commit_one(store, repo, circle, &ManifestChange::Merge { inputs, output })?))
}
