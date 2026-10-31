//! Note chunks from every text file of a notes circle's sources, read from disk: one
//! [`crate::Kind::Note`] per Markdown section, one [`crate::Kind::NoteFile`] per other text file
//! (code, diffs, logs), and a path-only `NoteFile` per binary file, so pointer counts see every
//! file (EVAL §1.3). Paths are `<label>/<path in dir>`.
//! Each chunk carries its blob key (copies collapse) and outward edges: tracked repo paths,
//! `path:line` and commit hexes that resolve.
