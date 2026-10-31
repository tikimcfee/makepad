//! BM25F within one opened circle: candidates from the circle's segments and overlay, scored
//! with that circle's own statistics only.
//!
//! A query is read word by word ([`Plan`]):
//! - function words (`what`, `is`, `the`, ...) are dropped, unless the query has nothing else;
//! - a path-shaped word (`libs/code_arch`, `push-tree.sh`, `lower.rs:11369`) is matched as a
//!   path: chunks whose path is it, lies under it or ends with it, found through its rarest part;
//! - a package-shaped word (`makepad-hal`) pins that package's crate card;
//! - an identifier-shaped word (`KeyCode`, `HOST_CACHED`, `ids!`, `unit_here()`, `A::b`) is a
//!   subject: its exact definitions are pinned first, a kind word in the query (`enum`, `macro`)
//!   orders them, and the word is absent when the circle holds no whole-word match of it;
//! - every other word scores as text.
//!
//! A word's whole fold is its primary term (matched as a whole word or an identifier part); its
//! exact spelling is a bonus. Per term, each field's tf is normalized by that field's length
//! (`b`), weighted, summed, then saturated once (`k1`): BM25F, not a sum of per-field BM25
//! scores. The score is then scaled by word coverage (a word with several parts covers a chunk
//! when all of them match) and by the chunk's prior (definitions above uses, crate cards up,
//! generated and fallback files down). A word's parts are looked up only when nothing matched,
//! to rank the nearest hits of an absence.

use std::collections::{HashMap, HashSet};

use crate::terms::{field_index, function_word, identifier_shaped, words};
use crate::{
    BlobReader, ChunkAt, ChunkId, ChunkView, Family, Field, Key, KeyKind, Keys, Kind, KindFilter, OpenCircle, QueryTerm, TermForm,
    Why,
};

/// BM25 saturation (Sourcegraph's BM25F setting).
pub const K1: f32 = 1.2;
/// BM25 length normalization.
pub const B: f32 = 0.75;
/// Field weights, in [`Field::ALL`] order: name ×5 (Sourcegraph's symbol boost); path,
/// signature and doc ×2; body ×1.
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
/// An impl header: its methods are chunks of their own and its type is defined elsewhere, so
/// the header itself says little.
pub const IMPL_PRIOR: f32 = 0.5;
pub const FALLBACK_PRIOR: f32 = 0.6;
/// A chunk of a kind the query names (`enum`, `macro`, `crate`).
pub const KIND_PRIOR: f32 = 1.5;
/// Coverage's smoothing: a chunk covering nothing still keeps this share.
pub const COVER_FLOOR: f32 = 0.5;
/// How much of its word an inner-run match covers.
pub const RUN_COVER: f32 = 0.5;
/// Coverage weight of a named word against a descriptive one.
pub const NAMED_WEIGHT: f32 = 2.0;
/// A chunk at or under a path the query names.
pub const PATH_PRIOR: f32 = 1.5;
/// Nearest hits shown under an absence.
pub const NEAREST: usize = 3;
/// Entries ranked in full; the rest are counted, not ordered.
pub const KEEP: usize = 300;
/// Documented files under a directory the query names, pinned after its crate card.
pub const DIR_PINS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Primary,
    Expansion,
    Exact,
    /// Consecutive query words written as one identifier (`memory type` → `memory_type`,
    /// `memorytype`): covers the words it spans.
    Phrase,
    /// A long identifier's inner run (`memory_budget` in `VK_EXT_memory_budget`), scored on an
    /// item's own name only (the last segment of `A::b`): a definition named by the run is a
    /// likely referent; a chunk that merely mentions `FOO_BAR`, or a method of a type it names,
    /// is not about `FOO_BAR_BAZ`. Covers its word.
    Run,
    /// A part of a path-shaped word other than its rarest: scored only on chunks the rarest
    /// part found, so the word covers a chunk only when all its parts are there.
    PathPart,
    /// A part of an identifier prefix ending in `_` (`spin` in `SPIN_`): a placeholder the
    /// circle replaces by the prefix's real terms ([`Role::PrefixTerm`]).
    PrefixPart,
    /// A term of this circle that starts with an `_` prefix (`foo_bar_name` for `FOO_BAR_`),
    /// from the dictionary: any one covers the word.
    PrefixTerm,
    /// An English inflection of a plain word (`remotely` → `remote`, `guides` → `guide`): an
    /// alternative that covers the word, at [`VARIANT_WEIGHT`].
    Variant,
}

/// Terms one prefix expands to in a circle, the most frequent first.
pub const PREFIX_TERMS: usize = 64;

/// Weight of an inflection variant against the word as written.
pub const VARIANT_WEIGHT: f32 = 0.8;

/// Light English inflections of a plain lower-case word: -ly, plurals, -ing, -ed. Prose only
/// (code identifiers never reach here: they are not plain words); each must occur in the circle
/// to be scored.
pub fn variants(w: &str) -> Vec<String> {
    let mut out = Vec::new();
    if w.len() < 5 || !w.bytes().all(|b| b.is_ascii_lowercase()) {
        return out;
    }
    let mut push = |v: String| {
        if v.len() >= 3 && v != w && !out.contains(&v) {
            out.push(v);
        }
    };
    if let Some(b) = w.strip_suffix("ly") {
        push(b.to_string());
    }
    if let Some(b) = w.strip_suffix("ies") {
        push(format!("{b}y"));
    } else if let Some(b) = w.strip_suffix("es").filter(|b| b.ends_with(['s', 'x', 'z']) || b.ends_with("ch") || b.ends_with("sh")) {
        push(b.to_string());
    } else if let Some(b) = w.strip_suffix('s').filter(|b| !b.ends_with('s')) {
        push(b.to_string());
    }
    for suffix in ["ing", "ed"] {
        if let Some(b) = w.strip_suffix(suffix) {
            push(b.to_string());
            push(format!("{b}e"));
        }
    }
    out
}

