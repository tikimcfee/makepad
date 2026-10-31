//! makepad index: ranked keyword search for agents over makepad's code, docs, history and notes.
//!
//! Records live in circles. A base circle is its own index (segments and term statistics),
//! built from one source: a repo tree at a revision, a git history, or notes directories. A
//! derived circle is a view over other circles and owns no segments. Records in different
//! circles relate only through shared keys ([`Key`]: path, commit, blob, identifier, package);
//! scores never cross circles.
//!
//! This file is the contract between the parts. The data types are fixed; each trait is the
//! boundary one part implements and the others call (and fake in their tests):
//! - [`Repo`]: git reads at a revision (`extract/git.rs`, over makepad-git);
//! - [`Extract`]: chunks from a base circle's sources (`extract/`);
//! - [`Tokenize`]: terms per field for chunks and queries (`terms.rs`);
//! - [`SegmentSink`], [`SegmentRead`], [`Store`]: segments, bases and manifests (`store/`);
//! - [`Circles`]: the registry, `def_hash`, and opening circles at a revision (`circles.rs`,
//!   `open.rs`);
//! - [`Search`]: ranked per-circle results, handle resolution, refs (`query/`);
//! - [`Render`]: text and `--json` output for every verb (`output.rs`);
//! - [`Harness`]: the eval (`eval/`).
//!
//! Index data is machine-local, under `~/.makepad/index/` (`MAKEPAD_HOME` moves it), never in a
//! tree: `repo-<id>/<circle dir>/` for a repo's circles and `notes-<topic>/` for notes circles.
//!
//! One hash everywhere: SHA-1 (`makepad_git::oid::hash_object("blob", bytes)`). Segment files
//! are named by its full hex; [`RepoId`] and [`SegmentKey::def_hash`] are its first 8 bytes,
//! little-endian. Never a process-seeded hasher.

pub mod circles;
pub mod cli;
pub mod delta;
pub mod eval;
pub mod extract;
pub mod open;
pub mod output;
pub mod query;
pub mod store;
pub mod terms;

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

/// Bumped whenever extraction, tokenizing or the segment format changes: a base is keyed by
/// (tree oid, indexer version), so a new indexer never reads an old segment.
pub const INDEXER_VERSION: u32 = 1;
/// Hits per `find`, unless `-n` says otherwise.
pub const DEFAULT_LIMIT: usize = 10;
/// Hits shown per package group before the group's count line.
pub const HITS_PER_GROUP: usize = 3;
/// A text hit line, at most.
pub const MAX_LINE_CHARS: usize = 120;
/// A hit's gist (from a doc comment, description or heading; never generated), at most.
pub const MAX_GIST_CHARS: usize = 80;
/// `find` output budget: p90 at most this many bytes.
pub const FIND_P90_BYTES: usize = 1536;
/// `show` prints at most this many lines unless `--all`.
pub const SHOW_MAX_LINES: usize = 80;
/// Repo paths the built-in `makepad` circle never admits: the index's own eval data and
/// fixture are not makepad content (provenance [`Provenance::IndexOwnData`]).
pub const INDEX_OWN_DATA: &[&str] = &["libs/repo_index/eval/", "libs/repo_index/fixture/"];

// ---------------------------------------------------------------------------------------------
// Ids and revisions

/// A git object id (SHA-1): commits, trees and blobs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid(pub [u8; 20]);

/// A repository, by the hash of its git common dir's canonical path: every worktree of one
/// clone shares it, and two clones never collide. Names the `repo-<id>` directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RepoId(pub u64);

/// A revision as asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevSpec {
    /// The worktree: HEAD plus the query-time overlay of everything between the base and the
    /// files on disk. The default for every verb but `eval`.
    Worktree,
    /// A pinned revision: a full or abbreviated commit hex, `HEAD`, a branch (`work`), a remote
    /// branch (`upstream/work`) or a full ref name. Read from git objects only; no overlay. A
    /// question's `rev: "tip"` is `Pinned("HEAD")`: the HEAD of the worktree under test, never
    /// its uncommitted files (the overlay has its own fixture, EVAL §3.5).
    Pinned(String),
}

/// A resolved revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rev {
    pub spec: RevSpec,
    pub commit: Oid,
    pub tree: Oid,
}

// ---------------------------------------------------------------------------------------------
// Circles

/// A circle's display name: `makepad`, `makepad:history`, `notes:<topic>`, `commercial`. Its
/// directory name replaces `:` with `-` (NTFS forbids `:`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CircleName(pub String);

