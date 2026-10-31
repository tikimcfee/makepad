//! [`crate::SegmentRead`] over a mapped segment (`makepad_shared_bytes`). Opening checks the
//! structure (magic, format, section bounds, the footer against the file's content name), not
//! every byte: a segment is published only after it is fsynced, so a reader never sees a
//! partial one. [`Segment::verify`] rehashes the whole body. A read that runs into bytes the
//! checks did not cover logs the corruption and yields nothing for it.

use super::segment::*;
use super::{io_err, log};
use crate::{ChunkId, ChunkView, CircleStats, Detail, Edge, Error, Key, Keys, Kind, Lines, Oid, Posting, SegmentId, SegmentKey, SegmentRead, Span, TermForm};
use makepad_git::oid::ObjectHasher;
use makepad_git::ObjectId;
use makepad_shared_bytes::{MappedBytes, SharedBytes};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Segment {
    path: PathBuf,
    bytes: SharedBytes,
    key: SegmentKey,
    stats: CircleStats,
    count: u32,
    /// (offset, len) per section id.
    sections: [(usize, usize); 16],
}

/// Postings of one term, decoded as iterated.
pub struct Postings<'a> {
    r: Reader<'a>,
    prev: Option<u32>,
    left: u32,
    seg: &'a Segment,
}

impl Iterator for Postings<'_> {
    type Item = Posting;
    fn next(&mut self) -> Option<Posting> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        match get_posting(&mut self.r, self.prev) {
            Some((chunk, tf)) if chunk < self.seg.count => {
                self.prev = Some(chunk);
                Some(Posting { chunk: ChunkId(chunk), tf })
            }
            _ => {
                self.seg.corrupt("a postings list runs past its bounds");
                self.left = 0;
                None
            }
        }
    }
}

fn corrupt(path: &Path, message: &str) -> Error {
    Error::Corrupt { path: path.to_path_buf(), message: message.to_string() }
}

