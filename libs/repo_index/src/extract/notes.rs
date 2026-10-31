//! Note chunks from every text file of a notes circle's sources, read from disk: one
//! [`crate::Kind::Note`] per Markdown section, one [`crate::Kind::NoteFile`] per other text file
//! (code, diffs, logs); binary files are skipped and reported. Paths are `<label>/<path in dir>`.
//! Each chunk carries its blob key (copies collapse) and outward edges: tracked repo paths,
//! `path:line` and commit hexes that resolve.