/// Every circle this machine knows: the built-in ones for the repo it stands in, plus
/// `~/.makepad/index/circles.conf` (machine-local, never committed).
#[derive(Clone, Debug, Default)]
pub struct CircleManifest {
    pub defs: Vec<CircleDef>,
    pub conf: Option<PathBuf>,
    /// Machine-local eval question files (`eval = <file.jsonl>` lines of `circles.conf`).
    pub eval: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct CircleDef {
    pub name: CircleName,
    pub body: CircleBody,
    pub origin: Origin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Defined in this crate's source (`makepad`, `makepad:history`, `commercial`, ...).
    BuiltIn,
    /// A line of `circles.conf`.
    Conf { line: u32 },
}

#[derive(Clone, Debug)]
pub enum CircleBody {
    Base(BaseCircle),
    Derived(CircleExpr),
}

/// A circle that owns segments. Records are admitted by provenance (where they come from),
/// never by path pattern at query time or by topic words.
#[derive(Clone, Debug)]
pub struct BaseCircle {
    pub source: BaseSource,
    /// The record families it admits.
    pub families: Vec<Family>,
    /// Sources within `source` that belong elsewhere; applied at extraction, in the base build,
    /// a persisted delta and the query-time overlay alike, so excluded files never become
    /// chunks, terms or statistics.
    pub exclude: Vec<Exclusion>,
}

#[derive(Clone, Debug)]
pub enum BaseSource {
    /// A repo's tree at a revision. `root` is the repo's worktree root.
    Tree { root: PathBuf },
    /// Commits reachable from `upstream` (for example `refs/remotes/upstream/work`) or from a
    /// `local` ref. Which are local-only is computed at query time ([`Visibility`]).
    History { root: PathBuf, upstream: String, local: Vec<String> },
    /// Notes: every text file of the sources' directories, read from disk.
    Notes { sources: Vec<NotesSource> },
}

/// One directory of a notes circle. Its chunks' paths are `<label>/<path in dir>`, so sources
/// never collide and a note gold path (EVAL §1.3) is a chunk path as is. Labels are unique
/// within a circle; a duplicate is a `circles.conf` error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotesSource {
    pub dir: PathBuf,
    /// Default: the directory's name for a directory outside a repo (`<topic>`); for one inside
    /// a repo, the checkout directory's name and the path from its root (`makepad/local/plans`
    /// in a checkout named `makepad`).
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exclusion {
    /// A `/`-separated prefix, relative to the source root.
    pub prefix: String,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// The index's own eval data and fixture ([`INDEX_OWN_DATA`]).
    IndexOwnData,
    /// A notes directory another circle claims in `circles.conf` (for example a
    /// `local/agent_state/<dir>` mapped to `notes:<topic>`).
    ClaimedBy(CircleName),
}

/// A derived circle: composition over other circles. Containment holds under every form; each
/// base keeps its own statistics and a composite only arranges results.
#[derive(Clone, Debug)]
pub enum CircleExpr {
    Circle(CircleName),
    /// Per-circle blocks, or merged by rank (RRF); never by score.
    Union { of: Vec<CircleExpr>, fuse: Fuse },
    /// A sub-circle.
    Select { of: Box<CircleExpr>, by: Selector },
    /// Records of `left` and `right` that share a key (a pullback over the key).
    Overlap { left: Box<CircleExpr>, right: Box<CircleExpr>, on: KeyKind },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fuse {
    Blocks,
    Rank,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selector {
    Kinds(Vec<KindFilter>),
    PathPrefix(String),
    Package(String),
}

/// Which commits a history circle shows at a revision, and which are local-only. Computed when
/// the circle is opened, never stored: both change whenever a ref moves.
#[derive(Clone, Debug, Default)]
pub struct Visibility {
    /// Commits reachable from the revision the circle is opened at.
    pub reachable: HashSet<Oid>,
    /// Commits reachable from a `local` ref and not from `upstream`.
    pub local: HashSet<Oid>,
    /// The short names of the local refs `local` was computed from (`work`), for
    /// [`Pointer`]'s branch.
    pub local_refs: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Keys: how records relate across circles

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyKind {
    Path,
    Commit,
    Blob,
    Ident,
    Package,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// A repo-relative path, optionally a line in it.
    Path { path: String, line: Option<u32> },
    Commit(Oid),
    /// Content identity: byte-identical copies share it and collapse to one hit.
    Blob(Oid),
    Ident(String),
    Package(String),
}

/// A stored morphism: a record in one circle citing a record of another (a note naming a
/// makepad path, `path:line` or commit). Lives in the citing circle only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub to: CircleName,
    pub key: Key,
}

// ---------------------------------------------------------------------------------------------
// Chunks

/// A group of kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Family {
    Crate,
    File,
    Item,
    Doc,
    Splash,
    Commit,
    Note,
}

impl Family {
    pub fn as_str(self) -> &'static str {
        match self {
            Family::Crate => "crate",
            Family::File => "file",
            Family::Item => "item",
            Family::Doc => "doc",
            Family::Splash => "splash",
            Family::Commit => "commit",
            Family::Note => "note",
        }
    }
}

/// What `--kind` takes: a kind name (`fn`, `struct`, ...) or a family name (`item`, `doc`,
/// `commit`, `note`, ...). `crate`, `file`, `doc`, `splash` and `commit` name both, meaning the
/// family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KindFilter {
    Kind(Kind),
    Family(Family),
}

/// A chunk's kind; `as_str` is the stable name in text and `--json` output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Crate,
    File,
    Fn,
    /// A method of an impl or trait: its own item, parented to the impl header.
    Method,
    Struct,
    Enum,
    Union,
    Trait,
    Impl,
    Mod,
    Type,
    Const,
    Static,
    Macro,
    /// One Markdown section.
    Doc,
    /// A top-level definition in a `.splash` file.
    Splash,
    Commit,
    /// One bullet of a commit message, parented to its commit.
    Bullet,
    /// One record of a Markdown note, split as [`PointerUnit::Blobs`] says (fences are not
    /// special, unlike a tree circle's [`Kind::Doc`]).
    Note,
    /// A notes file that is not Markdown: one chunk, the whole file (code, diffs, logs), or its
    /// path alone for a binary file ([`Flags::binary`]).
    NoteFile,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Crate => "crate",
            Kind::File => "file",
            Kind::Fn => "fn",
            Kind::Method => "method",
            Kind::Struct => "struct",
            Kind::Enum => "enum",
            Kind::Union => "union",
            Kind::Trait => "trait",
            Kind::Impl => "impl",
            Kind::Mod => "mod",
            Kind::Type => "type",
            Kind::Const => "const",
            Kind::Static => "static",
            Kind::Macro => "macro",
            Kind::Doc => "doc",
            Kind::Splash => "splash",
            Kind::Commit => "commit",
            Kind::Bullet => "bullet",
            Kind::Note => "note",
            Kind::NoteFile => "note-file",
        }
    }
    pub fn family(self) -> Family {
        match self {
            Kind::Crate => Family::Crate,
            Kind::File => Family::File,
            Kind::Fn
            | Kind::Method
            | Kind::Struct
            | Kind::Enum
            | Kind::Union
            | Kind::Trait
            | Kind::Impl
            | Kind::Mod
            | Kind::Type
            | Kind::Const
            | Kind::Static
            | Kind::Macro => Family::Item,
            Kind::Doc => Family::Doc,
            Kind::Splash => Family::Splash,
            Kind::Commit | Kind::Bullet => Family::Commit,
            Kind::Note | Kind::NoteFile => Family::Note,
        }
    }
}

/// The BM25F fields, in posting order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Field {
    Name,
    Path,
    Sig,
    Doc,
    Body,
}

impl Field {
    pub const ALL: [Field; 5] = [Field::Name, Field::Path, Field::Sig, Field::Doc, Field::Body];
}

/// Byte range in the source file, `lo..hi`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub lo: u32,
    pub hi: u32,
}

/// 1-based inclusive line range.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lines {
    pub start: u32,
    pub end: u32,
}