impl Segment {
    /// Maps the segment (reading it into memory, logged, where mapping fails) and checks its
    /// structure.
    pub fn open(path: &Path) -> Result<Segment, Error> {
        let bytes = match MappedBytes::open(path) {
            Ok(m) => SharedBytes::from_mapped(Arc::new(m)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(io_err(path, "open", e)),
            Err(e) => {
                log(&format!("mapping {} failed ({e}); reading it into memory", path.display()));
                SharedBytes::from_file(path).map_err(|e| io_err(path, "read", e))?
            }
        };
        Self::from_bytes(path, bytes)
    }

    fn from_bytes(path: &Path, bytes: SharedBytes) -> Result<Segment, Error> {
        let b = bytes.as_slice();
        let bad = |m: &str| corrupt(path, m);
        if b.len() < HEADER_LEN + FOOTER_LEN || &b[0..4] != MAGIC {
            return Err(bad("not a segment (no MPXS header)"));
        }
        let format = le_u32(b, 4);
        if format != FORMAT {
            return Err(bad(&format!("segment format {format}, this indexer reads {FORMAT}")));
        }
        let body_len = b.len() - FOOTER_LEN;
        if &b[b.len() - 4..] != FOOTER_MAGIC {
            return Err(bad("no MPXE footer (truncated)"));
        }
        if let Some(hex) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix("seg-")).and_then(|n| n.strip_suffix(".mpx")) {
            let footer = ObjectId::from_bytes(b[body_len..body_len + 20].try_into().unwrap()).to_hex();
            if footer != hex {
                return Err(bad(&format!("footer hash {footer} differs from the file name")));
            }
        }
        let n = le_u32(b, 8) as usize;
        let table_end = HEADER_LEN + n * TABLE_ENTRY_LEN;
        if n != sec::ALL.len() || table_end > body_len {
            return Err(bad("section table out of bounds"));
        }
        let mut sections = [(usize::MAX, 0usize); 16];
        let mut r = Reader::at(b, HEADER_LEN);
        for _ in 0..n {
            let (id, _, off, len) = (r.u32(), r.u32(), r.u64(), r.u64());
            let (Some(id), Some(off), Some(len)) = (id, off, len) else { return Err(bad("section table out of bounds")) };
            let (off, len) = (off as usize, len as usize);
            if !sec::ALL.contains(&id) || sections[id as usize].0 != usize::MAX || off < table_end || off.checked_add(len).is_none_or(|e| e > body_len) {
                return Err(bad(&format!("section {id} is unknown, repeated or out of bounds")));
            }
            sections[id as usize] = (off, len);
        }
        let s = |id: u32| &b[sections[id as usize].0..sections[id as usize].0 + sections[id as usize].1];
        let (key, stats, count) = decode_meta(s(sec::META)).ok_or_else(|| bad("META does not decode"))?;
        if s(sec::CHUNKS).len() != count as usize * CHUNK_LEN || stats.chunks != count as u64 {
            return Err(bad("chunk table length differs from the chunk count"));
        }
        let offs = s(sec::STR_OFFS);
        if offs.len() < 8 || offs.len() % 4 != 0 || le_u32(offs, 0) != 0 || le_u32(offs, offs.len() - 4) as usize != s(sec::STR_BYTES).len() {
            return Err(bad("string offsets do not span the string bytes"));
        }
        if s(sec::OIDS).len() % 20 != 0 {
            return Err(bad("oid table length is not a multiple of 20"));
        }
        for d in [sec::TERMS, sec::KEYS, sec::CITES] {
            if DictView::new(s(d), s(d + 1), s(d + 2)).is_none() {
                return Err(bad(&format!("dictionary {d}'s index does not span its blocks")));
            }
        }
        Ok(Segment { path: path.to_path_buf(), bytes, key, stats, count, sections })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Rehashes the body against the footer: every byte, not just the structure.
    pub fn verify(&self) -> Result<(), Error> {
        let b = self.bytes.as_slice();
        let body = &b[..b.len() - FOOTER_LEN];
        let mut h = ObjectHasher::new("blob", body.len());
        h.update(body);
        if h.finish().as_bytes()[..] != b[body.len()..body.len() + 20] {
            return Err(corrupt(&self.path, "body does not match its footer hash"));
        }
        Ok(())
    }

    fn corrupt(&self, what: &str) {
        log(&format!("corrupt segment {}: {what}; `makepad index gc` and a rebuild replace it", self.path.display()));
    }

    fn sec(&self, id: u32) -> &[u8] {
        let (off, len) = self.sections[id as usize];
        &self.bytes.as_slice()[off..off + len]
    }

    fn dict(&self, d: u32) -> DictView<'_> {
        DictView::new(self.sec(d), self.sec(d + 1), self.sec(d + 2)).expect("checked at open")
    }

    fn str(&self, id: u32) -> &str {
        let offs = self.sec(sec::STR_OFFS);
        let at = id as usize * 4;
        if at + 8 > offs.len() {
            self.corrupt(&format!("string {id} out of range"));
            return "";
        }
        let (lo, hi) = (le_u32(offs, at) as usize, le_u32(offs, at + 4) as usize);
        match self.sec(sec::STR_BYTES).get(lo..hi).map(std::str::from_utf8) {
            Some(Ok(s)) => s,
            _ => {
                self.corrupt(&format!("string {id} out of bounds or not UTF-8"));
                ""
            }
        }
    }


    fn oid(&self, id: u32) -> Option<Oid> {
        if id == NONE {
            return None;
        }
        let o = self.sec(sec::OIDS).get(id as usize * 20..id as usize * 20 + 20);
        if o.is_none() {
            self.corrupt(&format!("oid {id} out of range"));
        }
        o.map(|o| Oid(o.try_into().unwrap()))
    }

    fn record(&self, id: ChunkId) -> &[u8] {
        assert!(id.0 < self.count, "chunk {} of a segment holding {}", id.0, self.count);
        let at = id.0 as usize * CHUNK_LEN;
        &self.sec(sec::CHUNKS)[at..at + CHUNK_LEN]
    }

    fn extra(&self, id: ChunkId) -> ExtraIds {
        let at = le_u32(self.record(id), rec::EXTRA) as usize;
        match decode_extra(&mut Reader::at(self.sec(sec::EXTRA), at)) {
            Some(x) => x,
            None => {
                self.corrupt(&format!("chunk {}'s keys do not decode", id.0));
                ExtraIds::default()
            }
        }
    }

    pub fn chunk_count_raw(&self) -> u32 {
        self.count
    }

