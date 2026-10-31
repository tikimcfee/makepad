//! Store tests over the fixture, with a fake tokenizer and fake extraction, and a test where
//! several processes build and read at once.

use super::manifest::{self, MERGE_FACTOR};
use super::read::Segment;
use super::*;
use crate::*;
use makepad_git::oid::hash_object;
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------------------------
// Fakes

struct FakeTokens;

fn words(s: &str) -> Vec<&str> {
    s.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).collect()
}

impl Tokenize for FakeTokens {
    fn chunk_terms(&self, c: &Chunk) -> FieldTerms {
        let mut tf: HashMap<(Field, TermForm, String), u32> = HashMap::new();
        let mut lengths = [0u32; 5];
        let f = &c.fields;
        for (i, (field, text)) in [(Field::Name, &f.name), (Field::Path, &f.path), (Field::Sig, &f.sig), (Field::Doc, &f.doc), (Field::Body, &f.body)].into_iter().enumerate() {
            for w in words(text) {
                lengths[i] += 1;
                *tf.entry((field, TermForm::Exact, w.to_string())).or_default() += 1;
                *tf.entry((field, TermForm::Folded, w.to_lowercase())).or_default() += 1;
                for p in w.split('_').filter(|p| !p.is_empty() && *p != w) {
                    *tf.entry((field, TermForm::Part, p.to_lowercase())).or_default() += 1;
                }
            }
        }
        let mut sorted: Vec<((Field, TermForm, String), u32)> = tf.into_iter().collect();
        sorted.sort_by(|a, b| a.0 .2.cmp(&b.0 .2));
        let mut terms = FieldTerms::default();
        for ((field, form, text), tf) in sorted {
            terms.push(field, form, &text, tf);
        }
        terms.lengths = lengths;
        terms
    }
    fn query_terms(&self, words: &[String]) -> Vec<QueryTerm> {
        words.iter().enumerate().map(|(i, w)| QueryTerm { form: TermForm::Folded, text: w.to_lowercase(), word: i as u16 }).collect()
    }
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixture")
}

fn files_under(dir: &Path, rel: &str, out: &mut Vec<(String, Vec<u8>)>) {
    let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().into_string().unwrap();
        let rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        if e.file_type().unwrap().is_dir() {
            files_under(&e.path(), &rel, out);
        } else {
            out.push((rel, fs::read(e.path()).unwrap()));
        }
    }
}

fn blob(bytes: &[u8]) -> Oid {
    Oid(*hash_object("blob", bytes).as_bytes())
}

fn circle(name: &str) -> CircleName {
    CircleName(name.to_string())
}

fn chunk(circle: &CircleName, kind: Kind, path: &str, name: &str) -> Chunk {
    Chunk {
        kind,
        circle: circle.clone(),
        path: path.to_string(),
        span: Span::default(),
        lines: Lines::default(),
        name: name.to_string(),
        label_len: 0,
        gist: String::new(),
        parent: None,
        fields: Fields { name: name.to_string(), path: path.to_string(), ..Fields::default() },
        keys: Keys { paths: vec![path.to_string()], ..Keys::default() },
        flags: Flags::default(),
        detail: Detail::None,
    }
}

/// A fake extraction of the fixture as the `makepad` tree circle: a file chunk per file, an fn
/// chunk per `fn` line under it, a crate card per `Cargo.toml`; `excluded/` and `notes/` left out.
fn tree_chunks(c: &CircleName) -> Vec<Chunk> {
    let mut files = Vec::new();
    files_under(&fixture(), "", &mut files);
    let mut out = Vec::new();
    for (path, bytes) in files.iter().filter(|(p, _)| !p.starts_with("excluded/") && !p.starts_with("notes/")) {
        let text = String::from_utf8_lossy(bytes);
        let package = path.split_once('/').map(|(d, _)| format!("fixture-{d}"));
        let mut f = chunk(c, Kind::File, path, path.rsplit('/').next().unwrap());
        f.fields.body = text.to_string();
        f.fields.doc = text.lines().filter_map(|l| l.strip_prefix("//!")).collect::<Vec<_>>().join("\n");
        f.gist = f.fields.doc.lines().next().unwrap_or("").trim().to_string();
        f.keys.blob = Some(blob(bytes));
        f.keys.package = package.clone();
        f.lines = Lines { start: 1, end: text.lines().count() as u32 };
        f.span = Span { lo: 0, hi: bytes.len() as u32 };
        let file_id = out.len() as u32;
        out.push(f);
        for (i, line) in text.lines().enumerate() {
            let Some(rest) = line.trim_start().strip_prefix("pub fn ").or_else(|| line.trim_start().strip_prefix("fn ")) else { continue };
            let name: String = rest.chars().take_while(|ch| ch.is_alphanumeric() || *ch == '_').collect();
            let mut item = chunk(c, Kind::Fn, path, &name);
            item.parent = Some(file_id);
            item.lines = Lines { start: i as u32 + 1, end: i as u32 + 3 };
            item.fields.sig = line.trim().to_string();
            item.keys.idents = vec![name.clone()];
            item.keys.blob = Some(blob(bytes));
            item.keys.package = package.clone();
            item.flags = Flags { public: line.contains("pub "), definition: true, ..Flags::default() };
            out.push(item);
        }
        if path.ends_with("Cargo.toml") && path.contains('/') {
            let dir = path.trim_end_matches("/Cargo.toml");
            let description = text.lines().find_map(|l| l.strip_prefix("description = \"")).map(|d| d.trim_end_matches('"').to_string()).unwrap_or_default();
            let name = format!("fixture-{dir}");
            let mut card = chunk(c, Kind::Crate, dir, &name);
            card.keys.package = Some(name.clone());
            card.gist = description.clone();
            card.fields.doc = description.clone();
            card.detail = Detail::Crate(CrateCard {
                dir: dir.to_string(),
                description,
                deps: if text.contains("fixture-beta") { vec![("fixture-beta".into(), String::new()), ("fixture-beta".into(), "cfg(windows)".into())] } else { Vec::new() },
                dependents: if dir == "beta" { vec!["fixture-alpha".into()] } else { Vec::new() },
                apps: if dir == "alpha" { vec!["spinner".into()] } else { Vec::new() },
                targets: vec!["lib".into()],
                rs_files: 2,
                modules: vec!["tables".into()],
                private_members: dir == "beta",
            });
            out.push(card);
        }
    }
    out
}