/// Field text as extracted; tokenized by [`Tokenize`], never stored whole.
#[derive(Clone, Debug, Default)]
pub struct Fields {
    /// `Device::memory_type`, a package name, a heading, a commit subject.
    pub name: String,
    /// The record's path (for a note, its path within its source, without the label); for a
    /// commit, its touched paths (low weight, collapsed in output).
    pub path: String,
    pub sig: String,
    pub doc: String,
    /// For [`Kind::Note`] and [`Kind::NoteFile`], the record's whole text exactly (the
    /// [`PointerUnit::Blobs`] rule matches it), empty for a binary file; the store keeps it as
    /// [`SegmentRead::record_text`].
    pub body: String,
}

/// The keys a record carries: the morphisms computed at query time, plus stored edges. All of
/// them are stored and looked up by [`SegmentRead::chunks_with`] / [`SegmentRead::cited_by`].
#[derive(Clone, Debug, Default)]
pub struct Keys {
    /// Its own path, or a commit's touched paths.
    pub paths: Vec<String>,
    pub commit: Option<Oid>,
    pub blob: Option<Oid>,
    /// Identifiers defined here (exact case), for `def` and identifier overlaps.
    pub idents: Vec<String>,
    pub package: Option<String>,
    pub edges: Vec<Edge>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    pub public: bool,
    pub documented: bool,
    /// A definition (ranked above uses).
    pub definition: bool,
    /// Generated or vendored: skeleton only, ranked down.
    pub generated: bool,
    /// The lexer could not pair it: a file-level bag of identifiers.
    pub fallback: bool,
    /// A binary notes file (a NUL in its first 8000 bytes): a record of its path only.
    pub binary: bool,
}

/// One record, as extraction produces it.
#[derive(Clone, Debug)]
pub struct Chunk {
    pub kind: Kind,
    pub circle: CircleName,
    /// Source-root-relative, `/`-separated; a note's is `<label>/<path in dir>`.
    pub path: String,
    /// For a note, the byte length of the `<label>` prefix of `path` (labels may contain `/`);
    /// 0 for every other chunk. [`ChunkView::path_in_source`] is the rest.
    pub label_len: u16,
    pub span: Span,
    pub lines: Lines,
    pub name: String,
    /// At most [`MAX_GIST_CHARS`]: the first doc sentence, crate description or heading; never
    /// generated. Filled by [`Extract`], stored verbatim.
    pub gist: String,
    /// The stream index of an earlier chunk (an impl for its methods, a commit for its bullets,
    /// a file for its items): chunks are numbered from 0 in the order [`Extract`] emits them,
    /// which is the [`ChunkId`] the sink assigns, so nothing is dropped between the two. For
    /// [`Extract::extract_file`] the stream is that one call.
    pub parent: Option<u32>,
    pub fields: Fields,
    pub keys: Keys,
    pub flags: Flags,
    pub detail: Detail,
}

/// Per-family data beyond the fields.
#[derive(Clone, Debug, Default)]
pub enum Detail {
    #[default]
    None,
    Crate(CrateCard),
    Commit(CommitInfo),
}

/// The "what is this" card.
#[derive(Clone, Debug, Default)]
pub struct CrateCard {
    pub dir: String,
    pub description: String,
    /// Path dependencies, with the target cfg they hang under (empty: all targets).
    pub deps: Vec<(String, String)>,
    pub dependents: Vec<String>,
    /// `libs/app_registry/apps.json` entries whose package reaches this one.
    pub apps: Vec<String>,
    /// `lib`, `bin <name>`, ...
    pub targets: Vec<String>,
    pub rs_files: u32,
    /// Top-level modules of the crate root.
    pub modules: Vec<String>,
    /// `Cargo.toml` names paths in a private or ignored tree (`apps/commercial`), so "used by 0"
    /// is said for the public tree only.
    pub private_members: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CommitInfo {
    /// Committer time (landing date on squashed history), seconds since the epoch.
    pub time: i64,
    pub parents: Vec<Oid>,
}

// ---------------------------------------------------------------------------------------------
// Terms

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TermForm {
    /// The identifier as written (exact case).
    Exact,
    /// Folded whole (`libs/search::fold`).
    Folded,
    /// A part from a snake_case, CamelCase, acronym or digit split, folded.
    Part,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermOcc {
    pub field: Field,
    pub form: TermForm,
    pub text: String,
    pub tf: u32,
}

/// A chunk's terms plus its field lengths (BM25F norms).
#[derive(Clone, Debug, Default)]
pub struct FieldTerms {
    pub terms: Vec<TermOcc>,
    pub lengths: [u32; 5],
}

/// One query term; `word` indexes the query word it came from (coverage).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryTerm {
    pub form: TermForm,
    pub text: String,
    pub word: u16,
}

// ---------------------------------------------------------------------------------------------
// Segments, bases and manifests

/// What a segment was built from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SourceStamp {
    Tree { tree: Oid },
    /// The tips the commits were walked from.
    History { tips: Vec<Oid> },
    /// A hash of the notes sources' (label, path, size, content hash) list.
    Notes { content: u64 },
}

/// A segment's identity. A base of the code circle is the segment for `(tree oid, indexer
/// version)`: every worktree of the repo on that tree shares it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SegmentKey {
    pub repo: Option<RepoId>,
    pub circle: CircleName,
    pub source: SourceStamp,
    /// [`Circles::def_hash`] of the circle's [`BaseCircle`]: families, exclusions, ref names and
    /// notes labels, never machine paths (`root`, `dir`), so worktrees share bases and changing
    /// an exclusion makes a new key, never a mix.
    pub def_hash: u64,
    pub indexer: u32,
}

/// A segment file's name in its circle directory, `seg-<sha1 hex of its content>.mpx`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SegmentId(pub String);

/// A circle's term statistics (summed over its live segments; never pooled across circles).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CircleStats {
    pub chunks: u64,
    pub field_lengths: [u64; 5],
}

/// A chunk's index within its segment: its stream index ([`Chunk::parent`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkId(pub u32);

/// One posting: a chunk and its term frequency per [`Field`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Posting {
    pub chunk: ChunkId,
    pub tf: [u32; 5],
}

