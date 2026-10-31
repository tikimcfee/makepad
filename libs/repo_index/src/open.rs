//! [`crate::Circles`]: opening circles at a revision. Resolve the revision, take the base for
//! its merge-base with upstream (open it, or build it once through extract, tokenize and the
//! segment sink), build the worktree overlay (`delta.rs`), and compute a history circle's
//! visibility. cli and eval both open circles here and nowhere else.
//!
//! - A tree circle at [`RevSpec::Worktree`] uses the base of HEAD's merge-base with the
//!   upstream ref of the history circle on the same root (`makepad:history` for `makepad`), plus
//!   the overlay; with no such history circle the base is HEAD itself. At a pinned revision the
//!   base is that commit's tree and there is no overlay.
//! - A notes circle is current when its manifest records the stamp of its sources as they are on
//!   disk now; else it is rebuilt whole (`ManifestChange::Replace`). `notes:local` belongs to its
//!   repo (`SegmentKey::repo` set); other notes circles to the machine.
//! - A history circle is current when its manifest records the tips of its refs; its
//!   [`Visibility`] is computed at every open.
//! - [`Opened::others`] are opened only when already built, never built, and carry no overlay:
//!   they serve pointer counts. A notes index older than its sources still serves them, and says
//!   so on stderr.

use crate::{
    circles, delta, BaseCircle, BaseSource, Chunk, CircleBody, CircleExpr, CircleManifest, CircleName, CircleStats, Circles,
    Error, Extract, Key, ManifestChange, NotesSource, OpenCircle, Opened, Repo, Rev, RevSpec, SegmentKey, SegmentRead,
    SegmentSink, SourceStamp, Store, Tokenize, Visibility, INDEXER_VERSION,
};
use std::path::{Path, PathBuf};

/// Opens repos other than the one the caller stands in (another tree circle's root).
pub type RepoOpener = Box<dyn FnMut(&Path) -> Result<Box<dyn Repo>, Error>>;

/// The [`Circles`] implementation, over the other parts.
pub struct Opener {
    /// The repo the caller stands in.
    pub repo: Box<dyn Repo>,
    pub open_repo: RepoOpener,
    pub store: Box<dyn Store>,
    pub extract: Box<dyn Extract>,
    pub tokenize: Box<dyn Tokenize>,
    /// `circles.conf`; absent is fine (built-in circles only).
    pub conf: PathBuf,
    /// `~` in `circles.conf`.
    pub home: PathBuf,
    /// Other repos opened so far, by root.
    others: Vec<(PathBuf, Box<dyn Repo>)>,
}

impl Opener {
    pub fn new(
        repo: Box<dyn Repo>,
        open_repo: RepoOpener,
        store: Box<dyn Store>,
        extract: Box<dyn Extract>,
        tokenize: Box<dyn Tokenize>,
        conf: PathBuf,
        home: PathBuf,
    ) -> Self {
        Opener { repo, open_repo, store, extract, tokenize, conf, home, others: Vec::new() }
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// The repo whose worktree root is `root`.
fn repo_at<'a>(
    main: &'a mut Box<dyn Repo>,
    others: &'a mut Vec<(PathBuf, Box<dyn Repo>)>,
    open_repo: &mut RepoOpener,
    root: &Path,
) -> Result<&'a mut dyn Repo, Error> {
    if same_dir(&main.root(), root) {
        return Ok(main.as_mut());
    }
    let at = match others.iter().position(|(r, _)| same_dir(r, root)) {
        Some(i) => i,
        None => {
            others.push((root.to_path_buf(), open_repo(root)?));
            others.len() - 1
        }
    };
    Ok(others[at].1.as_mut())
}

/// The package a path belongs to in a base: that of its own chunks, else of the nearest
/// enclosing directory's `Cargo.toml` card (a file added since the base).
pub fn base_package(segment: &dyn SegmentRead, path: &str) -> Option<String> {
    let of = |p: &str| {
        segment
            .chunks_with(&Key::Path { path: p.to_string(), line: None })
            .into_iter()
            .map(|id| segment.chunk(id).package.to_string())
            .find(|s| !s.is_empty())
    };
    if let Some(p) = of(path) {
        return Some(p);
    }
    let mut dir = path;
    while let Some((up, _)) = dir.rsplit_once('/') {
        if let Some(p) = of(&format!("{up}/Cargo.toml")) {
            return Some(p);
        }
        dir = up;
    }
    of("Cargo.toml")
}

fn sum_stats(segments: &[Box<dyn SegmentRead>]) -> CircleStats {
    let mut s = CircleStats::default();
    for seg in segments {
        let x = seg.stats();
        s.chunks += x.chunks;
        for (a, b) in s.field_lengths.iter_mut().zip(x.field_lengths) {
            *a += b;
        }
    }
    s
}