/// A fake extraction of the fixture's notes as `notes:topic-a`: Markdown split before every
/// heading line, other files whole, binary files path only; `path:line` citations as edges.
fn notes_chunks(c: &CircleName) -> Vec<Chunk> {
    let mut out = Vec::new();
    for (dir, label) in [("notes/topic-a", "topic-a"), ("notes/local-a", "fixture/notes/local-a")] {
        let mut files = Vec::new();
        files_under(&fixture().join(dir), "", &mut files);
        for (rel, bytes) in files {
            let path = format!("{label}/{rel}");
            let binary = bytes.iter().take(8000).any(|&b| b == 0);
            let text = String::from_utf8_lossy(&bytes).to_string();
            let pieces: Vec<String> = if binary {
                vec![String::new()]
            } else if rel.ends_with(".md") {
                let mut pieces = vec![String::new()];
                for line in text.split_inclusive('\n') {
                    if line.starts_with('#') && !pieces.last().unwrap().is_empty() {
                        pieces.push(String::new());
                    }
                    pieces.last_mut().unwrap().push_str(line);
                }
                pieces.into_iter().filter(|p| !p.trim().is_empty()).collect()
            } else {
                vec![text.clone()]
            };
            for piece in pieces {
                let kind = if rel.ends_with(".md") { Kind::Note } else { Kind::NoteFile };
                let mut n = chunk(c, kind, &path, piece.lines().next().unwrap_or(&rel));
                n.fields.path = rel.clone();
                n.label_len = label.len() as u16;
                n.fields.body = piece.clone();
                n.keys.blob = Some(blob(&bytes));
                n.flags.binary = binary;
                for w in piece.split_whitespace() {
                    if let Some((p, l)) = w.trim_end_matches(['.', ',']).rsplit_once(':') {
                        if let Ok(line) = l.parse::<u32>() {
                            n.keys.edges.push(Edge { to: circle("makepad"), key: Key::Path { path: p.to_string(), line: Some(line) } });
                        }
                    }
                }
                if piece.contains("Gizmo::spin") {
                    n.keys.edges.push(Edge { to: circle("makepad"), key: Key::Ident("Gizmo::spin".into()) });
                }
                out.push(n);
            }
        }
    }
    out
}

fn commit_chunks(c: &CircleName, tag: &str, n: usize) -> Vec<Chunk> {
    let mut out = Vec::new();
    for i in 0..n {
        let oid = blob(format!("{tag}-{i}").as_bytes());
        let mut k = chunk(c, Kind::Commit, "", &format!("commit {tag} {i}: spin the gizmo"));
        k.keys.paths = vec!["alpha/src/lib.rs".into()];
        k.fields.path = "alpha/src/lib.rs".into();
        k.fields.body = format!("history body {tag} common_word");
        k.keys.commit = Some(oid);
        k.detail = Detail::Commit(CommitInfo { time: 1_700_000_000 + i as i64, parents: vec![blob(tag.as_bytes())] });
        let parent = out.len() as u32;
        out.push(k);
        let mut b = chunk(c, Kind::Bullet, "", &format!("bullet {tag} {i}"));
        b.parent = Some(parent);
        b.keys.paths = Vec::new();
        b.keys.commit = Some(oid);
        out.push(b);
    }
    out
}

static ROOT_SEQ: AtomicU64 = AtomicU64::new(0);

/// A scratch index root, removed when dropped.
struct Root(PathBuf);

