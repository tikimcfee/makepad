//! [`crate::SegmentSink`]: builds a segment in memory, writes it to a tmp file, fsyncs it and
//! links it to its content name (put-if-absent: an existing file of that name already holds
//! these bytes, so nothing is ever replaced, mapped or not); bases are built once under an
//! exclusive OS file lock on their key, released if its holder dies.

use super::read::Segment;
use super::segment::*;
use super::{io_err, log};
use crate::{Chunk, ChunkId, CircleStats, Detail, Error, FieldTerms, Flags, Key, Keys, Kind, Lines, Oid, SegmentId, SegmentKey, SegmentSink, Span};
use makepad_git::oid::ObjectHasher;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// One chunk as stored, whether from extraction ([`Builder::add`]) or another segment (merge).
pub struct RecordIn<'a> {
    pub kind: Kind,
    pub flags: Flags,
    pub path: &'a str,
    pub label_len: u16,
    pub name: &'a str,
    pub gist: &'a str,
    pub sig: &'a str,
    pub span: Span,
    pub lines: Lines,
    pub parent: Option<u32>,
    pub lengths: [u32; 5],
    pub keys: &'a Keys,
    pub detail: &'a Detail,
    pub text: Option<&'a str>,
}

/// FxHash (rustc's): fast for the short keys a build hashes millions of times; in memory only.
#[derive(Default, Clone, Copy)]
pub struct Fx(u64);

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for w in words {
            self.add(u64::from_le_bytes(*w));
        }
        let mut tail = 0u64;
        for (i, &b) in rest.iter().enumerate() {
            tail |= (b as u64) << (i * 8);
        }
        self.add(tail ^ (bytes.len() as u64) << 56);
    }
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

impl Fx {
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

pub type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

#[derive(Default)]
struct Strings {
    bytes: Vec<u8>,
    offs: Vec<u32>,
    ids: FxMap<Box<str>, u32>,
}

impl Strings {
    fn new() -> Self {
        let mut s = Self { offs: vec![0], ..Self::default() };
        s.id("");
        s
    }
    fn id(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.ids.get(s) {
            return id;
        }
        let id = (self.offs.len() - 1) as u32;
        self.bytes.extend_from_slice(s.as_bytes());
        self.offs.push(self.bytes.len() as u32);
        self.ids.insert(s.into(), id);
        id
    }
}

/// A list being appended to: postings, chunk ids, or cite pairs.
#[derive(Default)]
struct List {
    bytes: Vec<u8>,
    last: Option<u32>,
    count: u32,
}

/// A segment under construction. Chunk ids are assigned in order of addition.
pub struct Builder {
    key: SegmentKey,
    strings: Strings,
    oids: Vec<Oid>,
    oid_ids: FxMap<Oid, u32>,
    chunks: Vec<u8>,
    extra: Vec<u8>,
    terms: FxMap<Vec<u8>, List>,
    keys: FxMap<Vec<u8>, List>,
    cites: FxMap<Vec<u8>, List>,
    stats: CircleStats,
    count: u32,
    /// Scratch for lookup keys, reused across chunks.
    probe: Vec<u8>,
}

/// The list under `key`, created (and the key allocated) only when it is new.
fn append<'m>(map: &'m mut FxMap<Vec<u8>, List>, key: &[u8]) -> &'m mut List {
    if map.get(key).is_none() {
        map.insert(key.to_vec(), List::default());
    }
    map.get_mut(key).expect("just inserted")
}

/// Appends a posting to the list under `key`: one lookup when the term is known.
fn post(map: &mut FxMap<Vec<u8>, List>, key: &[u8], id: u32, tf: &[u32; 5]) {
    let fresh;
    let list = match map.get_mut(key) {
        Some(list) => list,
        None => {
            fresh = map.entry(key.to_vec()).or_default();
            fresh
        }
    };
    put_posting(&mut list.bytes, list.last, id, tf);
    list.last = Some(id);
    list.count += 1;
}