/// One scored term of the query.
#[derive(Clone, Debug)]
pub struct Group {
    pub text: String,
    /// The (first) query word it came from.
    pub word: u16,
    pub role: Role,
    /// Query words it covers, from `word` on (a phrase covers several).
    pub span: u16,
}

/// A path-shaped query word.
#[derive(Clone, Debug)]
pub struct PathWord {
    pub word: u16,
    /// As written, without `./`, leading and trailing `/`, or a `:line` suffix.
    pub token: String,
    /// Its folded parts; the circle's rarest one finds the candidates.
    pub parts: Vec<String>,
}

/// How a query word reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Class {
    Function,
    Path,
    Package,
    /// Identifier-shaped: pinned definitions and an absence check.
    Subject { call: bool, macro_call: bool },
    /// An identifier prefix (`FOO_BAR_`, `foo::`): matches identifiers that start with it.
    Prefix,
    Plain,
}

/// A query word without its sentence punctuation, quoting and `:line` suffix, and how it reads.
pub fn classify(raw: &str) -> (String, Class) {
    let quotes = |c: char| matches!(c, '`' | '"' | '\'' | '\u{201c}' | '\u{201d}' | '\u{2018}' | '\u{2019}' | ',' | ';' | '?' | '[' | ']' | '{' | '}' | '<' | '>' | '*');
    let mut s = raw.trim_matches(quotes);
    s = s.strip_prefix('(').unwrap_or(s);
    let call = s.ends_with("()");
    if call {
        s = &s[..s.len() - 2];
    } else if s.ends_with(')') && !s.contains('(') {
        s = &s[..s.len() - 1];
    }
    for possessive in ["'s", "\u{2019}s"] {
        s = s.strip_suffix(possessive).unwrap_or(s);
    }
    // A trailing `_` or `::` leaves the identifier open: a prefix.
    let stem = s.trim_end_matches(['_', ':']);
    let prefix = stem.len() < s.len() && (s.ends_with('_') || s.ends_with("::")) && stem.chars().any(char::is_alphanumeric) && !stem.contains('/');
    if prefix {
        return (stem.to_string(), Class::Prefix);
    }
    s = s.trim_end_matches([':', '.']);
    if let Some((head, tail)) = s.rsplit_once(':') {
        let digits = |t: &str| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit());
        if digits(tail) || tail.split_once('-').is_some_and(|(a, b)| digits(a) && digits(b)) {
            s = head;
        }
    }
    let macro_call = s.ends_with('!');
    let s = s.trim_end_matches('!').trim_matches(quotes);
    let core = s.to_string();
    if core.is_empty() || !core.chars().any(char::is_alphanumeric) {
        return (core, Class::Function);
    }
    let class = if path_shaped(&core) {
        Class::Path
    } else if core.contains('-') && core.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        Class::Package
    } else if macro_call || call || core.contains("::") || words(&core).any(identifier_shaped) {
        Class::Subject { call, macro_call }
    } else if words(&core).count() == 1 && core.chars().all(|c| !c.is_uppercase()) && function_word(&core) {
        Class::Function
    } else {
        Class::Plain
    };
    (core, class)
}

/// `a/b`, `a/`, or `name.ext` (a stem of two or more characters, an extension of one to eight
/// alphanumerics holding a letter).
fn path_shaped(s: &str) -> bool {
    if s.contains('/') {
        return !s.contains("::");
    }
    match s.rsplit_once('.') {
        Some((stem, ext)) => {
            stem.chars().count() >= 2
                && (1..=8).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && ext.chars().any(|c| c.is_ascii_alphabetic())
        }
        None => false,
    }
}

/// The kind a query word names: an item kind's name, or `crate` / `package` (Cargo's word for a
/// crate) for crate cards. It orders pinned definitions and raises chunks of that kind.
fn kind_word(w: &str) -> Option<Kind> {
    const KINDS: [Kind; 13] = [
        Kind::Crate, Kind::Fn, Kind::Method, Kind::Struct, Kind::Enum, Kind::Union, Kind::Trait, Kind::Impl, Kind::Mod,
        Kind::Type, Kind::Const, Kind::Static, Kind::Macro,
    ];
    if w == "package" || w == "packages" || w == "crates" {
        return Some(Kind::Crate);
    }
    KINDS.iter().copied().find(|k| k.as_str() == w)
}

/// Longest run of consecutive query words joined into one identifier.
pub const MAX_PHRASE: usize = 3;