/// A stored chunk, read from a mapped segment. Text beyond name, gist and sig is read back from
/// the source (worktree file, blob, or notes file) when shown.
#[derive(Clone, Copy, Debug)]
pub struct ChunkView<'a> {
    pub kind: Kind,
    pub path: &'a str,
    pub span: Span,
    pub lines: Lines,
    pub name: &'a str,
    pub parent: Option<ChunkId>,
    pub package: &'a str,
    /// At most [`MAX_GIST_CHARS`]: the first doc sentence, description or heading.
    pub gist: &'a str,
    pub sig: &'a str,
    pub blob: Option<Oid>,
    pub commit: Option<Oid>,
    pub flags: Flags,
    pub field_lengths: [u32; 5],
    /// As [`Chunk::label_len`].
    pub label_len: u16,
}

impl<'a> ChunkView<'a> {
    /// A note's path within its notes source (the pointer rule matches on it); for any other
    /// chunk, its path.
    pub fn path_in_source(&self) -> &'a str {
        match self.label_len {
            0 => self.path,
            n => &self.path[n as usize + 1..],
        }
    }
}

/// Writes one segment: tmp file, fsync, rename to its content name. `add` returns `ChunkId(n)`
/// for the n-th chunk added.
pub trait SegmentSink {
    fn add(&mut self, chunk: &Chunk, terms: &FieldTerms) -> Result<ChunkId, Error>;
    fn finish(self: Box<Self>) -> Result<SegmentId, Error>;
}

/// A mapped, immutable segment. Its format holds every key and edge from the first version, so
/// the lookups composition needs never change the format.
pub trait SegmentRead {
    fn key(&self) -> &SegmentKey;
    /// The segment's content name (`circles` and `status` list it).
    fn id(&self) -> SegmentId;
    fn stats(&self) -> CircleStats;
    fn chunk_count(&self) -> u32;
    fn df(&self, form: TermForm, term: &str) -> u32;
    fn postings(&self, form: TermForm, term: &str) -> Box<dyn Iterator<Item = Posting> + '_>;
    fn chunk(&self, id: ChunkId) -> ChunkView<'_>;
    fn keys(&self, id: ChunkId) -> Keys;
    fn detail(&self, id: ChunkId) -> Detail;
    /// Chunks carrying a key: `Path` (the overlay's shadowing, `show`), `Package` (`crate`),
    /// `Commit` (a commit handle), `Ident` (`def`), `Blob` (copies), and overlaps on any.
    fn chunks_with(&self, key: &Key) -> Vec<ChunkId>;
    /// Chunks whose stored edge targets `key`, with the edge ("cited by notes:X").
    fn cited_by(&self, key: &Key) -> Vec<(ChunkId, Edge)>;
    /// A notes record's whole text (notes segments keep it, empty for a binary file; others
    /// return `None`), for exact pointer counts.
    fn record_text(&self, id: ChunkId) -> Option<&str>;
}

/// The numbered manifest of an append circle (history, notes): version N is committed by
/// `hard_link` put-if-absent of `manifest-<N>`; a loser re-applies its change on N and tries
/// N+1. Readers take the newest manifest with a valid checksum.
#[derive(Clone, Debug)]
pub struct SegmentManifest {
    pub circle: CircleName,
    pub version: u64,
    pub segments: Vec<SegmentId>,
    pub stats: CircleStats,
    pub sources: Vec<SourceStamp>,
    /// Who committed it (pid and worktree), for the log.
    pub writer: String,
}

#[derive(Clone, Debug)]
pub enum ManifestChange {
    Add(SegmentId),
    /// A tiered merge; abandoned when an input is no longer live.
    Merge { inputs: Vec<SegmentId>, output: SegmentId },
    /// A whole-circle rebuild (a notes topic).
    Replace(SegmentId),
}

/// Bases (content-addressed, built once under an exclusive OS file lock on the key, released
/// if its holder dies; a loser blocks, then maps the winner's file. The lock only dedupes
/// builds: correctness rests on content names and put-if-absent), manifests, and GC. Deletes that fail (Windows: still mapped) are
/// retried and logged, never fatal. Choosing which base a worktree needs is [`Circles::open`]'s.
pub trait Store {
    fn open_base(&self, key: &SegmentKey) -> Result<Option<Box<dyn SegmentRead>>, Error>;
    /// Builds the base for `key` unless it exists; `fill` writes its chunks.
    fn build_base(
        &self,
        key: &SegmentKey,
        fill: &mut dyn FnMut(&mut dyn SegmentSink) -> Result<(), Error>,
    ) -> Result<Box<dyn SegmentRead>, Error>;
    fn new_segment(&self, key: &SegmentKey) -> Result<Box<dyn SegmentSink>, Error>;
    fn manifest(&self, repo: Option<RepoId>, circle: &CircleName) -> Result<Option<SegmentManifest>, Error>;
    fn commit(&self, repo: Option<RepoId>, circle: &CircleName, change: ManifestChange) -> Result<SegmentManifest, Error>;
    fn open_segment(&self, repo: Option<RepoId>, circle: &CircleName, id: &SegmentId) -> Result<Box<dyn SegmentRead>, Error>;
    /// Sweeps only `repo`: its bases not in `live_bases` and its manifest circles not in
    /// `live_circles` (under `repo-<repo>/`), plus the machine's notes circles (`notes-<topic>/`,
    /// repo `None`) not in `live_circles`, each once its newest file is older than the grace
    /// period. Never touches another `repo-<id>/` directory; a store-wide sweep would be its own
    /// verb.
    fn gc(&self, repo: RepoId, live_bases: &[SegmentKey], live_circles: &[(Option<RepoId>, CircleName)]) -> Result<GcReport, Error>;
}

#[derive(Clone, Debug, Default)]
pub struct GcReport {
    pub removed: Vec<PathBuf>,
    /// Kept for now, with why (grace period, still mapped).
    pub deferred: Vec<(PathBuf, String)>,
}

// ---------------------------------------------------------------------------------------------
// Git, extraction, tokenizing

/// One file of a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeFile {
    pub path: String,
    pub blob: Oid,
    pub mode: u32,
}

/// A path that differs between a base and the worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Added(String),
    Modified(String),
    Deleted(String),
}

