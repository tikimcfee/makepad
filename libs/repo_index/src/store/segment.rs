//! The segment file format: header, section table, strings, chunks, sorted terms with
//! delta-varint postings and per-field term frequencies, field lengths, statistics, the key
//! table (paths, packages, commits, blobs, identifiers: [`crate::SegmentRead::chunks_with`]),
//! edges and their reverse index ([`crate::SegmentRead::cited_by`]), crate cards, notes record
//! text ([`crate::SegmentRead::record_text`]), and a checksum footer.
//!
//! Little-endian throughout; nothing needs alignment (every read is `from_le_bytes` on a slice).
//! ```text
//! file      body footer
//! body      header(16) table(24 × sections) sections..
//! header    "MPXS" format:u32 sections:u32 0:u32
//! table     id:u32 0:u32 offset:u64 len:u64              offsets from the start of the file
//! footer    sha1:[20] "MPXE"                               sha1 = git blob hash of the body,
//!                                                          the hex in the file name
//! META      key, stats (chunks, 5 field-length sums), chunk count
//! STR_OFFS  (n+1) × u32 into STR_BYTES; string 0 is ""
//! OIDS      n × [20]
//! CHUNKS    count × 66 B: kind u8, flags u8, label_len u16, path name package gist sig (string ids),
//!           span lo hi, lines start end, parent, field lengths 5 × u16, blob commit (oid ids),
//!           extra (offset into EXTRA); NONE = u32::MAX; a length of 0xFFFF means "in EXTRA"
//! EXTRA     per chunk, varints: paths, idents, edges, detail, record text, exact lengths
//! dict      TERMS, KEYS, CITES, each three sections:
//!           INDEX   n:u32, then (blocks+1) × u32 offsets into BLOCKS
//!           BLOCKS  per 16 sorted keys: (shared prefix, suffix len, suffix, count,
//!                   zigzag value offset delta) varints; a block's first key shares nothing
//!           VALUES  the lists, each stored once however many keys name it
//! ```
//! A term key is its [`crate::TermForm`] byte and its text; its value is postings: per chunk a
//! varint `gap << 6 | field mask << 1 | all-ones`, then, unless every nonzero tf is 1, the tfs
//! of the masked fields (varints). A key-table key is a [`crate::Key`] without its line (tag
//! byte, payload); its value is delta-varint chunk ids. A cite key is an edge's key the same
//! way; its value is (chunk gap, edge index) pairs.

use crate::{CircleName, CircleStats, CommitInfo, CrateCard, Detail, Edge, Field, Flags, Key, Kind, Oid, RepoId, SegmentKey, SourceStamp, TermForm};

pub const MAGIC: &[u8; 4] = b"MPXS";
pub const FOOTER_MAGIC: &[u8; 4] = b"MPXE";
/// The segment format; part of [`crate::INDEXER_VERSION`]'s promise.
pub const FORMAT: u32 = 2;
pub const HEADER_LEN: usize = 16;
pub const TABLE_ENTRY_LEN: usize = 24;
pub const FOOTER_LEN: usize = 24;
pub const CHUNK_LEN: usize = 66;
/// Keys per front-coded dictionary block.
pub const BLOCK: usize = 16;
/// A stored field length that does not fit u16 (the exact one is in EXTRA).
pub const LONG: u16 = u16::MAX;
pub const NONE: u32 = u32::MAX;

/// Section ids, in file order.
pub mod sec {
    pub const META: u32 = 1;
    pub const STR_OFFS: u32 = 2;
    pub const STR_BYTES: u32 = 3;
    pub const OIDS: u32 = 4;
    pub const CHUNKS: u32 = 5;
    pub const EXTRA: u32 = 6;
    /// A dictionary's sections are `INDEX`, `INDEX + 1` (blocks) and `INDEX + 2` (values).
    pub const TERMS: u32 = 7;
    pub const KEYS: u32 = 10;
    pub const CITES: u32 = 13;
    pub const ALL: [u32; 15] = [META, STR_OFFS, STR_BYTES, OIDS, CHUNKS, EXTRA, 7, 8, 9, 10, 11, 12, 13, 14, 15];
}