/// The query's terms, grouped, plus what the short-circuits look up.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// Primary and exact terms of the text words.
    pub groups: Vec<Group>,
    /// Parts of compound words, looked up only when nothing matched (nearest hits).
    pub expansions: Vec<Group>,
    /// Content words: coverage's denominator.
    pub words: u16,
    /// The content words' indices in the query.
    pub content: Vec<u16>,
    /// Named words (path-, package- and identifier-shaped): they count [`NAMED_WEIGHT`] in
    /// coverage, so chunks holding the thing named outrank chunks holding only description.
    pub named: HashSet<u16>,
    /// Identifiers whose exact definitions are pinned, with the `A::b` path the query wrote.
    pub pinned: Vec<(String, Option<String>)>,
    /// Kinds the query names; they order pinned definitions.
    pub kinds: Vec<Kind>,
    pub paths: Vec<PathWord>,
    /// Package-shaped words: their crate cards are pinned.
    pub packages: Vec<String>,
    /// Identifier-shaped words judged for absence: (folded whole, as written).
    pub subjects: Vec<(String, String)>,
    /// Identifier prefixes: (word, folded stem, folded parts, as written with its `_`).
    pub prefixes: Vec<(u16, String, Vec<String>, String)>,
}

impl Plan {
    pub fn new(raw_words: &[String], terms: &[QueryTerm]) -> Plan {
        let classes: Vec<(String, Class)> = raw_words.iter().map(|w| classify(w)).collect();
        let only_function = classes.iter().all(|(_, c)| *c == Class::Function);
        let mut plan = Plan::default();
        let mut kept: HashSet<u16> = HashSet::new();
        for (i, (core, class)) in classes.iter().enumerate() {
            let word = i.min(u16::MAX as usize) as u16;
            if (*class == Class::Function && !only_function) || core.is_empty() {
                continue;
            }
            if let Some(k) = kind_word(core) {
                plan.kinds.push(k);
            }
            match class {
                Class::Path => {
                    let token = core.trim_start_matches("./").trim_matches('/').to_string();
                    let parts: Vec<String> = words(&token).map(makepad_search::fold).collect();
                    if !parts.is_empty() {
                        plan.paths.push(PathWord { word, token, parts });
                        kept.insert(word);
                    }
                    continue;
                }
                Class::Package => plan.packages.push(core.clone()),
                Class::Prefix => {
                    // All its parts must match; an `_` prefix's parts only as identifier parts,
                    // a `::` prefix (a module) as words. Lines check that a token really starts
                    // with it.
                    let parts = crate::terms::split_parts(core);
                    let module = raw_words[i].trim_end_matches(['`', '"', '\'', ',', ';', '?', ')', '*']).ends_with("::");
                    let role = if module { Role::Primary } else { Role::PrefixPart };
                    for p in &parts {
                        plan.groups.push(Group { text: p.clone(), word, role, span: 1 });
                    }
                    plan.prefixes.push((word, makepad_search::fold(core), parts, format!("{core}_")));
                }
                Class::Subject { call, macro_call } => {
                    if *call {
                        plan.kinds.extend([Kind::Fn, Kind::Method]);
                    }
                    if *macro_call {
                        plan.kinds.push(Kind::Macro);
                    }
                    let subs: Vec<&str> = words(core).collect();
                    if let Some(last) = subs.last() {
                        if core.contains("::") {
                            plan.pinned.push((last.to_string(), Some(subs.join("::"))));
                            plan.subjects.push((makepad_search::fold(last), core.clone()));
                        } else {
                            for sub in &subs {
                                plan.pinned.push((sub.to_string(), None));
                                plan.subjects.push((makepad_search::fold(sub), sub.to_string()));
                            }
                        }
                    }
                }
                _ => {}
            }
            kept.insert(word);
        }
        // A query of one text word pins its definitions too.
        if kept.len() == 1 && plan.paths.is_empty() && plan.packages.is_empty() && plan.pinned.is_empty() {
            if let Some((core, _)) = kept.iter().next().map(|w| &classes[*w as usize]) {
                for sub in words(core) {
                    plan.pinned.push((sub.to_string(), None));
                }
            }
        }
        let path_words: HashSet<u16> = plan.paths.iter().map(|p| p.word).chain(plan.prefixes.iter().map(|p| p.0)).collect();
        for t in terms.iter().filter(|t| kept.contains(&t.word) && !path_words.contains(&t.word)) {
            let role = match t.form {
                TermForm::Folded => Role::Primary,
                TermForm::Part => Role::Expansion,
                TermForm::Exact => Role::Exact,
            };
            let list = if role == Role::Expansion { &mut plan.expansions } else { &mut plan.groups };
            if !list.iter().any(|g| g.text == t.text && g.role == role && g.word == t.word) {
                list.push(Group { text: t.text.clone(), word: t.word, role, span: 1 });
            }
        }
        // A long identifier's inner runs of parts (`VK_EXT_memory_budget` → `memory_budget`,
        // `ext_memory`), as the shorter identifiers code names the same thing by.
        for (i, (core, class)) in classes.iter().enumerate() {
            if !kept.contains(&(i as u16)) || !matches!(class, Class::Subject { .. }) {
                continue;
            }
            for sub in words(core) {
                let parts = crate::terms::split_parts(sub);
                for len in 2..parts.len() {
                    for run in parts.windows(len) {
                        for text in [run.join("_"), run.concat()] {
                            if !plan.groups.iter().any(|g| g.text == text) {
                                plan.groups.push(Group { text, word: i as u16, role: Role::Run, span: 1 });
                            }
                        }
                    }
                }
            }
        }
        // Inflections of plain words, as alternatives.
        for (i, (core, class)) in classes.iter().enumerate() {
            if kept.contains(&(i as u16)) && *class == Class::Plain {
                for text in variants(core) {
                    plan.groups.push(Group { text, word: i as u16, role: Role::Variant, span: 1 });
                }
            }
        }
        // Runs of adjacent single-part words, as the identifiers they would spell.
        let simple: Vec<Option<String>> = classes
            .iter()
            .enumerate()
            .map(|(i, (core, class))| {
                let one = words(core).count() == 1 && core.chars().all(char::is_alphanumeric);
                (kept.contains(&(i as u16)) && matches!(class, Class::Plain | Class::Subject { .. }) && one)
                    .then(|| makepad_search::fold(core))
            })
            .collect();
        for i in 0..simple.len() {
            for len in 2..=MAX_PHRASE {
                let Some(run) = simple.get(i..i + len).and_then(|w| w.iter().cloned().collect::<Option<Vec<String>>>()) else { break };
                for text in [run.join("_"), run.concat()] {
                    plan.groups.push(Group { text, word: i as u16, role: Role::Phrase, span: len as u16 });
                }
            }
        }
        let primaries: HashSet<String> = plan.groups.iter().filter(|g| g.role == Role::Primary).map(|g| g.text.clone()).collect();
        plan.expansions.retain(|g| !primaries.contains(&g.text));
        plan.words = kept.len() as u16;
        plan.content = kept.iter().copied().collect();
        plan.content.sort_unstable();
        plan.named = kept
            .iter()
            .copied()
            .filter(|w| matches!(classes[*w as usize].1, Class::Path | Class::Package | Class::Subject { .. } | Class::Prefix))
            .collect();
        plan
    }
}

