//! Search over the fixture, through an in-memory fake of the store's [`SegmentRead`]. The
//! chunks stand for what extraction emits for `fixture/` (hand-written here: extraction is
//! another part); notes records hold the fixture files' real text.

use std::collections::HashSet;

use super::handle::{format, parse};
use super::*;
use crate::output::Renderer;
use crate::{
    Chunk, ChunkId, ChunkView, CircleStats, CrateCard, Detail, Edge, FieldTerms, Fields, Flags, Key, Keys, Lines,
    OverlayFile, Output, Pointer, PointerUnit, Posting, Render, Rev, SegmentId, SegmentKey, SegmentRead, SourceStamp, Span, TermForm,
    Visibility,
};

struct MemSegment {
    key: SegmentKey,
    chunks: Vec<Chunk>,
    terms: Vec<FieldTerms>,
    notes: bool,
}

impl MemSegment {
    fn new(circle: &str, chunks: Vec<Chunk>) -> MemSegment {
        let terms = chunks.iter().map(|c| Terms.chunk_terms(c)).collect();
        MemSegment {
            key: SegmentKey {
                repo: None,
                circle: CircleName(circle.into()),
                source: SourceStamp::Notes { content: 0 },
                def_hash: 0,
                indexer: crate::INDEXER_VERSION,
            },
            chunks,
            terms,
            notes: circle.starts_with("notes:"),
        }
    }
    fn tf(&self, i: usize, form: TermForm, term: &str) -> [u32; 5] {
        let mut tf = [0; 5];
        for t in &self.terms[i].terms {
            if t.form == form && t.text == term {
                tf[crate::terms::field_index(t.field)] += t.tf;
            }
        }
        tf
    }
}

fn has(k: &Keys, key: &Key) -> bool {
    match key {
        Key::Path { path, .. } => k.paths.contains(path),
        Key::Commit(o) => k.commit == Some(*o),
        Key::Blob(o) => k.blob == Some(*o),
        Key::Ident(i) => k.idents.contains(i),
        Key::Package(p) => k.package.as_ref() == Some(p),
    }
}

impl SegmentRead for MemSegment {
    fn key(&self) -> &SegmentKey {
        &self.key
    }
    fn id(&self) -> SegmentId {
        SegmentId(format!("seg-mem-{}", self.key.circle.0))
    }
    fn stats(&self) -> CircleStats {
        let mut field_lengths = [0u64; 5];
        for t in &self.terms {
            for (sum, l) in field_lengths.iter_mut().zip(t.lengths) {
                *sum += l as u64;
            }
        }
        CircleStats { chunks: self.chunks.len() as u64, field_lengths }
    }
    fn chunk_count(&self) -> u32 {
        self.chunks.len() as u32
    }
    fn df(&self, form: TermForm, term: &str) -> u32 {
        (0..self.chunks.len()).filter(|i| self.tf(*i, form, term).iter().any(|t| *t > 0)).count() as u32
    }
    fn postings(&self, form: TermForm, term: &str) -> Box<dyn Iterator<Item = Posting> + '_> {
        let term = term.to_string();
        Box::new((0..self.chunks.len()).filter_map(move |i| {
            let tf = self.tf(i, form, &term);
            tf.iter().any(|t| *t > 0).then_some(Posting { chunk: ChunkId(i as u32), tf })
        }))
    }
    fn terms_with_prefix(&self, form: TermForm, prefix: &str) -> Vec<String> {
        let mut out: Vec<String> = self.terms.iter().flat_map(|t| &t.terms).filter(|o| o.form == form && o.text.starts_with(prefix)).map(|o| o.text.clone()).collect();
        out.sort();
        out.dedup();
        out
    }
    fn chunk(&self, id: ChunkId) -> ChunkView<'_> {
        let c = &self.chunks[id.0 as usize];
        ChunkView {
            kind: c.kind,
            path: &c.path,
            span: c.span,
            lines: c.lines,
            name: &c.name,
            parent: c.parent.map(ChunkId),
            package: c.keys.package.as_deref().unwrap_or(""),
            gist: &c.gist,
            sig: &c.fields.sig,
            blob: c.keys.blob,
            commit: c.keys.commit,
            flags: c.flags,
            field_lengths: self.terms[id.0 as usize].lengths,
            label_len: c.label_len,
        }
    }
    fn keys(&self, id: ChunkId) -> Keys {
        self.chunks[id.0 as usize].keys.clone()
    }
    fn detail(&self, id: ChunkId) -> Detail {
        self.chunks[id.0 as usize].detail.clone()
    }
    fn chunks_with(&self, key: &Key) -> Vec<ChunkId> {
        (0..self.chunks.len()).filter(|i| has(&self.chunks[*i].keys, key)).map(|i| ChunkId(i as u32)).collect()
    }
    fn cited_by(&self, key: &Key) -> Vec<(ChunkId, Edge)> {
        let mut out = Vec::new();
        for (i, c) in self.chunks.iter().enumerate() {
            for e in &c.keys.edges {
                if &e.key == key {
                    out.push((ChunkId(i as u32), e.clone()));
                }
            }
        }
        out
    }
    fn record_text(&self, id: ChunkId) -> Option<&str> {
        self.notes.then(|| self.chunks[id.0 as usize].fields.body.as_str())
    }
}

fn oid(n: u8) -> Oid {
    Oid([n; 20])
}

struct C {
    kind: Kind,
    path: &'static str,
    lines: (u32, u32),
    name: &'static str,
    package: &'static str,
    sig: &'static str,
    doc: &'static str,
    body: &'static str,
    idents: &'static [&'static str],
    blob: u8,
}

fn chunk(circle: &str, c: C) -> Chunk {
    let definition = !matches!(c.kind, Kind::File | Kind::Doc | Kind::Note | Kind::NoteFile | Kind::Commit | Kind::Bullet);
    Chunk {
        kind: c.kind,
        circle: CircleName(circle.into()),
        path: c.path.into(),
        label_len: 0,
        // Byte offsets stand in as line × 100: distinct records of one file never share a span.
        span: Span { lo: c.lines.0 * 100, hi: c.lines.1 * 100 + 99 },
        lines: Lines { start: c.lines.0, end: c.lines.1 },
        name: c.name.into(),
        gist: first_sentence(c.doc),
        parent: None,
        // As extraction does: a crate card's path field is its directory.
        fields: Fields {
            name: c.name.into(),
            path: if c.kind == Kind::Crate { c.path.trim_end_matches("/Cargo.toml").into() } else { c.path.into() },
            sig: c.sig.into(),
            doc: c.doc.into(),
            body: c.body.into(),
        },
        keys: Keys {
            paths: vec![c.path.into()],
            blob: (c.blob > 0).then(|| oid(c.blob)),
            idents: c.idents.iter().map(|s| s.to_string()).collect(),
            package: (!c.package.is_empty()).then(|| c.package.to_string()),
            ..Default::default()
        },
        flags: Flags { public: true, documented: !c.doc.is_empty(), definition, ..Default::default() },
        detail: Detail::None,
    }
}

/// What extraction stores as a gist: the doc's first sentence, at most 80 chars.
fn first_sentence(doc: &str) -> String {
    let line = doc.lines().next().unwrap_or("");
    let line = line.find(". ").map_or(line, |e| &line[..e + 1]);
    line.chars().take(crate::MAX_GIST_CHARS).collect()
}

const A: &str = "fixture-alpha";
const B: &str = "fixture-beta";