/// Byte offsets of the fields of a chunk record.
pub mod rec {
    pub const KIND: usize = 0;
    pub const FLAGS: usize = 1;
    pub const LABEL_LEN: usize = 2;
    pub const PATH: usize = 4;
    pub const NAME: usize = 8;
    pub const PACKAGE: usize = 12;
    pub const GIST: usize = 16;
    pub const SIG: usize = 20;
    pub const SPAN: usize = 24;
    pub const LINES: usize = 32;
    pub const PARENT: usize = 40;
    pub const LENGTHS: usize = 44;
    pub const BLOB: usize = 54;
    pub const COMMIT: usize = 58;
    pub const EXTRA: usize = 62;
}

// ---------------------------------------------------------------------------------------------
// Stable codes

pub fn kind_code(kind: Kind) -> u8 {
    match kind {
        Kind::Crate => 0,
        Kind::File => 1,
        Kind::Fn => 2,
        Kind::Method => 3,
        Kind::Struct => 4,
        Kind::Enum => 5,
        Kind::Union => 6,
        Kind::Trait => 7,
        Kind::Impl => 8,
        Kind::Mod => 9,
        Kind::Type => 10,
        Kind::Const => 11,
        Kind::Static => 12,
        Kind::Macro => 13,
        Kind::Doc => 14,
        Kind::Splash => 15,
        Kind::Commit => 16,
        Kind::Bullet => 17,
        Kind::Note => 18,
        Kind::NoteFile => 19,
    }
}

pub fn kind_of(code: u8) -> Option<Kind> {
    Some(match code {
        0 => Kind::Crate,
        1 => Kind::File,
        2 => Kind::Fn,
        3 => Kind::Method,
        4 => Kind::Struct,
        5 => Kind::Enum,
        6 => Kind::Union,
        7 => Kind::Trait,
        8 => Kind::Impl,
        9 => Kind::Mod,
        10 => Kind::Type,
        11 => Kind::Const,
        12 => Kind::Static,
        13 => Kind::Macro,
        14 => Kind::Doc,
        15 => Kind::Splash,
        16 => Kind::Commit,
        17 => Kind::Bullet,
        18 => Kind::Note,
        19 => Kind::NoteFile,
        _ => return None,
    })
}

pub fn form_code(form: TermForm) -> u8 {
    match form {
        TermForm::Exact => 0,
        TermForm::Folded => 1,
        TermForm::Part => 2,
    }
}

pub fn field_index(field: Field) -> usize {
    match field {
        Field::Name => 0,
        Field::Path => 1,
        Field::Sig => 2,
        Field::Doc => 3,
        Field::Body => 4,
    }
}

pub fn flags_code(f: Flags) -> u8 {
    (f.public as u8)
        | (f.documented as u8) << 1
        | (f.definition as u8) << 2
        | (f.generated as u8) << 3
        | (f.fallback as u8) << 4
        | (f.binary as u8) << 5
}

pub fn flags_of(b: u8) -> Flags {
    Flags {
        public: b & 1 != 0,
        documented: b & 2 != 0,
        definition: b & 4 != 0,
        generated: b & 8 != 0,
        fallback: b & 16 != 0,
        binary: b & 32 != 0,
    }
}

pub const KEY_PATH: u8 = 1;
pub const KEY_COMMIT: u8 = 2;
pub const KEY_BLOB: u8 = 3;
pub const KEY_IDENT: u8 = 4;
pub const KEY_PACKAGE: u8 = 5;

/// A key as a key-table or cite-table key: tag and payload, without a path's line.
pub fn key_lookup_bytes(key: &Key) -> Vec<u8> {
    let mut out = Vec::new();
    match key {
        Key::Path { path, .. } => {
            out.push(KEY_PATH);
            out.extend_from_slice(path.as_bytes());
        }
        Key::Commit(oid) => {
            out.push(KEY_COMMIT);
            out.extend_from_slice(&oid.0);
        }
        Key::Blob(oid) => {
            out.push(KEY_BLOB);
            out.extend_from_slice(&oid.0);
        }
        Key::Ident(s) => {
            out.push(KEY_IDENT);
            out.extend_from_slice(s.as_bytes());
        }
        Key::Package(s) => {
            out.push(KEY_PACKAGE);
            out.extend_from_slice(s.as_bytes());
        }
    }
    out
}