/// A commit as history extraction reads it.
#[derive(Clone, Debug)]
pub struct CommitRecord {
    pub oid: Oid,
    pub info: CommitInfo,
    pub message: String,
    pub touched: Vec<String>,
}

/// Git reads over one repository (makepad-git; no shell-outs).
pub trait Repo {
    fn id(&self) -> RepoId;
    fn root(&self) -> PathBuf;
    /// Resolves a revision (see [`RevSpec::Pinned`]); an ambiguous abbreviation is an error.
    fn resolve(&mut self, spec: &RevSpec) -> Result<Rev, Error>;
    fn files(&mut self, tree: Oid) -> Result<Vec<TreeFile>, Error>;
    fn read_blob(&mut self, blob: Oid) -> Result<Vec<u8>, Error>;
    /// Paths that differ between `base` and the worktree: committed since, staged, unstaged
    /// (stat-checked against the git index, content-checked when the stat differs) and deleted;
    /// plus untracked files that the ignore rules do not ignore, as `Added`. Ignored files are
    /// never changes (that is how `local/` stays out of `makepad`).
    fn changes(&mut self, base: &Rev) -> Result<Vec<Change>, Error>;
    /// Commits reachable from some `tips` and from none of `hide` (`git rev-list tips ^hide`),
    /// newest first.
    fn commits(&mut self, tips: &[Oid], hide: &[Oid]) -> Result<Vec<CommitRecord>, Error>;
    /// The ids of the commits reachable from some `tips`, newest first: [`Repo::commits`]
    /// without messages or touched paths (what a notes cite needs).
    fn commit_ids(&mut self, tips: &[Oid]) -> Result<Vec<Oid>, Error>;
    fn merge_base(&mut self, a: Oid, b: Oid) -> Result<Option<Oid>, Error>;
    /// Every worktree of this repo (the main checkout and each linked one) with its `HEAD`
    /// commit; GC keeps the bases their merge-bases name.
    fn worktrees(&mut self) -> Result<Vec<(PathBuf, Oid)>, Error>;
}

#[derive(Clone, Debug, Default)]
pub struct ExtractReport {
    pub files: u64,
    pub chunks: u64,
    /// Files admitted only as a bag of identifiers, with the lexer's reason.
    pub fallbacks: Vec<(String, String)>,
    /// Files skipped by an exclusion, counted per prefix.
    pub excluded: Vec<(String, u64)>,
    /// Files not admitted at all (unreadable), with why.
    pub skipped: Vec<(String, String)>,
}

/// Chunks from a base circle's sources. `rev` is `None` for notes circles.
pub trait Extract {
    fn extract(
        &mut self,
        circle: &CircleName,
        def: &BaseCircle,
        rev: Option<&Rev>,
        repo: &mut dyn Repo,
        sink: &mut dyn FnMut(Chunk) -> Result<(), Error>,
    ) -> Result<ExtractReport, Error>;
    /// The chunks of one file's bytes (the overlay re-extracts changed files with it). Empty for
    /// a path the circle's exclusions cover.
    fn extract_file(&mut self, circle: &CircleName, def: &BaseCircle, path: &str, bytes: &[u8]) -> Result<Vec<Chunk>, Error>;
}

pub trait Tokenize {
    fn chunk_terms(&self, chunk: &Chunk) -> FieldTerms;
    fn query_terms(&self, words: &[String]) -> Vec<QueryTerm>;
}

// ---------------------------------------------------------------------------------------------
// Opening circles

/// The overlay chunks of one changed file; their `parent` indexes this file's `chunks`.
#[derive(Clone, Debug)]
pub struct OverlayFile {
    pub path: String,
    pub chunks: Vec<(Chunk, FieldTerms)>,
}

/// A circle opened at a revision: its segments, statistics, and (worktree) overlay.
pub struct OpenCircle {
    pub name: CircleName,
    /// `None` for notes circles.
    pub rev: Option<Rev>,
    pub segments: Vec<Box<dyn SegmentRead>>,
    pub stats: CircleStats,
    pub overlay: Vec<OverlayFile>,
    /// Base paths the overlay replaces or deletes.
    pub shadowed: Vec<String>,
    /// History circles: what is visible at `rev`, and what is local-only.
    pub visible: Option<Visibility>,
}

/// The circles a query named, plus the registered ones it did not (for pointer counts only).
pub struct Opened {
    pub open: Vec<OpenCircle>,
    /// Opened only when already built (a base or manifest exists), never built or rebuilt
    /// during a query.
    pub others: Vec<OpenCircle>,
    /// Registered circles not named and not built: no pointer, listed by `status`.
    pub unbuilt: Vec<CircleName>,
}

/// The circle registry and the one way to open circles: resolve the revision, take the base
/// for its merge-base with upstream (`Store::open_base`, else `build_base` through
/// [`Extract`] → [`Tokenize`] → [`SegmentSink`]), build the overlay (`delta.rs`) and, for history,
/// the [`Visibility`]. The implementation holds its [`Repo`], [`Store`], [`Extract`] and
/// [`Tokenize`]; cli and eval both open through it.
pub trait Circles {
    fn manifest(&mut self) -> Result<CircleManifest, Error>;
    /// The repo the caller stands in: `makepad` in a makepad worktree, `notes:<topic>` in a
    /// notes directory.
    fn default_scope(&self, manifest: &CircleManifest, cwd: &Path) -> Vec<CircleName>;
    /// `--in a,b` and derived names into expressions; an unknown circle is an error.
    fn parse_in(&self, manifest: &CircleManifest, arg: &str) -> Result<Vec<CircleExpr>, Error>;
    /// The SHA-1 of the canonical encoding of families, exclusions, ref names and notes labels.
    fn def_hash(&self, def: &BaseCircle) -> u64;
    /// Builds what `names` need (bases once, under the store's lock); the other circles are
    /// opened only if already built ([`Opened::others`]).
    fn open(&mut self, manifest: &CircleManifest, names: &[CircleName], rev: &RevSpec) -> Result<Opened, Error>;
}

// ---------------------------------------------------------------------------------------------
// Queries and hits

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Find,
    /// Exact definition lookup.
    Def,
    /// Name-based uses (not semantic resolution).
    Refs,
    History,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub mode: Mode,
    pub words: Vec<String>,
    /// The circles named by `--in`, or the default scope of the repo the caller stands in.
    pub circles: Vec<CircleExpr>,
    pub rev: RevSpec,
    pub kinds: Vec<KindFilter>,
    pub packages: Vec<String>,
    pub paths: Vec<String>,
    pub limit: usize,
    pub pointers: bool,
}

