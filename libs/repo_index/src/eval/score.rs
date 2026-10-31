//! Scoring one `find` output against a question's gold, one row per circle
//! block. The rules, on ranked hits:
//! - a path or note item matches a hit in that file (or inside that directory, or the crate card
//!   at it) whose lines overlap `[line-15, end+15]` or an `alt_lines` range; no line: any hit in
//!   the file. Copies listed in `also_at` count;
//! - a package matches a hit of that package; a commit a commit or bullet hit whose hex shares
//!   the item's 7-digit prefix (or an `alt`'s), and with `via_pointer` a pointer naming it;
//! - an absence matches the block's absent flag, or for a `path:`, `package:` or `fn` scope, no
//!   hit of that path, package or function; a semantic absence is for agent mode only;
//! - a pointer matches a pointer line to its circle, with the same count when the item has one;
//! - `dependents` need the `crate` card, so retrieval mode skips them, reported as skipped.
//!
//! An item whose `requires` circle is not configured is skipped. A question is solved when all
//! its scorable rel-2 items match in some block.
//!
//! Absence is judged per term ([`CircleBlock::absent_terms`]): a block may rank real hits and
//! truthfully say a named term occurs nowhere in its circle. So the scorer only collects each
//! block's named absent terms ([`Scored::absent_claims`]); whether a claim is false (the term
//! does occur in that circle at the revision) is checked lexically by the harness
//! ([`crate::eval::verify::occurrences`]), whatever the gold says.

use crate::{AbsentScope, CircleBlock, CircleName, FindOutput, Gold, GoldTarget, Hit, Kind, Lines, Question, QuestionRow, Target};

/// Line tolerance around a gold range.
pub const SLACK: u32 = 15;

/// What one item asks of a block.
enum Need {
    /// A ranked hit.
    Rank,
    /// A fact about the whole output (absence, pointer); `true` when met.
    Met(bool),
    /// Not scorable in retrieval mode, or its circle is not configured.
    Skip,
}

fn hit_paths(hit: &Hit) -> impl Iterator<Item = &str> {
    std::iter::once(hit.path.as_str()).chain(hit.also_at.iter().map(String::as_str))
}

fn path_in(gold: &str, path: &str, kind: Kind) -> bool {
    let dir = gold.trim_end_matches('/');
    path == gold || path.starts_with(&format!("{dir}/")) || (kind == Kind::Crate && path == format!("{dir}/Cargo.toml"))
}

fn lines_meet(hit: Lines, line: Option<u32>, end: Option<u32>, alt: &[Lines]) -> bool {
    let Some(line) = line else { return true };
    let mut ranges = vec![(line, end.unwrap_or(line).max(line))];
    ranges.extend(alt.iter().map(|l| (l.start, l.end)));
    ranges.iter().any(|&(lo, hi)| hit.start <= hi + SLACK && hit.end.max(hit.start) + SLACK >= lo)
}

fn commit_hex(hit: &Hit) -> Option<&str> {
    match (&hit.handle.target, hit.kind) {
        (Target::Commit(h), Kind::Commit | Kind::Bullet) => Some(h),
        _ => None,
    }
}

fn same_commit(a: &str, b: &str) -> bool {
    let n = a.len().min(b.len());
    n >= 7 && a[..n].eq_ignore_ascii_case(&b[..n])
}

/// Whether `hit` satisfies a rankable item.
fn hit_matches(target: &GoldTarget, hit: &Hit) -> bool {
    match target {
        GoldTarget::Path { path, line, end, alt_lines } | GoldTarget::Note { path, line, end, alt_lines } => {
            hit_paths(hit).any(|p| path_in(path, p, hit.kind)) && lines_meet(hit.lines, *line, *end, alt_lines)
        }
        GoldTarget::Package(p) => hit.package == *p,
        GoldTarget::Commit { hex, alt, .. } => {
            commit_hex(hit).is_some_and(|h| std::iter::once(hex).chain(alt).any(|g| same_commit(g, h)))
        }
        _ => false,
    }
}

fn ranked(block: &CircleBlock, k: usize) -> Vec<&Hit> {
    block.hits.iter().chain(&block.nearest).take(k).collect()
}