/// The canonical encoding of a segment key: stored in META, and hashed to name a base.
pub fn encode_key(key: &SegmentKey) -> Vec<u8> {
    let mut out = vec![1u8];
    match key.repo {
        None => out.push(0),
        Some(RepoId(id)) => {
            out.push(1);
            out.extend_from_slice(&id.to_le_bytes());
        }
    }
    put_bytes(&mut out, key.circle.0.as_bytes());
    match &key.source {
        SourceStamp::Tree { tree } => {
            out.push(0);
            out.extend_from_slice(&tree.0);
        }
        SourceStamp::History { tips } => {
            out.push(1);
            put_varint(&mut out, tips.len() as u64);
            for tip in tips {
                out.extend_from_slice(&tip.0);
            }
        }
        SourceStamp::Notes { content } => {
            out.push(2);
            out.extend_from_slice(&content.to_le_bytes());
        }
    }
    out.extend_from_slice(&key.def_hash.to_le_bytes());
    out.extend_from_slice(&key.indexer.to_le_bytes());
    out
}

pub fn decode_key(r: &mut Reader) -> Option<SegmentKey> {
    if r.u8()? != 1 {
        return None;
    }
    let repo = match r.u8()? {
        0 => None,
        1 => Some(RepoId(r.u64()?)),
        _ => return None,
    };
    let circle = CircleName(std::str::from_utf8(r.bytes()?).ok()?.to_string());
    let source = match r.u8()? {
        0 => SourceStamp::Tree { tree: r.oid()? },
        1 => {
            let n = r.varint()? as usize;
            let mut tips = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                tips.push(r.oid()?);
            }
            SourceStamp::History { tips }
        }
        2 => SourceStamp::Notes { content: r.u64()? },
        _ => return None,
    };
    let def_hash = r.u64()?;
    let indexer = r.u32()?;
    Some(SegmentKey { repo, circle, source, def_hash, indexer })
}

pub fn encode_meta(key: &SegmentKey, stats: &CircleStats, count: u32) -> Vec<u8> {
    let mut out = encode_key(key);
    out.extend_from_slice(&stats.chunks.to_le_bytes());
    for len in stats.field_lengths {
        out.extend_from_slice(&len.to_le_bytes());
    }
    out.extend_from_slice(&count.to_le_bytes());
    out
}

pub fn decode_meta(bytes: &[u8]) -> Option<(SegmentKey, CircleStats, u32)> {
    let mut r = Reader::new(bytes);
    let key = decode_key(&mut r)?;
    let mut stats = CircleStats { chunks: r.u64()?, ..CircleStats::default() };
    for len in &mut stats.field_lengths {
        *len = r.u64()?;
    }
    let count = r.u32()?;
    r.at_end().then_some((key, stats, count))
}

// ---------------------------------------------------------------------------------------------
// Varints and a bounded reader

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

pub fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Reads from a slice; every read is bounds-checked and `None` past the end or on overflow.
pub struct Reader<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    pub fn at(buf: &'a [u8], pos: usize) -> Self {
        Self { buf, pos }
    }
    pub fn at_end(&self) -> bool {
        self.pos == self.buf.len()
    }
    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    pub fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    pub fn oid(&mut self) -> Option<Oid> {
        Some(Oid(self.take(20)?.try_into().ok()?))
    }
    pub fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            if shift == 63 && b > 1 {
                return None;
            }
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }
    pub fn varint_u32(&mut self) -> Option<u32> {
        u32::try_from(self.varint()?).ok()
    }
    pub fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.varint()?).ok()?;
        self.take(n)
    }
}

pub fn le_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

// ---------------------------------------------------------------------------------------------
// Per-chunk extra

/// What EXTRA holds for one chunk, with strings and oids as table ids.
#[derive(Default)]
pub struct ExtraIds {
    pub paths: Vec<u32>,
    pub idents: Vec<u32>,
    pub edges: Vec<EdgeIds>,
    pub detail: DetailIds,
    /// Record text's string id + 1, or 0.
    pub text: u32,
    /// The exact field lengths, when one does not fit the record's u16 ([`LONG`]).
    pub lengths: Option<[u32; 5]>,
}

/// An edge: target circle (string id), key tag, payload (string or oid id), line + 1 or 0.
#[derive(Clone, Copy)]
pub struct EdgeIds {
    pub to: u32,
    pub tag: u8,
    pub payload: u32,
    pub line: u32,
}