impl Builder {
    pub fn new(key: SegmentKey) -> Self {
        Self {
            key,
            strings: Strings::new(),
            oids: Vec::new(),
            oid_ids: FxMap::default(),
            chunks: Vec::new(),
            extra: Vec::new(),
            terms: FxMap::default(),
            keys: FxMap::default(),
            cites: FxMap::default(),
            stats: CircleStats::default(),
            count: 0,
            probe: Vec::new(),
        }
    }

    pub fn key(&self) -> &SegmentKey {
        &self.key
    }

    fn oid(&mut self, oid: Oid) -> u32 {
        let next = self.oids.len() as u32;
        let id = *self.oid_ids.entry(oid).or_insert(next);
        if id == next {
            self.oids.push(oid);
        }
        id
    }

    /// Adds an extracted chunk and its terms. A chunk of another circle is refused: circles are
    /// separate indexes, kept apart by the store itself.
    pub fn add(&mut self, chunk: &Chunk, terms: &FieldTerms) -> Result<ChunkId, Error> {
        if chunk.circle != self.key.circle {
            return Err(Error::Corrupt {
                path: PathBuf::from(&chunk.path),
                message: format!("a chunk of circle `{}` offered to a segment of `{}`", chunk.circle.0, self.key.circle.0),
            });
        }
        let notes = matches!(chunk.kind, Kind::Note | Kind::NoteFile);
        let id = self.push(RecordIn {
            kind: chunk.kind,
            flags: chunk.flags,
            path: &chunk.path,
            label_len: chunk.label_len,
            name: &chunk.name,
            gist: &chunk.gist,
            sig: &chunk.fields.sig,
            span: chunk.span,
            lines: chunk.lines,
            parent: chunk.parent,
            lengths: terms.lengths,
            keys: &chunk.keys,
            detail: &chunk.detail,
            text: notes.then_some(chunk.fields.body.as_str()),
        })?;
        // one posting per (form, text), its tfs summed per field
        let mut occ: Vec<(u8, &str, usize, u32)> = terms.iter().map(|t| (form_code(t.form), t.text, field_index(t.field), t.tf)).collect();
        occ.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        let mut i = 0;
        while i < occ.len() {
            let (form, text) = (occ[i].0, occ[i].1);
            let mut tf = [0u32; 5];
            while i < occ.len() && occ[i].0 == form && occ[i].1 == text {
                tf[occ[i].2] += occ[i].3;
                i += 1;
            }
            if tf.iter().all(|&t| t == 0) {
                continue;
            }
            self.probe.clear();
            self.probe.push(form);
            self.probe.extend_from_slice(text.as_bytes());
            post(&mut self.terms, &self.probe, id, &tf);
        }
        Ok(ChunkId(id))
    }

