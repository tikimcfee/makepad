//! BM25F within one opened circle: candidates from the circle's segments and overlay, scored
//! with that circle's own statistics only.
//!
//! A query word's whole fold is its primary term (matched as a whole word or an identifier
//! part); its parts are expansions; its exact spelling is a bonus. Per term, each field's tf is
//! normalized by that field's length (`b`), weighted, summed, then saturated once (`k1`): BM25F,
//! not a sum of per-field BM25 scores. The score is then scaled by query-word coverage and by
//! the chunk's prior (definitions above uses, crate cards up, generated and fallback files down).

use std::collections::{HashMap, HashSet};

use crate::terms::{field_index, identifier_shaped, words};
use crate::{
    ChunkAt, ChunkId, ChunkView, Family, Field, Key, KeyKind, Keys, Kind, KindFilter, OpenCircle, QueryTerm, TermForm,
    Why,
};

/// BM25 saturation (Sourcegraph's BM25F setting).
pub const K1: f32 = 1.2;
/// BM25 length normalization.
pub const B: f32 = 0.75;
/// Field weights, in [`Field::ALL`] order: name and path carry Sourcegraph's ×5 symbol and
/// filename boost on name; path, signature and doc ×2; body ×1.
pub const FIELD_WEIGHTS: [f32; 5] = [5.0, 2.0, 2.0, 2.0, 1.0];
/// A commit's path field holds every touched path: low weight (PLAN §3).
pub const COMMIT_PATH_WEIGHT: f32 = 0.3;
/// An identifier part counts this much of a whole-word occurrence for a primary term.
pub const PART_TF: f32 = 0.7;
/// Weight of an expansion term (a part of a compound query word) against a primary term.
pub const EXPANSION_WEIGHT: f32 = 0.3;
/// Weight of the exact-case bonus term.
pub const EXACT_WEIGHT: f32 = 0.25;
/// A definition of a query word (it matched the name).
pub const DEFINITION_PRIOR: f32 = 1.3;
pub const CRATE_PRIOR: f32 = 1.3;
pub const GENERATED_PRIOR: f32 = 0.3;
pub const FALLBACK_PRIOR: f32 = 0.6;
/// Nearest hits shown under an absence.
pub const NEAREST: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Primary,
    Expansion,
    Exact,
}

/// One scored term of the query.
#[derive(Clone, Debug)]
pub struct Group {
    pub text: String,
    pub word: u16,
    pub role: Role,
}

/// The query's terms, grouped, plus what the short-circuit looks up.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub groups: Vec<Group>,
    /// Distinct query words with a primary term (coverage's denominator).
    pub words: u16,
    /// Identifiers whose exact definitions are pinned first, with the `A::b` path the query
    /// wrote, if any.
    pub pinned: Vec<(String, Option<String>)>,
}

impl Plan {
    pub fn new(raw_words: &[String], terms: &[QueryTerm]) -> Plan {
        let mut groups: Vec<Group> = Vec::new();
        for t in terms {
            let role = match t.form {
                TermForm::Folded => Role::Primary,
                TermForm::Part => Role::Expansion,
                TermForm::Exact => Role::Exact,
            };
            if !groups.iter().any(|g| g.text == t.text && g.role == role) {
                groups.push(Group { text: t.text.clone(), word: t.word, role });
            }
        }
        // A part that is itself a primary term of another word is scored as primary only.
        let primaries: HashSet<String> =
            groups.iter().filter(|g| g.role == Role::Primary).map(|g| g.text.clone()).collect();
        groups.retain(|g| g.role != Role::Expansion || !primaries.contains(&g.text));
        let word_count = groups.iter().filter(|g| g.role == Role::Primary).map(|g| g.word).collect::<HashSet<_>>().len() as u16;

        let subwords: Vec<&str> = raw_words.iter().flat_map(|w| words(w)).collect();
        let mut pinned = Vec::new();
        for raw in raw_words {
            let subs: Vec<&str> = words(raw).collect();
            let Some(last) = subs.last() else { continue };
            if raw.contains("::") {
                pinned.push((last.to_string(), Some(subs.join("::"))));
                continue;
            }
            for sub in subs {
                if subwords.len() == 1 || identifier_shaped(sub) {
                    pinned.push((sub.to_string(), None));
                }
            }
        }
        Plan { groups, words: word_count, pinned }
    }
}