#[derive(Default)]
pub enum DetailIds {
    #[default]
    None,
    Crate(CrateIds),
    Commit { time: i64, parents: Vec<u32> },
}

pub struct CrateIds {
    pub dir: u32,
    pub description: u32,
    pub deps: Vec<(u32, u32)>,
    pub dependents: Vec<u32>,
    pub apps: Vec<u32>,
    pub targets: Vec<u32>,
    pub rs_files: u32,
    pub modules: Vec<u32>,
    pub private_members: bool,
}

fn put_ids(out: &mut Vec<u8>, ids: &[u32]) {
    put_varint(out, ids.len() as u64);
    for &id in ids {
        put_varint(out, id as u64);
    }
}

fn get_ids(r: &mut Reader) -> Option<Vec<u32>> {
    let n = r.varint()? as usize;
    let mut out = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        out.push(r.varint_u32()?);
    }
    Some(out)
}

pub fn encode_extra(out: &mut Vec<u8>, x: &ExtraIds) {
    put_ids(out, &x.paths);
    put_ids(out, &x.idents);
    put_varint(out, x.edges.len() as u64);
    for e in &x.edges {
        put_varint(out, e.to as u64);
        out.push(e.tag);
        put_varint(out, e.payload as u64);
        put_varint(out, e.line as u64);
    }
    match &x.detail {
        DetailIds::None => out.push(0),
        DetailIds::Crate(c) => {
            out.push(1);
            put_varint(out, c.dir as u64);
            put_varint(out, c.description as u64);
            put_varint(out, c.deps.len() as u64);
            for &(name, cfg) in &c.deps {
                put_varint(out, name as u64);
                put_varint(out, cfg as u64);
            }
            put_ids(out, &c.dependents);
            put_ids(out, &c.apps);
            put_ids(out, &c.targets);
            put_varint(out, c.rs_files as u64);
            put_ids(out, &c.modules);
            out.push(c.private_members as u8);
        }
        DetailIds::Commit { time, parents } => {
            out.push(2);
            put_varint(out, zigzag(*time));
            put_ids(out, parents);
        }
    }
    put_varint(out, x.text as u64);
    match x.lengths {
        None => out.push(0),
        Some(l) => {
            out.push(5);
            for v in l {
                put_varint(out, v as u64);
            }
        }
    }
}

pub fn decode_extra(r: &mut Reader) -> Option<ExtraIds> {
    let paths = get_ids(r)?;
    let idents = get_ids(r)?;
    let n = r.varint()? as usize;
    let mut edges = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        edges.push(EdgeIds { to: r.varint_u32()?, tag: r.u8()?, payload: r.varint_u32()?, line: r.varint_u32()? });
    }
    let detail = match r.u8()? {
        0 => DetailIds::None,
        1 => {
            let dir = r.varint_u32()?;
            let description = r.varint_u32()?;
            let n = r.varint()? as usize;
            let mut deps = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                deps.push((r.varint_u32()?, r.varint_u32()?));
            }
            DetailIds::Crate(CrateIds {
                dir,
                description,
                deps,
                dependents: get_ids(r)?,
                apps: get_ids(r)?,
                targets: get_ids(r)?,
                rs_files: r.varint_u32()?,
                modules: get_ids(r)?,
                private_members: r.u8()? != 0,
            })
        }
        2 => DetailIds::Commit { time: unzigzag(r.varint()?), parents: get_ids(r)? },
        _ => return None,
    };
    let text = r.varint_u32()?;
    let lengths = match r.u8()? {
        0 => None,
        5 => Some([r.varint_u32()?, r.varint_u32()?, r.varint_u32()?, r.varint_u32()?, r.varint_u32()?]),
        _ => return None,
    };
    Some(ExtraIds { paths, idents, edges, detail, text, lengths })
}

/// Rebuilds a [`Detail`] from its ids.
pub fn detail_from_ids(d: &DetailIds, s: &dyn Fn(u32) -> String, o: &dyn Fn(u32) -> Oid) -> Detail {
    let strs = |ids: &[u32]| ids.iter().map(|&i| s(i)).collect::<Vec<_>>();
    match d {
        DetailIds::None => Detail::None,
        DetailIds::Crate(c) => Detail::Crate(CrateCard {
            dir: s(c.dir),
            description: s(c.description),
            deps: c.deps.iter().map(|&(n, cfg)| (s(n), s(cfg))).collect(),
            dependents: strs(&c.dependents),
            apps: strs(&c.apps),
            targets: strs(&c.targets),
            rs_files: c.rs_files,
            modules: strs(&c.modules),
            private_members: c.private_members,
        }),
        DetailIds::Commit { time, parents } => Detail::Commit(CommitInfo { time: *time, parents: parents.iter().map(|&p| o(p)).collect() }),
    }
}