/// The stamp of a notes circle's sources as they are on disk: a hash of every admitted file's
/// (label, path, size, content id), sorted.
pub fn notes_stamp(sources: &[NotesSource], def: &BaseCircle) -> Result<u64, Error> {
    let mut lines = Vec::new();
    for s in sources {
        for (rel, full) in walk_files(&s.dir)? {
            if circles::excluded(def, &rel).is_some() {
                continue;
            }
            let bytes = std::fs::read(&full).map_err(|error| Error::Io { context: format!("notes: reading {}", full.display()), error })?;
            let id = makepad_git::oid::hash_object("blob", &bytes);
            lines.push(format!("{}\t{rel}\t{}\t{id}", s.label, bytes.len()));
        }
    }
    lines.sort();
    let oid = makepad_git::oid::hash_object("blob", lines.join("\n").as_bytes());
    let b = oid.as_bytes();
    Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

/// Every file under `dir` (directory symlinks not followed), as (`/`-separated path within
/// `dir`, full path), sorted.
pub fn walk_files(dir: &Path) -> Result<Vec<(String, PathBuf)>, Error> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).map_err(|error| Error::Io { context: format!("notes: listing {}", d.display()), error })?;
        for e in entries {
            let e = e.map_err(|error| Error::Io { context: format!("notes: listing {}", d.display()), error })?;
            let ft = e.file_type().map_err(|error| Error::Io { context: format!("notes: {}", e.path().display()), error })?;
            let p = e.path();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() || (ft.is_symlink() && p.is_file()) {
                let rel = p.strip_prefix(dir).map(circles::slash_path).unwrap_or_default();
                out.push((rel, p));
            }
        }
    }
    out.sort();
    Ok(out)
}

impl Opener {
    /// The upstream ref of the history circle on the same root as a tree circle.
    fn upstream_of(manifest: &CircleManifest, root: &Path) -> Option<String> {
        manifest.defs.iter().find_map(|d| match &d.body {
            CircleBody::Base(BaseCircle { source: BaseSource::History { root: r, upstream, .. }, .. }) if same_dir(r, root) => {
                Some(upstream.clone())
            }
            _ => None,
        })
    }

    /// One base circle at `rev`; `build` builds what is missing, else an unbuilt circle is `None`.
    fn open_one(
        &mut self,
        manifest: &CircleManifest,
        name: &CircleName,
        rev: &RevSpec,
        build: bool,
    ) -> Result<Option<OpenCircle>, Error> {
        let def = match circles::find_def(manifest, name).map(|d| &d.body) {
            Some(CircleBody::Base(b)) => b.clone(),
            Some(CircleBody::Derived(_)) => return Err(Error::Parse {
                file: manifest.conf.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                line: 0,
                message: format!("`{}` is derived; open its base circles", name.0),
            }),
            None => return Err(Error::UnknownCircle(name.0.clone())),
        };
        let def_hash = circles::def_hash(&def);
        let main_id = self.repo.id();
        let Opener { repo, open_repo, store, extract, tokenize, others, .. } = self;
        match &def.source {
            BaseSource::Tree { root } => {
                let repo = repo_at(repo, others, open_repo, root)?;
                let (indexed, overlay_from) = match rev {
                    RevSpec::Worktree => {
                        let head = repo.resolve(&RevSpec::Pinned("HEAD".into()))?;
                        let base = match Self::upstream_of(manifest, root) {
                            None => head.clone(),
                            Some(up) => base_for(repo, &head, &up)?,
                        };
                        (base.clone(), Some(base))
                    }
                    RevSpec::Pinned(_) => (repo.resolve(rev)?, None),
                };
                let key = SegmentKey {
                    repo: Some(repo.id()),
                    circle: name.clone(),
                    source: SourceStamp::Tree { tree: indexed.tree },
                    def_hash,
                    indexer: INDEXER_VERSION,
                };
                let segment = match store.open_base(&key)? {
                    Some(s) => s,
                    None if build => store.build_base(&key, &mut |sink: &mut dyn SegmentSink| {
                        let report = extract.extract(name, &def, Some(&indexed), repo, &mut |c: Chunk| {
                            let t = tokenize.chunk_terms(&c);
                            sink.add(&c, &t).map(|_| ())
                        })?;
                        for (path, why) in &report.skipped {
                            eprintln!("makepad index: {}: skipped {path}: {why}", name.0);
                        }
                        Ok(())
                    })?,
                    None => return Ok(None),
                };
                let (overlay, shadowed) = match overlay_from.filter(|_| build) {
                    Some(base) => {
                        let package = |p: &str| base_package(segment.as_ref(), p);
                        let over = delta::Over { circle: name, def: &def, base: &base, root, package: &package };
                        let o = delta::overlay(repo, extract.as_mut(), tokenize.as_ref(), &over)?;
                        (o.files, o.shadowed)
                    }
                    None => (Vec::new(), Vec::new()),
                };
                let segments = vec![segment];
                Ok(Some(OpenCircle {
                    name: name.clone(),
                    rev: Some(Rev { spec: rev.clone(), commit: indexed.commit, tree: indexed.tree }),
                    stats: sum_stats(&segments),
                    segments,
                    overlay,
                    shadowed,
                    visible: None,
                }))
            }
            BaseSource::Notes { sources } => {
                let repo_key = (name.0 == circles::NOTES_LOCAL).then_some(main_id);
                let stamp = SourceStamp::Notes { content: notes_stamp(sources, &def)? };
                let current = store.manifest(repo_key, name)?;
                let manifest_now = match current {
                    Some(m) if m.sources.contains(&stamp) => m,
                    stale if build => {
                        let key = SegmentKey { repo: repo_key, circle: name.clone(), source: stamp, def_hash, indexer: INDEXER_VERSION };
                        let mut sink = store.new_segment(&key)?;
                        let report = extract.extract(name, &def, None, repo.as_mut(), &mut |c: Chunk| {
                            let t = tokenize.chunk_terms(&c);
                            sink.add(&c, &t).map(|_| ())
                        })?;
                        for (path, why) in &report.skipped {
                            eprintln!("makepad index: {}: skipped {path}: {why}", name.0);
                        }
                        let id = sink.finish()?;
                        let change = match stale {
                            Some(_) => ManifestChange::Replace(id),
                            None => ManifestChange::Add(id),
                        };
                        store.commit(repo_key, name, change)?
                    }
                    Some(m) => {
                        eprintln!(
                            "makepad index: {}: index is older than its sources (manifest {}); pointer counts may lag until `makepad index build --in {}`",
                            name.0, m.version, name.0
                        );
                        m
                    }
                    None => return Ok(None),
                };
                let segments = manifest_now
                    .segments
                    .iter()
                    .map(|id| store.open_segment(repo_key, name, id))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Some(OpenCircle {
                    name: name.clone(),
                    rev: None,
                    stats: sum_stats(&segments),
                    segments,
                    overlay: Vec::new(),
                    shadowed: Vec::new(),
                    visible: None,
                }))
            }
            BaseSource::History { root, upstream, local } => {
                let repo = repo_at(repo, others, open_repo, root)?;
                let at = match rev {
                    RevSpec::Worktree => repo.resolve(&RevSpec::Pinned("HEAD".into()))?,
                    RevSpec::Pinned(_) => repo.resolve(rev)?,
                };
                let up = repo.resolve(&RevSpec::Pinned(upstream.clone()))?;
                let mut tips = vec![up.commit];
                let mut local_tips = Vec::new();
                let mut local_refs = Vec::new();
                for r in local {
                    match repo.resolve(&RevSpec::Pinned(r.clone())) {
                        Ok(x) => {
                            tips.push(x.commit);
                            local_tips.push(x.commit);
                            local_refs.push(short_ref(r));
                        }
                        Err(e) => eprintln!("makepad index: {}: local ref {r} not walked: {e}", name.0),
                    }
                }
                let repo_key = Some(repo.id());
                let stamp = SourceStamp::History { tips: tips.clone() };
                let manifest_now = match store.manifest(repo_key, name)? {
                    Some(m) if m.sources.contains(&stamp) => m,
                    stale if build => {
                        let key = SegmentKey { repo: repo_key, circle: name.clone(), source: stamp, def_hash, indexer: INDEXER_VERSION };
                        let mut sink = store.new_segment(&key)?;
                        extract.extract(name, &def, Some(&at), repo, &mut |c: Chunk| {
                            let t = tokenize.chunk_terms(&c);
                            sink.add(&c, &t).map(|_| ())
                        })?;
                        let id = sink.finish()?;
                        let change = match stale {
                            Some(_) => ManifestChange::Replace(id),
                            None => ManifestChange::Add(id),
                        };
                        store.commit(repo_key, name, change)?
                    }
                    Some(m) => {
                        eprintln!("makepad index: {}: index predates its refs' tips; run `makepad index build --in {}`", name.0, name.0);
                        m
                    }
                    None => return Ok(None),
                };
                let visible = Visibility {
                    reachable: repo.commits(&[at.commit], &[])?.into_iter().map(|c| c.oid).collect(),
                    local: repo.commits(&local_tips, &[up.commit])?.into_iter().map(|c| c.oid).collect(),
                    local_refs,
                };
                let segments = manifest_now
                    .segments
                    .iter()
                    .map(|id| store.open_segment(repo_key, name, id))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Some(OpenCircle {
                    name: name.clone(),
                    rev: Some(at),
                    stats: sum_stats(&segments),
                    segments,
                    overlay: Vec::new(),
                    shadowed: Vec::new(),
                    visible: Some(visible),
                }))
            }
        }
    }
}