/// Filters a block applies (`--kind`, `--crate`, `--path`, and a derived circle's selector).
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub kinds: Vec<KindFilter>,
    pub packages: Vec<String>,
    pub paths: Vec<String>,
}

impl Filter {
    pub fn admits(&self, view: &ChunkView<'_>) -> bool {
        (self.kinds.is_empty()
            || self.kinds.iter().any(|k| match k {
                KindFilter::Kind(kind) => view.kind == *kind,
                KindFilter::Family(family) => view.kind.family() == *family,
            }))
            && (self.packages.is_empty() || self.packages.iter().any(|p| p == view.package))
            && (self.paths.is_empty() || self.paths.iter().any(|p| view.path.starts_with(p.as_str())))
    }
}

/// A scored chunk.
#[derive(Clone, Debug)]
pub struct Scored {
    /// Index into [`crate::Opened::open`] (or `others`, for pointers).
    pub circle: usize,
    pub at: ChunkAt,
    pub score: f32,
    /// Query words matched by a primary term.
    pub covered: u16,
    /// Matched a primary term (counts toward totals); else only expansions (nearest).
    pub primary: bool,
    /// A definition of a query word: matched in the name, or pinned.
    pub definition: bool,
    pub pinned: bool,
    pub why: Vec<Why>,
    pub also_at: Vec<String>,
}

/// One circle's ranking.
#[derive(Clone, Debug, Default)]
pub struct Ranked {
    /// Primary matches, best first, copies collapsed.
    pub entries: Vec<Scored>,
    pub definitions: u64,
    /// Expansion-only matches, best first.
    pub near: Vec<Scored>,
}

/// A candidate's weighted tf per field, per query term group.
type GroupTfs = Vec<(usize, [f32; 5])>;

/// A hashable stand-in for a [`ChunkAt`].
pub type AtKey = (bool, usize, usize);

pub fn at_key(at: ChunkAt) -> AtKey {
    match at {
        ChunkAt::Segment { segment, chunk } => (false, segment, chunk.0 as usize),
        ChunkAt::Overlay { file, chunk } => (true, file, chunk),
    }
}

/// The stored view of a chunk, from a segment or the overlay.
pub fn view(c: &OpenCircle, at: ChunkAt) -> ChunkView<'_> {
    match at {
        ChunkAt::Segment { segment, chunk } => c.segments[segment].chunk(chunk),
        ChunkAt::Overlay { file, chunk } => {
            let (ch, ft) = &c.overlay[file].chunks[chunk];
            ChunkView {
                kind: ch.kind,
                path: &ch.path,
                span: ch.span,
                lines: ch.lines,
                name: &ch.name,
                parent: ch.parent.map(ChunkId),
                package: ch.keys.package.as_deref().unwrap_or(""),
                gist: crate::output::gist(&ch.fields.doc),
                sig: &ch.fields.sig,
                blob: ch.keys.blob,
                commit: ch.keys.commit,
                flags: ch.flags,
                field_lengths: ft.lengths,
                label_len: ch.label_len,
            }
        }
    }
}

pub fn keys(c: &OpenCircle, at: ChunkAt) -> Keys {
    match at {
        ChunkAt::Segment { segment, chunk } => c.segments[segment].keys(chunk),
        ChunkAt::Overlay { file, chunk } => c.overlay[file].chunks[chunk].0.keys.clone(),
    }
}

/// Every live chunk (segment chunks not shadowed, plus overlay chunks) carrying `key`.
pub fn chunks_with(c: &OpenCircle, key: &Key) -> Vec<ChunkAt> {
    let shadowed: HashSet<&str> = c.shadowed.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for (si, seg) in c.segments.iter().enumerate() {
        for id in seg.chunks_with(key) {
            if !shadowed.contains(seg.chunk(id).path) {
                out.push(ChunkAt::Segment { segment: si, chunk: id });
            }
        }
    }
    for (fi, file) in c.overlay.iter().enumerate() {
        for (ci, (chunk, _)) in file.chunks.iter().enumerate() {
            if has_key(&chunk.keys, key) {
                out.push(ChunkAt::Overlay { file: fi, chunk: ci });
            }
        }
    }
    out
}

fn has_key(k: &Keys, key: &Key) -> bool {
    match key {
        Key::Path { path, .. } => k.paths.iter().any(|p| p == path),
        Key::Commit(oid) => k.commit == Some(*oid),
        Key::Blob(oid) => k.blob == Some(*oid),
        Key::Ident(i) => k.idents.iter().any(|x| x == i),
        Key::Package(p) => k.package.as_deref() == Some(p.as_str()),
    }
}