/// Rebuilds an [`Edge`] from its ids; `None` for an unknown key tag.
pub fn edge_from_ids(e: EdgeIds, s: &dyn Fn(u32) -> String, o: &dyn Fn(u32) -> Oid) -> Option<Edge> {
    let key = match e.tag {
        KEY_PATH => Key::Path { path: s(e.payload), line: e.line.checked_sub(1) },
        KEY_COMMIT => Key::Commit(o(e.payload)),
        KEY_BLOB => Key::Blob(o(e.payload)),
        KEY_IDENT => Key::Ident(s(e.payload)),
        KEY_PACKAGE => Key::Package(s(e.payload)),
        _ => return None,
    };
    Some(Edge { to: CircleName(s(e.to)), key })
}

// ---------------------------------------------------------------------------------------------
// Postings and id lists

fn gap(prev: Option<u32>, id: u32) -> u64 {
    (id - prev.map_or(0, |p| p + 1)) as u64
}

fn ungap(r: &mut Reader, prev: Option<u32>) -> Option<u32> {
    prev.map_or(0, |p| p + 1).checked_add(r.varint_u32()?)
}

/// Appends one posting after `prev` (ids strictly ascend): `gap << 6 | mask << 1 | all-ones`,
/// then the masked tfs unless they are all 1.
pub fn put_posting(out: &mut Vec<u8>, prev: Option<u32>, chunk: u32, tf: &[u32; 5]) {
    let mask = tf.iter().enumerate().fold(0u64, |m, (i, &t)| if t != 0 { m | 1 << i } else { m });
    let ones = tf.iter().all(|&t| t <= 1);
    put_varint(out, gap(prev, chunk) << 6 | mask << 1 | ones as u64);
    if !ones {
        for &t in tf {
            if t != 0 {
                put_varint(out, t as u64);
            }
        }
    }
}

pub fn get_posting(r: &mut Reader, prev: Option<u32>) -> Option<(u32, [u32; 5])> {
    let h = r.varint()?;
    let chunk = prev.map_or(0, |p| p + 1).checked_add(u32::try_from(h >> 6).ok()?)?;
    let mask = (h >> 1) & 31;
    if mask == 0 {
        return None;
    }
    let mut tf = [0u32; 5];
    for (i, t) in tf.iter_mut().enumerate() {
        if mask & (1 << i) != 0 {
            *t = if h & 1 == 1 { 1 } else { r.varint_u32()? };
        }
    }
    Some((chunk, tf))
}

/// Appends a chunk id to a delta-varint id list.
pub fn put_id(out: &mut Vec<u8>, prev: Option<u32>, id: u32) {
    put_varint(out, gap(prev, id));
}

pub fn get_id(r: &mut Reader, prev: Option<u32>) -> Option<u32> {
    ungap(r, prev)
}

// ---------------------------------------------------------------------------------------------
// Front-coded dictionaries

/// Encodes sorted `(key, count, value offset)` entries as an INDEX and a BLOCKS section.
pub fn encode_dict(entries: &[(&[u8], u32, u32)]) -> (Vec<u8>, Vec<u8>) {
    let mut index = Vec::with_capacity(8 + entries.len().div_ceil(BLOCK) * 4);
    index.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    let mut blocks = Vec::new();
    for block in entries.chunks(BLOCK) {
        index.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
        let (mut prev_key, mut prev_val): (&[u8], u32) = (&[], 0);
        for &(key, count, val) in block {
            let shared = prev_key.iter().zip(key).take_while(|(a, b)| a == b).count();
            put_varint(&mut blocks, shared as u64);
            put_bytes(&mut blocks, &key[shared..]);
            put_varint(&mut blocks, count as u64);
            put_varint(&mut blocks, zigzag(val as i64 - prev_val as i64));
            (prev_key, prev_val) = (key, val);
        }
    }
    index.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
    (index, blocks)
}