/// A ref's short name: `refs/heads/work` is `work`, `refs/remotes/upstream/work` is
/// `upstream/work`.
pub fn short_ref(r: &str) -> String {
    r.strip_prefix("refs/heads/").or_else(|| r.strip_prefix("refs/remotes/")).or_else(|| r.strip_prefix("refs/")).unwrap_or(r).to_string()
}

/// HEAD's base: its merge-base with `upstream`. An upstream ref that does not resolve, or no
/// merge-base, makes HEAD the base, said on stderr with the cause.
fn base_for(repo: &mut dyn Repo, head: &Rev, upstream: &str) -> Result<Rev, Error> {
    let up = match repo.resolve(&RevSpec::Pinned(upstream.to_string())) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("makepad index: upstream {upstream} does not resolve ({e}); the base is HEAD, so the overlay holds only uncommitted work");
            return Ok(head.clone());
        }
    };
    match repo.merge_base(head.commit, up.commit)? {
        Some(mb) if mb == head.commit => Ok(head.clone()),
        Some(mb) => repo.resolve(&RevSpec::Pinned(hex(&mb))),
        None => {
            eprintln!("makepad index: HEAD and {upstream} share no history; the base is HEAD");
            Ok(head.clone())
        }
    }
}

/// An oid as lowercase hex.
pub fn hex(oid: &crate::Oid) -> String {
    oid.0.iter().map(|b| format!("{b:02x}")).collect()
}

impl Circles for Opener {
    fn manifest(&mut self) -> Result<CircleManifest, Error> {
        let root = self.repo.root();
        match std::fs::read_to_string(&self.conf) {
            Ok(text) => circles::manifest_for(&root, Some((&self.conf, &text)), &self.home),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => circles::manifest_for(&root, None, &self.home),
            Err(error) => Err(Error::Io { context: format!("reading {}", self.conf.display()), error }),
        }
    }