/// The keys of one kind a chunk carries (for overlaps).
pub fn keys_of_kind(k: &Keys, kind: KeyKind) -> Vec<Key> {
    match kind {
        KeyKind::Path => k.paths.iter().map(|p| Key::Path { path: p.clone(), line: None }).collect(),
        KeyKind::Commit => k.commit.map(Key::Commit).into_iter().collect(),
        KeyKind::Blob => k.blob.map(Key::Blob).into_iter().collect(),
        KeyKind::Ident => k.idents.iter().cloned().map(Key::Ident).collect(),
        KeyKind::Package => k.package.clone().map(Key::Package).into_iter().collect(),
    }
}

/// True when a chunk is visible at the circle's revision (history: reachable commits only).
fn visible(c: &OpenCircle, v: &ChunkView<'_>) -> bool {
    match (&c.visible, v.commit) {
        (Some(vis), Some(commit)) => vis.reachable.contains(&commit),
        _ => true,
    }
}

fn idf(n: f32, df: f32) -> f32 {
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln().max(0.01)
}

/// Ranks one circle for `plan` under `filter`. `circle` is its index, carried into each entry.
pub fn rank(c: &OpenCircle, circle: usize, plan: &Plan, filter: &Filter) -> Ranked {
    let n = c.stats.chunks.max(1) as f32;
    let avg: Vec<f32> = c.stats.field_lengths.iter().map(|l| *l as f32 / n).collect();
    let forms = |role: Role| -> &'static [(TermForm, f32)] {
        match role {
            Role::Primary | Role::Expansion => &[(TermForm::Folded, 1.0), (TermForm::Part, PART_TF)],
            Role::Exact => &[(TermForm::Exact, 1.0)],
        }
    };
    let idfs: Vec<f32> = plan
        .groups
        .iter()
        .map(|g| {
            let df: u32 = c
                .segments
                .iter()
                .map(|s| forms(g.role).iter().map(|(form, _)| s.df(*form, &g.text)).sum::<u32>())
                .sum();
            idf(n, (df as f32).min(n))
        })
        .collect();

    // Per candidate: (group, weighted tf per field).
    let mut acc: HashMap<AtKey, (ChunkAt, GroupTfs)> = HashMap::new();
    let mut add = |at: ChunkAt, gi: usize, tf: [f32; 5]| {
        let list = &mut acc.entry(at_key(at)).or_insert_with(|| (at, Vec::new())).1;
        match list.iter_mut().find(|(g, _)| *g == gi) {
            Some((_, t)) => (0..5).for_each(|f| t[f] += tf[f]),
            None => list.push((gi, tf)),
        }
    };
    for (gi, g) in plan.groups.iter().enumerate() {
        for (si, seg) in c.segments.iter().enumerate() {
            for (form, w) in forms(g.role) {
                for p in seg.postings(*form, &g.text) {
                    add(ChunkAt::Segment { segment: si, chunk: p.chunk }, gi, p.tf.map(|t| t as f32 * w));
                }
            }
        }
        for (fi, file) in c.overlay.iter().enumerate() {
            for (ci, (_, ft)) in file.chunks.iter().enumerate() {
                let mut tf = [0f32; 5];
                for t in &ft.terms {
                    if let Some((_, w)) = forms(g.role).iter().find(|(form, _)| *form == t.form) {
                        if t.text == g.text {
                            tf[field_index(t.field)] += t.tf as f32 * w;
                        }
                    }
                }
                if tf.iter().any(|t| *t > 0.0) {
                    add(ChunkAt::Overlay { file: fi, chunk: ci }, gi, tf);
                }
            }
        }
    }

    let mut pinned: HashSet<AtKey> = HashSet::new();
    for (ident, path) in &plan.pinned {
        for at in chunks_with(c, &Key::Ident(ident.clone())) {
            let v = view(c, at);
            if path.as_ref().is_none_or(|p| v.name == p || v.name.ends_with(&format!("::{p}"))) {
                pinned.insert(at_key(at));
                acc.entry(at_key(at)).or_insert_with(|| (at, Vec::new()));
            }
        }
    }

    let shadowed: HashSet<&str> = c.shadowed.iter().map(String::as_str).collect();
    let mut primary = Vec::new();
    let mut near = Vec::new();
    for (_, (at, tfs)) in acc {
        let v = view(c, at);
        if matches!(at, ChunkAt::Segment { .. }) && shadowed.contains(v.path) {
            continue;
        }
        if !visible(c, &v) || !filter.admits(&v) {
            continue;
        }
        let mut weights = FIELD_WEIGHTS;
        if v.kind.family() == Family::Commit {
            weights[1] = COMMIT_PATH_WEIGHT;
        }
        let norm: Vec<f32> = (0..5)
            .map(|f| if avg[f] > 0.0 { 1.0 - B + B * v.field_lengths[f] as f32 / avg[f] } else { 1.0 })
            .collect();
        let mut score = 0.0;
        let mut covered: HashSet<u16> = HashSet::new();
        let mut in_name = false;
        let mut why = Vec::new();
        for (gi, tf) in &tfs {
            let g = &plan.groups[*gi];
            let pseudo: f32 = (0..5).map(|f| weights[f] * tf[f] / norm[f]).sum();
            if pseudo <= 0.0 {
                continue;
            }
            let role_weight = match g.role {
                Role::Primary => 1.0,
                Role::Expansion => EXPANSION_WEIGHT,
                Role::Exact => EXACT_WEIGHT,
            };
            score += role_weight * idfs[*gi] * pseudo / (K1 + pseudo);
            if g.role == Role::Primary {
                covered.insert(g.word);
                in_name |= tf[0] > 0.0;
            }
            for (f, field) in Field::ALL.iter().enumerate() {
                if tf[f] > 0.0 && !why.iter().any(|w: &Why| w.field == *field && w.term == g.text) {
                    why.push(Why { field: *field, term: g.text.clone() });
                }
            }
        }
        let is_pinned = pinned.contains(&at_key(at));
        let covered = covered.len() as u16;
        let coverage = (1.0 + covered as f32) / (1.0 + plan.words as f32);
        let definition = is_pinned || in_name;
        let mut prior = 1.0;
        if definition {
            prior *= DEFINITION_PRIOR;
        }
        if v.kind == Kind::Crate {
            prior *= CRATE_PRIOR;
        }
        if v.flags.generated {
            prior *= GENERATED_PRIOR;
        }
        if v.flags.fallback {
            prior *= FALLBACK_PRIOR;
        }
        let entry = Scored {
            circle,
            at,
            score: score * coverage * coverage * prior,
            covered,
            primary: covered > 0 || is_pinned,
            definition,
            pinned: is_pinned,
            why,
            also_at: Vec::new(),
        };
        if entry.primary {
            primary.push(entry);
        } else {
            near.push(entry);
        }
    }
    let order = |a: &Scored, b: &Scored| {
        let (va, vb) = (view(c, a.at), view(c, b.at));
        b.pinned
            .cmp(&a.pinned)
            .then(b.score.total_cmp(&a.score))
            .then(va.path.cmp(vb.path))
            .then(va.lines.start.cmp(&vb.lines.start))
    };
    primary.sort_by(order);
    near.sort_by(order);
    let entries = collapse(c, primary);
    let mut near = collapse(c, near);
    near.truncate(NEAREST);
    let definitions = entries.iter().filter(|e| e.definition).count() as u64;
    Ranked { entries, definitions, near }
}

/// Byte-identical copies (same blob, same record) collapse into the best-ranked one, which lists
/// the others' locations.
fn collapse(c: &OpenCircle, sorted: Vec<Scored>) -> Vec<Scored> {
    let mut out: Vec<Scored> = Vec::with_capacity(sorted.len());
    let mut seen: HashMap<(crate::Oid, u32, u32, &str, &str), usize> = HashMap::new();
    for e in sorted {
        let v = view(c, e.at);
        if let Some(blob) = v.blob {
            let record = (blob, v.lines.start, v.lines.end, v.kind.as_str(), v.name);
            if let Some(&first) = seen.get(&record) {
                out[first].also_at.push(location(&v));
                continue;
            }
            seen.insert(record, out.len());
        }
        out.push(e);
    }
    out
}

/// `path:start` or `path:start-end`.
pub fn location(v: &ChunkView<'_>) -> String {
    if v.lines.end > v.lines.start {
        format!("{}:{}-{}", v.path, v.lines.start, v.lines.end)
    } else {
        format!("{}:{}", v.path, v.lines.start)
    }
}