fn tree_chunks() -> Vec<Chunk> {
    let m = "makepad";
    let mut v = vec![
        chunk(m, C { kind: Kind::Crate, path: "alpha/Cargo.toml", lines: (1, 9), name: "fixture-alpha", package: A, sig: "", doc: "Spinning gizmos for the index fixture", body: "", idents: &[], blob: 1 }),
        chunk(m, C { kind: Kind::Crate, path: "beta/Cargo.toml", lines: (1, 6), name: "fixture-beta", package: B, sig: "", doc: "", body: "", idents: &[], blob: 2 }),
        chunk(m, C { kind: Kind::File, path: "alpha/src/lib.rs", lines: (1, 44), name: "alpha/src/lib.rs", package: A, sig: "MAX_TURN Gizmo Tick gizmo_count count_two tables", doc: "Spinning gizmos: a gizmo turns by a step each tick.", body: "", idents: &[], blob: 3 }),
        chunk(m, C { kind: Kind::Const, path: "alpha/src/lib.rs", lines: (6, 6), name: "MAX_TURN", package: A, sig: "pub const MAX_TURN: u32 = 8", doc: "Turns per tick, at most.", body: "", idents: &["MAX_TURN"], blob: 3 }),
        chunk(m, C { kind: Kind::Struct, path: "alpha/src/lib.rs", lines: (9, 11), name: "Gizmo", package: A, sig: "pub struct Gizmo", doc: "A gizmo that spins.", body: "pub angle: u32,", idents: &["Gizmo"], blob: 3 }),
        chunk(m, C { kind: Kind::Impl, path: "alpha/src/lib.rs", lines: (13, 22), name: "impl Gizmo", package: A, sig: "impl Gizmo", doc: "", body: "", idents: &[], blob: 3 }),
        chunk(m, C { kind: Kind::Method, path: "alpha/src/lib.rs", lines: (14, 17), name: "Gizmo::spin", package: A, sig: "pub fn spin(&mut self, step: u32)", doc: "Spins the gizmo by `step`, clamped to [`MAX_TURN`].", body: "self.angle = self.angle.wrapping_add(clamp_turn(step, MAX_TURN));", idents: &["spin"], blob: 3 }),
        chunk(m, C { kind: Kind::Method, path: "alpha/src/lib.rs", lines: (19, 21), name: "Gizmo::reset", package: A, sig: "pub fn reset(&mut self)", doc: "", body: "self.angle = 0;", idents: &["reset"], blob: 3 }),
        chunk(m, C { kind: Kind::Trait, path: "alpha/src/lib.rs", lines: (25, 27), name: "Tick", package: A, sig: "pub trait Tick", doc: "Things that can be ticked.", body: "fn tick(&mut self);", idents: &["Tick"], blob: 3 }),
        chunk(m, C { kind: Kind::Method, path: "alpha/src/lib.rs", lines: (30, 32), name: "Gizmo::tick", package: A, sig: "fn tick(&mut self)", doc: "", body: "self.spin(1);", idents: &["tick"], blob: 3 }),
        chunk(m, C { kind: Kind::Macro, path: "alpha/src/lib.rs", lines: (35, 37), name: "gizmo_count", package: A, sig: "macro_rules! gizmo_count", doc: "", body: "0 $(+ { let _ = $g; 1 })*", idents: &["gizmo_count"], blob: 3 }),
        chunk(m, C { kind: Kind::Fn, path: "alpha/src/lib.rs", lines: (39, 41), name: "count_two", package: A, sig: "pub fn count_two() -> u32", doc: "", body: "gizmo_count!(1, 2)", idents: &["count_two"], blob: 3 }),
        chunk(m, C { kind: Kind::Static, path: "alpha/src/tables.rs", lines: (3, 3), name: "SPIN_TABLE", package: A, sig: "pub static SPIN_TABLE: [u32; 16]", doc: "", body: "", idents: &["SPIN_TABLE"], blob: 4 }),
        chunk(m, C { kind: Kind::Fn, path: "beta/src/lib.rs", lines: (1, 3), name: "clamp_turn", package: B, sig: "pub fn clamp_turn(step: u32, max: u32) -> u32", doc: "", body: "step.min(max)", idents: &["clamp_turn"], blob: 5 }),
        chunk(m, C { kind: Kind::File, path: "beta/src/broken.rs", lines: (1, 4), name: "beta/src/broken.rs", package: B, sig: "", doc: "", body: "half_written turns u32 unfinished_turn turns", idents: &[], blob: 6 }),
        chunk(m, C { kind: Kind::Doc, path: "docs/guide.md", lines: (1, 4), name: "Gizmo guide", package: "", sig: "", doc: "Gizmos spin by a clamped step each tick.", body: "Gizmos spin by a clamped step each tick.", idents: &[], blob: 7 }),
        chunk(m, C { kind: Kind::Doc, path: "docs/guide.md", lines: (5, 12), name: "Spinning", package: "", sig: "", doc: "Call `Gizmo::spin` with a step; steps above `MAX_TURN` are clamped.", body: "Call `Gizmo::spin` with a step; steps above `MAX_TURN` are clamped.", idents: &[], blob: 7 }),
        chunk(m, C { kind: Kind::Doc, path: "docs/guide.md", lines: (13, 15), name: "Resetting", package: "", sig: "", doc: "`Gizmo::reset` puts the angle back to zero.", body: "`Gizmo::reset` puts the angle back to zero.", idents: &[], blob: 7 }),
        chunk(m, C { kind: Kind::Splash, path: "ui/demo.splash", lines: (3, 5), name: "GizmoView", package: "", sig: "let GizmoView = View{", doc: "", body: "width: Fill height: 40", idents: &["GizmoView"], blob: 8 }),
        chunk(m, C { kind: Kind::Splash, path: "ui/demo.splash", lines: (7, 9), name: "spin_label", package: "", sig: "fn spin_label(step)", doc: "", body: "\"spin \" + step", idents: &["spin_label"], blob: 8 }),
    ];
    v[12].flags.generated = true;
    v[14].flags.fallback = true;
    v[0].detail = Detail::Crate(CrateCard { dir: "alpha".into(), description: "Spinning gizmos for the index fixture".into(), deps: vec![(B.into(), String::new())], apps: vec!["spinner".into()], ..Default::default() });
    v
}

fn note(path: &'static str, label: &str, kind: Kind, lines: (u32, u32), name: &'static str, text: &str, blob: u8) -> Chunk {
    let mut c = chunk("notes:topic-a", C { kind, path, lines, name, package: "", sig: "", doc: "", body: "", idents: &[], blob });
    c.fields.body = text.to_string();
    c.fields.path = path[label.len() + 1..].to_string();
    c.label_len = label.len() as u16;
    c
}

fn notes_chunks() -> Vec<Chunk> {
    let plan = include_str!("../../fixture/notes/topic-a/PLAN.md");
    let (head, spin) = plan.split_at(plan.find("## ").unwrap());
    let mut v = Vec::new();
    for file in ["topic-a/PLAN.md", "topic-a/COPY.md"] {
        v.push(note(file, "topic-a", Kind::Note, (1, 2), "Topic A plan", head, 1));
        let mut s = note(file, "topic-a", Kind::Note, (3, 5), "Gizmo spin", spin, 1);
        s.keys.edges = vec![Edge { to: CircleName("makepad".into()), key: Key::Path { path: "alpha/src/lib.rs".into(), line: Some(15) } }];
        v.push(s);
    }
    v.push(note("topic-a/spin.rs", "topic-a", Kind::NoteFile, (1, 4), "spin.rs", include_str!("../../fixture/notes/topic-a/spin.rs"), 2));
    let mut bin = note("topic-a/pixel.bin", "topic-a", Kind::NoteFile, (1, 1), "pixel.bin", "", 3);
    bin.flags.binary = true;
    v.push(bin);
    v.push(note("fixture/notes/local-a/spin.rs", "fixture/notes/local-a", Kind::NoteFile, (1, 4), "spin.rs", include_str!("../../fixture/notes/local-a/spin.rs"), 4));
    v
}

fn commit(n: u8, subject: &'static str, paths: &[&str]) -> Chunk {
    let mut c = chunk("makepad:history", C { kind: Kind::Commit, path: "", lines: (0, 0), name: subject, package: "", sig: "", doc: "", body: subject, idents: &[], blob: 0 });
    c.fields.path = paths.join(" ");
    c.keys.paths = paths.iter().map(|p| p.to_string()).collect();
    c.keys.commit = Some(oid(0xc0 + n));
    c
}

fn rev(n: u8) -> Rev {
    Rev { spec: RevSpec::Worktree, commit: oid(n), tree: oid(n + 1) }
}