    fn default_scope(&self, manifest: &CircleManifest, cwd: &Path) -> Vec<CircleName> {
        circles::default_scope(manifest, cwd)
    }

    fn parse_in(&self, manifest: &CircleManifest, arg: &str) -> Result<Vec<CircleExpr>, Error> {
        circles::parse_in(manifest, arg)
    }

    fn def_hash(&self, def: &BaseCircle) -> u64 {
        circles::def_hash(def)
    }

    fn open(&mut self, manifest: &CircleManifest, names: &[CircleName], rev: &RevSpec) -> Result<Opened, Error> {
        let mut wanted: Vec<CircleName> = Vec::new();
        for n in names {
            for b in circles::base_names(manifest, &CircleExpr::Circle(n.clone()))? {
                if !wanted.contains(&b) {
                    wanted.push(b);
                }
            }
        }
        let mut open = Vec::new();
        for n in &wanted {
            match self.open_one(manifest, n, rev, true)? {
                Some(c) => open.push(c),
                None => return Err(Error::Corrupt { path: PathBuf::new(), message: format!("{}: built but not opened", n.0) }),
            }
        }
        let mut others = Vec::new();
        let mut unbuilt = Vec::new();
        for d in &manifest.defs {
            if !matches!(d.body, CircleBody::Base(_)) || wanted.contains(&d.name) {
                continue;
            }
            match self.open_one(manifest, &d.name, rev, false) {
                Ok(Some(c)) => others.push(c),
                Ok(None) => unbuilt.push(d.name.clone()),
                Err(e) => {
                    eprintln!("makepad index: {} not opened for pointers: {e}", d.name.0);
                    unbuilt.push(d.name.clone());
                }
            }
        }
        Ok(Opened { open, others, unbuilt })
    }
}

/// Fakes of the other parts, for this crate's cli, delta, open and eval tests.
#[cfg(test)]
pub(crate) mod fakes {
    use crate::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    pub fn oid(n: u8) -> Oid {
        Oid([n; 20])
    }

    /// A repo of named revisions, each a tree of (path, text) files.
    pub struct FakeRepo {
        pub root: PathBuf,
        pub revs: HashMap<String, Rev>,
        pub trees: HashMap<Oid, Vec<(String, String)>>,
        pub changes: Vec<Change>,
        pub merge_bases: HashMap<(Oid, Oid), Oid>,
        pub commits: Vec<CommitRecord>,
        pub resolved: Rc<RefCell<Vec<String>>>,
        pub worktrees: Vec<(PathBuf, Oid)>,
    }

    impl FakeRepo {
        pub fn new(root: &Path) -> Self {
            FakeRepo {
                root: root.to_path_buf(),
                revs: HashMap::new(),
                trees: HashMap::new(),
                changes: Vec::new(),
                merge_bases: HashMap::new(),
                commits: Vec::new(),
                resolved: Rc::new(RefCell::new(Vec::new())),
                worktrees: Vec::new(),
            }
        }
        /// A revision named `names` (commit `n`, tree `n + 100`) holding `files`.
        pub fn rev(&mut self, names: &[&str], n: u8, files: &[(&str, &str)]) -> Rev {
            let r = Rev { spec: RevSpec::Pinned(names[0].into()), commit: oid(n), tree: oid(n + 100) };
            for name in names {
                self.revs.insert(name.to_string(), r.clone());
            }
            self.revs.insert(crate::open::hex(&oid(n)), r.clone());
            self.trees.insert(r.tree, files.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect());
            r
        }
        fn blob_of(path: &str, text: &str) -> Oid {
            Oid(*makepad_git::oid::hash_object("blob", format!("{path}\0{text}").as_bytes()).as_bytes())
        }
    }

    impl Repo for FakeRepo {
        fn id(&self) -> RepoId {
            RepoId(7)
        }
        fn root(&self) -> PathBuf {
            self.root.clone()
        }
        fn resolve(&mut self, spec: &RevSpec) -> Result<Rev, Error> {
            let s = match spec {
                RevSpec::Worktree => "HEAD".to_string(),
                RevSpec::Pinned(s) => s.clone(),
            };
            self.resolved.borrow_mut().push(s.clone());
            let full = self.revs.get(&s).cloned().or_else(|| {
                let hits: Vec<&Rev> = self.revs.iter().filter(|(k, _)| s.len() >= 4 && k.starts_with(&s)).map(|(_, v)| v).collect();
                (hits.len() == 1).then(|| hits[0].clone())
            });
            full.map(|r| Rev { spec: spec.clone(), ..r }).ok_or_else(|| Error::Rev(format!("{s} does not resolve")))
        }
        fn files(&mut self, tree: Oid) -> Result<Vec<TreeFile>, Error> {
            let t = self.trees.get(&tree).ok_or_else(|| Error::Git(format!("no tree {}", crate::open::hex(&tree))))?;
            Ok(t.iter().map(|(p, text)| TreeFile { path: p.clone(), blob: Self::blob_of(p, text), mode: 0o100644 }).collect())
        }
        fn read_blob(&mut self, blob: Oid) -> Result<Vec<u8>, Error> {
            for t in self.trees.values() {
                for (p, text) in t {
                    if Self::blob_of(p, text) == blob {
                        return Ok(text.as_bytes().to_vec());
                    }
                }
            }
            Err(Error::Git(format!("no blob {}", crate::open::hex(&blob))))
        }
        fn changes(&mut self, _base: &Rev) -> Result<Vec<Change>, Error> {
            Ok(self.changes.clone())
        }
        fn commits(&mut self, tips: &[Oid], hide: &[Oid]) -> Result<Vec<CommitRecord>, Error> {
            Ok(self
                .commits
                .iter()
                .filter(|c| tips.contains(&c.oid) || c.info.parents.iter().any(|p| tips.contains(p)))
                .filter(|c| !hide.contains(&c.oid))
                .cloned()
                .collect())
        }
        fn worktrees(&mut self) -> Result<Vec<(PathBuf, Oid)>, Error> {
            Ok(self.worktrees.clone())
        }
        fn merge_base(&mut self, a: Oid, b: Oid) -> Result<Option<Oid>, Error> {
            if a == b {
                return Ok(Some(a));
            }
            Ok(self.merge_bases.get(&(a, b)).or_else(|| self.merge_bases.get(&(b, a))).copied())
        }
    }