/// Filters a block applies (`--kind`, `--crate`, `--path`, and a derived circle's selector).
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub kinds: Vec<KindFilter>,
    pub packages: Vec<String>,
    pub paths: Vec<String>,
    /// History: keep commits the revision does not reach (the branch pointer's local-only
    /// commits at a pinned upstream revision).
    pub any_rev: bool,
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
    /// Query words that cover the chunk.
    pub covered: u16,
    /// Matched a content word (counts toward totals); else only expansions (nearest).
    pub primary: bool,
    /// A definition of a query word: matched in the name, or pinned.
    pub definition: bool,
    /// Pinned ahead of every scored chunk, highest first: 6 a named package's card, 5 the crate
    /// card of a named directory, 4 the record of a named path, 3 an exact definition of the
    /// kind the query names, 2 an exact definition; 0 not pinned.
    pub pin: u8,
    pub why: Vec<Why>,
    pub also_at: Vec<String>,
    /// The line that matched best, when line scoring read its file: the hit reports it.
    pub line: Option<u32>,
}

/// One circle's ranking.
#[derive(Clone, Debug, Default)]
pub struct Ranked {
    /// Content matches, best first, copies collapsed; at most the `keep` asked for.
    pub entries: Vec<Scored>,
    /// Content matches in all, before `keep` and the collapse of copies.
    pub total: u64,
    pub definitions: u64,
    /// Content matches covering every content word.
    pub all_words: u64,
    /// Expansion-only matches, best first (only when there are no content matches).
    pub near: Vec<Scored>,
    /// The query's identifier- and path-shaped words this circle holds no match of.
    pub absent_terms: Vec<String>,
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
                gist: &ch.gist,
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

/// How a chunk's path meets a path the query names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PathMatch {
    /// Ends with it, or holds it as inner components.
    Mention,
    /// Lies under it.
    Under,
    /// Is it (a crate card: its directory is it).
    Exact,
}

/// How a chunk meets a path the query names: by its own path (a crate card: its directory), or
/// for a commit, the best of its touched paths (it touched the thing, or something under it).
fn path_match(c: &OpenCircle, at: ChunkAt, v: &ChunkView<'_>, token: &str) -> Option<PathMatch> {
    match v.kind.family() {
        Family::Commit => keys(c, at).paths.iter().filter_map(|p| match_one(p, token)).max().map(|m| m.max(PathMatch::Under)),
        _ => match_one(
            match v.kind {
                Kind::Crate => v.path.strip_suffix("/Cargo.toml").unwrap_or(""),
                _ => v.path_in_source(),
            },
            token,
        ),
    }
}

fn match_one(path: &str, token: &str) -> Option<PathMatch> {
    let (p, t) = (path.to_ascii_lowercase(), token.to_ascii_lowercase());
    if p == t {
        Some(PathMatch::Exact)
    } else if p.starts_with(&format!("{t}/")) {
        Some(PathMatch::Under)
    } else if p.ends_with(&format!("/{t}")) || p.contains(&format!("/{t}/")) {
        Some(PathMatch::Mention)
    } else {
        None
    }
}

