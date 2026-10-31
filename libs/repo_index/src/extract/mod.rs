//! Chunks from a base circle's sources ([`crate::Extract`]): Rust items, Cargo crate cards,
//! Markdown sections, splash definitions, commits and notes, all filtered by the circle's
//! exclusions before anything is read.
//!
//! A tree circle streams, per package (by directory): its crate card, then each of its files
//! (`.rs` as a file chunk and its items, `.md` as sections, `.splash` as definitions); then the
//! files no package owns. Files of a crates.io port or with a generated header are skeletons.
//! Every chunk of a file carries that file's blob, so a hit's copies are found by
//! (blob, span). Chunks outside the circle's families are not emitted, and a parent that is not
//! emitted becomes the next emitted ancestor's index (or none).
//!
//! [`Extract::extract_file`] (the overlay) has no tree around the file: its chunks carry no
//! package, and a `Cargo.toml` becomes a card with its dependencies as written (no dependents,
//! no apps) until the base is rebuilt.

pub mod cargo;
pub mod git;
pub mod history;
pub mod markdown;
pub mod notes;
pub mod rust;
pub mod splash;

use crate::{
    BaseCircle, BaseSource, Chunk, CircleName, CommitRecord, CrateCard, Detail, Error, Exclusion, Extract, ExtractReport, Family, Fields,
    Flags, Keys, Kind, Lines, Oid, Repo, Rev, RevSpec, Span,
};
use cargo::{Graph, APPS_JSON};
use notes::Cite;
use std::collections::BTreeMap;

/// The [`Extract`] implementation. With a [`Cite`], notes circles record edges to that circle.
#[derive(Default)]
pub struct Extractor {
    pub cite: Option<Cite>,
}

/// A chunk whose `parent` indexes its own batch.
type Batch = Vec<Chunk>;

fn excluded<'a>(path: &str, exclude: &'a [Exclusion]) -> Option<&'a str> {
    exclude.iter().find(|e| path.starts_with(e.prefix.as_str())).map(|e| e.prefix.as_str())
}

fn count_excluded(report: &mut ExtractReport, prefix: &str) {
    match report.excluded.iter_mut().find(|(p, _)| p == prefix) {
        Some((_, n)) => *n += 1,
        None => report.excluded.push((prefix.to_string(), 1)),
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn ext(path: &str) -> &str {
    let name = file_name(path);
    name.rfind('.').map_or("", |i| &name[i + 1..])
}

/// A chunk with the common fields filled; callers set the rest.
fn base_chunk(kind: Kind, circle: &CircleName, path: &str, name: &str) -> Chunk {
    Chunk {
        kind,
        circle: circle.clone(),
        path: path.to_string(),
        span: Span::default(),
        lines: Lines::default(),
        name: name.to_string(),
        parent: None,
        fields: Fields { name: name.to_string(), path: path.to_string(), ..Default::default() },
        keys: Keys { paths: vec![path.to_string()], ..Default::default() },
        flags: Flags::default(),
        detail: Detail::None,
        gist: String::new(),
        label_len: 0,
    }
}

/// The first sentence of a text's first paragraph (Markdown heading, quote and bullet markers
/// dropped), at most [`crate::MAX_GIST_CHARS`]: a hit's gist, never generated.
pub fn gist(text: &str) -> String {
    let mut para = String::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("```") || t.starts_with("~~~") {
            if para.is_empty() {
                continue;
            }
            break;
        }
        let t = t.trim_start_matches('#').trim_start_matches('>').trim_start();
        let t = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).unwrap_or(t);
        if !para.is_empty() {
            para.push(' ');
        }
        para.push_str(t.trim());
    }
    let end = para.find(". ").map_or(para.len(), |i| i + 1);
    let sentence = para[..end].trim();
    if sentence.chars().count() <= crate::MAX_GIST_CHARS {
        return sentence.to_string();
    }
    let mut cut: String = sentence.chars().take(crate::MAX_GIST_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// The chunks of a batch the families admit, parents remapped within the result: a dropped
/// chunk's children attach to its own (remapped) parent.
fn admit(batch: Batch, families: &[Family]) -> Batch {
    let mut attach: Vec<Option<u32>> = Vec::with_capacity(batch.len());
    let mut out = Vec::new();
    for mut c in batch {
        let parent = c.parent.and_then(|p| attach.get(p as usize).copied().flatten());
        if families.contains(&c.kind.family()) {
            c.parent = parent;
            attach.push(Some(out.len() as u32));
            out.push(c);
        } else {
            attach.push(parent);
        }
    }
    out
}

/// Emits batches into the stream, numbering chunks as the sink will.
struct Emit<'s> {
    families: &'s [Family],
    sink: &'s mut dyn FnMut(Chunk) -> Result<(), Error>,
    next: u32,
}

impl Emit<'_> {
    /// Emits `batch` with `outer` as the parent of its parentless chunks; the stream index of
    /// its first emitted chunk.
    fn batch(&mut self, batch: Batch, outer: Option<u32>) -> Result<Option<u32>, Error> {
        let base = self.next;
        let admitted = admit(batch, self.families);
        let first = (!admitted.is_empty()).then_some(base);
        for mut c in admitted {
            c.parent = match c.parent {
                Some(p) => Some(base + p),
                None => outer,
            };
            (self.sink)(c)?;
            self.next += 1;
        }
        Ok(first)
    }
}