    /// One `file` chunk per file, plus a `fn` chunk per `fn <name>` it holds.
    pub struct FakeExtract;

    impl FakeExtract {
        fn chunks(circle: &CircleName, path: &str, text: &str, label_len: u16) -> Vec<Chunk> {
            let mut out = vec![Chunk {
                kind: if label_len > 0 { Kind::NoteFile } else { Kind::File },
                circle: circle.clone(),
                path: path.into(),
                label_len,
                gist: String::new(),
                span: Span::default(),
                lines: Lines { start: 1, end: text.lines().count().max(1) as u32 },
                name: path.into(),
                parent: None,
                fields: Fields { name: path.into(), path: path.into(), body: text.into(), ..Fields::default() },
                keys: Keys { paths: vec![path.into()], ..Keys::default() },
                flags: Flags::default(),
                detail: Detail::None,
            }];
            for (i, line) in text.lines().enumerate() {
                if let Some(rest) = line.trim().strip_prefix("fn ") {
                    let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                    out.push(Chunk {
                        kind: Kind::Fn,
                        name: name.clone(),
                        lines: Lines { start: i as u32 + 1, end: i as u32 + 1 },
                        parent: Some(0),
                        fields: Fields { name: name.clone(), path: path.into(), body: line.into(), ..Fields::default() },
                        keys: Keys { paths: vec![path.into()], idents: vec![name], ..Keys::default() },
                        flags: Flags { definition: true, ..Flags::default() },
                        ..out[0].clone()
                    });
                }
            }
            out
        }
    }

    impl Extract for FakeExtract {
        fn extract(
            &mut self,
            circle: &CircleName,
            def: &BaseCircle,
            rev: Option<&Rev>,
            repo: &mut dyn Repo,
            sink: &mut dyn FnMut(Chunk) -> Result<(), Error>,
        ) -> Result<ExtractReport, Error> {
            let mut report = ExtractReport::default();
            // (path within the source root, chunk path, bytes)
            let files: Vec<(String, String, Vec<u8>)> = match (&def.source, rev) {
                (BaseSource::Tree { .. }, Some(rev)) => {
                    let mut v = Vec::new();
                    for f in repo.files(rev.tree)? {
                        v.push((f.path.clone(), f.path.clone(), repo.read_blob(f.blob)?));
                    }
                    v
                }
                (BaseSource::Notes { sources }, None) => {
                    let mut v = Vec::new();
                    for s in sources {
                        for (rel, full) in crate::open::walk_files(&s.dir)? {
                            v.push((rel.clone(), format!("{}/{rel}", s.label), std::fs::read(full).unwrap()));
                        }
                    }
                    v
                }
                _ => Vec::new(),
            };
            for (rel, path, bytes) in files {
                if crate::circles::excluded(def, &rel).is_some() {
                    report.excluded.push((path, 1));
                    continue;
                }
                report.files += 1;
                let label_len = if path == rel { 0 } else { (path.len() - rel.len() - 1) as u16 };
                for c in Self::chunks(circle, &path, &String::from_utf8_lossy(&bytes), label_len) {
                    report.chunks += 1;
                    sink(c)?;
                }
            }
            Ok(report)
        }
        fn extract_file(&mut self, circle: &CircleName, def: &BaseCircle, path: &str, bytes: &[u8]) -> Result<Vec<Chunk>, Error> {
            if crate::circles::excluded(def, path).is_some() {
                return Ok(Vec::new());
            }
            Ok(Self::chunks(circle, path, &String::from_utf8_lossy(bytes), 0))
        }
    }

    /// Lowercased words of name and body.
    pub struct FakeTokenize;