    /// Stores one chunk record and indexes its keys and edges; terms are the caller's.
    pub fn push(&mut self, r: RecordIn) -> Result<u32, Error> {
        let id = self.count;
        if id == NONE - 1 {
            return Err(self.overflow("chunks"));
        }
        if r.label_len != 0 && r.path.as_bytes().get(r.label_len as usize) != Some(&b'/') {
            return Err(Error::Corrupt {
                path: PathBuf::from(r.path),
                message: format!("label length {} does not end at a `/` of the path", r.label_len),
            });
        }
        if let Some(p) = r.parent {
            if p >= id {
                return Err(Error::Corrupt {
                    path: PathBuf::from(r.path),
                    message: format!("chunk {id}'s parent {p} is not an earlier chunk"),
                });
            }
        }
        let k = r.keys;
        let mut x = ExtraIds {
            paths: k.paths.iter().map(|p| self.strings.id(p)).collect(),
            idents: k.idents.iter().map(|i| self.strings.id(i)).collect(),
            ..ExtraIds::default()
        };
        for e in &k.edges {
            let to = self.strings.id(&e.to.0);
            let (tag, payload, line) = match &e.key {
                Key::Path { path, line } => (KEY_PATH, self.strings.id(path), line.map_or(0, |l| l + 1)),
                Key::Commit(o) => (KEY_COMMIT, self.oid(*o), 0),
                Key::Blob(o) => (KEY_BLOB, self.oid(*o), 0),
                Key::Ident(s) => (KEY_IDENT, self.strings.id(s), 0),
                Key::Package(s) => (KEY_PACKAGE, self.strings.id(s), 0),
            };
            x.edges.push(EdgeIds { to, tag, payload, line });
        }
        x.detail = match r.detail {
            Detail::None => DetailIds::None,
            Detail::Crate(c) => DetailIds::Crate(CrateIds {
                dir: self.strings.id(&c.dir),
                description: self.strings.id(&c.description),
                deps: c.deps.iter().map(|(n, cfg)| (self.strings.id(n), self.strings.id(cfg))).collect(),
                dependents: c.dependents.iter().map(|s| self.strings.id(s)).collect(),
                apps: c.apps.iter().map(|s| self.strings.id(s)).collect(),
                targets: c.targets.iter().map(|s| self.strings.id(s)).collect(),
                rs_files: c.rs_files,
                modules: c.modules.iter().map(|s| self.strings.id(s)).collect(),
                private_members: c.private_members,
            }),
            Detail::Commit(c) => DetailIds::Commit { time: c.time, parents: c.parents.iter().map(|p| self.oid(*p)).collect() },
        };
        x.text = r.text.map_or(0, |t| self.strings.id(t) + 1);
        if r.lengths.iter().any(|&l| l >= LONG as u32) {
            x.lengths = Some(r.lengths);
        }

        let extra_at = self.extra.len() as u32;
        encode_extra(&mut self.extra, &x);

        let path = self.strings.id(r.path);
        let name = self.strings.id(r.name);
        let package = k.package.as_deref().map_or(NONE, |p| self.strings.id(p));
        let gist = self.strings.id(r.gist);
        let sig = self.strings.id(r.sig);
        let blob = k.blob.map_or(NONE, |o| self.oid(o));
        let commit = k.commit.map_or(NONE, |o| self.oid(o));
        let c = &mut self.chunks;
        c.push(kind_code(r.kind));
        c.push(flags_code(r.flags));
        c.extend_from_slice(&r.label_len.to_le_bytes());
        for v in [path, name, package, gist, sig, r.span.lo, r.span.hi, r.lines.start, r.lines.end, r.parent.unwrap_or(NONE)] {
            c.extend_from_slice(&v.to_le_bytes());
        }
        for v in r.lengths {
            c.extend_from_slice(&u16::try_from(v).ok().filter(|&v| v < LONG).unwrap_or(LONG).to_le_bytes());
        }
        for v in [blob, commit, extra_at] {
            c.extend_from_slice(&v.to_le_bytes());
        }

        let mut keyed: Vec<(u8, &[u8])> = Vec::with_capacity(k.paths.len() + k.idents.len() + 3);
        keyed.extend(k.paths.iter().map(|p| (KEY_PATH, p.as_bytes())));
        keyed.extend(k.commit.as_ref().map(|o| (KEY_COMMIT, &o.0[..])));
        keyed.extend(k.blob.as_ref().map(|o| (KEY_BLOB, &o.0[..])));
        keyed.extend(k.idents.iter().map(|i| (KEY_IDENT, i.as_bytes())));
        keyed.extend(k.package.as_ref().map(|p| (KEY_PACKAGE, p.as_bytes())));
        for (tag, payload) in keyed {
            self.probe.clear();
            self.probe.push(tag);
            self.probe.extend_from_slice(payload);
            let list = append(&mut self.keys, &self.probe);
            if list.last != Some(id) {
                put_id(&mut list.bytes, list.last, id);
                list.last = Some(id);
                list.count += 1;
            }
        }
        for (i, e) in k.edges.iter().enumerate() {
            let list = append(&mut self.cites, &key_lookup_bytes(&e.key));
            put_varint(&mut list.bytes, (id - list.last.unwrap_or(0)) as u64);
            put_varint(&mut list.bytes, i as u64);
            list.last = Some(id);
            list.count += 1;
        }

        self.stats.chunks += 1;
        for (sum, len) in self.stats.field_lengths.iter_mut().zip(r.lengths) {
            *sum += len as u64;
        }
        self.count += 1;
        Ok(id)
    }