fn open(name: &str, rev: Option<Rev>, chunks: Vec<Chunk>, visible: Option<Visibility>) -> OpenCircle {
    let seg = MemSegment::new(name, chunks);
    let stats = seg.stats();
    OpenCircle { name: CircleName(name.into()), rev, segments: vec![Box::new(seg)], stats, overlay: Vec::new(), shadowed: Vec::new(), visible }
}

fn makepad() -> OpenCircle {
    open("makepad", Some(rev(0x9a)), tree_chunks(), None)
}

fn notes() -> OpenCircle {
    open("notes:topic-a", None, notes_chunks(), None)
}

fn history() -> OpenCircle {
    let vis = Visibility {
        reachable: [oid(0xc1), oid(0xc2)].into_iter().collect(),
        local: [oid(0xc2)].into_iter().collect(),
        local_refs: vec!["work".into()],
        local_files: Vec::new(),
    };
    open(
        "makepad:history",
        Some(rev(0x9a)),
        vec![
            commit(1, "alpha: gizmos spin by a clamped step", &["alpha/src/lib.rs"]),
            commit(2, "beta: clamp the turn harder", &["beta/src/lib.rs", "beta/src/broken.rs"]),
            commit(3, "spin on another branch", &["alpha/src/lib.rs"]),
        ],
        Some(vis),
    )
}

fn opened(open: Vec<OpenCircle>, others: Vec<OpenCircle>) -> Opened {
    Opened { open, others, unbuilt: Vec::new(), blobs: None }
}

fn query(words: &str, circles: &[&str]) -> Query {
    Query {
        mode: Mode::Find,
        words: words.split_whitespace().map(String::from).collect(),
        circles: circles.iter().map(|c| CircleExpr::Circle(CircleName(c.to_string()))).collect(),
        rev: RevSpec::Worktree,
        kinds: Vec::new(),
        packages: Vec::new(),
        paths: Vec::new(),
        limit: crate::DEFAULT_LIMIT,
        pointers: true,
    }
}

fn find(o: &Opened, q: &Query) -> FindOutput {
    Engine::default().find(o, q).expect("find")
}

fn names(b: &CircleBlock) -> Vec<&str> {
    b.hits.iter().map(|h| h.name.as_str()).collect()
}

#[test]
fn exact_definition_is_pinned_first() {
    let o = opened(vec![makepad()], vec![]);
    let out = find(&o, &query("spin", &["makepad"]));
    let b = &out.blocks[0];
    assert_eq!(b.hits[0].name, "Gizmo::spin");
    assert_eq!(b.hits[0].kind, Kind::Method);
    assert!(!b.absent);
    assert!(b.definitions >= 1 && b.uses >= 1, "{} defs {} uses", b.definitions, b.uses);
    assert_eq!(b.total, b.definitions + b.uses);
    assert_eq!(format(&b.hits[0].handle), "alpha/src/lib.rs:14-17");
    // Every chunk of a file carries the file's blob: distinct records of one file never collapse.
    assert!(b.hits.iter().filter(|h| h.path == "alpha/src/lib.rs").count() >= 2);
    assert!(b.hits.iter().all(|h| h.also_at.is_empty()));
}

#[test]
fn coverage_ranks_more_query_words_first() {
    let o = opened(vec![makepad()], vec![]);
    let out = find(&o, &query("angle zero", &["makepad"]));
    // "Resetting" holds both words; the method bodies hold only `angle`.
    assert_eq!(out.blocks[0].hits[0].name, "Resetting");
}

#[test]
fn name_match_beats_body_match() {
    let o = opened(vec![makepad()], vec![]);
    let out = find(&o, &query("ticked things", &["makepad"]));
    assert_eq!(out.blocks[0].hits[0].name, "Tick");
    let out = find(&o, &query("reset", &["makepad"]));
    assert_eq!(out.blocks[0].hits[0].name, "Gizmo::reset");
}

#[test]
fn identifier_parts_match_split_words() {
    let o = opened(vec![makepad()], vec![]);
    let out = find(&o, &query("clamp turn", &["makepad"]));
    assert_eq!(out.blocks[0].hits[0].name, "clamp_turn", "{:?}", names(&out.blocks[0]));
    let out = find(&o, &query("ClampTurn", &["makepad"]));
    assert!(out.blocks[0].absent, "ClampTurn folds to clampturn, a different word");
    assert_eq!(out.blocks[0].nearest[0].name, "clamp_turn");
}

#[test]
fn generated_and_fallback_rank_down() {
    let o = opened(vec![makepad()], vec![]);
    let out = find(&o, &query("spin table", &["makepad"]));
    let b = &out.blocks[0];
    let table = b.hits.iter().find(|h| h.name == "SPIN_TABLE").expect("generated still found");
    let mut plain = tree_chunks();
    plain[12].flags.generated = false;
    let o2 = opened(vec![open("makepad", Some(rev(0x9a)), plain, None)], vec![]);
    let out2 = find(&o2, &query("spin table", &["makepad"]));
    let table2 = out2.blocks[0].hits.iter().find(|h| h.name == "SPIN_TABLE").unwrap();
    assert!((table.score / table2.score - rank::GENERATED_PRIOR).abs() < 1e-4);
}

#[test]
fn filters_kind_crate_path() {
    let o = opened(vec![makepad()], vec![]);
    let mut q = query("gizmo", &["makepad"]);
    q.kinds = vec![kind_filter("method").unwrap()];
    assert!(find(&o, &q).blocks[0].hits.iter().all(|h| h.kind == Kind::Method));
    q.kinds = vec![kind_filter("doc").unwrap()];
    assert!(find(&o, &q).blocks[0].hits.iter().all(|h| h.kind == Kind::Doc));
    let mut q = query("turn", &["makepad"]);
    q.packages = vec![B.into()];
    let b = &find(&o, &q).blocks[0];
    assert!(!b.hits.is_empty() && b.hits.iter().all(|h| h.package == B));
    let mut q = query("gizmo", &["makepad"]);
    q.paths = vec!["docs/".into()];
    assert!(find(&o, &q).blocks[0].hits.iter().all(|h| h.path.starts_with("docs/")));
    assert_eq!(kind_filter("item"), Some(KindFilter::Family(crate::Family::Item)));
    assert_eq!(kind_filter("crate"), Some(KindFilter::Family(crate::Family::Crate)));
    assert_eq!(kind_filter("bogus"), None);
}

#[test]
fn grouping_caps_three_per_package_and_orders_groups_by_best_hit() {
    let o = opened(vec![makepad()], vec![]);
    let out = find(&o, &query("gizmo", &["makepad"]));
    let b = &out.blocks[0];
    // A chunk with no package groups by its file.
    let group = |h: &Hit| if h.package.is_empty() { h.path.clone() } else { h.package.clone() };
    let mut seen: Vec<String> = Vec::new();
    for h in &b.hits {
        let g = group(h);
        if seen.last() != Some(&g) {
            assert!(!seen.contains(&g), "a group's hits are contiguous");
            seen.push(g);
        }
    }
    for g in &seen {
        assert!(b.hits.iter().filter(|h| group(h) == *g).count() <= crate::HITS_PER_GROUP);
    }
    assert!(b.total > b.hits.len() as u64);
}

#[test]
fn absence_is_an_answer_with_nearest_hits() {
    let o = opened(vec![makepad()], vec![]);
    let out = find(&o, &query("spin_wheel_limit", &["makepad"]));
    let b = &out.blocks[0];
    assert!(b.absent);
    assert_eq!((b.total, b.definitions, b.uses), (0, 0, 0));
    assert!(b.hits.is_empty());
    assert!(!b.nearest.is_empty() && b.nearest.len() <= rank::NEAREST);
    assert!(b.nearest.iter().any(|h| h.name.to_lowercase().contains("spin")));
    let text = Renderer.text(&Output::Find(out));
    assert!(text.starts_with("makepad @"), "{text}");
    assert!(text.lines().next().unwrap().ends_with(" · 0 definitions, 0 uses · absent: spin_wheel_limit"), "{text}");
    assert!(text.contains("nearest:"), "{text}");
}