/// Ranks one circle for `plan` under `filter`, ordering at most `keep` entries. `circle` is its
/// index, carried into each entry.
pub fn rank(c: &OpenCircle, circle: usize, plan: &Plan, filter: &Filter, keep: usize, blobs: Option<&BlobReader>) -> Ranked {
    let n = c.stats.chunks.max(1) as f32;
    let mut x = Ctx {
        c,
        circle,
        plan,
        filter,
        path_hits: HashMap::new(),
        path_idf: HashMap::new(),
        shadowed: c.shadowed.iter().map(String::as_str).collect(),
        avg: c.stats.field_lengths.iter().map(|l| *l as f32 / n).collect(),
        n,
    };
    let overlay_has = |form: TermForm, text: &str| {
        c.overlay.iter().any(|f| f.chunks.iter().any(|(_, ft)| ft.terms.iter().any(|t| t.form == form && t.text == text)))
    };
    let tree = c.rev.is_some() && c.visible.is_none();
    let mut absent_terms = Vec::new();
    // An identifier is absent when no chunk holds it as a whole word (its parts don't count:
    // `vec3d` is not `3d`), in every circle; a history circle with no commit naming it says so.
    for (folded, written) in &plan.subjects {
        let present = c.segments.iter().any(|seg| seg.df(TermForm::Folded, folded) > 0) || overlay_has(TermForm::Folded, folded);
        if !present && !absent_terms.contains(written) {
            absent_terms.push(written.clone());
        }
    }
    // An `_` prefix becomes this circle's terms that start with it (the dictionary's range):
    // the prefix itself is matched, not just its parts. It is absent when no term starts with it.
    // A `::` prefix (a module) is matched as its word and judged by it.
    let mut groups: Vec<Group> = plan.groups.clone();
    for (word, stem, parts, written) in &plan.prefixes {
        let open = groups.iter().any(|g| g.word == *word && g.role == Role::PrefixPart);
        if !open {
            let present = parts.iter().all(|p| c.segments.iter().any(|seg| seg.df(TermForm::Folded, p) > 0) || overlay_has(TermForm::Folded, p));
            if !present && !absent_terms.contains(written) {
                absent_terms.push(written.clone());
            }
            continue;
        }
        groups.retain(|g| !(g.word == *word && g.role == Role::PrefixPart));
        let start = format!("{stem}_");
        let mut terms: Vec<(u32, String)> = Vec::new();
        for seg in &c.segments {
            for t in seg.terms_with_prefix(TermForm::Folded, &start) {
                if !terms.iter().any(|(_, x)| *x == t) {
                    terms.push((0, t));
                }
            }
        }
        for f in &c.overlay {
            for (_, ft) in &f.chunks {
                for t in ft.terms.iter().filter(|t| t.form == TermForm::Folded && t.text.starts_with(&start)) {
                    if !terms.iter().any(|(_, x)| *x == t.text) {
                        terms.push((0, t.text.clone()));
                    }
                }
            }
        }
        if terms.is_empty() {
            if !absent_terms.contains(written) {
                absent_terms.push(written.clone());
            }
            continue;
        }
        for (df, t) in terms.iter_mut() {
            *df = c.segments.iter().map(|s| s.df(TermForm::Folded, t)).sum();
        }
        terms.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        for (_, text) in terms.into_iter().take(PREFIX_TERMS) {
            groups.push(Group { text, word: *word, role: Role::PrefixTerm, span: 1 });
        }
    }

    // Path words: each through its rarest part (by df in this circle) as a text term, and the
    // chunks whose path meets it.
    for pw in &plan.paths {
        let rarest = pw
            .parts
            .iter()
            .map(|p| (p, x.df(Role::Primary, p)))
            .filter(|(_, df)| *df > 0)
            .min_by_key(|(_, df)| *df);
        let mut found = false;
        if let Some((part, df)) = rarest {
            x.path_idf.insert(pw.word, idf(n, df as f32));
            let path_hits = &mut x.path_hits;
            // The rarest part finds the candidates (it comes first); the others must be there too.
            groups.insert(0, Group { text: part.clone(), word: pw.word, role: Role::Primary, span: 1 });
            for other in pw.parts.iter().filter(|p| *p != part) {
                groups.push(Group { text: other.clone(), word: pw.word, role: Role::PathPart, span: 1 });
            }
            let mut consider = |at: ChunkAt| {
                let v = view(c, at);
                if let Some(m) = path_match(c, at, &v, &pw.token) {
                    found = true;
                    let e = path_hits.entry(at_key(at)).or_insert((at, m, pw.word));
                    if m > e.1 {
                        *e = (at, m, pw.word);
                    }
                }
            };
            for (si, seg) in c.segments.iter().enumerate() {
                for form in [TermForm::Folded, TermForm::Part] {
                    for p in seg.postings(form, part) {
                        if p.tf[field_index(Field::Path)] > 0 {
                            consider(ChunkAt::Segment { segment: si, chunk: p.chunk });
                        }
                    }
                }
            }
            for (fi, file) in c.overlay.iter().enumerate() {
                for ci in 0..file.chunks.len() {
                    consider(ChunkAt::Overlay { file: fi, chunk: ci });
                }
            }
        }
        if tree && !found && !absent_terms.contains(&pw.token) {
            absent_terms.push(pw.token.clone());
        }
    }

    let first = score_groups(&x, &groups);
    let (mut primary, mut near): (Vec<Scored>, Vec<Scored>) = first.into_iter().partition(|e| e.primary);
    if primary.is_empty() && !plan.expansions.is_empty() {
        x.path_hits.clear();
        near.extend(score_groups(&x, &plan.expansions));
    }
    let total = primary.len() as u64;
    let definitions = primary.iter().filter(|e| e.definition).count() as u64;
    let all_words = primary.iter().filter(|e| e.covered >= plan.words).count() as u64;
    order(c, &mut primary, keep);
    super::lines::refine(c, circle, plan, &mut primary, blobs);
    order(c, &mut primary, keep);
    order(c, &mut near, NEAREST * 4);
    let entries = collapse(c, primary);
    let mut near = collapse(c, near);
    near.truncate(NEAREST);
    Ranked { entries, total, definitions, all_words, near, absent_terms }
}