/// A `.rs` file: its file chunk (index 0) and items.
fn rust_batch(circle: &CircleName, path: &str, bytes: &[u8], blob: Option<Oid>, package: Option<&str>, generated: bool, report: &mut ExtractReport) -> (Batch, Vec<String>) {
    let scanned = rust::scan(bytes, generated);
    let mut file = base_chunk(Kind::File, circle, path, file_name(path));
    file.fields.name = file_name(path).trim_end_matches(".rs").to_string();
    file.fields.doc = scanned.header.clone();
    file.span = Span { lo: 0, hi: bytes.len() as u32 };
    file.lines = Lines { start: 1, end: scanned.lines.max(1) };
    file.keys.blob = blob;
    file.keys.package = package.map(str::to_string);
    file.flags.generated = generated;
    file.flags.documented = !scanned.header.is_empty();
    file.gist = gist(&scanned.header);
    if let Some(reason) = &scanned.fallback {
        file.flags.fallback = true;
        file.fields.body = rust::identifier_bag(bytes);
        report.fallbacks.push((path.to_string(), reason.clone()));
        return (vec![file], Vec::new());
    }
    let top: Vec<&str> = scanned.items.iter().filter(|i| i.parent.is_none()).map(|i| i.name.as_str()).collect();
    file.fields.body = top.join(" ");
    let mut batch = vec![file];
    for item in scanned.items {
        let mut c = base_chunk(item.kind, circle, path, &item.name);
        c.span = item.span;
        c.lines = item.lines;
        c.parent = Some(item.parent.map_or(0, |p| p as u32 + 1));
        c.fields.sig = item.sig;
        c.gist = gist(&item.doc);
        c.fields.doc = item.doc;
        c.fields.body = item.body;
        if !item.ident.is_empty() {
            c.keys.idents = vec![item.ident];
        }
        c.keys.blob = blob;
        c.keys.package = package.map(str::to_string);
        c.flags = Flags {
            public: item.public,
            documented: !c.fields.doc.is_empty(),
            definition: item.kind != Kind::Impl,
            generated,
            ..Default::default()
        };
        batch.push(c);
    }
    (batch, scanned.mods)
}

fn doc_batch(circle: &CircleName, path: &str, bytes: &[u8], blob: Option<Oid>, package: Option<&str>) -> Batch {
    let text = String::from_utf8_lossy(bytes);
    markdown::doc_sections(&text)
        .into_iter()
        .map(|s| {
            let name = if s.heading.is_empty() { file_name(path).to_string() } else { s.heading.clone() };
            let mut c = base_chunk(Kind::Doc, circle, path, &name);
            c.span = s.span;
            c.lines = s.lines;
            // the section's prose, its heading line left to the name
            c.fields.doc = if s.heading.is_empty() { s.text } else { s.text.split_once('\n').map_or(String::new(), |(_, rest)| rest.to_string()) };
            c.gist = match gist(&c.fields.doc) {
                g if g.is_empty() => gist(&s.heading),
                g => g,
            };
            c.keys.blob = blob;
            c.keys.package = package.map(str::to_string);
            c.flags.documented = true;
            c
        })
        .collect()
}