#[test]
fn overlay_shadows_base_chunks_of_changed_paths() {
    let mut c = makepad();
    c.shadowed = vec!["alpha/src/lib.rs".into()];
    let mut spin = tree_chunks().remove(6);
    spin.fields.doc = "Spins the gizmo twice as fast.".into();
    spin.gist = first_sentence(&spin.fields.doc);
    let ft = Terms.chunk_terms(&spin);
    c.overlay = vec![OverlayFile { path: "alpha/src/lib.rs".into(), chunks: vec![(spin, ft)] }];
    let o = opened(vec![c], vec![]);
    let out = find(&o, &query("spin", &["makepad"]));
    let b = &out.blocks[0];
    assert_eq!(b.hits[0].name, "Gizmo::spin");
    assert!(b.hits[0].overlay);
    assert_eq!(b.hits[0].gist, "Spins the gizmo twice as fast.");
    let from_lib: Vec<&Hit> = b.hits.iter().filter(|h| h.path == "alpha/src/lib.rs").collect();
    assert!(from_lib.iter().all(|h| h.overlay), "base chunks of a shadowed path never show");
}

#[test]
fn other_circles_are_pointers_never_hits() {
    let o = opened(vec![makepad()], vec![notes(), history()]);
    let out = find(&o, &query("spin", &["makepad"]));
    assert_eq!(out.blocks.len(), 1);
    assert!(out.blocks[0].hits.iter().all(|h| !h.path.starts_with("topic-a") && h.handle.circle.is_none()));
    let notes_ptr = out.pointers.iter().find(|p| p.circle.as_ref().is_some_and(|c| c.0 == "notes:topic-a")).unwrap();
    // PLAN.md and COPY.md are one blob; topic-a/spin.rs and local-a/spin.rs two more; the
    // binary note's path does not hold "spin".
    assert_eq!((notes_ptr.count, notes_ptr.unit), (3, PointerUnit::Blobs));
    let hist = out.pointers.iter().find(|p| p.circle.as_ref().is_some_and(|c| c.0 == "makepad:history")).unwrap();
    // c3 is not reachable at the revision.
    assert_eq!((hist.count, hist.unit), (1, PointerUnit::Commits));
    assert!(!out.pointers.iter().any(|p| p.branch.is_some()), "the matching commit is upstream");
}

#[test]
fn notes_pointer_matches_path_within_source_not_label() {
    let o = opened(vec![makepad()], vec![notes()]);
    let count = |words: &str| find(&o, &query(words, &["makepad"])).pointers.iter().map(|p| p.count).sum::<u64>();
    // Only the label `fixture/notes/local-a` holds "notes": not a match.
    assert_eq!(count("notes"), 0);
    // A path within its source matches (`pixel.bin`, a binary with empty text).
    assert_eq!(count("pixel"), 1);
    // Every term must hold, in text or path: "topic" is in the PLAN/COPY blob's text only.
    assert_eq!(count("topic plan"), 1);
    assert_eq!(count("topic draft"), 0);
}

#[test]
fn branch_pointer_counts_local_commits_files() {
    let o = opened(vec![makepad()], vec![history()]);
    let out = find(&o, &query("clamp", &["makepad"]));
    let br: &Pointer = out.pointers.iter().find(|p| p.branch.is_some()).unwrap();
    assert_eq!(br.branch.as_deref(), Some("work"));
    assert_eq!((br.count, br.commits, br.unit), (2, 1, PointerUnit::Files));
    assert_eq!(br.sample, format!("{} beta: clamp the turn harder", short(&oid(0xc2))));
    let text = Renderer.text(&Output::Find(out));
    assert!(text.contains("also: makepad:history 1 commit · branch work: 2 files in 1 commit not upstream ("), "{text}");
}

#[test]
fn opened_notes_circle_collapses_copies() {
    let o = opened(vec![notes()], vec![]);
    let out = find(&o, &query("gizmo spin", &["notes:topic-a"]));
    let b = &out.blocks[0];
    let first = &b.hits[0];
    assert_eq!(first.name, "Gizmo spin");
    assert_eq!(first.also_at.len(), 1, "{:?}", first.also_at);
    assert_eq!(b.hits.iter().filter(|h| h.name == "Gizmo spin").count(), 1);
    // Equal scores: the path order keeps COPY.md and lists PLAN.md.
    assert_eq!(first.path, "topic-a/COPY.md");
    assert_eq!(first.also_at, [String::from("topic-a/PLAN.md")]);
    assert_eq!(b.rev, "");
}

#[test]
fn union_blocks_stay_apart_and_rank_fusion_merges_by_rank() {
    let o = opened(vec![makepad(), notes()], vec![]);
    let out = find(&o, &query("spin", &["makepad", "notes:topic-a"]));
    assert_eq!(out.blocks.len(), 2);
    assert_eq!(out.blocks[1].circle.0, "notes:topic-a");
    assert!(out.blocks[0].hits.iter().all(|h| h.handle.circle.is_none()));
    assert!(out.blocks[1].hits.iter().all(|h| h.handle.circle.as_ref().is_some_and(|c| c.0 == "notes:topic-a")));
    let text = Renderer.text(&Output::Find(out));
    assert!(text.contains("\n  notes:topic-a/topic-a/"), "{text}");

    let mut q = query("spin", &[]);
    q.circles = vec![CircleExpr::Union {
        of: vec![CircleExpr::Circle(CircleName("makepad".into())), CircleExpr::Circle(CircleName("notes:topic-a".into()))],
        fuse: Fuse::Rank,
    }];
    let out = find(&o, &q);
    assert_eq!(out.blocks.len(), 1);
    assert_eq!(out.blocks[0].circle.0, "makepad+notes:topic-a");
    // Each circle's rank 1 makes the cut: fused by rank, so a big circle can't crowd out a small one.
    let from: HashSet<bool> = out.blocks[0].hits.iter().map(|h| h.handle.circle.is_some()).collect();
    assert_eq!(from.len(), 2);
    assert_eq!(out.blocks[0].hits[0].name, "Gizmo::spin");
}

#[test]
fn select_and_overlap() {
    let o = opened(vec![makepad(), history()], vec![]);
    let mut q = query("spin", &[]);
    q.circles = vec![CircleExpr::Select { of: Box::new(CircleExpr::Circle(CircleName("makepad".into()))), by: Selector::PathPrefix("docs/".into()) }];
    let out = find(&o, &q);
    assert!(!out.blocks[0].hits.is_empty() && out.blocks[0].hits.iter().all(|h| h.path.starts_with("docs/")));

    q.circles = vec![CircleExpr::Overlap {
        left: Box::new(CircleExpr::Circle(CircleName("makepad".into()))),
        right: Box::new(CircleExpr::Circle(CircleName("makepad:history".into()))),
        on: crate::KeyKind::Path,
    }];
    let b = &find(&o, &q).blocks[0];
    assert!(b.hits.iter().any(|h| h.name == "Gizmo::spin"));
    assert!(b.hits.iter().all(|h| ["alpha/src/lib.rs", "beta/src/lib.rs", "beta/src/broken.rs"].contains(&h.path.as_str())), "{:?}", names(b));
}

#[test]
fn unknown_circle_is_an_error() {
    let o = opened(vec![makepad()], vec![]);
    assert!(matches!(Engine::default().find(&o, &query("spin", &["notes:nope"])), Err(Error::UnknownCircle(c)) if c == "notes:nope"));
}

#[test]
fn def_mode_lists_only_exact_definitions() {
    let o = opened(vec![makepad()], vec![]);
    let mut q = query("spin", &["makepad"]);
    q.mode = Mode::Def;
    let b = &find(&o, &q).blocks[0];
    assert_eq!(names(b), ["Gizmo::spin"]);
    q.words = vec!["angle".into()];
    let b = &find(&o, &q).blocks[0];
    assert!(b.hits.is_empty() && !b.absent && b.uses > 0);
    assert!(!b.nearest.is_empty());
}