fn need(g: &Gold, out: &FindOutput, blocks: &[&CircleBlock], configured: &[CircleName]) -> Need {
    if g.requires.as_ref().is_some_and(|c| !configured.contains(c)) {
        return Need::Skip;
    }
    match &g.target {
        GoldTarget::Path { .. } | GoldTarget::Note { .. } | GoldTarget::Package(_) => Need::Rank,
        GoldTarget::Commit { hex, alt, via_pointer } => {
            if *via_pointer
                && out.pointers.iter().any(|p| {
                    let first = p.sample.split_whitespace().next().unwrap_or("");
                    std::iter::once(hex).chain(alt).any(|g| same_commit(g, first))
                })
            {
                Need::Met(true)
            } else {
                Need::Rank
            }
        }
        GoldTarget::Dependents { .. } | GoldTarget::Error(_) => Need::Skip,
        GoldTarget::Absent { semantic: true, .. } => Need::Skip,
        GoldTarget::Absent { scope, .. } => {
            let hits = || blocks.iter().flat_map(|b| b.hits.iter());
            Need::Met(match scope {
                None | Some(AbsentScope::Text(_)) => blocks.iter().any(|b| b.absent),
                Some(AbsentScope::Circle(c)) => blocks.iter().any(|b| &b.circle == c && b.absent),
                Some(AbsentScope::Path(p)) => !hits().any(|h| hit_paths(h).any(|x| path_in(p, x, h.kind) || x.ends_with(&format!("/{p}")))),
                Some(AbsentScope::Package(n)) => !hits().any(|h| &h.package == n),
                Some(AbsentScope::Fn(f)) => !hits().any(|h| {
                    matches!(h.kind, Kind::Fn | Kind::Method) && (h.name == *f || h.name.ends_with(&format!("::{f}")))
                }),
            })
        }
        GoldTarget::Pointer { circle, count, unit } => Need::Met(out.pointers.iter().any(|p| {
            p.circle.as_ref() == Some(circle) && p.unit == *unit && count.is_none_or(|c| p.count == c)
        })),
    }
}

/// Whether a forbid pattern names `hit`'s circle or source, in a block of `block_circle`.
pub fn forbidden(pattern: &str, block_circle: &CircleName, hit: &Hit) -> bool {
    let glob = |name: &str| match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    };
    let is_circle = pattern.contains(':') || !pattern.contains('/');
    if is_circle {
        let hit_circle = hit.handle.circle.as_ref().unwrap_or(block_circle);
        !glob(&block_circle.0) && glob(&hit_circle.0)
    } else {
        let prefix = pattern.trim_end_matches('*');
        hit_paths(hit).any(|p| p.starts_with(prefix) || p.contains(&format!("/{prefix}")))
    }
}

/// Discounted gain of the ranked hits against the rankable gold, normalised; `NaN` when the
/// question has no rankable gold.
fn ndcg(hits: &[&Hit], gold: &[(u8, &GoldTarget)]) -> f32 {
    let mut credited = vec![false; gold.len()];
    let mut dcg = 0.0f32;
    for (i, h) in hits.iter().enumerate() {
        let best = gold.iter().enumerate().filter(|(j, (_, t))| !credited[*j] && hit_matches(t, h)).max_by_key(|(_, (r, _))| *r);
        if let Some((j, (rel, _))) = best {
            credited[j] = true;
            dcg += f32::from(*rel) / (i as f32 + 2.0).log2();
        }
    }
    let mut ideal: Vec<u8> = gold.iter().map(|(r, _)| *r).collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let idcg: f32 = ideal.iter().take(hits.len().max(1)).enumerate().map(|(i, r)| f32::from(*r) / (i as f32 + 2.0).log2()).sum();
    if idcg == 0.0 {
        f32::NAN
    } else {
        dcg / idcg
    }
}

/// A question's rows, one per block, and the question-level verdicts.
pub struct Scored {
    pub rows: Vec<QuestionRow>,
    pub solved: bool,
    /// Each block's named absent terms, as (circle, term), for the false-absence check.
    pub absent_claims: Vec<(CircleName, String)>,
    pub forbidden: u32,
}

