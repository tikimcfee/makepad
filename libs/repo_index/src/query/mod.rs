//! Ranked, per-circle results ([`crate::Search`]): BM25F over name, path, sig, doc and body;
//! definitions above uses; query-term coverage; the exact definition short-circuit; filters;
//! package grouping; absence answers with the nearest hits; pointer counts for circles not
//! opened (distinct blobs); handle resolution; name-based refs. Scores never cross circles.
//!
//! Each named circle is ranked alone, with its own statistics, into its own block. A union
//! either keeps the blocks or merges them by rank (RRF, k = 60), never by score; a selection
//! filters one circle's ranking; an overlap keeps the left side's records that share a key
//! with the right side's. `find` ranks a [`crate::Mode::History`] query like any other over the
//! circles it names; [`crate::Mode::Refs`] goes through [`crate::Search::refs`].

pub mod handle;
pub mod lines;
pub mod pointers;
pub mod rank;
pub mod refs;

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use crate::terms::Terms;
use crate::{
    CircleBlock, CircleExpr, CircleName, Error, FindOutput, Fuse, Handle, Hit, Kind, KindFilter, Located, Mode,
    OpenCircle, Opened, Oid, Query, RefsOutput, RevSpec, Search, Selector, Target, Tokenize,
};
use rank::{keys, keys_of_kind, rank, view, Filter, Plan, Ranked, Scored};

/// Reciprocal rank fusion's constant (Cormack et al.).
pub const RRF_K: f32 = 60.0;

/// The [`Search`] implementation.
#[derive(Clone, Copy, Debug, Default)]
pub struct Engine {
    pub terms: Terms,
}