fn splash_batch(circle: &CircleName, path: &str, bytes: &[u8], blob: Option<Oid>, package: Option<&str>) -> Batch {
    let text = String::from_utf8_lossy(bytes);
    splash::defs(&text)
        .into_iter()
        .map(|d| {
            let mut c = base_chunk(Kind::Splash, circle, path, &d.name);
            c.span = d.span;
            c.lines = d.lines;
            c.fields.sig = d.sig;
            c.fields.body = d.text;
            c.keys.idents = vec![d.name.rsplit('.').next().unwrap_or(&d.name).to_string()];
            c.keys.blob = blob;
            c.keys.package = package.map(str::to_string);
            c.flags.definition = true;
            c
        })
        .collect()
}

fn crate_chunk(circle: &CircleName, name: &str, card: CrateCard, vendored: bool) -> Chunk {
    let manifest = if card.dir.is_empty() { "Cargo.toml".to_string() } else { format!("{}/Cargo.toml", card.dir) };
    let mut c = base_chunk(Kind::Crate, circle, &manifest, name);
    c.fields.path = card.dir.clone();
    c.fields.doc = card.description.clone();
    c.gist = gist(&card.description);
    c.keys.package = Some(name.to_string());
    c.flags.documented = !card.description.is_empty();
    c.flags.definition = true;
    c.flags.generated = vendored;
    c.detail = Detail::Crate(card);
    c
}

fn note_batch(circle: &CircleName, label: &str, rel: &str, bytes: &[u8], cite: Option<&Cite>) -> Result<Batch, Error> {
    let path = format!("{label}/{rel}");
    let label_len = u16::try_from(label.len()).map_err(|_| Error::Parse { file: path.clone(), line: 0, message: "a notes label longer than 65535 bytes".into() })?;
    let blob = git::blob_oid(bytes);
    Ok(notes::records(rel, bytes)
        .into_iter()
        .map(|r| {
            let mut c = base_chunk(if r.section { Kind::Note } else { Kind::NoteFile }, circle, &path, &r.name);
            c.span = r.span;
            c.lines = r.lines;
            // the path within the source, without the label (the pointer rule matches it)
            c.fields.path = rel.to_string();
            c.label_len = label_len;
            if r.section {
                let prose = r.text.split_once('\n').map_or("", |(_, rest)| rest);
                c.gist = match gist(prose) {
                    g if g.is_empty() => gist(&r.name),
                    g => g,
                };
            }
            c.keys.blob = Some(blob);
            c.keys.edges = cite.map(|k| k.edges(&r.text)).unwrap_or_default();
            c.flags.binary = r.binary;
            c.flags.documented = r.section;
            c.fields.body = r.text;
            c
        })
        .collect())
}

fn commit_batch(circle: &CircleName, rec: CommitRecord, touched: Vec<String>) -> Batch {
    let subject = rec.message.lines().next().unwrap_or("").trim().to_string();
    let mut c = base_chunk(Kind::Commit, circle, "", &subject);
    c.fields.path = touched.join("\n");
    c.fields.body = rec.message.clone();
    c.keys.paths = touched;
    c.keys.commit = Some(rec.oid);
    c.detail = Detail::Commit(rec.info);
    let mut batch = vec![c];
    for b in history::bullets(&rec.message) {
        let mut bc = base_chunk(Kind::Bullet, circle, "", &b.head);
        bc.keys.paths = Vec::new();
        bc.keys.commit = Some(rec.oid);
        bc.fields.body = b.text;
        bc.parent = Some(0);
        batch.push(bc);
    }
    batch
}

impl Extractor {
    pub fn new() -> Extractor {
        Extractor::default()
    }