#[test]
fn pinned_revision_handles_carry_the_rev() {
    let o = opened(vec![makepad()], vec![]);
    let mut q = query("spin", &["makepad"]);
    q.rev = RevSpec::Pinned("HEAD".into());
    let h = &find(&o, &q).blocks[0].hits[0].handle;
    assert_eq!(format(h), format!("alpha/src/lib.rs:14-17@{}", short(&oid(0x9a))));
    assert_eq!(&parse(&format(h), &[]).unwrap(), h);
}

#[test]
fn handles_parse_format_round_trip() {
    let known = [CircleName("commercial".into())];
    for text in [
        "platform/hal/src/backend/vulkan/device.rs:462-465",
        "alpha/src/lib.rs:15",
        "alpha/src/lib.rs::Gizmo::spin",
        "crate:makepad-hal",
        "c0ffee123",
        "notes:topic/topic/PLAN.md:64-85",
        "makepad:history/2271c0eb3",
        "commercial/apps/x/src/lib.rs:3",
        "alpha/src/lib.rs:14-17@c0ffee123",
        "docs/guide.md",
    ] {
        let h = parse(text, &known).unwrap();
        assert_eq!(format(&h), text);
    }
    let h = parse("notes:t/fixture/notes/local-a/spin.rs:2", &[]).unwrap();
    assert_eq!(h.circle.unwrap().0, "notes:t");
    assert!(matches!(h.target, Target::Lines { ref path, .. } if path == "fixture/notes/local-a/spin.rs"));
    assert!(parse("", &[]).is_err());
    assert!(parse("a.rs:0", &[]).is_err());
    assert!(parse("a.rs:9-3", &[]).is_err());
}

#[test]
fn resolve_names_the_innermost_chunk() {
    let o = opened(vec![makepad(), notes(), history()], vec![]);
    let e = Engine::default();
    let names_of = |text: &str| -> Vec<String> {
        e.resolve(&o, &parse(text, &[]).unwrap())
            .unwrap()
            .iter()
            .map(|l| view(&o.open[l.circle], l.at).name.to_string())
            .collect()
    };
    assert_eq!(names_of("alpha/src/lib.rs:15"), ["Gizmo::spin"]);
    assert_eq!(names_of("alpha/src/lib.rs:13-22"), ["impl Gizmo"]);
    assert_eq!(names_of("alpha/src/lib.rs::Gizmo::spin"), ["Gizmo::spin"]);
    assert_eq!(names_of("alpha/src/lib.rs::spin"), ["Gizmo::spin"]);
    assert_eq!(names_of("alpha/src/lib.rs"), ["alpha/src/lib.rs"]);
    assert_eq!(names_of("crate:fixture-alpha"), ["fixture-alpha"]);
    assert_eq!(names_of(&short(&oid(0xc2))), ["beta: clamp the turn harder"]);
    assert_eq!(names_of("notes:topic-a/topic-a/PLAN.md:4"), ["Gizmo spin"]);
    assert!(names_of("alpha/src/lib.rs:99").is_empty());
    assert!(matches!(e.resolve(&o, &parse("notes:zz/a.md:1", &[]).unwrap()), Err(Error::UnknownCircle(_))));
}

#[test]
fn refs_by_name() {
    let o = opened(vec![makepad()], vec![]);
    let r = Engine::default().refs(&o, "MAX_TURN", true).unwrap();
    assert_eq!(r.defs.len(), 1);
    assert_eq!(r.defs[0].name, "MAX_TURN");
    assert_eq!(r.uses.files, 2, "Gizmo::spin's body and the guide");
    assert!(r.uses.by_dir.iter().any(|(d, _)| d == "alpha/src"));
    assert!(r.uses.by_dir.iter().any(|(d, _)| d == "docs"));
    assert_eq!(r.use_hits.len(), 2);
    let r = Engine::default().refs(&o, "spin", false).unwrap();
    assert!(r.use_hits.is_empty());
    assert!(r.uses.total >= 1, "Gizmo::tick calls self.spin");
}

#[test]
fn excluded_sentinel_never_reaches_results() {
    let o = opened(vec![makepad()], vec![notes(), history()]);
    let out = find(&o, &query("fixture_sentinel_excluded", &["makepad"]));
    assert!(out.blocks[0].absent);
    assert!(out.pointers.is_empty());
}

#[test]
fn find_text_fits_the_byte_budget_and_line_width() {
    let o = opened(vec![makepad()], vec![notes(), history()]);
    for words in ["spin", "gizmo", "gizmo spin step", "turn", "spin_wheel_limit", "angle zero"] {
        let text = Renderer.text(&Output::Find(find(&o, &query(words, &["makepad"]))));
        assert!(text.len() <= crate::FIND_P90_BYTES, "{words}: {} bytes\n{text}", text.len());
        assert!(text.lines().all(|l| l.chars().count() <= crate::MAX_LINE_CHARS), "{text}");
    }
}

#[test]
fn find_json_lines_have_every_field() {
    let o = opened(vec![makepad()], vec![notes(), history()]);
    let out = Output::Find(find(&o, &query("clamp", &["makepad"])));
    let json = Renderer.json_lines(&Renderer.records(&out));
    let mut kinds = HashSet::new();
    for line in json.lines() {
        let v = makepad_strict_json::parse(line.as_bytes()).expect("valid json");
        let keys: &[&str] = if v.get("header").is_some() {
            kinds.insert("header");
            &["circle", "rev", "total", "definitions", "uses", "absent"]
        } else if v.get("pointer").is_some() {
            kinds.insert("pointer");
            &["circle", "branch", "count", "unit", "commits", "sample"]
        } else {
            kinds.insert("hit");
            &["handle", "circle", "rev", "kind", "package", "path", "start", "end", "name", "sig", "gist", "score", "why", "also_at", "overlay", "local", "nearest"]
        };
        for k in keys {
            assert!(v.get(k).is_some(), "{k} missing in {line}");
        }
    }
    assert_eq!(kinds.len(), 3);
}

#[test]
fn function_words_drop_unless_they_are_all_there_is() {
    let o = opened(vec![makepad()], vec![]);
    let plain = find(&o, &query("gizmo spin", &["makepad"]));
    let asked = find(&o, &query("what is the gizmo spin", &["makepad"]));
    assert_eq!(names(&plain.blocks[0]), names(&asked.blocks[0]));
    assert_eq!(plain.blocks[0].total, asked.blocks[0].total);
    // Only function words: they are the query.
    let only = find(&o, &query("by the", &["makepad"]));
    assert!(!only.blocks[0].absent);
    assert!(!only.blocks[0].hits.is_empty());
}

#[test]
fn classify_reads_paths_packages_and_identifiers() {
    use rank::{classify, Class};
    let path = |w: &str| classify(w);
    assert_eq!(path("libs/code_arch"), ("libs/code_arch".into(), Class::Path));
    assert_eq!(path("(local/plans/run-from-git.md)"), ("local/plans/run-from-git.md".into(), Class::Path));
    assert_eq!(path("lower.rs:11369-11374"), ("lower.rs".into(), Class::Path));
    assert_eq!(path("push-tree.sh"), ("push-tree.sh".into(), Class::Path));
    assert_eq!(path("apps/commercial:"), ("apps/commercial".into(), Class::Path));
    // All capitals read as an identifier (`HAL`, `README`): judged for absence, never pinned
    // unless something defines it.
    assert_eq!(path("README's"), ("README".into(), Class::Subject { call: false, macro_call: false }));
    assert_eq!(path("makepad-hal"), ("makepad-hal".into(), Class::Package));
    assert_eq!(path("ids!"), ("ids".into(), Class::Subject { call: false, macro_call: true }));
    assert_eq!(path("unit_here()"), ("unit_here".into(), Class::Subject { call: true, macro_call: false }));
    assert_eq!(path("`KeyCode`"), ("KeyCode".into(), Class::Subject { call: false, macro_call: false }));
    assert_eq!(path("Vulkan"), ("Vulkan".into(), Class::Plain));
    assert_eq!(path("what"), ("what".into(), Class::Function));
    assert_eq!(path("e.g."), ("e.g".into(), Class::Plain));
    assert_eq!(path("Device::memory_type"), ("Device::memory_type".into(), Class::Subject { call: false, macro_call: false }));
}