/// The full hex of an object id.
pub fn hex(oid: &Oid) -> String {
    oid.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// The abbreviated hex printed in headers and handles (9 digits).
pub fn short(oid: &Oid) -> String {
    hex(oid)[..9].to_string()
}

/// How hits name their circle and revision.
pub(crate) struct HitCtx {
    /// The query's first circle: its hits print no `<circle>/` prefix.
    pub first: Option<CircleName>,
    /// The query asked for a pinned revision: handles carry `@<rev>`.
    pub pinned: bool,
}

/// A block before selection: one circle, or a fused or filtered composite.
struct BlockRank {
    name: CircleName,
    rev: String,
    entries: Vec<Scored>,
    total: u64,
    definitions: u64,
    near: Vec<Scored>,
    absent_terms: Vec<String>,
}

fn circle_rev(c: &OpenCircle) -> String {
    c.rev.as_ref().map(|r| short(&r.commit)).unwrap_or_default()
}

impl Search for Engine {
    fn find(&self, opened: &Opened, query: &Query) -> Result<FindOutput, Error> {
        let terms = self.terms.query_terms(&query.words);
        let plan = Plan::new(&query.words, &terms);
        let filter = Filter { kinds: query.kinds.clone(), packages: query.packages.clone(), paths: query.paths.clone(), any_rev: false };
        let ctx = HitCtx { first: query.circles.first().and_then(first_name), pinned: matches!(query.rev, RevSpec::Pinned(_)) };
        let mut ranked = Vec::new();
        for expr in &query.circles {
            ranked.extend(self.eval(opened, expr, &plan, &filter)?);
        }
        // The hit limit is the whole answer's: blocks share it, so a two-circle find costs about
        // what a one-circle find does (PLAN §1's byte budget).
        let limit = if query.limit == 0 { crate::DEFAULT_LIMIT } else { query.limit };
        // A block with nothing to rank (its words are absent there) takes no share.
        let sharing = ranked.iter().filter(|b| !b.entries.is_empty()).count().max(1);
        let share = limit.div_ceil(sharing).max(crate::HITS_PER_GROUP);
        let blocks: Vec<CircleBlock> = ranked.into_iter().map(|b| self.block(opened, b, query, &ctx, share)).collect();
        let absent: Vec<String> = {
            let mut a: Vec<String> = Vec::new();
            for b in &blocks {
                for t in &b.absent_terms {
                    if !a.contains(t) {
                        a.push(t.clone());
                    }
                }
            }
            a
        };
        let pointers = if query.pointers { pointers::count(opened, &query.words, &plan, &absent) } else { Vec::new() };
        Ok(FindOutput { blocks, pointers })
    }

    fn resolve(&self, opened: &Opened, handle: &Handle) -> Result<Vec<Located>, Error> {
        handle::resolve(opened, handle)
    }

    fn refs(&self, opened: &Opened, ident: &str, uses: bool) -> Result<RefsOutput, Error> {
        refs::refs(opened, ident, uses)
    }
}

/// The name of an expression's first base circle.
fn first_name(expr: &CircleExpr) -> Option<CircleName> {
    match expr {
        CircleExpr::Circle(n) => Some(n.clone()),
        CircleExpr::Union { of, .. } => of.first().and_then(first_name),
        CircleExpr::Select { of, .. } => first_name(of),
        CircleExpr::Overlap { left, .. } => first_name(left),
    }
}

fn circle_index(opened: &Opened, name: &CircleName) -> Result<usize, Error> {
    opened.open.iter().position(|c| &c.name == name).ok_or_else(|| Error::UnknownCircle(name.0.clone()))
}

impl Engine {
    fn eval(&self, opened: &Opened, expr: &CircleExpr, plan: &Plan, filter: &Filter) -> Result<Vec<BlockRank>, Error> {
        Ok(match expr {
            CircleExpr::Circle(name) => {
                let ci = circle_index(opened, name)?;
                let c = &opened.open[ci];
                let Ranked { entries, total, definitions, near, absent_terms, .. } = rank(c, ci, plan, filter, rank::KEEP, opened.blobs.as_ref());
                vec![BlockRank { name: name.clone(), rev: circle_rev(c), entries, total, definitions, near, absent_terms }]
            }
            CircleExpr::Select { of, by } => {
                let mut f = filter.clone();
                match by {
                    Selector::Kinds(k) => f.kinds.extend(k.iter().copied()),
                    Selector::PathPrefix(p) => f.paths.push(p.clone()),
                    Selector::Package(p) => f.packages.push(p.clone()),
                }
                self.eval(opened, of, plan, &f)?
            }
            CircleExpr::Union { of, fuse } => {
                let mut blocks = Vec::new();
                for e in of {
                    blocks.extend(self.eval(opened, e, plan, filter)?);
                }
                match fuse {
                    Fuse::Blocks => blocks,
                    Fuse::Rank => vec![fuse_by_rank(blocks)],
                }
            }
            CircleExpr::Overlap { left, right, on } => {
                let rights = base_names(right);
                let right_circles: Vec<&OpenCircle> =
                    rights.iter().map(|n| circle_index(opened, n).map(|i| &opened.open[i])).collect::<Result<_, _>>()?;
                let mut out = Vec::new();
                for mut b in self.eval(opened, left, plan, filter)? {
                    let shares = |e: &Scored| {
                        let k = keys(&opened.open[e.circle], e.at);
                        keys_of_kind(&k, *on).iter().any(|key| right_circles.iter().any(|rc| !rank::chunks_with(rc, key).is_empty()))
                    };
                    b.entries.retain(|e| shares(e));
                    b.near.retain(|e| shares(e));
                    // Counted over the ranked entries kept, not the whole circle.
                    b.total = b.entries.len() as u64;
                    b.definitions = b.entries.iter().filter(|e| e.definition).count() as u64;
                    b.name = CircleName(format!("{} & {}", b.name.0, rights.iter().map(|n| n.0.as_str()).collect::<Vec<_>>().join("+")));
                    out.push(b);
                }
                out
            }
        })
    }

    /// Selects and groups a block's hits: at most [`crate::HITS_PER_GROUP`] per package, up to
    /// the limit, groups ordered by their best hit.
    fn block(&self, opened: &Opened, b: BlockRank, query: &Query, ctx: &HitCtx, limit: usize) -> CircleBlock {
        let def_mode = query.mode == Mode::Def;
        let (pool, nearest_pool): (Vec<&Scored>, Vec<&Scored>) = if def_mode {
            let defs: Vec<&Scored> = b.entries.iter().filter(|e| matches!(e.pin, 2 | 3)).collect();
            let near: Vec<&Scored> = if defs.is_empty() { b.entries.iter().chain(b.near.iter()).take(rank::NEAREST).collect() } else { Vec::new() };
            (defs, near)
        } else {
            (b.entries.iter().collect(), if b.entries.is_empty() { b.near.iter().collect() } else { Vec::new() })
        };
        let selected = group(opened, &pool, limit);
        let hits = selected.into_iter().map(|e| build_hit(opened, e, ctx)).collect();
        let nearest = nearest_pool.into_iter().take(rank::NEAREST).map(|e| build_hit(opened, e, ctx)).collect();
        let total = b.total;
        // Nothing here matches at all: every shaped word is absent. Plain words never are, and
        // a block with no shaped word absent claims no absence.
        let absent_terms = if total == 0 { shaped_words(query) } else { b.absent_terms };
        CircleBlock {
            circle: b.name,
            rev: b.rev,
            total: if def_mode { pool.len() as u64 } else { total },
            definitions: b.definitions,
            uses: total - b.definitions,
            hits,
            absent: !absent_terms.is_empty(),
            absent_terms,
            nearest,
        }
    }
}

/// The query's identifier-, prefix- and path-shaped words as written (absent terms when a
/// circle matches nothing).
fn shaped_words(query: &Query) -> Vec<String> {
    query
        .words
        .iter()
        .map(|w| rank::classify(w))
        .filter_map(|(core, c)| match c {
            rank::Class::Subject { .. } | rank::Class::Path => Some(core),
            rank::Class::Prefix => Some(format!("{core}_")),
            _ => None,
        })
        .collect()
}

/// Picks hits in rank order, at most [`crate::HITS_PER_GROUP`] per package, up to `limit`; then
/// orders them by group (groups by their best hit), rank order within a group.
fn group<'a>(opened: &Opened, ranked: &[&'a Scored], limit: usize) -> Vec<&'a Scored> {
    let mut per: HashMap<(usize, String), usize> = HashMap::new();
    let mut order: Vec<(usize, String)> = Vec::new();
    let mut picked: Vec<((usize, String), &Scored)> = Vec::new();
    for e in ranked {
        if picked.len() >= limit {
            break;
        }
        // Chunks with no package (docs, notes, splash outside a crate) group by their file.
        let v = view(&opened.open[e.circle], e.at);
        // A chunk with neither (a commit) is a group of its own.
        let key = (
            e.circle,
            match (v.package.is_empty(), v.path.is_empty()) {
                (false, _) => v.package.to_string(),
                (true, false) => v.path.to_string(),
                (true, true) => format!("#{}", picked.len()),
            },
        );
        let n = per.entry(key.clone()).or_insert(0);
        if *n >= crate::HITS_PER_GROUP {
            continue;
        }
        *n += 1;
        if !order.contains(&key) {
            order.push(key.clone());
        }
        picked.push((key, e));
    }
    order.iter().flat_map(|k| picked.iter().filter(move |(pk, _)| pk == k).map(|(_, e)| *e)).collect()
}

