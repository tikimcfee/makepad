//! Pointer counts recomputed from a notes circle's sources on disk, the
//! [`crate::PointerUnit::Blobs`] rule exactly: distinct blobs with one record that holds every
//! query term, path included. verify-gold compares these with the frozen gold counts.

use crate::{circles, open, BaseCircle, Error, NotesSource};
use std::collections::HashSet;

/// A file whose first 8000 bytes hold a NUL is binary: one record, its path only.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8000)].contains(&0)
}

/// A Markdown file's records: the pieces of a split before every line that starts with
/// `#{1,6} ` (fences are not special), blank pieces dropped, the whole text if none is left.
pub fn markdown_records(text: &str) -> Vec<&str> {
    let mut cuts = vec![0];
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let hashes = line.bytes().take_while(|b| *b == b'#').count();
        if at > 0 && (1..=6).contains(&hashes) && line.as_bytes().get(hashes) == Some(&b' ') {
            cuts.push(at);
        }
        at += line.len();
    }
    cuts.push(text.len());
    let pieces: Vec<&str> = cuts.windows(2).map(|w| &text[w[0]..w[1]]).filter(|p| !p.trim().is_empty()).collect();
    if pieces.is_empty() {
        vec![text]
    } else {
        pieces
    }
}

/// Whether one record (its text and its path within its source) holds every term.
pub fn record_matches(text: &str, path: &str, terms: &[String]) -> bool {
    let hay = format!("{}\n{}", text.to_lowercase(), path.to_lowercase());
    terms.iter().all(|t| hay.contains(t.as_str()))
}

/// Distinct blobs of the circle's sources with a record matching every whitespace-separated,
/// lowercased word of `query`. Files an exclusion of `def` covers are not the circle's.
pub fn count_blobs(sources: &[NotesSource], def: &BaseCircle, query: &str) -> Result<u64, Error> {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut blobs = HashSet::new();
    for s in sources {
        if !open::source_exists(s)? {
            continue; // nothing of it is in the circle (opening it says so)
        }
        for (rel, full) in open::walk_files(&s.dir)? {
            if circles::excluded(def, &rel).is_some() {
                continue;
            }
            let bytes = std::fs::read(&full).map_err(|error| Error::Io { context: format!("pointer count: {}", full.display()), error })?;
            let hit = if is_binary(&bytes) {
                record_matches("", &rel, &terms)
            } else {
                let text = String::from_utf8_lossy(&bytes);
                let records = if rel.ends_with(".md") { markdown_records(&text) } else { vec![&text[..]] };
                records.iter().any(|r| record_matches(r, &rel, &terms))
            };
            if hit {
                blobs.insert(*makepad_git::oid::hash_object("blob", &bytes).as_bytes());
            }
        }
    }
    Ok(blobs.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BaseSource, Exclusion, Provenance};
    use std::path::PathBuf;

    #[test]
    fn markdown_splits_like_the_reference_script() {
        assert_eq!(markdown_records("intro\n# a\nx\n## b\ny\n"), ["intro\n", "# a\nx\n", "## b\ny\n"]);
        assert_eq!(markdown_records("# a\n```\n# not special\n```\n"), ["# a\n```\n", "# not special\n```\n"]);
        assert_eq!(markdown_records("####### seven\n#nospace\n"), ["####### seven\n#nospace\n"]);
        assert_eq!(markdown_records("\n\n# only\n"), ["# only\n"]);
        assert_eq!(markdown_records("  \n"), ["  \n"]);
    }

    #[test]
    fn counts_distinct_blobs_with_all_terms_in_one_record() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixture/notes");
        let def = |exclude| BaseCircle {
            source: BaseSource::Notes { sources: Vec::new() },
            families: crate::circles::NOTES_FAMILIES.to_vec(),
            exclude,
        };
        let sources = vec![
            NotesSource { dir: fixture.join("topic-a"), label: "topic-a".into() },
            NotesSource { dir: fixture.join("local-a"), label: "fixture/notes/local-a".into() },
        ];
        // PLAN.md and COPY.md are one blob
        let plan = std::fs::read(fixture.join("topic-a/PLAN.md")).unwrap();
        assert_eq!(plan, std::fs::read(fixture.join("topic-a/COPY.md")).unwrap());
        assert_eq!(count_blobs(&sources, &def(Vec::new()), "CLAMPS gizmo").unwrap(), 1, "byte-identical copies count once");
        assert_eq!(count_blobs(&sources, &def(Vec::new()), "clamps spin_note_draft").unwrap(), 0, "terms in one record");
        // a binary file matches by its path only
        assert_eq!(count_blobs(&sources, &def(Vec::new()), "pixel").unwrap(), 1);
        assert_eq!(count_blobs(&sources, &def(Vec::new()), "pixel.bin zzz-not-there").unwrap(), 0);
        // two spin.rs files with different content are two blobs; an exclusion removes one
        assert_eq!(count_blobs(&sources, &def(Vec::new()), "spin.rs").unwrap(), 2);
        let claimed = vec![Exclusion { prefix: "spin.rs/".into(), provenance: Provenance::IndexOwnData }];
        assert_eq!(count_blobs(&sources, &def(claimed), "spin.rs").unwrap(), 0);
    }
}