    fn tree(&mut self, circle: &CircleName, def: &BaseCircle, rev: &Rev, repo: &mut dyn Repo, emit: &mut Emit, report: &mut ExtractReport) -> Result<(), Error> {
        let mut files = Vec::new();
        for f in repo.files(rev.tree)? {
            match excluded(&f.path, &def.exclude) {
                Some(prefix) => count_excluded(report, prefix),
                None => files.push(f),
            }
        }
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        let mut manifests = Vec::new();
        let mut apps = None;
        for f in &files {
            if file_name(&f.path) == "Cargo.toml" {
                manifests.push((f.path.clone(), String::from_utf8_lossy(&repo.read_blob(f.blob)?).into_owned()));
            } else if f.path == APPS_JSON {
                apps = Some(String::from_utf8_lossy(&repo.read_blob(f.blob)?).into_owned());
            }
        }
        let graph = Graph::build(&manifests, &paths, apps.as_deref());
        report.skipped.extend(graph.errors.iter().cloned());
        let mut groups: BTreeMap<(bool, usize), Vec<usize>> = BTreeMap::new();
        let by_dir: BTreeMap<&str, usize> = graph.packages.iter().enumerate().map(|(i, p)| (p.dir.as_str(), i)).collect();
        let order: BTreeMap<usize, usize> = by_dir.values().enumerate().map(|(rank, &i)| (i, rank)).collect();
        for (fi, f) in files.iter().enumerate() {
            if !matches!(ext(&f.path), "rs" | "md" | "splash") {
                continue;
            }
            let key = match graph.package_of(&f.path) {
                Some(i) => (false, order[&i]),
                None => (true, 0),
            };
            groups.entry(key).or_default().push(fi);
        }
        // packages with no content files still get their card
        for &rank in order.values() {
            groups.entry((false, rank)).or_default();
        }
        let by_rank: Vec<usize> = by_dir.values().copied().collect();
        for ((unowned, rank), members) in groups {
            let pkg = (!unowned).then(|| by_rank[rank]);
            let package = pkg.map(|i| graph.packages[i].name.clone());
            let vendored = pkg.is_some_and(|i| graph.packages[i].vendored);
            let mut batches = Vec::with_capacity(members.len());
            let mut modules = Vec::new();
            for fi in members {
                let f = &files[fi];
                let bytes = repo.read_blob(f.blob)?;
                report.files += 1;
                let batch = match ext(&f.path) {
                    "rs" => {
                        let generated = vendored || rust::is_generated(&bytes);
                        let (b, mods) = rust_batch(circle, &f.path, &bytes, Some(f.blob), package.as_deref(), generated, report);
                        if pkg.is_some_and(|i| graph.packages[i].roots.first() == Some(&f.path)) {
                            modules = mods;
                        }
                        b
                    }
                    "md" => doc_batch(circle, &f.path, &bytes, Some(f.blob), package.as_deref()),
                    _ => splash_batch(circle, &f.path, &bytes, Some(f.blob), package.as_deref()),
                };
                batches.push(batch);
            }
            let card = match pkg {
                Some(i) => emit.batch(vec![crate_chunk(circle, &graph.packages[i].name, graph.card(i, modules), vendored)], None)?,
                None => None,
            };
            for b in batches {
                emit.batch(b, card)?;
            }
        }
        Ok(())
    }

    fn notes(&mut self, circle: &CircleName, def: &BaseCircle, sources: &[crate::NotesSource], emit: &mut Emit, report: &mut ExtractReport) -> Result<(), Error> {
        for src in sources {
            if !src.dir.is_dir() {
                report.skipped.push((src.label.clone(), format!("notes directory {} is missing", src.dir.display())));
                continue;
            }
            for (rel, full) in notes::walk(&src.dir)? {
                if let Some(prefix) = excluded(&rel, &def.exclude) {
                    count_excluded(report, prefix);
                    continue;
                }
                let bytes = match std::fs::read(&full) {
                    Ok(b) => b,
                    Err(e) => {
                        report.skipped.push((format!("{}/{rel}", src.label), e.to_string()));
                        continue;
                    }
                };
                report.files += 1;
                emit.batch(note_batch(circle, &src.label, &rel, &bytes, self.cite.as_ref())?, None)?;
            }
        }
        Ok(())
    }