    /// Appends every chunk of `seg`, ids shifted past the ones already here (a merge).
    pub fn append(&mut self, seg: &Segment) -> Result<(), Error> {
        let offset = self.count;
        for i in 0..seg.chunk_count_raw() {
            let id = ChunkId(i);
            let v = seg.view(id);
            let keys = seg.keys_of(id);
            let detail = seg.detail_of(id);
            self.push(RecordIn {
                kind: v.kind,
                flags: v.flags,
                path: v.path,
                label_len: v.label_len,
                name: v.name,
                gist: v.gist,
                sig: v.sig,
                span: v.span,
                lines: v.lines,
                parent: v.parent.map(|p| p.0 + offset),
                lengths: v.field_lengths,
                keys: &keys,
                detail: &detail,
                text: seg.text_of(id),
            })?;
        }
        for (k, postings) in seg.all_terms() {
            let list = append(&mut self.terms, &k);
            for p in postings {
                let id = p.chunk.0 + offset;
                put_posting(&mut list.bytes, list.last, id, &p.tf);
                list.last = Some(id);
                list.count += 1;
            }
        }
        Ok(())
    }

    fn overflow(&self, what: &str) -> Error {
        overflow(&self.key.circle.0, what)
    }

    /// Writes the segment into `dir`: tmp file, fsync, link to `seg-<sha1>.mpx`, tmp removed.
    pub fn write(self, dir: &Path) -> Result<SegmentId, Error> {
        let meta = encode_meta(&self.key, &self.stats, self.count);
        let mut str_offs = Vec::with_capacity(self.strings.offs.len() * 4);
        for o in &self.strings.offs {
            str_offs.extend_from_slice(&o.to_le_bytes());
        }
        let mut oids = Vec::with_capacity(self.oids.len() * 20);
        for o in &self.oids {
            oids.extend_from_slice(&o.0);
        }
        let circle = &self.key.circle.0;
        if self.strings.bytes.len() > u32::MAX as usize || self.extra.len() > u32::MAX as usize {
            return Err(overflow(circle, "strings or extra"));
        }
        let terms = Dict::new(self.terms).ok_or_else(|| overflow(circle, "terms"))?;
        let keys = Dict::new(self.keys).ok_or_else(|| overflow(circle, "keys"))?;
        let cites = Dict::new(self.cites).ok_or_else(|| overflow(circle, "cites"))?;

        let parts: [(u32, Part); 15] = [
            (sec::META, Part::Bytes(&meta)),
            (sec::STR_OFFS, Part::Bytes(&str_offs)),
            (sec::STR_BYTES, Part::Bytes(&self.strings.bytes)),
            (sec::OIDS, Part::Bytes(&oids)),
            (sec::CHUNKS, Part::Bytes(&self.chunks)),
            (sec::EXTRA, Part::Bytes(&self.extra)),
            (sec::TERMS, Part::Bytes(&terms.index)),
            (sec::TERMS + 1, Part::Bytes(&terms.blocks)),
            (sec::TERMS + 2, Part::Lists(&terms.lists)),
            (sec::KEYS, Part::Bytes(&keys.index)),
            (sec::KEYS + 1, Part::Bytes(&keys.blocks)),
            (sec::KEYS + 2, Part::Lists(&keys.lists)),
            (sec::CITES, Part::Bytes(&cites.index)),
            (sec::CITES + 1, Part::Bytes(&cites.blocks)),
            (sec::CITES + 2, Part::Lists(&cites.lists)),
        ];
        let mut header = Vec::with_capacity(HEADER_LEN + parts.len() * TABLE_ENTRY_LEN);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&FORMAT.to_le_bytes());
        header.extend_from_slice(&(parts.len() as u32).to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        let mut at = (HEADER_LEN + parts.len() * TABLE_ENTRY_LEN) as u64;
        for (id, part) in &parts {
            header.extend_from_slice(&id.to_le_bytes());
            header.extend_from_slice(&0u32.to_le_bytes());
            header.extend_from_slice(&at.to_le_bytes());
            header.extend_from_slice(&(part.len() as u64).to_le_bytes());
            at += part.len() as u64;
        }
        let body_len = at as usize;

        let (tmp, file) = tmp_file(dir, "mpx")?;
        let result = (|| {
            let mut out = Out { w: BufWriter::with_capacity(1 << 20, file), h: ObjectHasher::new("blob", body_len), tmp: &tmp };
            out.put(&header)?;
            for (_, part) in &parts {
                match part {
                    Part::Bytes(b) => out.put(b)?,
                    Part::Lists(lists) => {
                        for l in lists.iter() {
                            out.put(&l.bytes)?;
                        }
                    }
                }
            }
            let hash = out.h.finish();
            let mut w = out.w;
            w.write_all(hash.as_bytes()).and_then(|_| w.write_all(FOOTER_MAGIC)).map_err(|e| io_err(&tmp, "write", e))?;
            let file = w.into_inner().map_err(|e| io_err(&tmp, "write", e.into_error()))?;
            file.sync_all().map_err(|e| io_err(&tmp, "fsync", e))?;
            Ok::<_, Error>(format!("seg-{}.mpx", hash.to_hex()))
        })();
        let name = match result {
            Ok(name) => name,
            Err(e) => {
                remove_tmp(&tmp);
                return Err(e);
            }
        };
        publish(&tmp, &dir.join(&name))?;
        Ok(SegmentId(name))
    }
}