/// A front-coded dictionary over its three sections.
pub struct DictView<'a> {
    n: usize,
    index: &'a [u8],
    blocks: &'a [u8],
    values: &'a [u8],
}

impl<'a> DictView<'a> {
    /// `None` when the index does not fit the blocks.
    pub fn new(index: &'a [u8], blocks: &'a [u8], values: &'a [u8]) -> Option<DictView<'a>> {
        let n = u32::from_le_bytes(index.get(0..4)?.try_into().ok()?) as usize;
        let nb = n.div_ceil(BLOCK);
        if index.len() != 4 + (nb + 1) * 4 || le_u32(index, 4) != 0 || le_u32(index, 4 + nb * 4) as usize != blocks.len() {
            return None;
        }
        Some(DictView { n, index, blocks, values })
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    fn block(&self, b: usize) -> Reader<'a> {
        Reader::at(self.blocks, le_u32(self.index, 4 + b * 4) as usize)
    }

    fn first_key(&self, b: usize) -> Option<&'a [u8]> {
        let mut r = self.block(b);
        (r.varint()? == 0).then_some(())?;
        r.bytes()
    }

    /// The value (from its offset to the end of VALUES; the count bounds it) and count.
    fn value(&self, off: i64, count: u32) -> Option<(&'a [u8], u32)> {
        Some((self.values.get(usize::try_from(off).ok()?..)?, count))
    }

    pub fn find(&self, key: &[u8]) -> Option<(&'a [u8], u32)> {
        let nb = self.n.div_ceil(BLOCK);
        // the last block whose first key is <= key
        let (mut lo, mut hi) = (0, nb);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.first_key(mid)? <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let b = lo.checked_sub(1)?;
        let mut r = self.block(b);
        let mut k: Vec<u8> = Vec::with_capacity(key.len());
        let mut val = 0i64;
        for _ in 0..BLOCK.min(self.n - b * BLOCK) {
            let shared = r.varint()? as usize;
            k.truncate(shared);
            k.extend_from_slice(r.bytes()?);
            let count = r.varint_u32()?;
            val += unzigzag(r.varint()?);
            match k.as_slice().cmp(key) {
                std::cmp::Ordering::Equal => return self.value(val, count),
                std::cmp::Ordering::Greater => return None,
                std::cmp::Ordering::Less => {}
            }
        }
        None
    }

    /// Every entry in key order: key, value, count. Stops at the first entry that does not
    /// decode (the caller reports it by the shortfall against [`DictView::len`]).
    pub fn iter(&self) -> impl Iterator<Item = (Vec<u8>, &'a [u8], u32)> + '_ {
        self.iter_block(0)
    }

    /// The entries whose key starts with `prefix`, in key order: from the block that can hold
    /// the first of them, decoded forward while keys match.
    pub fn with_prefix<'p>(&'p self, prefix: &'p [u8]) -> impl Iterator<Item = (Vec<u8>, &'a [u8], u32)> + 'p {
        let nb = self.n.div_ceil(BLOCK);
        // the last block whose first key is < prefix holds the first match, if any precedes it
        let (mut lo, mut hi) = (0, nb);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.first_key(mid) {
                Some(k) if k < prefix => lo = mid + 1,
                _ => hi = mid,
            }
        }
        self.iter_block(lo.saturating_sub(1)).skip_while(move |(k, _, _)| k.as_slice() < prefix).take_while(move |(k, _, _)| k.starts_with(prefix))
    }

    fn iter_block(&self, start: usize) -> impl Iterator<Item = (Vec<u8>, &'a [u8], u32)> + '_ {
        let mut i = (start * BLOCK).min(self.n);
        let mut r = self.block(start.min(self.n.div_ceil(BLOCK)));
        let mut k: Vec<u8> = Vec::new();
        let mut val = 0i64;
        std::iter::from_fn(move || {
            if i == self.n {
                return None;
            }
            if i.is_multiple_of(BLOCK) {
                r = self.block(i / BLOCK);
                val = 0;
            }
            i += 1;
            let shared = r.varint()? as usize;
            k.truncate(shared);
            k.extend_from_slice(r.bytes()?);
            let count = r.varint_u32()?;
            val += unzigzag(r.varint()?);
            let (v, n) = self.value(val, count)?;
            Some((k.clone(), v, n))
        })
    }
}