/// Scores `out` for `q`. `bytes` and `ms` are the rendered output's size and the query's time.
pub fn score(q: &Question, out: &FindOutput, k: usize, bytes: u32, ms: f32, configured: &[CircleName]) -> Scored {
    let blocks: Vec<&CircleBlock> = out.blocks.iter().filter(|b| q.circles.contains(&b.circle)).collect();
    let needs: Vec<(usize, Need)> = q.gold.iter().enumerate().map(|(i, g)| (i, need(g, out, &blocks, configured))).collect();
    let skipped = needs.iter().filter(|(_, n)| matches!(n, Need::Skip)).count() as u32;
    let rankable: Vec<(u8, &GoldTarget)> = needs
        .iter()
        .filter(|(_, n)| matches!(n, Need::Rank))
        .map(|(i, _)| (q.gold[*i].rel, &q.gold[*i].target))
        .collect();
    let mut met = vec![false; q.gold.len()];
    let mut rows = Vec::new();
    let mut forbidden_total = 0;
    for b in &blocks {
        let hits = ranked(b, k);
        let mut ranks = Vec::new();
        for (i, n) in &needs {
            match n {
                Need::Rank => {
                    let r = hits.iter().position(|h| hit_matches(&q.gold[*i].target, h)).map(|p| p as u32 + 1);
                    met[*i] |= r.is_some();
                    if q.gold[*i].rel == 2 {
                        ranks.push(r);
                    }
                }
                Need::Met(m) => met[*i] |= *m,
                Need::Skip => {}
            }
        }
        let first_gold = ranks.iter().flatten().min().copied();
        let first_distractor = hits
            .iter()
            .position(|h| q.distractors.iter().any(|d| hit_matches(&d.target, h)))
            .map(|p| p as u32 + 1);
        let forbidden_hits =
            b.hits.iter().chain(&b.nearest).filter(|h| q.forbid.iter().any(|f| forbidden(f, &b.circle, h))).count() as u32;
        forbidden_total += forbidden_hits;
        rows.push(QuestionRow {
            id: q.id.clone(),
            set: q.set.clone(),
            rev: b.rev.clone(),
            circle: b.circle.clone(),
            ranks_of_rel2: ranks,
            ndcg: ndcg(&hits, &rankable),
            distractor_above: match (first_distractor, first_gold) {
                (Some(d), Some(g)) => d < g,
                (Some(_), None) => true,
                _ => false,
            },
            bytes,
            ms,
            absent_said: b.absent,
            forbidden_hits,
            skipped_items: skipped,
            solved: false,
        });
    }
    let scorable_rel2: Vec<usize> =
        needs.iter().filter(|(i, n)| q.gold[*i].rel == 2 && !matches!(n, Need::Skip)).map(|(i, _)| *i).collect();
    let solved = !blocks.is_empty() && scorable_rel2.iter().all(|i| met[*i]);
    for r in &mut rows {
        r.solved = solved;
    }
    let mut absent_claims: Vec<(CircleName, String)> = Vec::new();
    for b in &blocks {
        for t in &b.absent_terms {
            if !absent_claims.iter().any(|(c, x)| c == &b.circle && x == t) {
                absent_claims.push((b.circle.clone(), t.clone()));
            }
        }
    }
    Scored { rows, solved, absent_claims, forbidden: forbidden_total }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{Handle, Pointer, PointerUnit, QuestionSet, RevSpec};

    pub fn hit(path: &str, start: u32, kind: Kind, name: &str) -> Hit {
        Hit {
            handle: Handle { circle: None, target: Target::Lines { path: path.into(), lines: Lines { start, end: start } }, rev: None },
            kind,
            package: String::new(),
            path: path.into(),
            lines: Lines { start, end: start + 2 },
            name: name.into(),
            sig: String::new(),
            gist: String::new(),
            score: 1.0,
            why: Vec::new(),
            also_at: Vec::new(),
            overlay: false,
            local: false,
        }
    }

    pub fn block(circle: &str, hits: Vec<Hit>, absent: bool) -> CircleBlock {
        CircleBlock {
            circle: CircleName(circle.into()),
            rev: "c0ffee123".into(),
            total: hits.len() as u64,
            definitions: 0,
            uses: 0,
            hits,
            absent,
            absent_terms: if absent { vec![circle.into()] } else { Vec::new() },
            nearest: Vec::new(),
        }
    }

    pub fn q(id: &str, circles: &[&str], gold: Vec<(u8, GoldTarget)>) -> Question {
        Question {
            id: id.into(),
            set: QuestionSet::Shipped,
            query: "x".into(),
            agent_prompt: None,
            precondition: None,
            rev: RevSpec::Pinned("c0ffee123".into()),
            circles: circles.iter().map(|c| CircleName(c.to_string())).collect(),
            qtype: "where".into(),
            subtype: "ident".into(),
            difficulty: "easy".into(),
            gold: gold.into_iter().map(|(rel, target)| Gold { rel, symbol: None, requires: None, target }).collect(),
            distractors: Vec::new(),
            forbid: Vec::new(),
            pins: Vec::new(),
            notes: String::new(),
            source: String::new(),
        }
    }

    fn path(p: &str, line: Option<u32>) -> GoldTarget {
        GoldTarget::Path { path: p.into(), line, end: None, alt_lines: Vec::new() }
    }

    fn mk() -> Vec<CircleName> {
        vec![CircleName("makepad".into())]
    }

    #[test]
    fn ranks_lines_tolerance_and_ndcg() {
        let question = q("w", &["makepad"], vec![(2, path("a/b.rs", Some(100))), (1, path("a/c.rs", None))]);
        let out = FindOutput {
            blocks: vec![block("makepad", vec![hit("a/x.rs", 1, Kind::Fn, "x"), hit("a/b.rs", 86, Kind::Fn, "y"), hit("a/c.rs", 3, Kind::Fn, "z")], false)],
            pointers: Vec::new(),
        };
        let s = score(&question, &out, 10, 900, 3.0, &mk());
        assert!(s.solved);
        assert_eq!(s.rows[0].ranks_of_rel2, [Some(2)]);
        assert!(s.rows[0].ndcg > 0.5 && s.rows[0].ndcg < 1.0);
        // 84 + 2 < 100 - 15: out of tolerance
        let far = FindOutput { blocks: vec![block("makepad", vec![hit("a/b.rs", 70, Kind::Fn, "y")], false)], pointers: Vec::new() };
        assert!(!score(&question, &far, 10, 0, 0.0, &mk()).solved);
        // k cuts the list
        assert!(!score(&question, &out, 1, 0, 0.0, &mk()).solved);
    }

    #[test]
    fn directories_crates_copies_and_commits() {
        let question = q(
            "a",
            &["makepad", "makepad:history"],
            vec![
                (2, path("libs/remote", None)),
                (2, path("dup/two.md", Some(1))),
                (2, GoldTarget::Commit { hex: "223ce04e3".into(), alt: vec![], via_pointer: false }),
            ],
        );
        let mut card = hit("libs/remote/Cargo.toml", 1, Kind::Crate, "makepad-remote");
        card.package = "makepad-remote".into();
        let mut copy = hit("dup/one.md", 1, Kind::Doc, "d");
        copy.also_at = vec!["dup/two.md".into()];
        let mut commit = hit("", 0, Kind::Commit, "director: moved");
        commit.handle.target = Target::Commit("223ce04e3aa".into());
        let out = FindOutput {
            blocks: vec![block("makepad", vec![card, copy], false), block("makepad:history", vec![commit], false)],
            pointers: Vec::new(),
        };
        assert!(score(&question, &out, 10, 0, 0.0, &[CircleName("makepad".into()), CircleName("makepad:history".into())]).solved);
    }

    #[test]
    fn absence_pointers_requires_and_absent_claims() {
        let n = |s: &str| CircleName(s.into());
        let pointer = Pointer { circle: Some(n("notes:t")), branch: None, count: 9, unit: PointerUnit::Blobs, commits: 0, sample: String::new() };
        let mut question = q(
            "x01",
            &["makepad"],
            vec![
                (2, GoldTarget::Absent { scope: None, semantic: false, term: None }),
                (2, GoldTarget::Pointer { circle: n("notes:t"), count: Some(9), unit: PointerUnit::Blobs }),
            ],
        );
        let absent = FindOutput { blocks: vec![block("makepad", vec![], true)], pointers: vec![pointer.clone()] };
        let s = score(&question, &absent, 10, 0, 0.0, &mk());
        assert!(s.solved);
        assert_eq!(s.absent_claims, [(n("makepad"), "makepad".to_string())], "the block's named absent terms, once");
        let wrong_count = FindOutput { pointers: vec![Pointer { count: 8, ..pointer.clone() }], ..absent.clone() };
        assert!(!score(&question, &wrong_count, 10, 0, 0.0, &mk()).solved);
        // an item that needs an unconfigured circle is skipped, not failed
        question.gold[1].requires = Some(n("notes:t"));
        let s = score(&question, &FindOutput { pointers: vec![], ..absent.clone() }, 10, 0, 0.0, &mk());
        assert!(s.solved);
        assert_eq!(s.rows[0].skipped_items, 1);
        // a block can rank real hits and still name an absent term: both are reported
        let mut both = block("makepad", vec![hit("a.rs", 1, Kind::Fn, "a")], false);
        both.absent = true;
        both.absent_terms = vec!["FOO_".into()];
        let plain = q("w", &["makepad"], vec![(2, path("a.rs", None))]);
        let s = score(&plain, &FindOutput { blocks: vec![both], pointers: vec![] }, 10, 0, 0.0, &mk());
        assert!(s.solved);
        assert_eq!(s.absent_claims, [(n("makepad"), "FOO_".to_string())]);
        // a fn scope looks at definitions
        let premise = q("w22", &["makepad"], vec![(2, path("jit.rs", Some(1243))), (1, GoldTarget::Absent { scope: Some(AbsentScope::Fn("unit_here".into())), semantic: false, term: None })]);
        let out = FindOutput { blocks: vec![block("makepad", vec![hit("jit.rs", 1243, Kind::Fn, "cur_unit")], true)], pointers: vec![] };
        assert!(score(&premise, &out, 10, 0, 0.0, &mk()).solved);
    }

    #[test]
    fn forbid_is_cross_circle_and_distractors_rank_only() {
        let mut question = q("x03", &["makepad"], vec![(2, GoldTarget::Absent { scope: None, semantic: false, term: None })]);
        question.forbid = vec!["notes:*".into(), "local/agent_state/*".into()];
        let leak = hit("local/agent_state/port/g.rs", 1, Kind::Fn, "R");
        let mut tagged = hit("x.md", 1, Kind::Note, "n");
        tagged.handle.circle = Some(CircleName("notes:t".into()));
        let out = FindOutput { blocks: vec![block("makepad", vec![leak, tagged, hit("ok.rs", 1, Kind::Fn, "f")], false)], pointers: vec![] };
        let s = score(&question, &out, 10, 0, 0.0, &mk());
        assert_eq!(s.forbidden, 2);
        // a notes path under its label is not `local/agent_state/` at the root of makepad
        assert!(forbidden("local/plans/*", &CircleName("notes:t".into()), &hit("makepad/local/plans/p.md", 1, Kind::Note, "p")));
        assert!(!forbidden("notes:*", &CircleName("notes:t".into()), &hit("a.md", 1, Kind::Note, "a")));
        let mut d = q("w03", &["makepad"], vec![(2, path("key_code/src/lib.rs", Some(36)))]);
        d.distractors = vec![Gold { rel: 0, symbol: None, requires: None, target: path("terminal.rs", Some(1027)) }];
        let out = FindOutput {
            blocks: vec![block("makepad", vec![hit("terminal.rs", 1027, Kind::Enum, "KeyCode"), hit("key_code/src/lib.rs", 36, Kind::Enum, "KeyCode")], false)],
            pointers: vec![],
        };
        let s = score(&d, &out, 10, 0, 0.0, &mk());
        assert!(s.solved && s.rows[0].distractor_above);
    }
}