fn overflow(circle: &str, what: &str) -> Error {
    Error::Corrupt { path: PathBuf::from(circle), message: format!("segment too large: {what} exceed the u32 offsets of the format") }
}

enum Part<'a> {
    Bytes(&'a [u8]),
    Lists(&'a [List]),
}

impl Part<'_> {
    fn len(&self) -> usize {
        match self {
            Part::Bytes(b) => b.len(),
            Part::Lists(l) => l.iter().map(|l| l.bytes.len()).sum(),
        }
    }
}

/// A sorted, front-coded dictionary ready to write, each distinct list stored once; `None`
/// when an offset overflows u32.
struct Dict {
    index: Vec<u8>,
    blocks: Vec<u8>,
    lists: Vec<List>,
}

impl Dict {
    fn new(map: FxMap<Vec<u8>, List>) -> Option<Dict> {
        let mut items: Vec<(Vec<u8>, List)> = map.into_iter().collect();
        items.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut at: FxMap<&[u8], u32> = FxMap::default();
        let mut order: Vec<usize> = Vec::new();
        let mut entries: Vec<(&[u8], u32, u32)> = Vec::with_capacity(items.len());
        let mut end = 0u64;
        for (i, (k, list)) in items.iter().enumerate() {
            let off = match at.get(list.bytes.as_slice()) {
                Some(&off) => off,
                None => {
                    let off = u32::try_from(end).ok()?;
                    at.insert(&list.bytes, off);
                    order.push(i);
                    end += list.bytes.len() as u64;
                    off
                }
            };
            entries.push((k, list.count, off));
        }
        u32::try_from(end).ok()?;
        let (index, blocks) = encode_dict(&entries);
        u32::try_from(blocks.len()).ok()?;
        drop((at, entries));
        let mut keep = vec![false; items.len()];
        for i in order {
            keep[i] = true;
        }
        let lists = items.into_iter().zip(keep).filter(|(_, k)| *k).map(|((_, l), _)| l).collect();
        Some(Dict { index, blocks, lists })
    }
}