/// What scoring one circle needs besides the groups.
struct Ctx<'a> {
    c: &'a OpenCircle,
    circle: usize,
    plan: &'a Plan,
    filter: &'a Filter,
    path_hits: HashMap<AtKey, (ChunkAt, PathMatch, u16)>,
    path_idf: HashMap<u16, f32>,
    shadowed: HashSet<&'a str>,
    avg: Vec<f32>,
    n: f32,
}

impl Ctx<'_> {
    fn df(&self, role: Role, text: &str) -> u32 {
        self.c.segments.iter().map(|s| forms(role).iter().map(|(form, _)| s.df(*form, text)).sum::<u32>()).sum()
    }
}

fn forms(role: Role) -> &'static [(TermForm, f32)] {
    match role {
        Role::Primary | Role::Expansion => &[(TermForm::Folded, 1.0), (TermForm::Part, PART_TF)],
        Role::Exact => &[(TermForm::Exact, 1.0)],
        Role::Phrase | Role::Run | Role::Variant => &[(TermForm::Folded, 1.0)],
        Role::PathPart => &[(TermForm::Folded, 1.0), (TermForm::Part, PART_TF)],
        Role::PrefixPart => &[(TermForm::Part, 1.0)],
        Role::PrefixTerm => &[(TermForm::Folded, 1.0)],
    }
}