#[test]
fn a_named_directory_pins_its_crate_card_then_its_root_file() {
    let o = opened(vec![makepad()], vec![]);
    let out = find(&o, &query("what is alpha/ and who uses it", &["makepad"]));
    let b = &out.blocks[0];
    assert_eq!(b.hits[0].kind, Kind::Crate);
    assert_eq!(b.hits[0].name, "fixture-alpha");
    assert_eq!((b.hits[1].kind, b.hits[1].path.as_str()), (Kind::File, "alpha/src/lib.rs"));
    assert!(!b.absent);
}

#[test]
fn a_named_file_pins_its_record() {
    let o = opened(vec![makepad()], vec![]);
    let b = &find(&o, &query("does docs/guide.md still say spin", &["makepad"])).blocks[0];
    assert_eq!((b.hits[0].path.as_str(), b.hits[0].lines.start), ("docs/guide.md", 1));
    let b = &find(&o, &query("broken.rs", &["makepad"])).blocks[0];
    assert_eq!(b.hits[0].path, "beta/src/broken.rs");
}

#[test]
fn a_named_package_pins_its_card() {
    let o = opened(vec![makepad()], vec![]);
    let b = &find(&o, &query("who depends on fixture-beta", &["makepad"])).blocks[0];
    assert_eq!((b.hits[0].kind, b.hits[0].name.as_str()), (Kind::Crate, "fixture-beta"));
}

#[test]
fn a_missing_named_thing_is_absent_beside_present_words() {
    let o = opened(vec![makepad(), notes(), history()], vec![]);
    // A path the tree lacks, next to one it has.
    let b = &find(&o, &query("alpha/src/lib.rs points at alpha/src/gone.rs", &["makepad"])).blocks[0];
    assert!(b.absent);
    assert!(!b.hits.is_empty(), "the present words still rank");
    // An identifier with no whole-word match, next to present words.
    let b = &find(&o, &query("is spin_wheel_limit in gizmo", &["makepad"])).blocks[0];
    assert!(b.absent);
    assert_eq!(b.absent_terms, ["spin_wheel_limit"]);
    let header = Renderer.text(&Output::Find(FindOutput { blocks: vec![b.clone()], pointers: Vec::new() }));
    assert!(header.lines().next().unwrap().ends_with("uses · absent: spin_wheel_limit"), "{header}");
    // Nothing matches at all: every content word is absent, function words aside.
    let b = &find(&o, &query("where is zzqx_none wombat", &["makepad"])).blocks[0];
    // Plain words are never named absent: only the identifier is.
    assert_eq!(b.absent_terms, ["zzqx_none"]);
    // Nothing shaped is missing and nothing matches: no absence is claimed.
    let b = &find(&o, &query("why does the wombat yodel", &["makepad"])).blocks[0];
    assert!(!b.absent && b.absent_terms.is_empty() && b.hits.is_empty());
    // Present things are never absent.
    for q in ["alpha/src/lib.rs", "clamp_turn in beta", "what is fixture-beta", "MAX_TURN"] {
        assert!(!find(&o, &query(q, &["makepad"])).blocks[0].absent, "{q}");
    }
    // Paths are judged in tree circles only.
    // (A note names alpha/src/lib.rs in its text; no note file has that path.)
    assert!(!find(&o, &query("alpha/src/lib.rs", &["notes:topic-a"])).blocks[0].absent);
    assert!(!find(&o, &query("beta/src/gone.rs clamp", &["makepad:history"])).blocks[0].absent);
    // An identifier is judged everywhere, on whole words: no commit names spin_wheel_limit,
    // though its parts match.
    let b = &find(&o, &query("spin_wheel_limit spin", &["makepad:history"])).blocks[0];
    assert_eq!(b.absent_terms, ["spin_wheel_limit"]);
}

#[test]
fn identifier_phrases_and_inner_runs() {
    let o = opened(vec![makepad()], vec![]);
    // Two words spelling one identifier.
    let b = &find(&o, &query("where do we clamp the turn", &["makepad"])).blocks[0];
    assert_eq!(b.hits[0].name, "clamp_turn");
    // An absent long identifier still reaches its inner run.
    let b = &find(&o, &query("FIXTURE_CLAMP_TURN_V2 step", &["makepad"])).blocks[0];
    assert!(b.absent);
    assert!(b.hits.iter().take(3).any(|h| h.name == "clamp_turn"), "{:?}", names(b));
    // A run counts in names only: MAX_TURN's definition, not the body that mentions it.
    let b = &find(&o, &query("MAX_TURN_CAP", &["makepad"])).blocks[0];
    assert_eq!(b.hits[0].name, "MAX_TURN");
    assert!(!b.hits.iter().any(|h| h.name == "Gizmo::spin"), "{:?}", names(b));
}

#[test]
fn kind_words_order_definitions() {
    let o = opened(vec![makepad()], vec![]);
    let b = &find(&o, &query("gizmo_count! macro definition", &["makepad"])).blocks[0];
    assert_eq!((b.hits[0].kind, b.hits[0].name.as_str()), (Kind::Macro, "gizmo_count"));
    let b = &find(&o, &query("Gizmo struct", &["makepad"])).blocks[0];
    assert_eq!((b.hits[0].kind, b.hits[0].name.as_str()), (Kind::Struct, "Gizmo"));
    // A kind word raises chunks of that kind, against the same hits without it.
    let ratio = |q: &str| {
        let b = find(&o, &query(q, &["makepad"])).blocks.remove(0);
        let score = |k: Kind| b.hits.iter().find(|h| h.kind == k).map(|h| h.score).unwrap();
        score(Kind::Crate) / score(Kind::Doc)
    };
    let (plain, asked) = (ratio("spinning"), ratio("spinning crate"));
    assert!((asked / plain - rank::KIND_PRIOR).abs() < 1e-3, "{asked} / {plain}");
}

#[test]
fn commits_meet_a_named_path_through_their_touched_paths() {
    let o = opened(vec![history()], vec![]);
    let b = &find(&o, &query("which commit touched beta/src", &["makepad:history"])).blocks[0];
    assert_eq!(b.hits[0].name, "beta: clamp the turn harder");
    // Each commit is a group of its own: no cap of three.
    let b = &find(&o, &query("a", &["makepad:history"])).blocks[0];
    assert!(b.hits.len() <= 3 || b.hits.iter().all(|h| h.package.is_empty()));
}

#[test]
fn branch_pointer_counts_local_commits_the_pinned_rev_does_not_reach() {
    let mut h = history();
    // Pinned at upstream: the local commit c2 is not reachable from the revision.
    if let Some(vis) = &mut h.visible {
        vis.reachable.remove(&oid(0xc2));
    }
    let o = opened(vec![makepad()], vec![h]);
    let out = find(&o, &query("clamp", &["makepad"]));
    let br = out.pointers.iter().find(|p| p.branch.is_some()).expect("a branch pointer");
    assert_eq!((br.count, br.commits, br.branch.as_deref()), (2, 1, Some("work")));
    // The commits pointer stays what the revision reaches.
    let hist = out.pointers.iter().find(|p| p.unit == PointerUnit::Commits);
    assert!(hist.is_none(), "c2 is the only commit naming clamp, and the rev does not reach it");
}