/// Merges blocks by reciprocal rank, never by score.
fn fuse_by_rank(blocks: Vec<BlockRank>) -> BlockRank {
    let name = CircleName(blocks.iter().map(|b| b.name.0.as_str()).collect::<Vec<_>>().join("+"));
    let rev = blocks.iter().map(|b| b.rev.as_str()).find(|r| !r.is_empty()).unwrap_or("").to_string();
    let definitions = blocks.iter().map(|b| b.definitions).sum();
    let total = blocks.iter().map(|b| b.total).sum();
    let mut absent_terms: Vec<String> = Vec::new();
    for b in &blocks {
        absent_terms.extend(b.absent_terms.iter().filter(|t| blocks.iter().all(|o| o.absent_terms.contains(t))).cloned());
    }
    absent_terms.dedup();
    let mut entries = Vec::new();
    let mut near = Vec::new();
    for b in blocks {
        for (r, mut e) in b.entries.into_iter().enumerate() {
            e.score = 1.0 / (RRF_K + r as f32 + 1.0);
            entries.push(e);
        }
        for (r, mut e) in b.near.into_iter().enumerate() {
            e.score = 1.0 / (RRF_K + r as f32 + 1.0);
            near.push(e);
        }
    }
    let by = |a: &Scored, b: &Scored| {
        b.pin.cmp(&a.pin).then(b.score.total_cmp(&a.score)).then(a.circle.cmp(&b.circle)).then(rank::at_key(a.at).cmp(&rank::at_key(b.at)))
    };
    entries.sort_by(by);
    near.sort_by(by);
    near.truncate(rank::NEAREST);
    BlockRank { name, rev, entries, total, definitions, near, absent_terms }
}

