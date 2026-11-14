//! What the runner leaves for others to read: state files, rewritten whole, and the record
//! of tries, one JSON object per line. Nothing reads the runner's log to learn what happened.

use std::io::{self, Write};
use std::path::Path;

/// A record file is rotated once to `.1` when it would grow past this.
const RECORD_BYTES: u64 = 1024 * 1024;

/// Now, in Unix seconds with milliseconds.
pub fn now() -> f64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (since.as_millis() as f64) / 1000.0
}

/// Replaces `path` with `text` at once: a reader sees the old file or the new one, never a
/// part of one.
pub fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Appends one line to a record file. A file that would pass the size limit becomes `.1`
/// first (the `.1` before it is dropped), so the newest megabyte or two are always there.
pub fn append(path: &Path, line: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > 0 && size + line.len() as u64 + 1 > RECORD_BYTES {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        std::fs::rename(path, old)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(format!("{line}\n").as_bytes())
}