    pub fn view(&self, id: ChunkId) -> ChunkView<'_> {
        let r = self.record(id);
        let u = |at| le_u32(r, at);
        let kind = kind_of(r[rec::KIND]).unwrap_or_else(|| {
            self.corrupt(&format!("chunk {} has kind code {}", id.0, r[rec::KIND]));
            Kind::File
        });
        let mut field_lengths = [0u32; 5];
        let mut long = false;
        for (i, l) in field_lengths.iter_mut().enumerate() {
            let v = u16::from_le_bytes([r[rec::LENGTHS + i * 2], r[rec::LENGTHS + i * 2 + 1]]);
            long |= v == LONG;
            *l = v as u32;
        }
        if long {
            match self.extra(id).lengths {
                Some(exact) => field_lengths = exact,
                None => self.corrupt(&format!("chunk {} has a long field length but no exact one", id.0)),
            }
        }
        let package = u(rec::PACKAGE);
        let parent = u(rec::PARENT);
        let path = self.str(u(rec::PATH));
        let mut label_len = u16::from_le_bytes([r[rec::LABEL_LEN], r[rec::LABEL_LEN + 1]]);
        if label_len != 0 && path.as_bytes().get(label_len as usize) != Some(&b'/') {
            self.corrupt(&format!("chunk {}'s label length {label_len} does not fit its path", id.0));
            label_len = 0;
        }
        ChunkView {
            kind,
            path,
            label_len,
            span: Span { lo: u(rec::SPAN), hi: u(rec::SPAN + 4) },
            lines: Lines { start: u(rec::LINES), end: u(rec::LINES + 4) },
            name: self.str(u(rec::NAME)),
            parent: (parent != NONE).then_some(ChunkId(parent)),
            package: if package == NONE { "" } else { self.str(package) },
            gist: self.str(u(rec::GIST)),
            sig: self.str(u(rec::SIG)),
            blob: self.oid(u(rec::BLOB)),
            commit: self.oid(u(rec::COMMIT)),
            flags: flags_of(r[rec::FLAGS]),
            field_lengths,
        }
    }

    fn oid_or_zero(&self, id: u32) -> Oid {
        self.oid(id).unwrap_or_default()
    }

    pub fn keys_of(&self, id: ChunkId) -> Keys {
        let r = self.record(id);
        let x = self.extra(id);
        let s = |i: u32| self.str(i).to_string();
        let o = |i: u32| self.oid_or_zero(i);
        let package = le_u32(r, rec::PACKAGE);
        Keys {
            paths: x.paths.iter().map(|&i| s(i)).collect(),
            commit: self.oid(le_u32(r, rec::COMMIT)),
            blob: self.oid(le_u32(r, rec::BLOB)),
            idents: x.idents.iter().map(|&i| s(i)).collect(),
            package: (package != NONE).then(|| s(package)),
            edges: x.edges.iter().filter_map(|&e| self.edge(e, &s, &o)).collect(),
        }
    }

    fn edge(&self, e: EdgeIds, s: &dyn Fn(u32) -> String, o: &dyn Fn(u32) -> Oid) -> Option<Edge> {
        let edge = edge_from_ids(e, s, o);
        if edge.is_none() {
            self.corrupt(&format!("edge key tag {}", e.tag));
        }
        edge
    }

    pub fn detail_of(&self, id: ChunkId) -> Detail {
        let x = self.extra(id);
        detail_from_ids(&x.detail, &|i| self.str(i).to_string(), &|i| self.oid_or_zero(i))
    }

    pub fn text_of(&self, id: ChunkId) -> Option<&str> {
        let t = self.extra(id).text;
        (t != 0).then(|| self.str(t - 1))
    }

    fn postings_of<'a>(&'a self, value: &'a [u8], count: u32) -> Postings<'a> {
        Postings { r: Reader::new(value), prev: None, left: count, seg: self }
    }

    /// Every term (form byte and text) with its postings, in key order (merges).
    pub fn all_terms(&self) -> Vec<(Vec<u8>, Postings<'_>)> {
        let d = self.dict(sec::TERMS);
        let all: Vec<_> = d.iter().map(|(k, v, n)| (k, self.postings_of(v, n))).collect();
        if all.len() != d.len() {
            self.corrupt(&format!("term entry {} does not decode", all.len()));
        }
        all
    }

    fn ids(&self, value: &[u8], count: u32) -> Vec<ChunkId> {
        let mut r = Reader::new(value);
        let mut out = Vec::with_capacity(count as usize);
        let mut prev = None;
        for _ in 0..count {
            match get_id(&mut r, prev) {
                Some(id) if id < self.count => {
                    out.push(ChunkId(id));
                    prev = Some(id);
                }
                _ => {
                    self.corrupt("a key's chunk list runs past its bounds");
                    break;
                }
            }
        }
        out
    }
}