fn base_names(expr: &CircleExpr) -> Vec<CircleName> {
    match expr {
        CircleExpr::Circle(n) => vec![n.clone()],
        CircleExpr::Union { of, .. } => of.iter().flat_map(base_names).collect(),
        CircleExpr::Select { of, .. } => base_names(of),
        CircleExpr::Overlap { left, .. } => base_names(left),
    }
}

/// A hit's readable handle and fields.
pub(crate) fn build_hit(opened: &Opened, e: &Scored, ctx: &HitCtx) -> Hit {
    let c = &opened.open[e.circle];
    let v = view(c, e.at);
    let target = match v.kind {
        Kind::Commit | Kind::Bullet => Target::Commit(v.commit.map(|o| short(&o)).unwrap_or_default()),
        Kind::Crate => Target::Crate(if v.package.is_empty() { v.name.to_string() } else { v.package.to_string() }),
        _ => Target::Lines { path: v.path.to_string(), lines: e.line.map_or(v.lines, |l| crate::Lines { start: l, end: l }) },
    };
    let rev = circle_rev(c);
    let handle = Handle {
        circle: if ctx.first.as_ref() == Some(&c.name) { None } else { Some(c.name.clone()) },
        target,
        rev: (ctx.pinned && !rev.is_empty()).then_some(rev),
    };
    let local = match (&c.visible, v.commit) {
        (Some(vis), Some(commit)) => vis.local.contains(&commit),
        _ => false,
    };
    Hit {
        handle,
        kind: v.kind,
        package: v.package.to_string(),
        path: v.path.to_string(),
        // The item's own range: the handle points at its best line, the hit spans the item.
        lines: v.lines,
        name: v.name.to_string(),
        sig: v.sig.to_string(),
        gist: v.gist.to_string(),
        score: e.score,
        why: e.why.clone(),
        also_at: e.also_at.clone(),
        overlay: matches!(e.at, crate::ChunkAt::Overlay { .. }),
        local,
    }
}

/// `--kind` words: a kind name or a family name (`crate`, `file`, `doc`, `splash`, `commit`
/// name the family).
pub fn kind_filter(word: &str) -> Option<KindFilter> {
    use crate::Family;
    let family = match word {
        "crate" => Some(Family::Crate),
        "file" => Some(Family::File),
        "item" => Some(Family::Item),
        "doc" => Some(Family::Doc),
        "splash" => Some(Family::Splash),
        "commit" => Some(Family::Commit),
        "note" => Some(Family::Note),
        _ => None,
    };
    if let Some(f) = family {
        return Some(KindFilter::Family(f));
    }
    const KINDS: [Kind; 20] = [
        Kind::Crate, Kind::File, Kind::Fn, Kind::Method, Kind::Struct, Kind::Enum, Kind::Union, Kind::Trait, Kind::Impl,
        Kind::Mod, Kind::Type, Kind::Const, Kind::Static, Kind::Macro, Kind::Doc, Kind::Splash, Kind::Commit,
        Kind::Bullet, Kind::Note, Kind::NoteFile,
    ];
    KINDS.iter().find(|k| k.as_str() == word).map(|k| KindFilter::Kind(*k))
}
