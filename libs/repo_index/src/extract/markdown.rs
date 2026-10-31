//! Markdown sections: one chunk per ATX heading. Tree-circle docs respect fences; notes split
//! exactly as `crate::PointerUnit::Blobs` says, so notes chunks are the pointer-count records.

use crate::{Lines, Span};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// The heading's text without its `#`s; empty for text before the first heading.
    pub heading: String,
    /// The whole piece, heading line included.
    pub text: String,
    pub span: Span,
    pub lines: Lines,
}

/// The ATX heading text of a line, if it is one: up to three leading spaces, 1–6 `#`, then a
/// space or the end of the line.
fn atx(line: &str) -> Option<String> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let hashes = rest.len() - rest.trim_start_matches('#').len();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after = &rest[hashes..];
    if !after.is_empty() && !after.starts_with([' ', '\t']) {
        return None;
    }
    Some(after.trim().trim_end_matches('#').trim_end().to_string())
}

/// Byte offsets where each line starts, plus the text length.
fn line_starts(src: &str) -> Vec<usize> {
    let mut v = vec![0];
    v.extend(src.match_indices('\n').map(|(i, _)| i + 1));
    v
}

fn line_of(starts: &[usize], pos: usize) -> u32 {
    starts.partition_point(|&s| s <= pos) as u32
}

fn piece(src: &str, starts: &[usize], lo: usize, hi: usize, heading: String) -> Section {
    let end_line = line_of(starts, hi.saturating_sub(1).max(lo));
    Section {
        heading,
        text: src[lo..hi].to_string(),
        span: Span { lo: lo as u32, hi: hi as u32 },
        lines: Lines { start: line_of(starts, lo), end: end_line },
    }
}

/// Tree-circle doc sections: split at ATX headings outside ``` and ~~~ fences. Text before the
/// first heading is a section when it is not blank.
pub fn doc_sections(src: &str) -> Vec<Section> {
    let starts = line_starts(src);
    let mut cuts: Vec<(usize, String)> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for &s in &starts {
        if s >= src.len() {
            break;
        }
        let line = src[s..].split('\n').next().unwrap_or("").trim_end_matches('\r');
        let t = line.trim_start();
        let marker = ['`', '~'].into_iter().find(|&c| t.starts_with(&String::from(c).repeat(3)));
        if let Some(c) = marker {
            let run = t.len() - t.trim_start_matches(c).len();
            match fence {
                None => fence = Some((c, run)),
                Some((fc, frun)) if fc == c && run >= frun && t.trim_start_matches(c).trim().is_empty() => fence = None,
                Some(_) => {}
            }
            continue;
        }
        if fence.is_none() {
            if let Some(h) = atx(line) {
                cuts.push((s, h));
            }
        }
    }
    let mut out = Vec::new();
    let first = cuts.first().map_or(src.len(), |c| c.0);
    if !src[..first].trim().is_empty() {
        out.push(piece(src, &starts, 0, first, String::new()));
    }
    for (i, (lo, h)) in cuts.iter().enumerate() {
        let hi = cuts.get(i + 1).map_or(src.len(), |c| c.0);
        out.push(piece(src, &starts, *lo, hi, h.clone()));
    }
    out
}

/// Notes records, exactly `eval/baseline/pointer_count.py`'s: split before every line that
/// starts with `#{1,6} ` (fences are not special), blank pieces dropped, the whole text as one
/// record when none is left.
pub fn note_records(src: &str) -> Vec<Section> {
    let starts = line_starts(src);
    let mut cuts = vec![0usize];
    for &s in &starts[1..] {
        if s < src.len() && starts_heading(&src[s..]) {
            cuts.push(s);
        }
    }
    let mut out = Vec::new();
    for (i, &lo) in cuts.iter().enumerate() {
        let hi = cuts.get(i + 1).copied().unwrap_or(src.len());
        let text = &src[lo..hi];
        if text.trim().is_empty() {
            continue;
        }
        let heading = if starts_heading(text) { text.lines().next().map(|l| l.trim_start_matches('#').trim().to_string()).unwrap_or_default() } else { String::new() };
        out.push(piece(src, &starts, lo, hi, heading));
    }
    if out.is_empty() {
        out.push(piece(src, &starts, 0, src.len(), String::new()));
    }
    out
}

/// `#{1,6} ` at the start of `s`.
fn starts_heading(s: &str) -> bool {
    let hashes = s.len() - s.trim_start_matches('#').len();
    (1..=6).contains(&hashes) && s[hashes..].starts_with(' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_guide_respects_fences() {
        let s = doc_sections(include_str!("../../fixture/docs/guide.md"));
        let heads: Vec<&str> = s.iter().map(|s| s.heading.as_str()).collect();
        assert_eq!(heads, vec!["Gizmo guide", "Spinning", "Resetting"]);
        assert_eq!(s[1].lines.start, 5);
        assert!(s[1].text.contains("# not a heading"));
        assert_eq!(s[2].lines, Lines { start: 13, end: 15 });
    }

    #[test]
    fn note_records_match_the_pointer_rule() {
        // the fake heading inside a fence splits a note, exactly as pointer_count.py does
        let s = note_records(include_str!("../../fixture/docs/guide.md"));
        assert_eq!(s.len(), 4);
        assert_eq!(s[2].heading, "not a heading: inside a fence");
        // text before the first heading is its own record; `####### x` and `#x` are not headings
        let s = note_records("intro\n####### seven\n#tag\n# One\nbody\n\n## Two\n");
        assert_eq!(s.iter().map(|s| s.heading.as_str()).collect::<Vec<_>>(), vec!["", "One", "Two"]);
        assert_eq!(s[0].text, "intro\n####### seven\n#tag\n");
        // a blank file is one record
        assert_eq!(note_records("\n\n").len(), 1);
        // the pieces cover the text
        let src = include_str!("../../fixture/notes/topic-a/PLAN.md");
        assert_eq!(note_records(src).iter().map(|s| s.text.as_str()).collect::<String>(), src);
    }

    #[test]
    fn doc_preamble_and_closing_hashes() {
        let s = doc_sections("lead text\n\n## Title ##\nx\n   ### Indented\n    #### code, not a heading\n");
        assert_eq!(s.iter().map(|s| s.heading.as_str()).collect::<Vec<_>>(), vec!["", "Title", "Indented"]);
    }
}
