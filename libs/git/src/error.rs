use std::fmt;
use std::io;

#[derive(Debug)]
pub enum GitError {
    Cancelled,
    /// Internal admission refusal, converted to coverage by bounded diffs.
    TreeDiffLimit(crate::diff::TreeDiffExhaustion),
    Io(io::Error),
    InvalidObjectId(String),
    InvalidObject(String),
    ObjectNotFound(String),
    InvalidRef(String),
    RefNotFound(String),
    InvalidIndex(String),
    CorruptPack(String),
    /// A revision that names no object, with what failed to resolve.
    BadRevision { rev: String, reason: String },
    /// An abbreviated id that names more than one object: every candidate,
    /// with its kind.
    AmbiguousObjectId {
        prefix: String,
        candidates: Vec<(crate::oid::ObjectId, crate::object::ObjectKind)>,
    },
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::Cancelled => write!(f, "cancelled"),
            GitError::TreeDiffLimit(reason) => write!(f, "tree diff limit: {:?}", reason),
            GitError::Io(e) => write!(f, "IO error: {}", e),
            GitError::InvalidObjectId(s) => write!(f, "invalid object id: {}", s),
            GitError::InvalidObject(s) => write!(f, "invalid object: {}", s),
            GitError::ObjectNotFound(s) => write!(f, "object not found: {}", s),
            GitError::InvalidRef(s) => write!(f, "invalid ref: {}", s),
            GitError::RefNotFound(s) => write!(f, "ref not found: {}", s),
            GitError::InvalidIndex(s) => write!(f, "invalid index: {}", s),
            GitError::CorruptPack(s) => write!(f, "corrupt pack: {}", s),
            GitError::BadRevision { rev, reason } => write!(f, "bad revision '{}': {}", rev, reason),
            GitError::AmbiguousObjectId { prefix, candidates } => {
                write!(f, "short object id {} is ambiguous; candidates:", prefix)?;
                for (i, (oid, kind)) in candidates.iter().enumerate() {
                    let sep = if i == 0 { " " } else { ", " };
                    write!(f, "{}{} {}", sep, oid, kind.as_str())?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for GitError {}

impl From<io::Error> for GitError {
    fn from(e: io::Error) -> Self {
        GitError::Io(e)
    }
}