struct Out<'a> {
    w: BufWriter<File>,
    h: ObjectHasher,
    tmp: &'a Path,
}

impl Out<'_> {
    fn put(&mut self, b: &[u8]) -> Result<(), Error> {
        self.h.update(b);
        self.w.write_all(b).map_err(|e| io_err(self.tmp, "write", e))
    }
}

/// [`SegmentSink`] over a [`Builder`], publishing into its circle directory.
pub struct SegmentWriter {
    pub builder: Builder,
    pub dir: PathBuf,
}

impl SegmentSink for SegmentWriter {
    fn add(&mut self, chunk: &Chunk, terms: &FieldTerms) -> Result<ChunkId, Error> {
        self.builder.add(chunk, terms)
    }
    fn finish(self: Box<Self>) -> Result<SegmentId, Error> {
        let this = *self;
        this.builder.write(&this.dir)
    }
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A new, uniquely named `tmp-<pid>-<seq>.<ext>.tmp` in `dir`.
pub fn tmp_file(dir: &Path, ext: &str) -> Result<(PathBuf, File), Error> {
    loop {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("tmp-{}-{seq}.{ext}.tmp", std::process::id()));
        match File::options().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((path, f)),
            // a crashed process with this pid left it; gc removes it after the grace period
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_err(&path, "create", e)),
        }
    }
}

/// Links a fsynced tmp file to its final name if absent (an existing file of a content name
/// holds the same bytes; a manifest's caller handles [`Published::Exists`]), then drops the
/// tmp name.
pub fn publish(tmp: &Path, to: &Path) -> Result<Published, Error> {
    let r = match fs::hard_link(tmp, to) {
        Ok(()) => {
            sync_dir(to);
            Ok(Published::Linked)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(Published::Exists),
        Err(e) => Err(io_err(to, "link", e)),
    };
    remove_tmp(tmp);
    r
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Published {
    Linked,
    Exists,
}

pub fn remove_tmp(tmp: &Path) {
    if let Err(e) = fs::remove_file(tmp) {
        log(&format!("could not remove {}: {e}; gc removes it after the grace period", tmp.display()));
    }
}

/// Makes a new directory entry durable (POSIX). Windows has no directory handle to sync;
/// NTFS journals the link itself.
pub fn sync_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        if let Err(e) = File::open(dir).and_then(|d| d.sync_all()) {
            log(&format!("fsync of {} failed: {e}; the new entry {} may not survive a crash", dir.display(), path.display()));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Writes `bytes` to a tmp file in `dir`, fsyncs it and links it to `to` if absent.
pub fn put_if_absent(dir: &Path, to: &Path, bytes: &[u8]) -> Result<Published, Error> {
    let (tmp, mut file) = tmp_file(dir, "put")?;
    if let Err(e) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        remove_tmp(&tmp);
        return Err(io_err(&tmp, "write", e));
    }
    drop(file);
    publish(&tmp, to)
}

/// The exclusive lock on a base key while it is built. Held through the OS (`flock`,
/// `LockFileEx`), so a waiter blocks without polling and a crashed holder releases it. It only
/// dedupes work: a base is content-named and published put-if-absent, so two builders racing
/// past it produce one file.
pub struct BuildLock {
    _file: File,
}

impl BuildLock {
    pub fn acquire(path: &Path, what: &str) -> Result<BuildLock, Error> {
        let file = File::options().read(true).write(true).create(true).truncate(false).open(path).map_err(|e| io_err(path, "open lock", e))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                log(&format!("waiting for another process building {what}"));
                file.lock().map_err(|e| io_err(path, "lock", e))?;
            }
            Err(fs::TryLockError::Error(e)) => return Err(io_err(path, "lock", e)),
        }
        Ok(BuildLock { _file: file })
    }
}