#[test]
fn branch_pointer_reads_changed_files_only_for_absent_terms() {
    use std::cell::Cell;
    use std::rc::Rc;
    let mut h = history();
    if let Some(vis) = &mut h.visible {
        vis.local_files = vec![("alpha/src/wheel.rs".into(), oid(0xd1)), ("beta/src/lib.rs".into(), oid(0xd2))];
    }
    let reads = Rc::new(Cell::new(0));
    let counter = reads.clone();
    let mut o = opened(vec![makepad()], vec![h]);
    o.blobs = Some(Box::new(move |blob: Oid| {
        // Count the branch's files only (line scoring reads the tree's files too).
        if blob == oid(0xd1) || blob == oid(0xd2) {
            counter.set(counter.get() + 1);
        }
        Ok(if blob == oid(0xd1) {
            b"pub const SPIN_WHEEL_LIMIT: u32 = 4; // spin_wheel_limit".to_vec()
        } else if blob == oid(0xd2) {
            b"fn zz_branch_only() {}".to_vec()
        } else {
            Vec::new()
        })
    }));
    let out = find(&o, &query("spin_wheel_limit", &["makepad"]));
    assert!(out.blocks[0].absent);
    let br = out.pointers.iter().find(|p| p.branch.is_some()).expect("a branch pointer");
    assert_eq!((br.count, br.commits, br.unit), (1, 0, PointerUnit::Files));
    assert_eq!(reads.get(), 2);
    // A matched file a local commit touched names that commit.
    let out = find(&o, &query("zz_branch_only", &["makepad"]));
    let br = out.pointers.iter().find(|p| p.branch.is_some()).expect("a branch pointer");
    assert_eq!((br.count, br.commits), (1, 1));
    assert_eq!(br.sample, format!("{} beta: clamp the turn harder", short(&oid(0xc2))));
    // Nothing absent: the changed files are never read.
    reads.set(0);
    let _ = find(&o, &query("spin", &["makepad"]));
    assert_eq!(reads.get(), 0);
}

/// The fixture's real sources by blob, for line scoring.
fn fixture_blobs() -> crate::BlobReader {
    Box::new(|blob: Oid| {
        Ok(match blob.0[0] {
            3 => include_str!("../../fixture/alpha/src/lib.rs").as_bytes().to_vec(),
            5 => include_str!("../../fixture/beta/src/lib.rs").as_bytes().to_vec(),
            7 => include_str!("../../fixture/docs/guide.md").as_bytes().to_vec(),
            _ => Vec::new(),
        })
    })
}

#[test]
fn line_scoring_reports_the_line_inside_an_item() {
    let mut o = opened(vec![makepad()], vec![]);
    o.blobs = Some(fixture_blobs());
    let b = &find(&o, &query("angle wrapping_add", &["makepad"])).blocks[0];
    let spin = b.hits.iter().find(|h| h.name == "Gizmo::spin").expect("spin");
    // Its handle points at the body line; the hit still spans the item.
    assert_eq!(format(&spin.handle), "alpha/src/lib.rs:16");
    assert_eq!((spin.lines.start, spin.lines.end), (14, 17));
}

#[test]
fn line_scoring_lifts_a_line_between_items() {
    let mut o = opened(vec![makepad()], vec![]);
    o.blobs = Some(fixture_blobs());
    // `use fixture_beta::clamp_turn;` sits between items: only the file chunk holds it.
    let b = &find(&o, &query("fixture_beta clamp_turn", &["makepad"])).blocks[0];
    let file = b.hits.iter().find(|h| h.kind == Kind::File && h.path == "alpha/src/lib.rs").expect("the file chunk");
    assert_eq!(format(&file.handle), "alpha/src/lib.rs:3");
    // Without a blob reader nothing is read and nothing moves.
    let o = opened(vec![makepad()], vec![]);
    let b = &find(&o, &query("fixture_beta clamp_turn", &["makepad"])).blocks[0];
    assert!(!b.hits.iter().any(|h| format(&h.handle) == "alpha/src/lib.rs:3"));
}

#[test]
fn line_scoring_leaves_one_word_scores_alone() {
    let mut with = opened(vec![makepad()], vec![]);
    with.blobs = Some(fixture_blobs());
    let without = opened(vec![makepad()], vec![]);
    let a = find(&with, &query("angle", &["makepad"]));
    let b = find(&without, &query("angle", &["makepad"]));
    let scores = |o: &FindOutput| o.blocks[0].hits.iter().map(|h| (h.name.clone(), h.score)).collect::<Vec<_>>();
    assert_eq!(scores(&a), scores(&b));
}

#[test]
fn plain_words_match_their_inflections() {
    assert_eq!(rank::variants("remotely"), ["remote"]);
    assert_eq!(rank::variants("guides"), ["guide"]);
    assert_eq!(rank::variants("batches"), ["batch"]);
    assert_eq!(rank::variants("entries"), ["entry"]);
    assert_eq!(rank::variants("resetting"), ["resett", "resette"]);
    assert_eq!(rank::variants("clamped"), ["clamp", "clampe"]);
    assert!(rank::variants("class").is_empty(), "no plural strip after s");
    assert!(rank::variants("spin").is_empty(), "short words stay");
    let o = opened(vec![makepad()], vec![]);
    // "spins" finds the chunks that say "spin" (the guide's heading says "Spinning"); its
    // matches now cover the word.
    let b = &find(&o, &query("gizmos spins", &["makepad"])).blocks[0];
    assert!(b.hits.iter().any(|h| h.name == "Gizmo::spin"), "{:?}", names(b));
}

#[test]
fn a_prefix_word_matches_identifiers_that_start_with_it() {
    use rank::{classify, Class};
    assert_eq!(classify("FOO_BAR_"), ("FOO_BAR".into(), Class::Prefix));
    assert_eq!(classify("`foo::`"), ("foo".into(), Class::Prefix));
    assert_eq!(classify("MAX_TURN"), ("MAX_TURN".into(), Class::Subject { call: false, macro_call: false }));
    let o = opened(vec![makepad()], vec![]);
    // SPIN_TABLE and spin_label start with SPIN_ (folded alike; the generated table ranks
    // down); a chunk that only says "spin" as a word does not count.
    let b = &find(&o, &query("SPIN_", &["makepad"])).blocks[0];
    assert!(!b.absent);
    assert_eq!(names(b), ["spin_label", "SPIN_TABLE"]);
    assert!(!b.hits.iter().any(|h| h.name == "Gizmo::spin"), "{:?}", names(b));
    let b = &find(&o, &query("where is MAX_ used", &["makepad"])).blocks[0];
    assert!(b.hits.iter().take(3).any(|h| h.name == "MAX_TURN"), "{:?}", names(b));
    // A prefix whose parts occur nowhere is absent, named as written.
    let b = &find(&o, &query("WOMBAT_YODEL_", &["makepad"])).blocks[0];
    assert_eq!(b.absent_terms, ["WOMBAT_YODEL_"]);
}

#[test]
fn a_prefix_matches_the_start_of_identifiers_only() {
    let m = "makepad";
    let chunks = vec![
        chunk(m, C { kind: Kind::Fn, path: "a/src/lib.rs", lines: (1, 3), name: "starts", package: A, sig: "fn starts()", doc: "", body: "let r = SPIN_RATE_MAX;", idents: &["starts"], blob: 21 }),
        chunk(m, C { kind: Kind::Fn, path: "a/src/lib.rs", lines: (5, 7), name: "inside", package: A, sig: "fn inside()", doc: "", body: "let r = REVERSE_SPIN_RATE;", idents: &["inside"], blob: 21 }),
        chunk(m, C { kind: Kind::Fn, path: "a/src/lib.rs", lines: (9, 11), name: "apart", package: A, sig: "fn apart()", doc: "", body: "spin at a rate", idents: &["apart"], blob: 21 }),
    ];
    let o = opened(vec![open("makepad", Some(rev(0x9a)), chunks, None)], vec![]);
    let b = &find(&o, &query("SPIN_RATE_", &["makepad"])).blocks[0];
    assert_eq!(names(b), ["starts"]);
    assert!(!b.absent);
    let b = &find(&o, &query("RATE_", &["makepad"])).blocks[0];
    assert!(b.absent, "no identifier starts with rate_");
}

