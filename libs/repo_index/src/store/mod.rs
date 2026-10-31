//! Segments, bases and manifests ([`crate::Store`]) under `~/.makepad/index/`.
//!
//! A circle's directory is `repo-<repo id>/<circle dir>/` for a repo's circles and `<circle dir>/`
//! for the others, the circle dir being its name with `:` as `-` (NTFS). It holds:
//! - `seg-<sha1>.mpx`: segments, immutable, named by content ([`segment`]);
//! - `base-<sha1 of the key>`: a base's pointer, naming its segment; with `.lock` (held while it
//!   is built) and `.used` (touched at most hourly by readers, for gc);
//! - `manifest-<N>`: the numbered manifests of history and notes circles ([`manifest`]);
//! - `tmp-*.tmp`: files being written, published by `hard_link`, never by renaming over a name.

pub mod gc;
pub mod manifest;
pub mod read;
pub mod segment;
pub mod write;

use crate::{CircleName, Error, GcReport, ManifestChange, RepoId, SegmentId, SegmentKey, SegmentManifest, SegmentRead, SegmentSink, Store};
use makepad_git::oid::hash_object;
use read::Segment;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use write::{BuildLock, Builder, Published, SegmentWriter};

/// How long unreferenced data (a base no live worktree names, a superseded manifest, a segment
/// no manifest references, a stale tmp file) is kept before gc removes it.
pub const GRACE: Duration = Duration::from_secs(24 * 3600);
/// A reader refreshes a base's `.used` stamp when it is older than this.
pub const TOUCH_EVERY: Duration = Duration::from_secs(3600);

pub(crate) fn log(msg: &str) {
    eprintln!("makepad index: {msg}");
}

pub(crate) fn io_err(path: &Path, what: &str, error: std::io::Error) -> Error {
    Error::Io { context: format!("{what} {}", path.display()), error }
}

/// The index root: `$MAKEPAD_HOME/index`, else `~/.makepad/index`.
pub fn index_root() -> Result<PathBuf, Error> {
    if let Some(h) = std::env::var_os("MAKEPAD_HOME").filter(|h| !h.is_empty()) {
        return Ok(PathBuf::from(h).join("index"));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).filter(|h| !h.is_empty());
    match home {
        Some(h) => Ok(PathBuf::from(h).join(".makepad").join("index")),
        None => Err(Error::Io {
            context: "find the index root".into(),
            error: std::io::Error::new(std::io::ErrorKind::NotFound, "neither MAKEPAD_HOME nor the home directory is set"),
        }),
    }
}

/// A circle's directory name: `:` becomes `-`. Names that could leave the index root, or that
/// NTFS cannot hold, are refused.
pub fn circle_dir_name(circle: &CircleName) -> Result<String, Error> {
    let n = &circle.0;
    let ok = !n.is_empty() && !n.starts_with(['.', '-']) && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.'));
    if !ok {
        return Err(Error::Io {
            context: format!("name a directory for circle `{n}`"),
            error: std::io::Error::new(std::io::ErrorKind::InvalidInput, "circle names are ASCII letters, digits, `:`, `-`, `_` and `.`"),
        });
    }
    Ok(n.replace(':', "-"))
}

/// A base's pointer name, `base-<sha1 of the canonical key>`.
pub fn base_name(key: &SegmentKey) -> String {
    format!("base-{}", hash_object("blob", &segment::encode_key(key)).to_hex())
}

/// A segment's path in `dir`; `id` must be a content name, `seg-<40 hex>.mpx`.
pub fn segment_path(dir: &Path, id: &SegmentId) -> Result<PathBuf, Error> {
    let hex = id.0.strip_prefix("seg-").and_then(|n| n.strip_suffix(".mpx"));
    match hex {
        Some(h) if h.len() == 40 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) => Ok(dir.join(&id.0)),
        _ => Err(Error::Corrupt { path: dir.join(&id.0), message: "not a segment name (seg-<sha1>.mpx)".into() }),
    }
}

/// The age of a file's last modification; `None` when it cannot be read (or lies ahead).
pub fn age(path: &Path) -> Option<Duration> {
    fs::metadata(path).ok()?.modified().ok()?.elapsed().ok()
}

/// [`Store`] on the file system.
pub struct FsStore {
    root: PathBuf,
    grace: Duration,
}

impl FsStore {
    pub fn new(root: PathBuf) -> FsStore {
        FsStore { root, grace: GRACE }
    }