/// Scores the candidates of `groups` (plus pinned and path-matched chunks).
fn score_groups(x: &Ctx<'_>, groups: &[Group]) -> Vec<Scored> {
    let (c, circle, plan, filter, path_hits, path_idf, shadowed, avg, n) =
        (x.c, x.circle, x.plan, x.filter, &x.path_hits, &x.path_idf, &x.shadowed, &x.avg, x.n);
    let idfs: Vec<f32> = groups.iter().map(|g| idf(n, (x.df(g.role, &g.text) as f32).min(n))).collect();
    // A word's coverage weight: how rare it is here (the best idf among its terms; a word
    // with no term here is as rare as can be), doubled for a named word. `makepad` in a makepad
    // circle covers little; the identifier the query names covers much.
    let mut word_idf: HashMap<u16, f32> = HashMap::new();
    for (g, i) in groups.iter().zip(&idfs) {
        if matches!(g.role, Role::Primary | Role::Phrase | Role::Run | Role::PrefixPart | Role::PrefixTerm) {
            let e = word_idf.entry(g.word).or_insert(0.0);
            *e = e.max(*i);
        }
    }
    let max_idf = idf(n, 0.0);
    let word_weight: HashMap<u16, f32> = plan
        .content
        .iter()
        .map(|w| {
            let rarity = word_idf.get(w).copied().or_else(|| path_idf.get(w).copied()).unwrap_or(max_idf) / max_idf;
            (*w, rarity * if plan.named.contains(w) { NAMED_WEIGHT } else { 1.0 })
        })
        .collect();
    let mut acc: HashMap<AtKey, (ChunkAt, GroupTfs)> = HashMap::new();
    for (gi, g) in groups.iter().enumerate() {
        let restrict = g.role == Role::PathPart;
        for (si, seg) in c.segments.iter().enumerate() {
            for (form, w) in forms(g.role) {
                for p in seg.postings(*form, &g.text) {
                    let at = ChunkAt::Segment { segment: si, chunk: p.chunk };
                    if restrict && !acc.contains_key(&at_key(at)) {
                        continue;
                    }
                    add(&mut acc, at, gi, p.tf.map(|t| t as f32 * w));
                }
            }
        }
        for (fi, file) in c.overlay.iter().enumerate() {
            for (ci, (_, ft)) in file.chunks.iter().enumerate() {
                let mut tf = [0f32; 5];
                for t in &ft.terms {
                    if t.text == g.text {
                        if let Some((_, w)) = forms(g.role).iter().find(|(form, _)| *form == t.form) {
                            tf[field_index(t.field)] += t.tf as f32 * w;
                        }
                    }
                }
                let at = ChunkAt::Overlay { file: fi, chunk: ci };
                if tf.iter().any(|t| *t > 0.0) && !(restrict && !acc.contains_key(&at_key(at))) {
                    add(&mut acc, at, gi, tf);
                }
            }
        }
    }

    // Pins: package cards, then the record of a named path, then exact definitions.
    let mut pins: HashMap<AtKey, u8> = HashMap::new();
    let mut pin = |acc: &mut HashMap<AtKey, (ChunkAt, GroupTfs)>, at: ChunkAt, rank: u8| {
        let e = pins.entry(at_key(at)).or_insert(0);
        *e = (*e).max(rank);
        acc.entry(at_key(at)).or_insert_with(|| (at, Vec::new()));
    };
    for p in &plan.packages {
        for at in chunks_with(c, &Key::Package(p.clone())) {
            if view(c, at).kind == Kind::Crate {
                pin(&mut acc, at, 6);
            }
        }
    }
    let mut dir_files: Vec<(usize, ChunkAt)> = Vec::new();
    for (at, m, _) in path_hits.values() {
        let v = view(c, *at);
        acc.entry(at_key(*at)).or_insert_with(|| (*at, Vec::new()));
        match (m, v.kind) {
            (PathMatch::Exact, Kind::Crate) => pin(&mut acc, *at, 5),
            (PathMatch::Exact | PathMatch::Mention, Kind::File) => pin(&mut acc, *at, 4),
            (PathMatch::Exact | PathMatch::Mention, Kind::Doc | Kind::Note | Kind::NoteFile) if v.lines.start <= 1 => pin(&mut acc, *at, 4),
            (PathMatch::Under, Kind::File | Kind::Doc) if v.flags.documented && v.lines.start <= 1 => {
                dir_files.push((v.path.matches('/').count(), *at));
            }
            _ => {}
        }
    }
    // Shallowest first; at one depth, a crate root or README (the file that says what the
    // directory is) before its siblings.
    let root = |at: ChunkAt| {
        let p = view(c, at).path;
        let name = p.rsplit('/').next().unwrap_or(p);
        !matches!(name, "lib.rs" | "main.rs" | "mod.rs" | "README.md")
    };
    dir_files.sort_by(|a, b| a.0.cmp(&b.0).then(root(a.1).cmp(&root(b.1))).then(view(c, a.1).path.cmp(view(c, b.1).path)));
    for (_, at) in dir_files.into_iter().take(DIR_PINS) {
        pin(&mut acc, at, 4);
    }
    for (ident, path) in &plan.pinned {
        for at in chunks_with(c, &Key::Ident(ident.clone())) {
            let v = view(c, at);
            if path.as_ref().is_none_or(|p| v.name == p || v.name.ends_with(&format!("::{p}"))) {
                pin(&mut acc, at, if plan.kinds.contains(&v.kind) { 3 } else { 2 });
            }
        }
    }

    // Words with several primary terms cover a chunk only when all of them match.
    let mut need: HashMap<u16, usize> = HashMap::new();
    for g in groups.iter().filter(|g| matches!(g.role, Role::Primary | Role::PathPart | Role::PrefixPart)) {
        *need.entry(g.word).or_insert(0) += 1;
    }
    let path_words: HashSet<u16> = plan.paths.iter().map(|p| p.word).collect();

    let mut out = Vec::with_capacity(acc.len());
    for (key, (at, tfs)) in acc {
        let v = view(c, at);
        if matches!(at, ChunkAt::Segment { .. }) && shadowed.contains(v.path) {
            continue;
        }
        if !(filter.any_rev || visible(c, &v)) || !filter.admits(&v) {
            continue;
        }
        let mut weights = FIELD_WEIGHTS;
        if v.kind.family() == Family::Commit {
            weights[1] = COMMIT_PATH_WEIGHT;
        }
        let mut score = 0.0;
        let mut matched: HashMap<u16, usize> = HashMap::new();
        let mut in_name = false;
        let mut phrase_covered: HashSet<u16> = HashSet::new();
        let mut run_covered: HashSet<u16> = HashSet::new();
        let mut why = Vec::new();
        for (gi, tf) in &tfs {
            let g = &groups[*gi];
            // A run counts in the item's own name only (the last segment of `A::b`): a method of a
            // type the run names is not the run's referent.
            if g.role == Role::Run {
                let own = v.name.rsplit("::").next().unwrap_or(v.name);
                if !words(own).any(|w| w.eq_ignore_ascii_case(&g.text) || makepad_search::fold(w) == g.text) {
                    continue;
                }
            }
            let fields = if g.role == Role::Run { 0..1 } else { 0..5 };
            let pseudo: f32 = fields
                .map(|f| {
                    let norm = if avg[f] > 0.0 { 1.0 - B + B * v.field_lengths[f] as f32 / avg[f] } else { 1.0 };
                    weights[f] * tf[f] / norm
                })
                .sum();
            if pseudo <= 0.0 {
                continue;
            }
            let role_weight = match g.role {
                Role::Primary | Role::Phrase | Role::Run | Role::PathPart | Role::PrefixPart | Role::PrefixTerm => 1.0,
                Role::Variant => VARIANT_WEIGHT,
                Role::Expansion => EXPANSION_WEIGHT,
                Role::Exact => EXACT_WEIGHT,
            };
            score += role_weight * idfs[*gi] * pseudo / (K1 + pseudo);
            match g.role {
                Role::Primary | Role::PathPart | Role::PrefixPart => {
                    *matched.entry(g.word).or_insert(0) += 1;
                    in_name |= tf[0] > 0.0 && !path_words.contains(&g.word);
                }
                Role::Phrase => {
                    for w in g.word..g.word.saturating_add(g.span) {
                        phrase_covered.insert(w);
                    }
                    in_name |= tf[0] > 0.0;
                }
                Role::Run => {
                    run_covered.insert(g.word);
                    in_name = true;
                }
                Role::Variant | Role::PrefixTerm => {
                    phrase_covered.insert(g.word);
                    in_name |= tf[0] > 0.0;
                }
                _ => {}
            }
            for (f, field) in Field::ALL.iter().enumerate() {
                if tf[f] > 0.0 && !(g.role == Role::Run && f > 0) && !why.iter().any(|w: &Why| w.field == *field && w.term == g.text) {
                    why.push(Why { field: *field, term: g.text.clone() });
                }
            }
        }
        let mut covered: HashSet<u16> =
            matched.iter().filter(|(w, m)| need.get(w).is_some_and(|n| *m >= n)).map(|(w, _)| *w).collect();
        covered.extend(phrase_covered);
        let mut prior = 1.0;
        if plan.kinds.contains(&v.kind) {
            prior *= KIND_PRIOR;
        }
        if let Some((_, m, word)) = path_hits.get(&key) {
            covered.insert(*word);
            let w = if *m == PathMatch::Mention { 0.5 } else { 1.0 };
            score += w * path_idf.get(word).copied().unwrap_or(1.0);
            if *m != PathMatch::Mention {
                prior *= PATH_PRIOR;
            }
            why.push(Why { field: Field::Path, term: plan.paths.iter().find(|p| p.word == *word).map(|p| p.token.clone()).unwrap_or_default() });
        }
        let pin_rank = pins.get(&key).copied().unwrap_or(0);
        let weight = |w: &u16| word_weight.get(w).copied().unwrap_or(1.0);
        let all: f32 = plan.content.iter().map(weight).sum();
        // A word met only through an inner run counts half: the whole token beats a part of it.
        // Summed in query-word order: a set's order changes per process, and so would the
        // float sum's last bits, and with them near-ties.
        let partial: f32 =
            plan.content.iter().filter(|w| run_covered.contains(*w) && !covered.contains(*w)).map(|w| weight(w) * RUN_COVER).sum();
        let full: f32 = plan.content.iter().filter(|w| covered.contains(*w)).map(weight).sum();
        let coverage = (COVER_FLOOR + full + partial) / (COVER_FLOOR + all);
        let covered_any = !covered.is_empty() || !run_covered.is_empty();
        let covered = covered.len() as u16;
        let definition = pin_rank >= 2 && pin_rank != 4 || in_name;
        if definition {
            prior *= DEFINITION_PRIOR;
        }
        if v.kind == Kind::Crate {
            prior *= CRATE_PRIOR;
        }
        if v.flags.generated {
            prior *= GENERATED_PRIOR;
        }
        if v.kind == Kind::Impl {
            prior *= IMPL_PRIOR;
        }
        if v.flags.fallback {
            prior *= FALLBACK_PRIOR;
        }
        let expansions_only = groups.iter().all(|g| g.role == Role::Expansion);
        out.push(Scored {
            circle,
            at,
            score: score * coverage * coverage * prior,
            covered,
            primary: !expansions_only && (covered_any || pin_rank > 0),
            definition,
            pin: pin_rank,
            why,
            also_at: Vec::new(),
            line: None,
        });
    }
    out
}