#[test]
fn a_query_passed_as_one_argument_reads_as_its_words() {
    let o = opened(vec![makepad(), history()], vec![]);
    let words = |q: &str, circles: &[&str]| find(&o, &query(q, circles));
    let mut one = query("", &["makepad", "makepad:history"]);
    one.words = vec!["why does the gizmo use every SPIN_ table".into()];
    let whole = Engine::default().find(&o, &one).unwrap();
    let split = words("why does the gizmo use every SPIN_ table", &["makepad", "makepad:history"]);
    for (a, b) in whole.blocks.iter().zip(&split.blocks) {
        assert_eq!(a.absent_terms, b.absent_terms);
        assert_eq!(names(a), names(b));
    }
    // A present prefix in a sentence is never absent in the tree.
    assert!(!split.blocks[0].absent, "{:?}", split.blocks[0].absent_terms);
    // History names only the shaped word it lacks, never the sentence.
    assert_eq!(split.blocks[1].absent_terms, ["SPIN_"]);
    // A missing prefix in a sentence is named alone.
    let b = &words("how are FOO_BAR_ values used", &["makepad"]).blocks[0];
    assert_eq!(b.absent_terms, ["FOO_BAR_"]);
}

// Phase 1d: the cross-review's query findings, one regression test each.

#[test]
fn a_notes_hit_handle_reads_back_one_shot() {
    // F1: a source labelled like a bare circle name (`makepad/local`) must not read as that
    // circle's prefix when the notes circle is the query's first.
    let mut chunks = notes_chunks();
    chunks.push(note("makepad/local/lanes.md", "makepad/local", Kind::Note, (1, 3), "Lanes", "lanes split the work", 9));
    let o = opened(vec![open("notes:topic-a", None, chunks, None), makepad()], vec![]);
    let out = find(&o, &query("lanes", &["notes:topic-a"]));
    let hit = &out.blocks[0].hits[0];
    let text = format(&hit.handle);
    assert!(text.starts_with("notes:topic-a/makepad/local/lanes.md:"), "{text}");
    let back = parse(&text, &[CircleName("makepad".into())]).unwrap();
    assert_eq!(back, hit.handle);
    let found = super::handle::resolve(&o, &back).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(view(&o.open[found[0].circle], found[0].at).name, "Lanes");
    // Tree hits of the first circle stay unprefixed.
    let tree = find(&o, &query("spin", &["makepad"]));
    assert!(tree.blocks[0].hits.iter().all(|h| h.handle.circle.is_none()));
}

#[test]
fn a_filter_with_no_hits_claims_no_absence() {
    // F2: absence is judged before `--kind/--crate/--path` and a derived circle's selector.
    let o = opened(vec![makepad()], vec![]);
    let mut q = query("MAX_TURN", &["makepad"]);
    q.paths = vec!["beta/".into()];
    let out = find(&o, &q);
    let b = &out.blocks[0];
    assert_eq!(b.total, 0);
    assert!(!b.absent, "{:?}", b.absent_terms);
    assert_eq!(b.under, ["--path beta/"]);
    let text = Renderer.text(&Output::Find(out.clone()));
    assert_eq!(text.lines().next().unwrap(), format!("makepad @{} · no hits under --path beta/", short(&oid(0x9a))));
    let out_find = Output::Find(out);
    let json = Renderer.json_lines(&Renderer.records(&out_find));
    assert!(json.lines().next().unwrap().contains(r#""under":["--path beta/"]"#), "{json}");
    let mut q = query("MAX_TURN", &[]);
    q.circles = vec![CircleExpr::Select { of: Box::new(CircleExpr::Circle(CircleName("makepad".into()))), by: Selector::PathPrefix("docs/".into()) }];
    q.kinds = vec![KindFilter::Kind(Kind::Struct)];
    let b = &find(&o, &q).blocks[0];
    assert_eq!(b.total, 0);
    assert!(!b.absent, "{:?}", b.absent_terms);
    assert_eq!(b.under, ["--kind struct", "--path docs/"]);
    // A word the circle lacks is still absent under a filter.
    let mut q = query("MAX_TURN NoSuchWidget", &["makepad"]);
    q.paths = vec!["beta/".into()];
    let out = find(&o, &q);
    assert_eq!(out.blocks[0].absent_terms, ["NoSuchWidget"]);
    assert!(Renderer.text(&Output::Find(out)).starts_with(&format!("makepad @{} · no hits under --path beta/ · absent: NoSuchWidget", short(&oid(0x9a)))));
    // Unfiltered, nothing matching is an absence.
    assert!(Renderer.text(&Output::Find(find(&o, &query("NoSuchWidget", &["makepad"])))).contains("0 definitions, 0 uses · absent: NoSuchWidget"));
}

#[test]
fn a_word_deleted_in_the_worktree_is_absent_beside_matching_words() {
    // F3: `SPIN_TABLE` lives only in a file the worktree deleted (shadowed, no overlay chunks).
    let mut c = makepad();
    c.shadowed = vec!["alpha/src/tables.rs".into()];
    let o = opened(vec![c], vec![]);
    let b = &find(&o, &query("SPIN_TABLE", &["makepad"])).blocks[0];
    assert_eq!(b.absent_terms, ["SPIN_TABLE"]);
    let b = &find(&o, &query("SPIN_TABLE clamp_turn", &["makepad"])).blocks[0];
    assert!(b.total > 0);
    assert_eq!(b.absent_terms, ["SPIN_TABLE"]);
    // A prefix whose only terms are shadowed is absent too (`spin_label` lives in the splash).
    let b = &find(&o, &query("SPIN_ clamp_turn", &["makepad"])).blocks[0];
    assert!(b.absent_terms.is_empty(), "{:?}", b.absent_terms);
    let mut c = makepad();
    c.shadowed = vec!["alpha/src/tables.rs".into(), "ui/demo.splash".into()];
    let o = opened(vec![c], vec![]);
    let b = &find(&o, &query("SPIN_ clamp_turn", &["makepad"])).blocks[0];
    assert_eq!(b.absent_terms, ["SPIN_"]);
    // History: a word only in a commit the revision does not reach is absent there.
    let mut h = history();
    let mut c3 = commit(3, "ZorbleThing on another branch", &["alpha/src/lib.rs"]);
    c3.fields.body = "ZorbleThing on another branch".into();
    let mut chunks = vec![commit(1, "alpha: gizmos spin by a clamped step", &["alpha/src/lib.rs"]), c3];
    chunks.push(commit(2, "beta: clamp the turn harder", &["beta/src/lib.rs"]));
    h.segments = vec![Box::new(MemSegment::new("makepad:history", chunks))];
    let o = opened(vec![h], vec![]);
    let b = &find(&o, &query("ZorbleThing gizmos", &["makepad:history"])).blocks[0];
    assert!(b.total > 0);
    assert_eq!(b.absent_terms, ["ZorbleThing"]);
}

#[test]
fn a_limit_below_a_group_is_honoured() {
    // F4.
    let o = opened(vec![makepad()], vec![]);
    for n in 1..=4 {
        let mut q = query("spin", &["makepad"]);
        q.limit = n;
        let b = &find(&o, &q).blocks[0];
        assert!(b.total >= 4);
        assert_eq!(b.hits.len(), n, "-n {n}");
    }
}

#[test]
fn rank_fusion_lists_each_absent_term_once() {
    // F5: two blocks sharing two absent terms.
    let o = opened(vec![makepad(), notes()], vec![]);
    let mut q = query("zz_nope_a zz_nope_b spin", &[]);
    q.circles = vec![CircleExpr::Union {
        of: vec![CircleExpr::Circle(CircleName("makepad".into())), CircleExpr::Circle(CircleName("notes:topic-a".into()))],
        fuse: Fuse::Rank,
    }];
    let b = &find(&o, &q).blocks[0];
    assert!(b.total > 0);
    assert_eq!(b.absent_terms, ["zz_nope_a", "zz_nope_b"]);
}

#[test]
fn def_counts_the_definitions_it_shows() {
    // XR §4: `def` printed the shown pool's size beside the circle's definition count.
    let o = opened(vec![makepad()], vec![]);
    let mut q = query("spin", &["makepad"]);
    q.mode = Mode::Def;
    let b = &find(&o, &q).blocks[0];
    assert_eq!(b.hits.len() as u64, b.total);
    assert_eq!(b.definitions, b.total, "{} of {} · {} definitions", b.hits.len(), b.total, b.definitions);
}