/// A readable hit handle: what a hit prints is what `show` takes, from any lane, with no
/// session store. Text forms:
/// - `path:line`, `path:start-end`: a range in the circle's source;
/// - `path::Item::method`: an item by name;
/// - `crate:<package>`;
/// - a commit's hex (7 or more digits);
/// - any of these prefixed by `<circle>/` outside the default circle, for example
///   `notes:<topic>/<label>/FILE.md:64`;
/// - suffixed by `@<rev>` when the hit came from a pinned revision, so its lines resolve there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handle {
    pub circle: Option<CircleName>,
    pub target: Target,
    pub rev: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Lines { path: String, lines: Lines },
    Item { path: String, item: String },
    Crate(String),
    Commit(String),
}

/// Where a resolved handle points: a circle of [`Opened::open`] and a chunk in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Located {
    pub circle: usize,
    pub at: ChunkAt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkAt {
    Segment { segment: usize, chunk: ChunkId },
    Overlay { file: usize, chunk: usize },
}

/// Why a hit matched: field and query term.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Why {
    pub field: Field,
    pub term: String,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub handle: Handle,
    pub kind: Kind,
    pub package: String,
    pub path: String,
    pub lines: Lines,
    pub name: String,
    pub sig: String,
    pub gist: String,
    pub score: f32,
    pub why: Vec<Why>,
    /// Other locations of byte-identical copies (one blob, one hit).
    pub also_at: Vec<String>,
    /// From the query-time overlay, not the base.
    pub overlay: bool,
    /// A commit reachable from local refs only (branch work not upstream).
    pub local: bool,
}

/// One circle's block: its own header, ranking and statistics.
#[derive(Clone, Debug)]
pub struct CircleBlock {
    pub circle: CircleName,
    /// Abbreviated commit of the revision indexed; empty for notes circles.
    pub rev: String,
    pub total: u64,
    pub definitions: u64,
    pub uses: u64,
    pub hits: Vec<Hit>,
    /// Set when the circle holds no definition or use: absence is an answer.
    pub absent: bool,
    /// The nearest in-circle hits shown under an absence.
    pub nearest: Vec<Hit>,
}

/// The unit a pointer counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerUnit {
    /// Notes circles only. Distinct blobs (byte-identical files count once) with at least one
    /// matching record; the rule is `eval/baseline/pointer_count.py`'s (EVAL §1.3), exactly:
    /// - a file is binary when its first 8000 bytes hold a NUL; its one record has empty text;
    /// - other files are decoded as lossy UTF-8 (`String::from_utf8_lossy`);
    /// - a `.md` file's records are the pieces of a split before every line that starts with
    ///   `#{1,6} ` (fences are not special), blank pieces dropped, the whole text if none is
    ///   left; any other file is one record;
    /// - a record matches when `lowercase(text) + "\n" + lowercase(path within its source)`
    ///   contains every term as a substring, the terms being the query split on whitespace and
    ///   lowercased (`str::to_lowercase`): one term may match the text and another the path.
    ///
    /// Notes chunks are exactly these records ([`SegmentRead::record_text`]). The count is a
    /// substring count over the whole query, so it can differ from the token-ranked totals of
    /// the same circle's block once it is opened with `--in`.
    Blobs,
    /// Tree circles: chunks matching every query term by token (`find`'s own matching).
    Chunks,
    /// History circles: commits with a matching message or touched path, by token.
    Commits,
    /// Files touched by branch-work commits. Every branch pointer.
    Files,
}

/// A count in a circle the query did not open (`circle`), or in branch work not upstream
/// (`branch`, with `commits` the number of unindexed commits).
#[derive(Clone, Debug)]
pub struct Pointer {
    pub circle: Option<CircleName>,
    pub branch: Option<String>,
    pub count: u64,
    pub unit: PointerUnit,
    pub commits: u64,
    /// One example (a commit's short hex and subject), if any.
    pub sample: String,
}

#[derive(Clone, Debug, Default)]
pub struct FindOutput {
    pub blocks: Vec<CircleBlock>,
    pub pointers: Vec<Pointer>,
}

/// Uses by name, counted.
#[derive(Clone, Debug, Default)]
pub struct UseSummary {
    pub total: u64,
    pub files: u64,
    /// Lines per directory, most first.
    pub by_dir: Vec<(String, u64)>,
}

/// `show`: an item, section or commit, bounded to [`SHOW_MAX_LINES`] unless `--all`.
#[derive(Clone, Debug)]
pub struct ShowOutput {
    pub circle: CircleName,
    pub rev: String,
    pub handle: Handle,
    pub kind: Kind,
    pub package: String,
    pub path: String,
    pub lines: Lines,
    pub name: String,
    pub text: Vec<String>,
    /// Lines cut by the bound.
    pub more: u32,
    /// Item uses by name (`refs` for all).
    pub uses: Option<UseSummary>,
    /// Records of registered notes circles that cite this one, counted per circle.
    pub cited_by: Vec<(CircleName, u64)>,
    pub also_at: Vec<String>,
}

/// `crate`: the card, as stored.
#[derive(Clone, Debug)]
pub struct CrateView {
    pub circle: CircleName,
    pub rev: String,
    pub name: String,
    pub card: CrateCard,
}