    fn history(&mut self, circle: &CircleName, def: &BaseCircle, repo: &mut dyn Repo, emit: &mut Emit, report: &mut ExtractReport) -> Result<(), Error> {
        let BaseSource::History { upstream, local, .. } = &def.source else { return Ok(()) };
        let mut tips = vec![repo.resolve(&RevSpec::Pinned(upstream.to_string()))?.commit];
        for name in local {
            match repo.resolve(&RevSpec::Pinned(name.clone())) {
                Ok(r) => tips.push(r.commit),
                Err(e) => report.skipped.push((name.clone(), format!("local ref not resolved: {e}"))),
            }
        }
        for rec in repo.commits(&tips, &[])? {
            let mut touched = Vec::with_capacity(rec.touched.len());
            let mut dropped = 0;
            for p in &rec.touched {
                match excluded(p, &def.exclude) {
                    Some(prefix) => {
                        count_excluded(report, prefix);
                        dropped += 1;
                    }
                    None => touched.push(p.clone()),
                }
            }
            if dropped > 0 && touched.is_empty() {
                // a commit of excluded data only: its message never reaches the circle
                continue;
            }
            emit.batch(commit_batch(circle, rec, touched), None)?;
        }
        Ok(())
    }
}

impl Extract for Extractor {
    fn extract(&mut self, circle: &CircleName, def: &BaseCircle, rev: Option<&Rev>, repo: &mut dyn Repo, sink: &mut dyn FnMut(Chunk) -> Result<(), Error>) -> Result<ExtractReport, Error> {
        let mut report = ExtractReport::default();
        let mut emit = Emit { families: &def.families, sink, next: 0 };
        match &def.source {
            BaseSource::Tree { .. } => {
                let rev = rev.ok_or_else(|| Error::Rev(format!("{}: a tree circle needs a revision", circle.0)))?;
                self.tree(circle, def, rev, repo, &mut emit, &mut report)?;
            }
            BaseSource::History { .. } => self.history(circle, def, repo, &mut emit, &mut report)?,
            BaseSource::Notes { sources } => self.notes(circle, def, sources, &mut emit, &mut report)?,
        }
        report.chunks = emit.next as u64;
        Ok(report)
    }

    fn extract_file(&mut self, circle: &CircleName, def: &BaseCircle, path: &str, bytes: &[u8]) -> Result<Vec<Chunk>, Error> {
        let batch = match &def.source {
            BaseSource::Tree { .. } => {
                if excluded(path, &def.exclude).is_some() {
                    return Ok(Vec::new());
                }
                let blob = Some(git::blob_oid(bytes));
                match ext(path) {
                    "rs" => rust_batch(circle, path, bytes, blob, None, rust::is_generated(bytes), &mut ExtractReport::default()).0,
                    "md" => doc_batch(circle, path, bytes, blob, None),
                    "splash" => splash_batch(circle, path, bytes, blob, None),
                    _ if file_name(path) == "Cargo.toml" => {
                        let text = String::from_utf8_lossy(bytes);
                        match cargo::written_card(path, &text) {
                            Ok(Some((name, card, vendored))) => vec![crate_chunk(circle, &name, card, vendored)],
                            Ok(None) => Vec::new(),
                            Err(message) => return Err(Error::Parse { file: path.to_string(), line: 0, message }),
                        }
                    }
                    _ => Vec::new(),
                }
            }
            BaseSource::Notes { sources } => {
                let Some((src, rel)) = sources.iter().find_map(|s| path.strip_prefix(&format!("{}/", s.label)).map(|rel| (s, rel))) else {
                    return Err(Error::Parse { file: path.to_string(), line: 0, message: format!("no source of {} has this path's label", circle.0) });
                };
                if excluded(rel, &def.exclude).is_some() {
                    return Ok(Vec::new());
                }
                note_batch(circle, &src.label, rel, bytes, self.cite.as_ref())?
            }
            BaseSource::History { .. } => {
                return Err(Error::Parse { file: path.to_string(), line: 0, message: format!("{} is a history circle: it has no files", circle.0) });
            }
        };
        Ok(admit(batch, &def.families))
    }
}

#[cfg(test)]
mod tests;