fn add(acc: &mut HashMap<AtKey, (ChunkAt, GroupTfs)>, at: ChunkAt, gi: usize, tf: [f32; 5]) {
    let list = &mut acc.entry(at_key(at)).or_insert_with(|| (at, Vec::new())).1;
    match list.iter_mut().find(|(g, _)| *g == gi) {
        Some((_, t)) => (0..5).for_each(|f| t[f] += tf[f]),
        None => list.push((gi, tf)),
    }
}

/// Orders entries best first (pins first, by pin rank), ties by path and line, and keeps the
/// best `keep`; only those are sorted.
fn order(c: &OpenCircle, entries: &mut Vec<Scored>, keep: usize) {
    let by_score = |a: &Scored, b: &Scored| b.pin.cmp(&a.pin).then(b.score.total_cmp(&a.score)).then(at_key(a.at).cmp(&at_key(b.at)));
    if entries.len() > keep {
        entries.select_nth_unstable_by(keep, by_score);
        entries.truncate(keep);
    }
    let mut keyed: Vec<(u8, f32, &str, u32, usize)> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let v = view(c, e.at);
            (e.pin, e.score, v.path, v.lines.start, i)
        })
        .collect();
    // Ties end on the chunk's place in its segment: the same order in every process (commits
    // have no path or line to tell them apart).
    keyed.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.total_cmp(&a.1))
            .then(a.2.cmp(b.2))
            .then(a.3.cmp(&b.3))
            .then(at_key(entries[a.4].at).cmp(&at_key(entries[b.4].at)))
    });
    let order: Vec<usize> = keyed.into_iter().map(|k| k.4).collect();
    let mut taken: Vec<Option<Scored>> = std::mem::take(entries).into_iter().map(Some).collect();
    *entries = order.into_iter().filter_map(|i| taken[i].take()).collect();
}

/// Byte-identical copies collapse into the best-ranked one, which lists the others' paths.
/// Every chunk of a file carries the file's blob, so a record is (blob, span), never the blob
/// alone; kind and name keep apart chunks that share a span (a file and its only item).
fn collapse(c: &OpenCircle, sorted: Vec<Scored>) -> Vec<Scored> {
    let mut out: Vec<Scored> = Vec::with_capacity(sorted.len());
    let mut seen: HashMap<(crate::Oid, u32, u32, &str, &str), usize> = HashMap::new();
    for e in sorted {
        let v = view(c, e.at);
        if let Some(blob) = v.blob {
            let record = (blob, v.span.lo, v.span.hi, v.kind.as_str(), v.name);
            if let Some(&first) = seen.get(&record) {
                // A copy has the same lines (same blob, same span): its path says it all.
                out[first].also_at.push(v.path.to_string());
                continue;
            }
            seen.insert(record, out.len());
        }
        out.push(e);
    }
    out
}