    impl Tokenize for FakeTokenize {
        fn chunk_terms(&self, chunk: &Chunk) -> FieldTerms {
            let mut terms = Vec::new();
            for (field, text) in [(Field::Name, &chunk.fields.name), (Field::Body, &chunk.fields.body)] {
                for w in text.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()) {
                    terms.push(TermOcc { field, form: TermForm::Folded, text: w.to_lowercase(), tf: 1 });
                }
            }
            FieldTerms { lengths: [terms.len() as u32, 0, 0, 0, 0], terms }
        }
        fn query_terms(&self, words: &[String]) -> Vec<QueryTerm> {
            words
                .iter()
                .enumerate()
                .map(|(i, w)| QueryTerm { form: TermForm::Folded, text: w.to_lowercase(), word: i as u16 })
                .collect()
        }
    }

    /// A segment held in memory.
    pub struct FakeSegment {
        pub key: SegmentKey,
        pub chunks: Vec<(Chunk, FieldTerms)>,
    }

    impl SegmentRead for FakeSegment {
        fn key(&self) -> &SegmentKey {
            &self.key
        }
        fn id(&self) -> SegmentId {
            SegmentId(format!("seg-{}-{:?}", self.key.circle.0, self.key.source))
        }
        fn stats(&self) -> CircleStats {
            CircleStats { chunks: self.chunks.len() as u64, field_lengths: [0; 5] }
        }
        fn chunk_count(&self) -> u32 {
            self.chunks.len() as u32
        }
        fn df(&self, _form: TermForm, term: &str) -> u32 {
            self.chunks.iter().filter(|(_, t)| t.terms.iter().any(|o| o.text == term)).count() as u32
        }
        fn postings(&self, _form: TermForm, term: &str) -> Box<dyn Iterator<Item = Posting> + '_> {
            let term = term.to_string();
            Box::new(
                self.chunks
                    .iter()
                    .enumerate()
                    .filter(move |(_, (_, t))| t.terms.iter().any(|o| o.text == term))
                    .map(|(i, _)| Posting { chunk: ChunkId(i as u32), tf: [1, 0, 0, 0, 0] }),
            )
        }
        fn chunk(&self, id: ChunkId) -> ChunkView<'_> {
            let c = &self.chunks[id.0 as usize].0;
            ChunkView {
                kind: c.kind,
                path: &c.path,
                span: c.span,
                lines: c.lines,
                name: &c.name,
                parent: c.parent.map(ChunkId),
                package: c.keys.package.as_deref().unwrap_or(""),
                gist: "",
                sig: &c.fields.sig,
                blob: c.keys.blob,
                commit: c.keys.commit,
                flags: c.flags,
                field_lengths: [0; 5],
                label_len: c.label_len,
            }
        }
        fn keys(&self, id: ChunkId) -> Keys {
            self.chunks[id.0 as usize].0.keys.clone()
        }
        fn detail(&self, id: ChunkId) -> Detail {
            self.chunks[id.0 as usize].0.detail.clone()
        }
        fn chunks_with(&self, key: &Key) -> Vec<ChunkId> {
            self.chunks
                .iter()
                .enumerate()
                .filter(|(_, (c, _))| match key {
                    Key::Path { path, .. } => c.keys.paths.contains(path),
                    Key::Package(p) => c.keys.package.as_ref() == Some(p),
                    Key::Ident(i) => c.keys.idents.contains(i),
                    Key::Blob(b) => c.keys.blob == Some(*b),
                    Key::Commit(o) => c.keys.commit == Some(*o),
                })
                .map(|(i, _)| ChunkId(i as u32))
                .collect()
        }
        fn cited_by(&self, key: &Key) -> Vec<(ChunkId, Edge)> {
            let mut out = Vec::new();
            for (i, (c, _)) in self.chunks.iter().enumerate() {
                for e in &c.keys.edges {
                    let hit = match (&e.key, key) {
                        (Key::Path { path: a, .. }, Key::Path { path: b, line: None }) => a == b,
                        (a, b) => a == b,
                    };
                    if hit {
                        out.push((ChunkId(i as u32), e.clone()));
                    }
                }
            }
            out
        }
        fn record_text(&self, id: ChunkId) -> Option<&str> {
            let c = &self.chunks[id.0 as usize].0;
            (c.kind.family() == Family::Note).then_some(c.fields.body.as_str())
        }
    }

    #[derive(Default)]
    pub struct StoreState {
        pub bases: HashMap<SegmentKey, Vec<(Chunk, FieldTerms)>>,
        pub segments: HashMap<String, (SegmentKey, Vec<(Chunk, FieldTerms)>)>,
        pub manifests: HashMap<String, SegmentManifest>,
        pub builds: u32,
    }

    /// A store in memory; `state` is shared so a test can look inside.
    #[derive(Clone, Default)]
    pub struct FakeStore {
        pub state: Rc<RefCell<StoreState>>,
    }

    struct FakeSink {
        state: Rc<RefCell<StoreState>>,
        key: SegmentKey,
        chunks: Vec<(Chunk, FieldTerms)>,
    }

    impl SegmentSink for FakeSink {
        fn add(&mut self, chunk: &Chunk, terms: &FieldTerms) -> Result<ChunkId, Error> {
            self.chunks.push((chunk.clone(), terms.clone()));
            Ok(ChunkId(self.chunks.len() as u32 - 1))
        }
        fn finish(self: Box<Self>) -> Result<SegmentId, Error> {
            let mut st = self.state.borrow_mut();
            let id = format!("seg-{}", st.segments.len());
            st.segments.insert(id.clone(), (self.key, self.chunks));
            Ok(SegmentId(id))
        }
    }

    fn mkey(repo: Option<RepoId>, circle: &CircleName) -> String {
        format!("{repo:?}/{}", circle.0)
    }

    impl Store for FakeStore {
        fn open_base(&self, key: &SegmentKey) -> Result<Option<Box<dyn SegmentRead>>, Error> {
            Ok(self.state.borrow().bases.get(key).map(|c| Box::new(FakeSegment { key: key.clone(), chunks: c.clone() }) as Box<dyn SegmentRead>))
        }
        fn build_base(
            &self,
            key: &SegmentKey,
            fill: &mut dyn FnMut(&mut dyn SegmentSink) -> Result<(), Error>,
        ) -> Result<Box<dyn SegmentRead>, Error> {
            let mut sink = FakeSink { state: self.state.clone(), key: key.clone(), chunks: Vec::new() };
            fill(&mut sink)?;
            let mut st = self.state.borrow_mut();
            st.builds += 1;
            st.bases.insert(key.clone(), sink.chunks.clone());
            Ok(Box::new(FakeSegment { key: key.clone(), chunks: sink.chunks }))
        }
        fn new_segment(&self, key: &SegmentKey) -> Result<Box<dyn SegmentSink>, Error> {
            Ok(Box::new(FakeSink { state: self.state.clone(), key: key.clone(), chunks: Vec::new() }))
        }
        fn manifest(&self, repo: Option<RepoId>, circle: &CircleName) -> Result<Option<SegmentManifest>, Error> {
            Ok(self.state.borrow().manifests.get(&mkey(repo, circle)).cloned())
        }
        fn commit(&self, repo: Option<RepoId>, circle: &CircleName, change: ManifestChange) -> Result<SegmentManifest, Error> {
            let mut st = self.state.borrow_mut();
            let prev = st.manifests.get(&mkey(repo, circle)).cloned();
            let (segments, version) = match (&change, &prev) {
                (ManifestChange::Add(id), Some(p)) => ([p.segments.clone(), vec![id.clone()]].concat(), p.version + 1),
                (ManifestChange::Add(id), None) | (ManifestChange::Replace(id), None) => (vec![id.clone()], 1),
                (ManifestChange::Replace(id), Some(p)) => (vec![id.clone()], p.version + 1),
                (ManifestChange::Merge { output, .. }, p) => (vec![output.clone()], p.as_ref().map_or(1, |p| p.version + 1)),
            };
            let sources = segments.iter().map(|s| st.segments[&s.0].0.source.clone()).collect();
            st.builds += 1;
            let m = SegmentManifest { circle: circle.clone(), version, segments, stats: CircleStats::default(), sources, writer: "test".into() };
            st.manifests.insert(mkey(repo, circle), m.clone());
            Ok(m)
        }
        fn open_segment(&self, _repo: Option<RepoId>, _circle: &CircleName, id: &SegmentId) -> Result<Box<dyn SegmentRead>, Error> {
            let st = self.state.borrow();
            let (key, chunks) = st.segments.get(&id.0).ok_or_else(|| Error::Corrupt { path: PathBuf::from(&id.0), message: "missing".into() })?;
            Ok(Box::new(FakeSegment { key: key.clone(), chunks: chunks.clone() }))
        }
        fn gc(&self, _live: &[SegmentKey], _circles: &[(Option<RepoId>, CircleName)]) -> Result<GcReport, Error> {
            Ok(GcReport::default())
        }
    }

    /// An [`Opener`](crate::open::Opener) over the fakes for a repo at `root` (the store is
    /// returned too, for inspection).
    pub fn opener(repo: FakeRepo, conf: &Path) -> (crate::open::Opener, FakeStore) {
        let store = FakeStore::default();
        let root = repo.root.clone();
        let o = crate::open::Opener::new(
            Box::new(repo),
            Box::new(|p: &Path| Err(Error::Git(format!("no other repo at {}", p.display())))),
            Box::new(store.clone()),
            Box::new(FakeExtract),
            Box::new(FakeTokenize),
            conf.to_path_buf(),
            root,
        );
        (o, store)
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::*;
    use super::*;
    use crate::Change;

    fn n(s: &str) -> CircleName {
        CircleName(s.into())
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("repo-index-open-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn files_of(c: &OpenCircle) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for s in &c.segments {
            for i in 0..s.chunk_count() {
                let p = s.chunk(crate::ChunkId(i)).path.to_string();
                if !v.contains(&p) {
                    v.push(p);
                }
            }
        }
        v.sort();
        v
    }

    #[test]
    fn worktree_opens_the_merge_base_plus_overlay_and_builds_once() {
        let root = scratch("wt").join("makepad");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a/x.rs"), "fn changed_here() {}\n").unwrap();
        let mut repo = FakeRepo::new(&root);
        let base = repo.rev(&["refs/remotes/upstream/work"], 1, &[("a/x.rs", "fn base_fn() {}\n"), ("libs/repo_index/eval/q.jsonl", "fn excluded() {}\n")]);
        let head = repo.rev(&["HEAD"], 2, &[("a/x.rs", "fn mid() {}\n")]);
        repo.merge_bases.insert((head.commit, base.commit), base.commit);
        repo.changes = vec![Change::Modified("a/x.rs".into())];
        let (mut o, store) = opener(repo, &root.join("no.conf"));
        let m = o.manifest().unwrap();
        let opened = o.open(&m, &[n("makepad")], &RevSpec::Worktree).unwrap();
        assert_eq!(opened.open.len(), 1);
        let c = &opened.open[0];
        assert_eq!(c.rev.as_ref().unwrap().commit, base.commit, "indexed at the merge-base");
        assert_eq!(files_of(c), ["a/x.rs"], "the eval dir never enters the base");
        assert_eq!(c.shadowed, ["a/x.rs"]);
        assert_eq!(c.overlay.len(), 1);
        assert!(c.overlay[0].chunks.iter().any(|(ch, _)| ch.name == "changed_here"));
        assert_eq!(opened.unbuilt, [n("makepad:history")], "history is not built by a tree query");
        // a second open reuses the base
        o.open(&m, &[n("makepad")], &RevSpec::Worktree).unwrap();
        assert_eq!(store.state.borrow().builds, 1);
    }

    #[test]
    fn pinned_revision_has_no_overlay() {
        let root = scratch("pin").join("makepad");
        std::fs::create_dir_all(&root).unwrap();
        let mut repo = FakeRepo::new(&root);
        repo.rev(&["refs/remotes/upstream/work"], 1, &[("a.rs", "fn up() {}\n")]);
        let pin = repo.rev(&["HEAD"], 9, &[("b.rs", "fn pinned() {}\n")]);
        repo.changes = vec![Change::Added("dirty.rs".into())];
        let (mut o, _) = opener(repo, &root.join("no.conf"));
        let m = o.manifest().unwrap();
        let opened = o.open(&m, &[n("makepad")], &RevSpec::Pinned(hex(&pin.commit)[..9].to_string())).unwrap();
        let c = &opened.open[0];
        assert_eq!(c.rev.as_ref().unwrap().commit, pin.commit);
        assert!(c.overlay.is_empty() && c.shadowed.is_empty());
        assert_eq!(files_of(c), ["b.rs"]);
    }

    #[test]
    fn unknown_circles_fail_loudly() {
        let root = scratch("unknown").join("makepad");
        std::fs::create_dir_all(&root).unwrap();
        let (mut o, _) = opener(FakeRepo::new(&root), &root.join("no.conf"));
        let m = o.manifest().unwrap();
        assert!(matches!(o.open(&m, &[n("notes:nope")], &RevSpec::Worktree), Err(Error::UnknownCircle(c)) if c == "notes:nope"));
        assert!(matches!(o.parse_in(&m, "commercial"), Err(Error::UnknownCircle(_))));
    }

    #[test]
    fn notes_rebuild_when_their_files_change_and_honour_claims() {
        let top = scratch("notes");
        let root = top.join("makepad");
        std::fs::create_dir_all(root.join("local/agent_state/port")).unwrap();
        std::fs::create_dir_all(root.join("local/plans")).unwrap();
        std::fs::write(root.join("local/plans/p.md"), "# plan\n").unwrap();
        std::fs::write(root.join("local/agent_state/port/x.md"), "# claimed\n").unwrap();
        std::fs::create_dir_all(top.join("mail/topic")).unwrap();
        std::fs::write(top.join("mail/topic/a.md"), "# a\n").unwrap();
        let conf = top.join("circles.conf");
        std::fs::write(&conf, format!("notes:topic = notes {}\nnotes:topic = notes local/agent_state/port\n", top.join("mail/topic").display())).unwrap();
        let mut repo = FakeRepo::new(&root);
        repo.rev(&["HEAD", "refs/remotes/upstream/work"], 1, &[]);
        let (mut o, store) = opener(repo, &conf);
        let m = o.manifest().unwrap();
        let opened = o.open(&m, &[n("notes:local"), n("notes:topic")], &RevSpec::Worktree).unwrap();
        assert_eq!(files_of(&opened.open[0]), ["makepad/local/plans/p.md"], "the claimed dir left notes:local");
        assert_eq!(files_of(&opened.open[1]), ["makepad/local/agent_state/port/x.md", "topic/a.md"]);
        let builds = store.state.borrow().builds;
        o.open(&m, &[n("notes:topic")], &RevSpec::Worktree).unwrap();
        assert_eq!(store.state.borrow().builds, builds, "unchanged notes are not rebuilt");
        std::fs::write(top.join("mail/topic/b.md"), "# b\n").unwrap();
        let opened = o.open(&m, &[n("notes:topic")], &RevSpec::Worktree).unwrap();
        assert_eq!(files_of(&opened.open[0]), ["makepad/local/agent_state/port/x.md", "topic/a.md", "topic/b.md"]);
        assert_eq!(store.state.borrow().manifests.len(), 2);
        // not named: opened from what is built, never built
        let opened = o.open(&m, &[n("makepad")], &RevSpec::Worktree).unwrap();
        let others: Vec<&str> = opened.others.iter().map(|c| c.name.0.as_str()).collect();
        assert_eq!(others, ["notes:local", "notes:topic"]);
    }

    #[test]
    fn derived_names_open_their_bases() {
        let top = scratch("derived");
        let root = top.join("makepad");
        std::fs::create_dir_all(top.join("mail/t")).unwrap();
        let conf = top.join("c.conf");
        std::fs::write(&conf, format!("notes:t = notes {}\nboth = union makepad notes:t\n", top.join("mail/t").display())).unwrap();
        let mut repo = FakeRepo::new(&root);
        repo.rev(&["HEAD", "refs/remotes/upstream/work"], 1, &[("a.rs", "fn a() {}\n")]);
        let (mut o, _) = opener(repo, &conf);
        let m = o.manifest().unwrap();
        let opened = o.open(&m, &[n("both")], &RevSpec::Worktree).unwrap();
        let names: Vec<&str> = opened.open.iter().map(|c| c.name.0.as_str()).collect();
        assert_eq!(names, ["makepad", "notes:t"]);
    }
}