fn term_key(form: TermForm, term: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + term.len());
    k.push(form_code(form));
    k.extend_from_slice(term.as_bytes());
    k
}

impl SegmentRead for Segment {
    fn key(&self) -> &SegmentKey {
        &self.key
    }
    fn id(&self) -> SegmentId {
        SegmentId(self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
    }
    fn stats(&self) -> CircleStats {
        self.stats
    }
    fn chunk_count(&self) -> u32 {
        self.count
    }
    fn df(&self, form: TermForm, term: &str) -> u32 {
        self.dict(sec::TERMS).find(&term_key(form, term)).map_or(0, |(_, n)| n)
    }
    fn postings(&self, form: TermForm, term: &str) -> Box<dyn Iterator<Item = Posting> + '_> {
        match self.dict(sec::TERMS).find(&term_key(form, term)) {
            Some((v, n)) => Box::new(self.postings_of(v, n)),
            None => Box::new(std::iter::empty()),
        }
    }
    fn terms_with_prefix(&self, form: TermForm, prefix: &str) -> Vec<String> {
        let probe = term_key(form, prefix);
        let mut out = Vec::new();
        for (k, _, _) in self.dict(sec::TERMS).with_prefix(&probe) {
            match String::from_utf8(k[1..].to_vec()) {
                Ok(t) => out.push(t),
                Err(_) => self.corrupt("a term is not UTF-8"),
            }
        }
        out
    }
    fn chunk(&self, id: ChunkId) -> ChunkView<'_> {
        self.view(id)
    }
    fn keys(&self, id: ChunkId) -> Keys {
        self.keys_of(id)
    }
    fn detail(&self, id: ChunkId) -> Detail {
        self.detail_of(id)
    }
    /// A `Path` with a line names the chunks whose own path it is and whose lines hold it.
    fn chunks_with(&self, key: &Key) -> Vec<ChunkId> {
        let Some((v, n)) = self.dict(sec::KEYS).find(&key_lookup_bytes(key)) else { return Vec::new() };
        let ids = self.ids(v, n);
        match key {
            Key::Path { path, line: Some(l) } => ids
                .into_iter()
                .filter(|&id| {
                    let c = self.view(id);
                    c.path == path && c.lines.start <= *l && *l <= c.lines.end
                })
                .collect(),
            _ => ids,
        }
    }
    /// A `Path` without a line matches edges to any line of it; with one, edges to that line.
    fn cited_by(&self, key: &Key) -> Vec<(ChunkId, Edge)> {
        let Some((v, n)) = self.dict(sec::CITES).find(&key_lookup_bytes(key)) else { return Vec::new() };
        let mut r = Reader::new(v);
        let mut out = Vec::new();
        let mut prev = 0u32;
        for _ in 0..n {
            let (Some(gap), Some(i)) = (r.varint_u32(), r.varint()) else {
                self.corrupt("a cite list runs past its bounds");
                break;
            };
            let Some(chunk) = prev.checked_add(gap).filter(|&c| c < self.count) else {
                self.corrupt("a cite names a chunk out of range");
                break;
            };
            prev = chunk;
            let id = ChunkId(chunk);
            let Some(edge) = self.keys_of(id).edges.into_iter().nth(i as usize) else {
                self.corrupt("a cite names an edge out of range");
                continue;
            };
            let keep = match (key, &edge.key) {
                (Key::Path { line: Some(want), .. }, Key::Path { line, .. }) => *line == Some(*want),
                _ => true,
            };
            if keep {
                out.push((id, edge));
            }
        }
        out
    }
    fn record_text(&self, id: ChunkId) -> Option<&str> {
        self.text_of(id)
    }
}
