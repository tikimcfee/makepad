//! Name-based refs ([`crate::Search::refs`]): exact definitions, then uses counted by name per
//! directory, then same-named definitions of other spellings. Not semantic resolution: a use is
//! any chunk whose signature or body holds the identifier as written, except a file chunk
//! (its signature is the skeleton of items counted on their own). Re-exports need `use`
//! items, which extraction does not emit yet, so they are always empty.

use std::collections::{HashMap, HashSet};

use crate::query::rank::{at_key, chunks_with, view, AtKey};
use crate::query::{build_hit, short, HitCtx};
use crate::terms::{field_index, word_terms};
use crate::{ChunkAt, Error, Field, Key, Kind, Opened, RefsOutput, TermForm, UseSummary};

/// Use hits listed with `--uses`, at most.
pub const MAX_USE_HITS: usize = 50;

pub fn refs(opened: &Opened, ident: &str, uses: bool) -> Result<RefsOutput, Error> {
    let Some(c) = opened.open.first() else {
        return Err(Error::UnknownCircle(String::from("(no circle opened)")));
    };
    let ctx = HitCtx { first: Some(c.name.clone()), pinned: false };
    let scored = |at: ChunkAt| crate::query::rank::Scored {
        circle: 0,
        at,
        score: 0.0,
        covered: 1,
        primary: true,
        definition: true,
        pin: 2,
        why: Vec::new(),
        also_at: Vec::new(),
    };
    let mut def_at = chunks_with(c, &Key::Ident(ident.to_string()));
    def_at.sort_by(|a, b| {
        let (va, vb) = (view(c, *a), view(c, *b));
        vb.flags.public.cmp(&va.flags.public).then(vb.flags.documented.cmp(&va.flags.documented)).then(va.path.cmp(vb.path))
    });
    let defs: HashSet<AtKey> = def_at.iter().map(|a| at_key(*a)).collect();

    // The identifier as written: its exact form when it has upper case, else its fold.
    let (form, text) = word_terms(ident)
        .into_iter()
        .find(|(f, _)| *f == TermForm::Exact)
        .unwrap_or((TermForm::Folded, makepad_search::fold(ident)));
    let shadowed: HashSet<&str> = c.shadowed.iter().map(String::as_str).collect();
    let mut use_tf: Vec<(ChunkAt, u32)> = Vec::new();
    for (si, seg) in c.segments.iter().enumerate() {
        for p in seg.postings(form, &text) {
            let at = ChunkAt::Segment { segment: si, chunk: p.chunk };
            let n = p.tf[field_index(Field::Sig)] + p.tf[field_index(Field::Body)];
            let v = view(c, at);
            if n > 0 && v.kind != Kind::File && !defs.contains(&at_key(at)) && !shadowed.contains(v.path) {
                use_tf.push((at, n));
            }
        }
    }
    for (fi, file) in c.overlay.iter().enumerate() {
        for (ci, (_, ft)) in file.chunks.iter().enumerate() {
            let at = ChunkAt::Overlay { file: fi, chunk: ci };
            let n: u32 = ft
                .terms
                .iter()
                .filter(|t| t.form == form && t.text == text && matches!(t.field, Field::Sig | Field::Body))
                .map(|t| t.tf)
                .sum();
            if n > 0 && view(c, at).kind != Kind::File && !defs.contains(&at_key(at)) {
                use_tf.push((at, n));
            }
        }
    }
    use_tf.sort_by(|a, b| b.1.cmp(&a.1).then(view(c, a.0).path.cmp(view(c, b.0).path)));

    let mut by_dir: HashMap<String, u64> = HashMap::new();
    let mut files: HashSet<&str> = HashSet::new();
    let mut total = 0u64;
    for (at, n) in &use_tf {
        let path = view(c, *at).path;
        files.insert(path);
        total += *n as u64;
        let dir: Vec<&str> = path.split('/').collect();
        let key = dir[..dir.len().saturating_sub(1).min(2)].join("/");
        *by_dir.entry(if key.is_empty() { String::from(".") } else { key }).or_insert(0) += *n as u64;
    }
    let mut by_dir: Vec<(String, u64)> = by_dir.into_iter().collect();
    by_dir.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    // Definitions of the same name in another spelling (`keyevent` for `KeyEvent`).
    let folded = makepad_search::fold(ident);
    let mut other_defs = Vec::new();
    for (si, seg) in c.segments.iter().enumerate() {
        for p in seg.postings(TermForm::Folded, &folded) {
            let at = ChunkAt::Segment { segment: si, chunk: p.chunk };
            let v = view(c, at);
            let last = v.name.rsplit("::").next().unwrap_or(v.name);
            if p.tf[field_index(Field::Name)] > 0 && v.flags.definition && last != ident && !shadowed.contains(v.path) {
                other_defs.push(build_hit(opened, &scored(at), &ctx));
            }
        }
    }
    other_defs.sort_by(|a, b| a.path.cmp(&b.path).then(a.lines.start.cmp(&b.lines.start)));

    Ok(RefsOutput {
        circle: c.name.clone(),
        rev: c.rev.as_ref().map(|r| short(&r.commit)).unwrap_or_default(),
        ident: ident.to_string(),
        defs: def_at.iter().map(|at| build_hit(opened, &scored(*at), &ctx)).collect(),
        reexports: Vec::new(),
        uses: UseSummary { total, files: files.len() as u64, by_dir },
        use_hits: if uses { use_tf.iter().take(MAX_USE_HITS).map(|(at, _)| build_hit(opened, &scored(*at), &ctx)).collect() } else { Vec::new() },
        other_defs,
    })
}
