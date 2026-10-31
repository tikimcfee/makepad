//! Readable handles: what a hit prints is what `show` takes ([`crate::Handle`]).
//!
//! Text forms, longest rule first: an optional `<circle>/` prefix (a circle name holding `:`,
//! or one of the known bare names), an optional `@<rev>` suffix, then `crate:<package>`, a
//! commit's hex (7 to 40 digits), `path::Item::method`, `path:start-end`, `path:line`, or a bare
//! path (the whole file: its file chunk).

use crate::query::rank::{chunks_with, view};
use crate::{ChunkAt, CircleName, Error, Handle, Key, Kind, Lines, Located, OpenCircle, Opened, Target};

/// Parses a handle. `known` lists the bare circle names (no `:`) that may prefix a handle.
pub fn parse(text: &str, known: &[CircleName]) -> Result<Handle, Error> {
    let text = text.trim();
    let usage = || Error::Parse { file: "handle".into(), line: 0, message: format!("`{text}` is not a handle (path:line, path::Item, crate:<pkg> or a commit)") };
    if text.is_empty() {
        return Err(usage());
    }
    let (body, rev) = match text.rsplit_once('@') {
        Some((body, rev)) if !body.is_empty() && !rev.is_empty() => (body, Some(rev.to_string())),
        _ => (text, None),
    };
    let (circle, rest) = match body.split_once('/') {
        Some((first, rest)) if first.contains(':') && !first.starts_with("crate:") => (Some(CircleName(first.to_string())), rest),
        Some((first, rest)) if known.iter().any(|k| k.0 == first) => (Some(CircleName(first.to_string())), rest),
        _ => (None, body),
    };
    if rest.is_empty() {
        return Err(usage());
    }
    let target = if let Some(pkg) = rest.strip_prefix("crate:") {
        Target::Crate(pkg.to_string())
    } else if (7..=40).contains(&rest.len()) && rest.chars().all(|c| c.is_ascii_hexdigit()) && !rest.contains('/') {
        Target::Commit(rest.to_ascii_lowercase())
    } else if let Some((path, item)) = rest.split_once("::") {
        Target::Item { path: path.to_string(), item: item.to_string() }
    } else if let Some((path, range)) = rest.rsplit_once(':') {
        let lines = match range.split_once('-') {
            Some((a, b)) => Lines { start: a.parse().map_err(|_| usage())?, end: b.parse().map_err(|_| usage())? },
            None => {
                let l = range.parse().map_err(|_| usage())?;
                Lines { start: l, end: l }
            }
        };
        if lines.start == 0 || lines.end < lines.start {
            return Err(usage());
        }
        Target::Lines { path: path.to_string(), lines }
    } else {
        Target::Lines { path: rest.to_string(), lines: Lines::default() }
    };
    Ok(Handle { circle, target, rev })
}

/// The text form of a handle; [`parse`] reads it back.
pub fn format(h: &Handle) -> String {
    let mut out = String::new();
    if let Some(c) = &h.circle {
        out.push_str(&c.0);
        out.push('/');
    }
    match &h.target {
        Target::Lines { path, lines } => {
            out.push_str(path);
            if lines.start > 0 {
                out.push_str(&format!(":{}", lines.start));
                if lines.end > lines.start {
                    out.push_str(&format!("-{}", lines.end));
                }
            }
        }
        Target::Item { path, item } => out.push_str(&format!("{path}::{item}")),
        Target::Crate(p) => out.push_str(&format!("crate:{p}")),
        Target::Commit(hex) => out.push_str(hex),
    }
    if let Some(rev) = &h.rev {
        out.push('@');
        out.push_str(rev);
    }
    out
}

/// Every chunk the handle names in the opened circles (only its own circle when it has a
/// prefix; that circle not being open is an error).
pub fn resolve(opened: &Opened, h: &Handle) -> Result<Vec<Located>, Error> {
    let circles: Vec<usize> = match &h.circle {
        Some(name) => match opened.open.iter().position(|c| &c.name == name) {
            Some(i) => vec![i],
            None => return Err(Error::UnknownCircle(name.0.clone())),
        },
        None => (0..opened.open.len()).collect(),
    };
    let mut out = Vec::new();
    for ci in circles {
        let c = &opened.open[ci];
        out.extend(in_circle(c, &h.target).into_iter().map(|at| Located { circle: ci, at }));
    }
    Ok(out)
}

fn in_circle(c: &OpenCircle, target: &Target) -> Vec<ChunkAt> {
    match target {
        Target::Lines { path, lines } => {
            let all = chunks_with(c, &Key::Path { path: path.clone(), line: None });
            let all: Vec<ChunkAt> = all.into_iter().filter(|at| view(c, *at).path == path).collect();
            if lines.start == 0 {
                let files: Vec<ChunkAt> = all.iter().copied().filter(|at| view(c, *at).kind == Kind::File).collect();
                return if files.is_empty() { all } else { files };
            }
            let exact: Vec<ChunkAt> = all.iter().copied().filter(|at| view(c, *at).lines == *lines).collect();
            if !exact.is_empty() {
                return exact;
            }
            let containing: Vec<(u32, ChunkAt)> = all
                .iter()
                .filter_map(|at| {
                    let l = view(c, *at).lines;
                    (l.start <= lines.start && l.end >= lines.end).then_some((l.end - l.start, *at))
                })
                .collect();
            let Some(inner) = containing.iter().map(|(w, _)| *w).min() else { return Vec::new() };
            containing.into_iter().filter(|(w, _)| *w == inner).map(|(_, at)| at).collect()
        }
        Target::Item { path, item } => {
            let all = chunks_with(c, &Key::Path { path: path.clone(), line: None });
            let exact: Vec<ChunkAt> = all.iter().copied().filter(|at| view(c, *at).name == item).collect();
            if !exact.is_empty() {
                return exact;
            }
            let suffix = format!("::{item}");
            all.into_iter().filter(|at| view(c, *at).name.ends_with(&suffix)).collect()
        }
        Target::Crate(pkg) => chunks_with(c, &Key::Package(pkg.clone()))
            .into_iter()
            .filter(|at| view(c, *at).kind == Kind::Crate)
            .collect(),
        Target::Commit(hex) => {
            let mut out = Vec::new();
            for (si, seg) in c.segments.iter().enumerate() {
                for id in 0..seg.chunk_count() {
                    let at = ChunkAt::Segment { segment: si, chunk: crate::ChunkId(id) };
                    let v = view(c, at);
                    if v.kind == Kind::Commit && v.commit.is_some_and(|o| crate::query::hex(&o).starts_with(hex.as_str())) {
                        out.push(at);
                    }
                }
            }
            out
        }
    }
}