    /// A store whose gc keeps unreferenced data for `grace` instead of [`GRACE`].
    pub fn with_grace(root: PathBuf, grace: Duration) -> FsStore {
        FsStore { root, grace }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn grace(&self) -> Duration {
        self.grace
    }

    pub fn circle_dir(&self, repo: Option<RepoId>, circle: &CircleName) -> Result<PathBuf, Error> {
        let name = circle_dir_name(circle)?;
        Ok(match repo {
            Some(RepoId(id)) => self.root.join(format!("repo-{id:016x}")).join(name),
            None => self.root.join(name),
        })
    }

    fn made_dir(&self, repo: Option<RepoId>, circle: &CircleName) -> Result<PathBuf, Error> {
        let dir = self.circle_dir(repo, circle)?;
        fs::create_dir_all(&dir).map_err(|e| io_err(&dir, "create", e))?;
        Ok(dir)
    }

    /// Opens a segment of the circle and checks it belongs there.
    pub fn segment(&self, repo: Option<RepoId>, circle: &CircleName, id: &SegmentId) -> Result<Segment, Error> {
        let path = segment_path(&self.circle_dir(repo, circle)?, id)?;
        let seg = Segment::open(&path)?;
        let k = seg.key();
        if k.circle != *circle || k.repo != repo {
            return Err(Error::Corrupt { path, message: format!("belongs to circle `{}`, not `{}`", k.circle.0, circle.0) });
        }
        Ok(seg)
    }

    /// The base's segment, if its pointer names one that exists. A dangling pointer is logged
    /// and reported as no base.
    fn base(&self, key: &SegmentKey) -> Result<Option<Segment>, Error> {
        let dir = self.circle_dir(key.repo, &key.circle)?;
        let pointer = dir.join(base_name(key));
        let id = match fs::read_to_string(&pointer) {
            Ok(s) => SegmentId(s.trim().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(&pointer, "read", e)),
        };
        let path = segment_path(&dir, &id)?;
        let seg = match Segment::open(&path) {
            Ok(seg) => seg,
            Err(Error::Io { error, .. }) if error.kind() == std::io::ErrorKind::NotFound => {
                log(&format!("{} names {} which is gone; the base is rebuilt", pointer.display(), id.0));
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        if seg.key() != key {
            return Err(Error::Corrupt { path, message: format!("is not the base {} names", pointer.display()) });
        }
        touch_used(&dir.join(format!("{}.used", base_name(key))));
        Ok(Some(seg))
    }
}

/// Stamps a base as used, at most every [`TOUCH_EVERY`]; a failure only weakens gc's grace.
fn touch_used(path: &Path) {
    if age(path).is_some_and(|a| a < TOUCH_EVERY) {
        return;
    }
    if let Err(e) = fs::write(path, std::process::id().to_string()) {
        log(&format!("could not stamp {}: {e}; gc may count the base unused", path.display()));
    }
}

impl Store for FsStore {
    fn open_base(&self, key: &SegmentKey) -> Result<Option<Box<dyn SegmentRead>>, Error> {
        Ok(self.base(key)?.map(|s| Box::new(s) as Box<dyn SegmentRead>))
    }

    fn build_base(&self, key: &SegmentKey, fill: &mut dyn FnMut(&mut dyn SegmentSink) -> Result<(), Error>) -> Result<Box<dyn SegmentRead>, Error> {
        if let Some(seg) = self.base(key)? {
            return Ok(Box::new(seg));
        }
        let dir = self.made_dir(key.repo, &key.circle)?;
        let name = base_name(key);
        let _lock = BuildLock::acquire(&dir.join(format!("{name}.lock")), &format!("base {name} of {}", key.circle.0))?;
        if let Some(seg) = self.base(key)? {
            return Ok(Box::new(seg));
        }
        let pointer = dir.join(&name);
        if pointer.exists() {
            // dangling (its segment is gone); we hold the lock, so nobody else replaces it
            fs::remove_file(&pointer).map_err(|e| io_err(&pointer, "remove dangling", e))?;
        }
        let mut sink = SegmentWriter { builder: Builder::new(key.clone()), dir: dir.clone() };
        fill(&mut sink)?;
        let id = Box::new(sink).finish()?;
        if write::put_if_absent(&dir, &pointer, format!("{}\n", id.0).as_bytes())? == Published::Exists {
            log(&format!("{} was published by another builder", pointer.display()));
        }
        match self.base(key)? {
            Some(seg) => Ok(Box::new(seg)),
            None => Err(Error::Corrupt { path: pointer, message: "base vanished right after it was built".into() }),
        }
    }

    fn new_segment(&self, key: &SegmentKey) -> Result<Box<dyn SegmentSink>, Error> {
        let dir = self.made_dir(key.repo, &key.circle)?;
        Ok(Box::new(SegmentWriter { builder: Builder::new(key.clone()), dir }))
    }

    fn manifest(&self, repo: Option<RepoId>, circle: &CircleName) -> Result<Option<SegmentManifest>, Error> {
        let dir = self.circle_dir(repo, circle)?;
        Ok(manifest::newest(&dir, circle)?.1.map(|e| e.manifest))
    }

    fn commit(&self, repo: Option<RepoId>, circle: &CircleName, change: ManifestChange) -> Result<SegmentManifest, Error> {
        manifest::commit(self, repo, circle, change)
    }

    fn open_segment(&self, repo: Option<RepoId>, circle: &CircleName, id: &SegmentId) -> Result<Box<dyn SegmentRead>, Error> {
        Ok(Box::new(self.segment(repo, circle, id)?))
    }

    fn gc(&self, live_bases: &[SegmentKey], live_circles: &[(Option<RepoId>, CircleName)]) -> Result<GcReport, Error> {
        gc::run(self, live_bases, live_circles)
    }
}