/// `refs`: definitions, then re-exports, then uses, then same-named definitions elsewhere.
#[derive(Clone, Debug)]
pub struct RefsOutput {
    pub circle: CircleName,
    pub rev: String,
    pub ident: String,
    pub defs: Vec<Hit>,
    pub reexports: Vec<Hit>,
    pub uses: UseSummary,
    /// The use lines themselves, with `--uses`.
    pub use_hits: Vec<Hit>,
    pub other_defs: Vec<Hit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleKind {
    /// The path or name does not resolve.
    Dangling,
    /// A successor says "was X" and X has no callers.
    Retired,
    /// The tree the code reads is absent.
    Missing,
    /// The names exist only in private or ignored trees.
    Outside,
    /// A design doc is cited but not present.
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct StaleFinding {
    pub kind: StaleKind,
    pub path: String,
    pub line: u32,
    pub name: String,
    pub detail: String,
    /// Other places with the same finding.
    pub also: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct StaleOutput {
    pub circle: CircleName,
    pub rev: String,
    pub findings: Vec<StaleFinding>,
}

/// One circle, for `circles` (every configured circle) and `status` (the ones opened here).
#[derive(Clone, Debug)]
pub struct CircleStatus {
    pub name: CircleName,
    pub origin: Origin,
    /// Its source or expression, one line.
    pub source: String,
    pub rev: String,
    pub segments: Vec<SegmentId>,
    pub chunks: u64,
    /// Files in the worktree overlay.
    pub overlay_files: u64,
    pub built: bool,
    /// Why not built, or what is pending.
    pub note: String,
}

#[derive(Clone, Debug, Default)]
pub struct StatusOutput {
    pub circles: Vec<CircleStatus>,
}

/// Everything a verb prints.
#[derive(Clone, Debug)]
pub enum Output {
    Find(FindOutput),
    Show(Box<ShowOutput>),
    Crate(Box<CrateView>),
    Refs(Box<RefsOutput>),
    Stale(StaleOutput),
    Status(StatusOutput),
}

/// Ranking, handle resolution and refs over opened circles (`others` only for pointers).
pub trait Search {
    fn find(&self, opened: &Opened, query: &Query) -> Result<FindOutput, Error>;
    /// Every chunk a handle names, in the circles opened; empty when none.
    fn resolve(&self, opened: &Opened, handle: &Handle) -> Result<Vec<Located>, Error>;
    fn refs(&self, opened: &Opened, ident: &str, uses: bool) -> Result<RefsOutput, Error>;
}

// ---------------------------------------------------------------------------------------------
// Output records

/// `--json` records, one JSON object per line, every field always present.
#[derive(Clone, Debug)]
pub enum Record {
    /// `{"header":true,"circle","rev","total","definitions","uses","absent"}`
    Header { circle: CircleName, rev: String, total: u64, definitions: u64, uses: u64, absent: bool },
    /// `{"handle","circle","rev","kind","package","path","start","end","name","sig","gist",
    /// "score","why","also_at","overlay","local","nearest"}`
    Hit { circle: CircleName, rev: String, hit: Box<Hit>, nearest: bool },
    /// `{"pointer":true,"circle","branch","count","unit","commits","sample"}`
    Pointer(Pointer),
    /// `{"show":true,"circle","rev","handle","kind","package","path","start","end","name",
    /// "text","more","uses","cited_by","also_at"}`
    Show(Box<ShowOutput>),
    /// `{"crate":true,"circle","rev","name","dir","description","deps","dependents","apps",
    /// "targets","rs_files","modules","private_members"}`
    Crate(Box<CrateView>),
    /// `{"refs":true,"circle","rev","ident","uses","by_dir"}`, then the defs, re-exports, use
    /// hits and other defs as `Hit` records.
    Refs { circle: CircleName, rev: String, ident: String, uses: UseSummary },
    /// `{"stale":"dangling|retired|missing|outside|unavailable","path","line","name","detail","also"}`
    Stale(StaleFinding),
    /// `{"circle_status":true,"name","origin","source","rev","segments","chunks","overlay_files",
    /// "built","note"}`
    Circle(CircleStatus),
}

/// Text output. `find`: a header line per circle (revision, counts, circle), hits grouped by
/// package (groups ordered by best hit, [`HITS_PER_GROUP`] each, a count footer), each hit one
/// line of at most [`MAX_LINE_CHARS`]: `handle  kind  name — gist`; then one `also:` pointer
/// line. The other verbs follow EVAL §4's examples.
pub trait Render {
    fn text(&self, out: &Output) -> String;
    fn records(&self, out: &Output) -> Vec<Record>;
    fn json_lines(&self, records: &[Record]) -> String;
}

// ---------------------------------------------------------------------------------------------
// Eval (EVAL §1.3 question and gold format, §3.1 metrics, §3.3 harness)

/// Which file a question came from; metrics are reported per set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuestionSet {
    /// `libs/repo_index/eval/questions.jsonl`.
    Shipped,
    /// An `eval =` file of `circles.conf`.
    Local(PathBuf),
    /// A held-out file whose path the caller passes; used only for acceptance.
    Heldout(PathBuf),
    /// Any other file named on the command line.
    File(PathBuf),
}

/// One eval question.
#[derive(Clone, Debug)]
pub struct Question {
    pub id: String,
    pub set: QuestionSet,
    pub query: String,
    /// The prompt an agent-mode run gives, when it differs from `query`.
    pub agent_prompt: Option<String>,
    pub precondition: Option<Precondition>,
    /// The revision the question is asked at, built from git objects; `"tip"` is
    /// `RevSpec::Pinned("HEAD")`.
    pub rev: RevSpec,
    pub circles: Vec<CircleName>,
    /// `where`, `what`, `why`, `current`, `circle`.
    pub qtype: String,
    /// `ident`, `concept`, `contain`, `cross`, `dedup`, `edge`, `error`, `paired-rev`,
    /// `self-exclusion`, ...
    pub subtype: String,
    pub difficulty: String,
    pub gold: Vec<Gold>,
    /// In-circle records (rel 0) that must not rank above the first rel-2 hit: ranking only,
    /// never a gate.
    pub distractors: Vec<Gold>,
    /// Other circles' sources (`notes:*`, `makepad`, `local/agent_state/*`,
    /// `libs/repo_index/eval/*`): a circle name or source path, `*` a glob. Any of them in the
    /// queried circle's ranked list is a containment failure (the hard gate).
    pub forbid: Vec<String>,
    pub notes: String,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Precondition {
    /// Skip, reported as skipped, when this circle is configured.
    Unconfigured(CircleName),
}

#[derive(Clone, Debug)]
pub struct Gold {
    /// 2 = required, 1 = supporting, 0 = a distractor.
    pub rel: u8,
    /// What verify-gold re-resolves the line by; present whenever the item has a line.
    pub symbol: Option<Anchor>,
    /// The scorer skips the item, reported as skipped, when this circle is not configured.
    pub requires: Option<CircleName>,
    pub target: GoldTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// A definition, as `fn publish_instances` or `enum KeyCode`.
    Def(String),
    Heading(String),
    Text(String),
}

#[derive(Clone, Debug)]
pub enum GoldTarget {
    /// A hit in the file whose range overlaps `[line-15, end+15]` or an `alt_lines` range; with
    /// no line, any hit in the file. A directory matches any hit inside it, or the crate card of
    /// the package at that directory.
    Path { path: String, line: Option<u32>, end: Option<u32>, alt_lines: Vec<Lines> },
    /// The same rules on a notes-circle record. The path is relative to the mailbox root or to
    /// the repo's parent (`makepad/local/...`): a notes chunk path ([`NotesSource::label`]).
    Note { path: String, line: Option<u32>, end: Option<u32>, alt_lines: Vec<Lines> },
    /// A crate card or hit for the package.
    Package(String),
    /// A history hit for the commit or an `alt` commit; with `via_pointer`, a pointer line
    /// naming it also counts.
    Commit { hex: String, alt: Vec<String>, via_pointer: bool },
    /// Empty: the answer states zero dependents in the scope. Otherwise every listed dependent
    /// is named (a superset is fine).
    Dependents { names: Vec<String>, scope: Option<String> },
    /// The output's absent flag for the scope (default: every queried circle). `semantic`:
    /// verify-gold does not check it lexically.
    Absent { scope: Option<AbsentScope>, semantic: bool },
    /// A pointer line to the circle; `count` in `unit` ([`PointerUnit::Blobs`]).
    Pointer { circle: CircleName, count: Option<u64>, unit: PointerUnit },
    /// The command fails with an error naming the circle (`"unconfigured circle"`), never
    /// an empty result.
    Error(String),
}

/// An absence's scope. The checkable ones are `path:<p>`, `package:<name>`, `fn <name>`, a
/// circle, or the query itself when it is one token; anything else is descriptive text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AbsentScope {
    Path(String),
    Package(String),
    Fn(String),
    Circle(CircleName),
    Text(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvalMode {
    Retrieval,
    Agent,
}

#[derive(Clone, Debug)]
pub struct EvalOpts {
    pub mode: EvalMode,
    pub sets: Vec<QuestionSet>,
    /// `--in`: only questions on these circles.
    pub circles: Vec<CircleName>,
    pub k: usize,
    pub json: bool,
    pub save: Option<PathBuf>,
}

/// One question's result in one circle (EVAL §3.3's per-question output).
#[derive(Clone, Debug)]
pub struct QuestionRow {
    pub id: String,
    pub set: QuestionSet,
    pub rev: String,
    pub circle: CircleName,
    /// The rank of each rel-2 item, `None` when not in the top k.
    pub ranks_of_rel2: Vec<Option<u32>>,
    pub ndcg: f32,
    /// A distractor ranked above the first rel-2 hit.
    pub distractor_above: bool,
    pub bytes: u32,
    pub ms: f32,
    pub absent_said: bool,
    pub forbidden_hits: u32,
    /// Items skipped for an unmet `requires`, or every item when the question was skipped.
    pub skipped_items: u32,
    pub solved: bool,
}

#[derive(Clone, Debug, Default)]
pub struct EvalReport {
    pub rows: Vec<QuestionRow>,
    /// Questions skipped (circle not configured, precondition), with why.
    pub skipped: Vec<(String, String)>,
    /// Hard gates that failed: containment, false absence, an unconfigured circle returning
    /// empty, a metric beyond its tolerance of the last saved run.
    pub failures: Vec<String>,
}

/// A gold item that no longer holds at its question's revision.
#[derive(Clone, Debug)]
pub struct GoldDrift {
    pub id: String,
    /// Index into the question's `gold`.
    pub gold: usize,
    pub drift: Drift,
}

#[derive(Clone, Debug)]
pub enum Drift {
    /// The path (or note file) is gone.
    Gone,
    /// The symbol moved within the file.
    Lines { by: i64, now: u32 },
    /// The symbol is no longer in the file.
    Lost,
    PackageGone,
    /// A commit or `alt` is not an ancestor of the rev.
    NotAncestor { commit: String },
    /// A `via_pointer` commit is not reachable from any ref.
    Unreachable { commit: String },
    /// A non-semantic absence now has hits in its scope.
    NoLongerAbsent { hits: u64 },
    /// A pointer count recomputed from `circles.conf` differs.
    PointerCount { gold: u64, now: u64 },
    /// The self-exclusion sentinel has no raw hit inside the eval dir, so the check is dead.
    SentinelInert,
    /// The sentinel has raw hits outside the eval dir, so the exclusion is not what hides it.
    SentinelOutside { hits: u64 },
    /// A commit mixes eval data with other paths, so its message reaches `makepad:history`.
    MixedEvalCommit { commit: String },
}

/// The harness. Questions are grouped by (revision, circles); each group's circles are opened
/// once through [`Circles::open`] at that revision.
pub trait Harness {
    fn load(&mut self, sets: &[QuestionSet]) -> Result<Vec<Question>, Error>;
    fn run(&mut self, questions: &[Question], opts: &EvalOpts) -> Result<EvalReport, Error>;
    /// Re-resolves every gold item at its question's revision; only changes are returned.
    fn verify_gold(&mut self, questions: &[Question]) -> Result<Vec<GoldDrift>, Error>;
}

// ---------------------------------------------------------------------------------------------
// Errors

#[derive(Debug)]
pub enum Error {
    Io { context: String, error: std::io::Error },
    Git(String),
    /// A circle that is not configured: always an error, never an empty result.
    UnknownCircle(String),
    Parse { file: String, line: u32, message: String },
    /// A segment or manifest that fails its checksum or format check.
    Corrupt { path: PathBuf, message: String },
    Rev(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { context, error } => write!(f, "{context}: {error}"),
            Error::Git(m) => write!(f, "git: {m}"),
            Error::UnknownCircle(c) => write!(f, "unknown circle `{c}` (see `makepad index circles`)"),
            Error::Parse { file, line, message } => write!(f, "{file}:{line}: {message}"),
            Error::Corrupt { path, message } => write!(f, "{}: {message}", path.display()),
            Error::Rev(m) => write!(f, "revision: {m}"),
        }
    }
}

impl std::error::Error for Error {}