impl Root {
    fn new() -> Root {
        let p = std::env::temp_dir().join(format!("makepad-repo-index-store-{}-{}", std::process::id(), ROOT_SEQ.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Root(p)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        if self.0.file_name().is_some_and(|n| n.to_string_lossy().starts_with("makepad-repo-index-store-")) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

fn tree_key(c: &CircleName, tree: &str) -> SegmentKey {
    SegmentKey { repo: Some(RepoId(0xfeed)), circle: c.clone(), source: SourceStamp::Tree { tree: blob(tree.as_bytes()) }, def_hash: 7, indexer: INDEXER_VERSION }
}

fn history_key(c: &CircleName, tip: &str) -> SegmentKey {
    SegmentKey { repo: Some(RepoId(0xfeed)), circle: c.clone(), source: SourceStamp::History { tips: vec![blob(tip.as_bytes())] }, def_hash: 9, indexer: INDEXER_VERSION }
}

fn write_segment(store: &FsStore, key: &SegmentKey, chunks: &[Chunk]) -> SegmentId {
    let mut sink = store.new_segment(key).unwrap();
    for (i, c) in chunks.iter().enumerate() {
        assert_eq!(sink.add(c, &FakeTokens.chunk_terms(c)).unwrap(), ChunkId(i as u32));
    }
    sink.finish().unwrap()
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    v.sort();
    v
}

// ---------------------------------------------------------------------------------------------
// Segments

/// Every stored field, key, detail, term and statistic reads back as written.
#[test]
fn segment_round_trip() {
    let root = Root::new();
    let store = FsStore::new(root.0.clone());
    let c = circle("makepad");
    let chunks = tree_chunks(&c);
    assert!(chunks.len() > 10);
    let key = tree_key(&c, "t1");
    let id = write_segment(&store, &key, &chunks);
    let seg = store.segment(key.repo, &c, &id).unwrap();
    seg.verify().unwrap();
    assert_eq!(seg.key(), &key);
    assert_eq!(seg.chunk_count(), chunks.len() as u32);
    assert_eq!(seg.id(), id);

    let mut expect: HashMap<(TermForm, String), Vec<Posting>> = HashMap::new();
    let mut lengths = [0u64; 5];
    for (i, ch) in chunks.iter().enumerate() {
        let terms = FakeTokens.chunk_terms(ch);
        for (s, l) in lengths.iter_mut().zip(terms.lengths) {
            *s += l as u64;
        }
        let mut tf: HashMap<(TermForm, String), [u32; 5]> = HashMap::new();
        for t in terms.iter() {
            tf.entry((t.form, t.text.to_string())).or_default()[segment::field_index(t.field)] += t.tf;
        }
        for (k, tf) in tf {
            expect.entry(k).or_default().push(Posting { chunk: ChunkId(i as u32), tf });
        }
    }
    assert_eq!(seg.stats(), CircleStats { chunks: chunks.len() as u64, field_lengths: lengths });
    for ((form, text), postings) in &expect {
        assert_eq!(seg.df(*form, text), postings.len() as u32, "df of {text}");
        assert_eq!(&seg.postings(*form, text).collect::<Vec<_>>(), postings, "postings of {text}");
    }
    assert!(chunks.iter().all(|c| !c.path.starts_with("excluded/")));
    assert_eq!(seg.postings(TermForm::Folded, "no-such-term").count(), 0);
    assert!(expect.contains_key(&(TermForm::Part, "turn".to_string())), "the fake splits clamp_turn");

    for (i, ch) in chunks.iter().enumerate() {
        let id = ChunkId(i as u32);
        let v = seg.chunk(id);
        assert_eq!((v.kind, v.path, v.name, v.gist, v.sig), (ch.kind, ch.path.as_str(), ch.name.as_str(), ch.gist.as_str(), ch.fields.sig.as_str()));
        assert_eq!((v.span, v.lines, v.flags), (ch.span, ch.lines, ch.flags));
        assert_eq!(v.parent, ch.parent.map(ChunkId));
        assert_eq!(v.package, ch.keys.package.as_deref().unwrap_or(""));
        assert_eq!((v.blob, v.commit), (ch.keys.blob, ch.keys.commit));
        assert_eq!(v.field_lengths, FakeTokens.chunk_terms(ch).lengths);
        let k = seg.keys(id);
        assert_eq!((&k.paths, &k.idents, &k.package, k.blob, k.commit), (&ch.keys.paths, &ch.keys.idents, &ch.keys.package, ch.keys.blob, ch.keys.commit));
        assert_eq!(seg.record_text(id), None, "tree chunks keep no text");
        match (seg.detail(id), &ch.detail) {
            (Detail::None, Detail::None) => {}
            (Detail::Crate(a), Detail::Crate(b)) => {
                assert_eq!((&a.dir, &a.description, &a.deps, &a.dependents, &a.apps), (&b.dir, &b.description, &b.deps, &b.dependents, &b.apps));
                assert_eq!((&a.targets, a.rs_files, &a.modules, a.private_members), (&b.targets, b.rs_files, &b.modules, b.private_members));
            }
            (a, b) => panic!("detail {a:?} read back for {b:?}"),
        }
    }

    // the key table
    let with = |k: Key| seg.chunks_with(&k).into_iter().map(|c| c.0).collect::<Vec<_>>();
    let all = |pred: &dyn Fn(&Chunk) -> bool| chunks.iter().enumerate().filter(|(_, c)| pred(c)).map(|(i, _)| i as u32).collect::<Vec<_>>();
    assert_eq!(with(Key::Path { path: "alpha/src/lib.rs".into(), line: None }), all(&|c| c.keys.paths.iter().any(|p| p == "alpha/src/lib.rs")));
    let spin = chunks.iter().position(|c| c.name == "spin").unwrap() as u32;
    let line = chunks[spin as usize].lines.start;
    assert!(with(Key::Path { path: "alpha/src/lib.rs".into(), line: Some(line) }).contains(&spin));
    assert!(!with(Key::Path { path: "alpha/src/lib.rs".into(), line: Some(line + 50) }).contains(&spin));
    assert_eq!(with(Key::Ident("spin".into())), vec![spin]);
    assert_eq!(with(Key::Package("fixture-alpha".into())), all(&|c| c.keys.package.as_deref() == Some("fixture-alpha")));
    let lib = blob(&fs::read(fixture().join("alpha/src/lib.rs")).unwrap());
    assert_eq!(with(Key::Blob(lib)), all(&|c| c.keys.blob == Some(lib)));
    assert!(with(Key::Ident("nothing_defines_this".into())).is_empty());
    assert!(seg.cited_by(&Key::Path { path: "alpha/src/lib.rs".into(), line: None }).is_empty());
}

/// Notes keep their record text (empty for a binary file), edges and reverse edges; copies
/// share a blob key.
#[test]
fn notes_segment_keeps_text_and_edges() {
    let root = Root::new();
    let store = FsStore::new(root.0.clone());
    let c = circle("notes:topic-a");
    let chunks = notes_chunks(&c);
    let key = SegmentKey { repo: None, circle: c.clone(), source: SourceStamp::Notes { content: 42 }, def_hash: 1, indexer: INDEXER_VERSION };
    let id = write_segment(&store, &key, &chunks);
    assert!(root.0.join("notes-topic-a").join(&id.0).is_file(), "notes circles live at the root, `:` as `-`");
    let seg = store.segment(None, &c, &id).unwrap();
    for (i, ch) in chunks.iter().enumerate() {
        assert_eq!(seg.chunk(ChunkId(i as u32)).label_len, ch.label_len);
        assert_eq!(seg.chunk(ChunkId(i as u32)).path_in_source(), ch.fields.path);
        assert_eq!(seg.record_text(ChunkId(i as u32)), Some(ch.fields.body.as_str()));
        assert_eq!(seg.keys(ChunkId(i as u32)).edges, ch.keys.edges);
    }
    let bin = chunks.iter().position(|c| c.path.ends_with("pixel.bin")).unwrap();
    assert_eq!(seg.record_text(ChunkId(bin as u32)), Some(""));
    assert!(seg.chunk(ChunkId(bin as u32)).flags.binary);

    let citing: BTreeSet<u32> = chunks.iter().enumerate().filter(|(_, c)| c.fields.body.contains("alpha/src/lib.rs:15")).map(|(i, _)| i as u32).collect();
    assert_eq!(citing.len(), 2, "PLAN.md and its byte-identical COPY.md");
    let cited = |k: Key| seg.cited_by(&k).into_iter().map(|(c, e)| (c.0, e)).collect::<Vec<_>>();
    let any_line = cited(Key::Path { path: "alpha/src/lib.rs".into(), line: None });
    assert_eq!(any_line.iter().map(|(c, _)| *c).collect::<BTreeSet<_>>(), citing);
    assert!(any_line.iter().all(|(_, e)| e.to == circle("makepad") && e.key == Key::Path { path: "alpha/src/lib.rs".into(), line: Some(15) }));
    assert_eq!(cited(Key::Path { path: "alpha/src/lib.rs".into(), line: Some(15) }).len(), 2);
    assert!(cited(Key::Path { path: "alpha/src/lib.rs".into(), line: Some(16) }).is_empty());
    assert_eq!(cited(Key::Ident("Gizmo::spin".into())).len(), 2);

    let plan = blob(&fs::read(fixture().join("notes/topic-a/PLAN.md")).unwrap());
    let copies: BTreeSet<String> = seg.chunks_with(&Key::Blob(plan)).into_iter().map(|c| seg.chunk(c).path.to_string()).collect();
    assert_eq!(copies, BTreeSet::from(["topic-a/COPY.md".to_string(), "topic-a/PLAN.md".to_string()]));
    let spins: BTreeSet<String> = chunks.iter().filter(|c| c.path.ends_with("spin.rs")).map(|c| c.path.clone()).collect();
    assert_eq!(spins.len(), 2, "label prefixes keep same-named files apart");
}

/// A segment is named by its content: the same chunks give the same file, once, and no tmp
/// file is left behind.
#[test]
fn segments_are_content_named() {
    let root = Root::new();
    let store = FsStore::new(root.0.clone());
    let c = circle("makepad");
    let key = tree_key(&c, "t1");
    let chunks = tree_chunks(&c);
    let a = write_segment(&store, &key, &chunks);
    let b = write_segment(&store, &key, &chunks);
    assert_eq!(a, b);
    let dir = store.circle_dir(key.repo, &c).unwrap();
    assert_eq!(names_in(&dir), vec![a.0.clone()]);
    let mut changed = chunks.clone();
    changed[0].gist.push('!');
    let other = write_segment(&store, &key, &changed);
    assert_ne!(a, other);
    let hex = a.0.strip_prefix("seg-").unwrap().strip_suffix(".mpx").unwrap();
    let bytes = fs::read(dir.join(&a.0)).unwrap();
    assert_eq!(hash_object("blob", &bytes[..bytes.len() - segment::FOOTER_LEN]).to_hex(), hex);
}

#[test]
fn corruption_is_caught() {
    let root = Root::new();
    let store = FsStore::new(root.0.clone());
    let c = circle("makepad");
    let key = tree_key(&c, "t1");
    let id = write_segment(&store, &key, &tree_chunks(&c));
    let dir = store.circle_dir(key.repo, &c).unwrap();
    let good = fs::read(dir.join(&id.0)).unwrap();

    // a flipped byte inside the string bytes: the structure holds, the hash does not
    let mut flipped = good.clone();
    let strings = segment::HEADER_LEN + segment::TABLE_ENTRY_LEN * 2;
    let at = u64::from_le_bytes(good[strings + 8..strings + 16].try_into().unwrap()) as usize + 1;
    flipped[at] ^= 0x55;
    let path = root.0.join(&id.0);
    fs::write(&path, &flipped).unwrap();
    assert!(matches!(Segment::open(&path).unwrap().verify(), Err(Error::Corrupt { .. })));

    // truncated
    fs::write(&path, &good[..good.len() / 2]).unwrap();
    assert!(matches!(Segment::open(&path), Err(Error::Corrupt { .. })));

    // a name that is not its content
    let wrong = root.0.join(format!("seg-{}.mpx", "0".repeat(40)));
    fs::write(&wrong, &good).unwrap();
    assert!(matches!(Segment::open(&wrong), Err(Error::Corrupt { .. })));

    // another circle's segment is refused
    let other = circle("other");
    assert!(matches!(store.segment(key.repo, &other, &id), Err(Error::Io { .. })));
    let odir = store.circle_dir(key.repo, &other).unwrap();
    fs::create_dir_all(&odir).unwrap();
    fs::write(odir.join(&id.0), &good).unwrap();
    assert!(matches!(store.segment(key.repo, &other, &id), Err(Error::Corrupt { .. })));
    assert!(segment_path(&dir, &SegmentId("../seg-x.mpx".into())).is_err());
}

#[test]
fn sink_refuses_foreign_chunks_and_late_parents() {
    let root = Root::new();
    let store = FsStore::new(root.0.clone());
    let c = circle("makepad");
    let key = tree_key(&c, "t1");
    let mut sink = store.new_segment(&key).unwrap();
    let foreign = chunk(&circle("notes:topic-a"), Kind::Note, "topic-a/PLAN.md", "x");
    assert!(sink.add(&foreign, &FieldTerms::default()).is_err(), "a chunk of another circle never enters this one");
    let mut unlabelled = chunk(&c, Kind::Note, "topic-a/PLAN.md", "x");
    unlabelled.label_len = 4;
    assert!(sink.add(&unlabelled, &FieldTerms::default()).is_err(), "a label ends at a `/`");
    let mut late = chunk(&c, Kind::Fn, "a.rs", "f");
    late.parent = Some(0);
    assert!(sink.add(&late, &FieldTerms::default()).is_err(), "chunk 0 cannot be its own parent");
    assert!(circle_dir_name(&circle("../escape")).is_err());
    assert_eq!(circle_dir_name(&circle("makepad:history")).unwrap(), "makepad-history");
}

// ---------------------------------------------------------------------------------------------
// Bases

#[test]
fn bases_are_built_once_per_key() {
    let root = Root::new();
    let store = FsStore::new(root.0.clone());
    let c = circle("makepad");
    let key = tree_key(&c, "t1");
    let chunks = tree_chunks(&c);
    assert!(store.open_base(&key).unwrap().is_none());
    let mut fills = 0;
    let mut fill = |sink: &mut dyn SegmentSink| {
        fills += 1;
        for ch in &chunks {
            sink.add(ch, &FakeTokens.chunk_terms(ch))?;
        }
        Ok(())
    };
    let a = store.build_base(&key, &mut fill).unwrap();
    let b = store.build_base(&key, &mut fill).unwrap();
    assert_eq!((a.key(), a.chunk_count()), (&key, chunks.len() as u32));
    assert_eq!(b.chunk_count(), chunks.len() as u32);
    assert!(store.open_base(&key).unwrap().is_some());
    assert!(store.open_base(&tree_key(&c, "t2")).unwrap().is_none(), "another tree is another base");
    let mut other_def = key.clone();
    other_def.def_hash += 1;
    assert!(store.open_base(&other_def).unwrap().is_none(), "another circle definition is another base");

    // a failed fill publishes nothing
    let t3 = tree_key(&c, "t3");
    assert!(store.build_base(&t3, &mut |_| Err(Error::Git("boom".into()))).is_err());
    assert!(store.open_base(&t3).unwrap().is_none());

    // a dangling pointer is rebuilt
    let dir = store.circle_dir(key.repo, &c).unwrap();
    let seg_name = fs::read_to_string(dir.join(base_name(&key))).unwrap().trim().to_string();
    drop((a, b));
    fs::remove_file(dir.join(&seg_name)).unwrap();
    assert!(store.open_base(&key).unwrap().is_none());
    store.build_base(&key, &mut fill).unwrap();
    assert!(store.open_base(&key).unwrap().is_some());
    assert_eq!(fills, 2, "built once, then once more after its segment was lost");
}

// ---------------------------------------------------------------------------------------------
// Manifests

#[test]
fn manifests_count_up_and_sum() {
    let root = Root::new();
    let store = FsStore::new(root.0.clone());
    let c = circle("makepad:history");
    let repo = Some(RepoId(0xfeed));
    assert!(store.manifest(repo, &c).unwrap().is_none());
    let mut ids = Vec::new();
    for tag in ["a", "b", "c"] {
        let id = write_segment(&store, &history_key(&c, tag), &commit_chunks(&c, tag, 3));
        let m = store.commit(repo, &c, ManifestChange::Add(id.clone())).unwrap();
        ids.push(id);
        assert_eq!(m.version, ids.len() as u64);
        assert_eq!(m.segments, ids);
    }
    let m = store.manifest(repo, &c).unwrap().unwrap();
    assert_eq!(m.stats.chunks, 18);
    assert_eq!(m.sources.len(), 3);
    assert_eq!(m.circle, c);
    assert!(m.writer.starts_with(&format!("pid {}", std::process::id())));
    let sum: u64 = m.segments.iter().map(|id| store.open_segment(repo, &c, id).unwrap().stats().field_lengths[0]).sum();
    assert_eq!(m.stats.field_lengths[0], sum);

    // adding a segment already there commits nothing
    assert_eq!(store.commit(repo, &c, ManifestChange::Add(ids[0].clone())).unwrap().version, 3);
    // a segment of another circle definition is refused
    let mut other = history_key(&c, "d");
    other.def_hash += 1;
    let odd = write_segment(&store, &other, &commit_chunks(&c, "d", 1));
    assert!(store.commit(repo, &c, ManifestChange::Add(odd.clone())).is_err());

    // a corrupt newest manifest is skipped by readers, and writers go above it
    let dir = store.circle_dir(repo, &c).unwrap();
    fs::write(manifest::path_of(&dir, 4), "makepad-index manifest 1\ncircle makepad:history\nversion 4\n").unwrap();
    assert_eq!(store.manifest(repo, &c).unwrap().unwrap().version, 3);
    let r = store.commit(repo, &c, ManifestChange::Replace(odd.clone())).unwrap();
    assert_eq!((r.version, r.segments.clone()), (5, vec![odd.clone()]));
    assert_eq!(store.manifest(repo, &c).unwrap().unwrap().segments, vec![odd]);

    // a merge whose input is gone is abandoned
    let out = write_segment(&store, &history_key(&c, "e"), &commit_chunks(&c, "e", 1));
    let m = store.commit(repo, &c, ManifestChange::Merge { inputs: vec![ids[0].clone()], output: out }).unwrap();
    assert_eq!(m.version, 5);
}

/// A full tier of history segments is merged into one holding every chunk and posting.
#[test]
fn full_tiers_merge() {
    let root = Root::new();
    let store = FsStore::new(root.0.clone());
    let c = circle("makepad:history");
    let repo = Some(RepoId(0xfeed));
    let mut all = Vec::new();
    let mut m = None;
    for i in 0..MERGE_FACTOR {
        let tag = format!("t{i}");
        let chunks = commit_chunks(&c, &tag, 1);
        all.extend(chunks.iter().map(|c| c.name.clone()));
        let id = write_segment(&store, &history_key(&c, &tag), &chunks);
        m = Some(store.commit(repo, &c, ManifestChange::Add(id)).unwrap());
        if i + 1 < MERGE_FACTOR {
            assert_eq!(m.as_ref().unwrap().segments.len(), i + 1);
        }
    }
    let m = m.unwrap();
    assert_eq!(m.segments.len(), 1, "8 segments of one tier fold into one");
    assert_eq!(m.version, MERGE_FACTOR as u64 + 1);
    assert_eq!(m.stats.chunks, all.len() as u64);
    let seg = store.segment(repo, &c, &m.segments[0]).unwrap();
    seg.verify().unwrap();
    let names: Vec<String> = (0..seg.chunk_count()).map(|i| seg.chunk(ChunkId(i)).name.to_string()).collect();
    assert_eq!(names, all);
    for i in 0..seg.chunk_count() {
        let v = seg.chunk(ChunkId(i));
        if v.kind == Kind::Bullet {
            assert_eq!(v.parent, Some(ChunkId(i - 1)), "parents shift with their chunks");
            assert_eq!(seg.keys(ChunkId(i)).commit, seg.chunk(ChunkId(i - 1)).commit);
        }
        assert_eq!(matches!(seg.detail(ChunkId(i)), Detail::Commit(_)), v.kind == Kind::Commit);
    }
    let common: Vec<u32> = seg.postings(TermForm::Exact, "common_word").map(|p| p.chunk.0).collect();
    assert_eq!(common, (0..MERGE_FACTOR as u32).map(|i| i * 2).collect::<Vec<_>>());
    assert_eq!(seg.chunks_with(&Key::Path { path: "alpha/src/lib.rs".into(), line: None }).len(), MERGE_FACTOR);
    let SourceStamp::History { tips } = &seg.key().source else { panic!("merged history keeps a history stamp") };
    assert_eq!(tips.len(), MERGE_FACTOR);
}

// ---------------------------------------------------------------------------------------------
// GC

#[test]
fn gc_keeps_what_is_live_or_young() {
    let root = Root::new();
    let store = FsStore::new(root.0.clone());
    let c = circle("makepad");
    let h = circle("makepad:history");
    let repo = Some(RepoId(0xfeed));
    let chunks = tree_chunks(&c);
    let live = tree_key(&c, "live");
    let dead = tree_key(&c, "dead");
    drop(store
        .build_base(&live, &mut |sink: &mut dyn SegmentSink| {
            for ch in &chunks {
                sink.add(ch, &FakeTokens.chunk_terms(ch))?;
            }
            Ok(())
        })
        .unwrap());
    drop(store
        .build_base(&dead, &mut |sink: &mut dyn SegmentSink| {
            let ch = chunk(&c, Kind::File, "only.rs", "only.rs");
            sink.add(&ch, &FakeTokens.chunk_terms(&ch)).map(|_| ())
        })
        .unwrap());
    let mut kept_history = Vec::new();
    for tag in ["a", "b"] {
        let id = write_segment(&store, &history_key(&h, tag), &commit_chunks(&h, tag, 2));
        kept_history = store.commit(repo, &h, ManifestChange::Add(id)).unwrap().segments;
    }
    let replaced = write_segment(&store, &history_key(&h, "r"), &commit_chunks(&h, "r", 1));
    store.commit(repo, &h, ManifestChange::Replace(replaced.clone())).unwrap();
    let stray = write_segment(&store, &history_key(&h, "stray"), &commit_chunks(&h, "stray", 1));
    let hdir = store.circle_dir(repo, &h).unwrap();
    fs::write(hdir.join("tmp-1-1.mpx.tmp"), b"half").unwrap();

    // within the grace period nothing goes
    let r = store.gc(RepoId(0xfeed), std::slice::from_ref(&live), &[(repo, h.clone())]).unwrap();
    assert!(r.removed.is_empty(), "{:?}", r.removed);
    assert!(r.deferred.iter().any(|(p, _)| p.ends_with(base_name(&dead))));
    assert!(r.deferred.iter().any(|(p, _)| p.ends_with(&stray.0)));

    // past it, only the live base, the newest manifest and their segments stay
    let store = FsStore::with_grace(root.0.clone(), Duration::ZERO);
    let r = store.gc(RepoId(0xfeed), std::slice::from_ref(&live), &[(repo, h.clone())]).unwrap();
    assert!(r.deferred.is_empty(), "{:?}", r.deferred);
    assert!(store.open_base(&live).unwrap().is_some());
    assert!(store.open_base(&dead).unwrap().is_none());
    let m = store.manifest(repo, &h).unwrap().unwrap();
    assert_eq!(m.segments, vec![replaced.clone()]);
    store.open_segment(repo, &h, &replaced).unwrap();
    assert_eq!(names_in(&hdir), vec![format!("manifest-{}", m.version), replaced.0.clone()]);
    for id in kept_history.iter().chain([&stray]) {
        assert!(r.removed.iter().any(|p| p.ends_with(&id.0)), "{} removed", id.0);
    }
    let cdir = store.circle_dir(repo, &c).unwrap();
    let live_seg = fs::read_to_string(cdir.join(base_name(&live))).unwrap().trim().to_string();
    assert_eq!(names_in(&cdir), vec![base_name(&live), format!("{}.used", base_name(&live)), live_seg]);
}

// ---------------------------------------------------------------------------------------------
// Several processes at once

const CHILD_ENV: &str = "MAKEPAD_REPO_INDEX_STORE_CHILD";
const BUILDERS: usize = 3;
const READERS: usize = 3;
const ROUNDS: usize = 6;
const BASE_CHUNKS: usize = 3000;

fn synthetic(c: &CircleName, n: usize) -> Chunk {
    let mut ch = chunk(c, Kind::Fn, &format!("src/f{}.rs", n % 50), &format!("item_{n}"));
    ch.fields.body = format!("alpha beta gamma_{} item_{n}", n % 17);
    ch.keys.idents = vec![format!("item_{n}")];
    ch
}

/// One child: builds the shared base and appends history, or reads both, checking every byte
/// of whatever it sees.
fn child(role: &str, root: &Path, i: usize) -> Result<(), String> {
    let store = FsStore::new(root.to_path_buf());
    let c = circle("makepad");
    let h = circle("makepad:history");
    let repo = Some(RepoId(0xfeed));
    let key = tree_key(&c, "shared");
    let e = |e: Error| e.to_string();
    let check_base = |seg: &Segment| -> Result<(), String> {
        seg.verify().map_err(e)?;
        if seg.chunk_count() as usize != BASE_CHUNKS || seg.chunk(ChunkId(BASE_CHUNKS as u32 - 1)).name != format!("item_{}", BASE_CHUNKS - 1) {
            return Err(format!("partial base: {} chunks", seg.chunk_count()));
        }
        Ok(())
    };
    let check_history = || -> Result<u64, String> {
        let Some(m) = store.manifest(repo, &h).map_err(e)? else { return Ok(0) };
        let mut chunks = 0;
        for id in &m.segments {
            let seg = store.segment(repo, &h, id).map_err(e)?;
            seg.verify().map_err(e)?;
            chunks += seg.chunk_count() as u64;
        }
        if chunks != m.stats.chunks {
            return Err(format!("manifest {} counts {} chunks, its segments {chunks}", m.version, m.stats.chunks));
        }
        Ok(m.version)
    };
    match role {
        "build" => {
            let log = root.join("fills.log");
            let seg = store
                .build_base(&key, &mut |sink| {
                    use std::io::Write;
                    let mut f = File::options().append(true).create(true).open(&log).map_err(|err| io_err(&log, "open", err))?;
                    writeln!(f, "fill by {}", std::process::id()).map_err(|err| io_err(&log, "write", err))?;
                    for n in 0..BASE_CHUNKS {
                        let ch = synthetic(&c, n);
                        sink.add(&ch, &FakeTokens.chunk_terms(&ch))?;
                    }
                    Ok(())
                })
                .map_err(e)?;
            if seg.chunk_count() as usize != BASE_CHUNKS {
                return Err("build_base returned a partial base".into());
            }
            check_base(&store.base(&key).map_err(e)?.ok_or("no base after build_base")?)?;
            for r in 0..ROUNDS {
                let tag = format!("w{i}r{r}");
                let id = write_segment(&store, &history_key(&h, &tag), &commit_chunks(&h, &tag, 1));
                store.commit(repo, &h, ManifestChange::Add(id)).map_err(e)?;
                check_history()?;
            }
        }
        "read" => {
            // read while any builder runs, then once more after the last one finished
            let (mut seen, mut versions, mut reads) = (0, BTreeSet::new(), 0u64);
            loop {
                let done = (0..BUILDERS).all(|b| root.join(format!("done-{b}")).exists());
                reads += 1;
                if let Some(seg) = store.base(&key).map_err(e)? {
                    check_base(&seg)?;
                    seen += 1;
                }
                versions.insert(check_history()?);
                if done {
                    break;
                }
            }
            if seen == 0 {
                return Err("never saw the base".into());
            }
            eprintln!("reader {i}: {reads} reads, the whole base {seen} times, {} manifest versions", versions.len());
        }
        _ => return Err(format!("unknown role {role}")),
    }
    Ok(())
}

/// Not a test of its own: the body of a child process of `processes_build_and_read_at_once`.
#[test]
fn store_child_process() {
    let Ok(spec) = std::env::var(CHILD_ENV) else { return };
    let mut w = spec.splitn(3, ' ');
    let (role, i, root) = (w.next().unwrap(), w.next().unwrap().parse().unwrap(), PathBuf::from(w.next().unwrap()));
    let r = child(role, &root, i);
    if role == "build" {
        // failed or not, so readers stop
        fs::write(root.join(format!("done-{i}")), b"").unwrap();
    }
    if let Err(m) = r {
        panic!("{role} {i}: {m}");
    }
}

/// Builders race for one base (one fill wins; the others wait and map it) and append history
/// segments through manifest commits (with tiered merges) while readers open both: no reader
/// sees a partial index, no version is lost, no chunk is lost.
#[test]
fn processes_build_and_read_at_once() {
    let root = Root::new();
    let exe = std::env::current_exe().unwrap();
    let mut kids = Vec::new();
    for i in 0..BUILDERS.max(READERS) {
        for (role, n) in [("read", READERS), ("build", BUILDERS)] {
            if i >= n {
                continue;
            }
            let child = std::process::Command::new(&exe)
                .args(["store::tests::store_child_process", "--exact", "--nocapture", "--test-threads=1"])
                .env(CHILD_ENV, format!("{role} {i} {}", root.0.display()))
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            kids.push((format!("{role} {i}"), child));
        }
    }
    for (name, kid) in kids {
        let out = kid.wait_with_output().unwrap();
        assert!(out.status.success(), "{name} failed:\n{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    }

    let fills = fs::read_to_string(root.0.join("fills.log")).unwrap();
    assert_eq!(fills.lines().count(), 1, "one build of the shared base, not {fills}");

    let store = FsStore::new(root.0.clone());
    let h = circle("makepad:history");
    let repo = Some(RepoId(0xfeed));
    let m = store.manifest(repo, &h).unwrap().unwrap();
    let dir = store.circle_dir(repo, &h).unwrap();
    let mut versions = manifest::numbers(&dir).unwrap();
    versions.reverse();
    assert_eq!(versions, (1..=m.version).collect::<Vec<_>>(), "every version committed exactly once");
    for n in &versions {
        manifest::read(&manifest::path_of(&dir, *n), *n, &h).unwrap();
    }
    let mut names = BTreeSet::new();
    for id in &m.segments {
        let seg = store.segment(repo, &h, id).unwrap();
        for c in 0..seg.chunk_count() {
            assert!(names.insert(seg.chunk(ChunkId(c)).name.to_string()), "a chunk counted twice");
        }
    }
    let hr = &h;
    let want: BTreeSet<String> = (0..BUILDERS)
        .flat_map(|i| (0..ROUNDS).flat_map(move |r| commit_chunks(hr, &format!("w{i}r{r}"), 1).into_iter().map(|c| c.name)))
        .collect();
    assert_eq!(names, want, "every appended chunk is in the newest manifest");
    assert!(m.segments.len() < BUILDERS * ROUNDS, "tiered merges ran");
    assert!(names_in(&dir).iter().all(|n| !n.ends_with(".tmp")), "no tmp file left");
}

/// A notes circle dropped from `circles.conf` loses its manifests and segments, record text
/// included, after the grace period; a configured one keeps them.
#[test]
fn gc_drops_unconfigured_circles() {
    let root = Root::new();
    let a = circle("notes:topic-a");
    let b = circle("notes:topic-b");
    let store = FsStore::new(root.0.clone());
    for c in [&a, &b] {
        let key = SegmentKey { repo: None, circle: c.clone(), source: SourceStamp::Notes { content: 1 }, def_hash: 1, indexer: INDEXER_VERSION };
        let id = write_segment(&store, &key, &notes_chunks(c));
        store.commit(None, c, ManifestChange::Replace(id)).unwrap();
    }
    let r = store.gc(RepoId(0xfeed), &[], &[(None, a.clone())]).unwrap();
    assert!(r.removed.is_empty());
    assert!(r.deferred.iter().any(|(p, _)| p.ends_with("notes-topic-b")));
    let store = FsStore::with_grace(root.0.clone(), Duration::ZERO);
    let r = store.gc(RepoId(0xfeed), &[], &[(None, a.clone())]).unwrap();
    assert!(r.removed.iter().any(|p| p.ends_with("notes-topic-b")), "{:?}", r.removed);
    assert!(!root.0.join("notes-topic-b").exists());
    assert!(store.manifest(None, &b).unwrap().is_none());
    let m = store.manifest(None, &a).unwrap().unwrap();
    store.open_segment(None, &a, &m.segments[0]).unwrap();
}

/// A gc from repo A sweeps A's dead bases after the grace period and leaves every base and
/// manifest circle of repo B alone, live or not: B's worktrees are not known to A.
#[test]
fn gc_sweeps_only_its_repo() {
    let root = Root::new();
    let store = FsStore::with_grace(root.0.clone(), Duration::ZERO);
    let c = circle("makepad");
    let h = circle("makepad:history");
    let (ra, rb) = (RepoId(0xa), RepoId(0xb));
    let base = |repo: RepoId, tree: &str| SegmentKey { repo: Some(repo), ..tree_key(&c, tree) };
    let mut one = |sink: &mut dyn SegmentSink| {
        let ch = chunk(&c, Kind::File, "only.rs", "only.rs");
        sink.add(&ch, &FakeTokens.chunk_terms(&ch)).map(|_| ())
    };
    for key in [base(ra, "dead"), base(ra, "live"), base(rb, "b")] {
        drop(store.build_base(&key, &mut one).unwrap());
    }
    for repo in [ra, rb] {
        let key = SegmentKey { repo: Some(repo), ..history_key(&h, "x") };
        let id = write_segment(&store, &key, &commit_chunks(&h, "x", 1));
        store.commit(Some(repo), &h, ManifestChange::Add(id)).unwrap();
    }
    store.gc(ra, &[base(ra, "live")], &[(Some(ra), h.clone())]).unwrap();
    assert!(store.open_base(&base(ra, "dead")).unwrap().is_none(), "A's dead base is swept");
    assert!(store.open_base(&base(ra, "live")).unwrap().is_some());
    assert!(store.open_base(&base(rb, "b")).unwrap().is_some(), "B's base is never A's to sweep");
    let m = store.manifest(Some(rb), &h).unwrap().expect("B's history circle stays");
    assert_eq!(store.open_segment(Some(rb), &h, &m.segments[0]).unwrap().id(), m.segments[0]);
    assert!(store.manifest(Some(ra), &h).unwrap().is_some());
}

// ---------------------------------------------------------------------------------------------
// Long fields

/// A field length past u16 reads back exactly.
#[test]
fn long_field_lengths_read_back_exactly() {
    let root = Root::new();
    let store = FsStore::new(root.0.clone());
    let c = circle("makepad");
    let mut sink = store.new_segment(&tree_key(&c, "t1")).unwrap();
    let lengths = [[3, 1, 0, 0, 70_000], [1, 1, 1, 1, 65_535], [1, 1, 1, 1, 65_534]];
    for (i, l) in lengths.iter().enumerate() {
        let ch = chunk(&c, Kind::File, &format!("big{i}.rs"), "big");
        let mut terms = FieldTerms::default();
        terms.push(Field::Body, TermForm::Exact, "x", 7);
        terms.lengths = *l;
        sink.add(&ch, &terms).unwrap();
    }
    let id = sink.finish().unwrap();
    let seg = store.segment(Some(RepoId(0xfeed)), &c, &id).unwrap();
    for (i, l) in lengths.iter().enumerate() {
        assert_eq!(seg.chunk(ChunkId(i as u32)).field_lengths, *l);
    }
    assert_eq!(seg.stats().field_lengths[4], 70_000 + 65_535 + 65_534);
    assert_eq!(seg.postings(TermForm::Exact, "x").map(|p| p.tf[4]).collect::<Vec<_>>(), vec![7, 7, 7]);
}

/// A prefix scan of the term dictionary gives exactly the terms a brute-force filter does, for
/// every prefix of every term (so across block boundaries), per form, in byte order.
#[test]
fn terms_with_prefix_scans_the_dictionary() {
    let root = Root::new();
    let store = FsStore::new(root.0.clone());
    let c = circle("makepad");
    let chunks = tree_chunks(&c);
    let id = write_segment(&store, &tree_key(&c, "t1"), &chunks);
    let seg = store.segment(Some(RepoId(0xfeed)), &c, &id).unwrap();
    let mut all: HashMap<TermForm, BTreeSet<String>> = HashMap::new();
    for ch in &chunks {
        for t in FakeTokens.chunk_terms(ch).iter() {
            all.entry(t.form).or_default().insert(t.text.to_string());
        }
    }
    let brute = |form: TermForm, prefix: &str| all.get(&form).map_or(Vec::new(), |s| s.iter().filter(|t| t.starts_with(prefix)).cloned().collect::<Vec<_>>());
    assert!(all.values().any(|t| t.len() > segment::BLOCK * 3), "enough terms to span blocks");
    for (form, terms) in &all {
        assert_eq!(seg.terms_with_prefix(*form, ""), brute(*form, ""), "{form:?}: every term");
        for t in terms {
            for end in (0..=t.len()).filter(|&e| t.is_char_boundary(e)) {
                assert_eq!(seg.terms_with_prefix(*form, &t[..end]), brute(*form, &t[..end]), "{form:?} `{}`", &t[..end]);
            }
        }
    }
    assert!(seg.terms_with_prefix(TermForm::Exact, "zzzz_no_such").is_empty());
    assert!(seg.terms_with_prefix(TermForm::Folded, "\u{10ffff}").is_empty());
    let spin = seg.terms_with_prefix(TermForm::Exact, "sp");
    assert!(spin.contains(&"spin".to_string()) && spin.iter().all(|t| t.starts_with("sp")));
}
