//! The eval harness ([`crate::Harness`]): questions grouped by (revision, circles), each group
//! indexed at its pinned revision; retrieval metrics per circle; `verify-gold`; the leak check
//! (no chunk under an exclusion, in the base or the overlay).
//!
//! A run opens each group's circles once through [`crate::Circles::open`], asks every question
//! as a `find`, renders the output to measure its bytes, and scores it (`score.rs`). Hard gates
//! become [`crate::EvalReport::failures`]: containment, a false absence, an unconfigured circle
//! that answers instead of failing, and a metric worse than the last saved run by more than its
//! tolerance. Questions on circles not configured here, and those whose precondition does not
//! hold, are skipped and listed. Agent mode is the EVAL §2 protocol, run by an agent through the
//! logging wrapper, not by this harness.

pub mod leak;
pub mod load;
pub mod pointer;
pub mod score;
pub mod verify;

use crate::{
    circles, CircleExpr, CircleManifest, CircleName, Circles, Error, EvalMode, EvalOpts, EvalReport, GoldDrift, GoldTarget,
    Harness, Mode, Output, Precondition, Query, Question, QuestionRow, QuestionSet, Render, Repo, RevSpec, Search,
};
use makepad_strict_json::{obj, s, Value};
use std::path::{Path, PathBuf};

/// Tolerances against the last saved run.
pub const MAX_R10_DROP: f32 = 0.03;
pub const MAX_MRR_DROP: f32 = 0.03;
pub const MAX_P90_BYTES_GROWTH: f32 = 0.20;

/// The harness over the other parts. `root` is the repo whose shipped set is read.
pub struct Eval<'a> {
    pub circles: &'a mut dyn Circles,
    pub search: &'a dyn Search,
    pub render: &'a dyn Render,
    pub repo: &'a mut dyn Repo,
    pub root: PathBuf,
}

fn set_name(set: &QuestionSet) -> String {
    match set {
        QuestionSet::Shipped => "shipped".into(),
        QuestionSet::Local(p) => format!("local:{}", p.display()),
        QuestionSet::Heldout(_) => "heldout".into(),
        QuestionSet::File(p) => p.display().to_string(),
    }
}

fn rev_text(rev: &RevSpec) -> String {
    match rev {
        RevSpec::Worktree => "worktree".into(),
        RevSpec::Pinned(r) => r.clone(),
    }
}

/// A call and its wall time in milliseconds. The index runs natively; a wasm build has no std
/// clock and reports 0 ms.
fn timed<T>(f: impl FnOnce() -> T) -> (T, f32) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let t = std::time::Instant::now();
        let r = f();
        (r, t.elapsed().as_secs_f32() * 1000.0)
    }
    #[cfg(target_arch = "wasm32")]
    {
        (f(), 0.0)
    }
}

/// Questions sharing one (revision, circles) open.
type Groups<'q> = Vec<((RevSpec, Vec<CircleName>), Vec<&'q Question>)>;

/// What every group of one run shares.
struct RunCtx<'a> {
    manifest: &'a CircleManifest,
    configured: &'a [CircleName],
    opts: &'a EvalOpts,
}

impl Eval<'_> {
    fn run_group(&mut self, ctx: &RunCtx, rev: &RevSpec, circles_of: &[CircleName], group: &[&Question], report: &mut EvalReport) {
        let RunCtx { manifest, configured, opts } = *ctx;
        let opened = match self.circles.open(manifest, circles_of, rev) {
            Ok(o) => o,
            Err(e) => {
                for q in group {
                    report.failures.push(format!("{}: opening {:?} at {}: {e}", q.id, names(circles_of), rev_text(rev)));
                }
                return;
            }
        };
        for f in leak::containment(&opened, manifest) {
            report.failures.push(format!("containment: {f}"));
        }
        for q in group {
            let query = Query {
                mode: Mode::Find,
                words: q.query.split_whitespace().map(str::to_string).collect(),
                circles: q.circles.iter().map(|c| CircleExpr::Circle(c.clone())).collect(),
                rev: rev.clone(),
                kinds: Vec::new(),
                packages: Vec::new(),
                paths: Vec::new(),
                limit: opts.k,
                pointers: true,
            };
            let (found, ms) = timed(|| self.search.find(&opened, &query));
            let out = match found {
                Ok(o) => o,
                Err(e) => {
                    report.failures.push(format!("{}: find: {e}", q.id));
                    continue;
                }
            };
            let bytes = self.render.text(&Output::Find(out.clone())).len() as u32;
            let scored = score::score(q, &out, opts.k, bytes, ms, configured);
            if scored.false_absence {
                report.failures.push(format!("{}: false absence (absent claimed, gold exists)", q.id));
            }
            if scored.forbidden > 0 {
                report.failures.push(format!("{}: containment: {} forbidden records ranked", q.id, scored.forbidden));
            }
            if scored.rows.is_empty() {
                report.failures.push(format!("{}: no block for {:?}", q.id, names(&q.circles)));
            }
            report.rows.extend(scored.rows);
        }
    }
}

fn names(c: &[CircleName]) -> Vec<&str> {
    c.iter().map(|n| n.0.as_str()).collect()
}

impl Harness for Eval<'_> {
    fn load(&mut self, sets: &[QuestionSet]) -> Result<Vec<Question>, Error> {
        let manifest = self.circles.manifest()?;
        let mut out = Vec::new();
        for set in sets {
            let path = match set {
                QuestionSet::Shipped => self.root.join(load::SHIPPED),
                QuestionSet::Local(p) | QuestionSet::Heldout(p) | QuestionSet::File(p) => p.clone(),
            };
            out.extend(load::file(&path, set, &manifest)?);
        }
        Ok(out)
    }

    fn run(&mut self, questions: &[Question], opts: &EvalOpts) -> Result<EvalReport, Error> {
        let mut report = EvalReport::default();
        if opts.mode == EvalMode::Agent {
            report.failures.push("agent mode is the EVAL §2 protocol, run by an agent through the logging wrapper; the harness runs retrieval mode".into());
            return Ok(report);
        }
        let manifest = self.circles.manifest()?;
        let configured: Vec<CircleName> = manifest.defs.iter().map(|d| d.name.clone()).collect();
        let mut groups: Groups = Vec::new();
        for q in questions {
            if !opts.circles.is_empty() && !q.circles.iter().any(|c| opts.circles.contains(c)) {
                continue;
            }
            if let Some(Precondition::Unconfigured(c)) = &q.precondition {
                if configured.contains(c) {
                    report.skipped.push((q.id.clone(), format!("precondition: {} is configured here", c.0)));
                    continue;
                }
            }
            if q.gold.iter().any(|g| matches!(g.target, GoldTarget::Error(_))) {
                // the circle must fail to resolve, loudly, never answer empty
                let arg = q.circles.iter().map(|c| c.0.as_str()).collect::<Vec<_>>().join(",");
                let outcome = circles::parse_in(&manifest, &arg).and_then(|exprs| {
                    let mut bases = Vec::new();
                    for e in &exprs {
                        bases.extend(circles::base_names(&manifest, e)?);
                    }
                    self.circles.open(&manifest, &bases, &q.rev).map(|_| ())
                });
                let solved = matches!(outcome, Err(Error::UnknownCircle(_)));
                if !solved {
                    report.failures.push(format!("{}: an unconfigured circle {arg} answered instead of failing ({outcome:?})", q.id));
                }
                report.rows.push(QuestionRow {
                    id: q.id.clone(),
                    set: q.set.clone(),
                    rev: rev_text(&q.rev),
                    circle: q.circles[0].clone(),
                    ranks_of_rel2: Vec::new(),
                    ndcg: f32::NAN,
                    distractor_above: false,
                    bytes: 0,
                    ms: 0.0,
                    absent_said: false,
                    forbidden_hits: 0,
                    skipped_items: 0,
                    solved,
                });
                continue;
            }
            if let Some(c) = q.circles.iter().find(|c| !configured.contains(c)) {
                report.skipped.push((q.id.clone(), format!("circle {} is not configured here", c.0)));
                continue;
            }
            let key = (q.rev.clone(), q.circles.clone());
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, g)) => g.push(q),
                None => groups.push((key, vec![q])),
            }
        }
        let ctx = RunCtx { manifest: &manifest, configured: &configured, opts };
        for ((rev, circles_of), group) in &groups {
            self.run_group(&ctx, rev, circles_of, group, &mut report);
        }
        if let Some(save) = &opts.save {
            let previous = latest_run(save)?;
            write_run(save, &report)?;
            if let Some(prev) = previous {
                report.failures.extend(regressions(&summarize(&report), &prev));
            }
        }
        Ok(report)
    }

    fn verify_gold(&mut self, questions: &[Question]) -> Result<Vec<GoldDrift>, Error> {
        let manifest = self.circles.manifest()?;
        verify::verify(self.repo, &manifest, questions)
    }
}

/// Aggregates per set and question type.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub set: String,
    pub qtype: String,
    pub questions: u32,
    pub solved: u32,
    pub r1: f32,
    pub r5: f32,
    pub r10: f32,
    pub mrr: f32,
    pub ndcg: f32,
    pub distractor_above: f32,
    pub bytes_p50: u32,
    pub bytes_p90: u32,
    pub ms_p50: f32,
    pub ms_p90: f32,
}

fn pct<T: Copy + PartialOrd>(v: &mut [T], p: f32) -> Option<T> {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (!v.is_empty()).then(|| v[((v.len() as f32 - 1.0) * p).round() as usize])
}

/// Aggregates per set over every question type. Recall and MRR count each rel-2 item in each
/// circle it was asked in; a question counts once for solved, bytes and time.
pub fn summarize(report: &EvalReport) -> Vec<Summary> {
    summarize_with(report, &|_| "all".to_string())
}

/// [`summarize`], with each question's type from `qtype` (by id).
pub fn summarize_with(report: &EvalReport, qtype: &dyn Fn(&str) -> String) -> Vec<Summary> {
    use std::collections::BTreeMap;
    struct Acc {
        ids: Vec<String>,
        solved: u32,
        r: [f32; 3],
        rr: f32,
        ranked_items: u32,
        ndcg: Vec<f32>,
        distractor: u32,
        bytes: Vec<u32>,
        ms: Vec<f32>,
    }
    let mut accs: BTreeMap<(String, String), Acc> = BTreeMap::new();
    let mut seen: Vec<(String, String)> = Vec::new();
    for row in &report.rows {
        let set = set_name(&row.set);
        let t = qtype(&row.id);
        let types = if t == "all" { vec![t] } else { vec![t, "all".to_string()] };
        for t in types {
            let a = accs.entry((set.clone(), t.clone())).or_insert(Acc {
                ids: Vec::new(),
                solved: 0,
                r: [0.0; 3],
                rr: 0.0,
                ranked_items: 0,
                ndcg: Vec::new(),
                distractor: 0,
                bytes: Vec::new(),
                ms: Vec::new(),
            });
            let key = (row.id.clone(), format!("{set}/{t}"));
            if !seen.contains(&key) {
                seen.push(key);
                a.ids.push(row.id.clone());
                a.solved += u32::from(row.solved);
                a.bytes.push(row.bytes);
                a.ms.push(row.ms);
            }
            for r in &row.ranks_of_rel2 {
                a.ranked_items += 1;
                if let Some(r) = r {
                    for (i, k) in [1, 5, 10].iter().enumerate() {
                        if r <= k {
                            a.r[i] += 1.0;
                        }
                    }
                    a.rr += 1.0 / *r as f32;
                }
            }
            if !row.ndcg.is_nan() {
                a.ndcg.push(row.ndcg);
            }
            a.distractor += u32::from(row.distractor_above);
        }
    }
    accs.into_iter()
        .map(|((set, qtype), mut a)| {
            let n = a.ranked_items.max(1) as f32;
            Summary {
                set,
                qtype,
                questions: a.ids.len() as u32,
                solved: a.solved,
                r1: a.r[0] / n,
                r5: a.r[1] / n,
                r10: a.r[2] / n,
                mrr: a.rr / n,
                ndcg: if a.ndcg.is_empty() { f32::NAN } else { a.ndcg.iter().sum::<f32>() / a.ndcg.len() as f32 },
                distractor_above: a.distractor as f32 / a.ids.len().max(1) as f32,
                bytes_p50: pct(&mut a.bytes, 0.5).unwrap_or(0),
                bytes_p90: pct(&mut a.bytes, 0.9).unwrap_or(0),
                ms_p50: pct(&mut a.ms, 0.5).unwrap_or(0.0),
                ms_p90: pct(&mut a.ms, 0.9).unwrap_or(0.0),
            }
        })
        .collect()
}

/// The report as text: one line per (set, type), then the skipped and the failures.
pub fn report_text(report: &EvalReport, qtype: &dyn Fn(&str) -> String) -> String {
    let mut t = String::from("set  type  n  solved  R@1  R@5  R@10  MRR  nDCG  distractor  bytes p50/p90  ms p50/p90\n");
    for s in summarize_with(report, qtype) {
        t.push_str(&format!(
            "{}  {}  {}  {} ({:.0}%)  {:.2}  {:.2}  {:.2}  {:.2}  {:.2}  {:.2}  {}/{}  {:.1}/{:.1}\n",
            s.set,
            s.qtype,
            s.questions,
            s.solved,
            100.0 * s.solved as f32 / s.questions.max(1) as f32,
            s.r1,
            s.r5,
            s.r10,
            s.mrr,
            s.ndcg,
            s.distractor_above,
            s.bytes_p50,
            s.bytes_p90,
            s.ms_p50,
            s.ms_p90
        ));
    }
    for (id, why) in &report.skipped {
        t.push_str(&format!("skipped {id}: {why}\n"));
    }
    for f in &report.failures {
        t.push_str(&format!("FAIL {f}\n"));
    }
    t
}

fn row_json(r: &QuestionRow) -> Value {
    obj(vec![
        ("id", s(&r.id)),
        ("set", s(set_name(&r.set))),
        ("rev", s(&r.rev)),
        ("circle", s(&r.circle.0)),
        ("ranks", Value::Arr(r.ranks_of_rel2.iter().map(|x| x.map_or(Value::Null, |v| Value::Int(i64::from(v)))).collect())),
        ("ndcg", if r.ndcg.is_nan() { Value::Null } else { Value::F64(f64::from(r.ndcg)) }),
        ("distractor_above", Value::Bool(r.distractor_above)),
        ("bytes", Value::Int(i64::from(r.bytes))),
        ("ms", Value::F64(f64::from(r.ms))),
        ("absent_said", Value::Bool(r.absent_said)),
        ("forbidden_hits", Value::Int(i64::from(r.forbidden_hits))),
        ("skipped_items", Value::Int(i64::from(r.skipped_items))),
        ("solved", Value::Bool(r.solved)),
    ])
}

/// The report as JSON Lines: one object per row, then one per skip and failure.
pub fn report_json(report: &EvalReport) -> String {
    let mut out = String::new();
    for r in &report.rows {
        row_json(r).write_into(&mut out);
        out.push('\n');
    }
    for (id, why) in &report.skipped {
        obj(vec![("skipped", s(id)), ("why", s(why))]).write_into(&mut out);
        out.push('\n');
    }
    for f in &report.failures {
        obj(vec![("failure", s(f))]).write_into(&mut out);
        out.push('\n');
    }
    out
}

/// A saved run's per-set aggregates.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedSummary {
    pub set: String,
    pub r10: f32,
    pub mrr: f32,
    pub bytes_p90: u32,
}

/// The newest run file in `save`'s directory other than `save` itself, by name.
fn latest_run(save: &Path) -> Result<Option<Vec<SavedSummary>>, Error> {
    let dir = save.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(None) };
    let mut runs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl") && p.as_path() != save)
        .collect();
    runs.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    match runs.last() {
        Some(p) => read_saved(p).map(Some),
        None => Ok(None),
    }
}

/// Reads a run file's summary lines.
pub fn read_saved(path: &Path) -> Result<Vec<SavedSummary>, Error> {
    let text = std::fs::read_to_string(path).map_err(|error| Error::Io { context: format!("run {}", path.display()), error })?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let v = makepad_strict_json::parse(line.as_bytes()).map_err(|e| Error::Parse { file: path.display().to_string(), line: i as u32 + 1, message: e.into() })?;
        if let Some(set) = v.get("summary").and_then(Value::as_str) {
            let f = |k: &str| match v.get(k) {
                Some(Value::F64(x)) => *x as f32,
                Some(Value::Int(x)) => *x as f32,
                _ => 0.0,
            };
            out.push(SavedSummary { set: set.to_string(), r10: f("r10"), mrr: f("mrr"), bytes_p90: f("bytes_p90") as u32 });
        }
    }
    Ok(out)
}

/// Saves a run: its summary lines (per set, all types), then its rows.
fn write_run(save: &Path, report: &EvalReport) -> Result<(), Error> {
    if let Some(dir) = save.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|error| Error::Io { context: format!("run dir {}", dir.display()), error })?;
    }
    let mut out = String::new();
    for sm in summarize(report).iter().filter(|s| s.qtype == "all") {
        obj(vec![
            ("summary", s(&sm.set)),
            ("r10", Value::F64(f64::from(sm.r10))),
            ("mrr", Value::F64(f64::from(sm.mrr))),
            ("bytes_p90", Value::Int(i64::from(sm.bytes_p90))),
        ])
        .write_into(&mut out);
        out.push('\n');
    }
    out.push_str(&report_json(report));
    std::fs::write(save, out).map_err(|error| Error::Io { context: format!("saving run {}", save.display()), error })
}

/// Failures for metrics beyond their tolerance of a previous run.
pub fn regressions(now: &[Summary], prev: &[SavedSummary]) -> Vec<String> {
    let mut out = Vec::new();
    for n in now.iter().filter(|s| s.qtype == "all") {
        let Some(p) = prev.iter().find(|p| p.set == n.set) else { continue };
        if n.r10 < p.r10 - MAX_R10_DROP {
            out.push(format!("{}: R@10 {:.3} fell from {:.3}", n.set, n.r10, p.r10));
        }
        if n.mrr < p.mrr - MAX_MRR_DROP {
            out.push(format!("{}: MRR {:.3} fell from {:.3}", n.set, n.mrr, p.mrr));
        }
        if p.bytes_p90 > 0 && n.bytes_p90 as f32 > p.bytes_p90 as f32 * (1.0 + MAX_P90_BYTES_GROWTH) {
            out.push(format!("{}: p90 bytes {} grew from {}", n.set, n.bytes_p90, p.bytes_p90));
        }
    }
    out
}

/// Per-question changes between two saved runs: solved flips and rank changes, wins first.
pub fn diff_runs(a: &Path, b: &Path) -> Result<String, Error> {
    type Rows = Vec<(String, bool, Vec<Option<i64>>)>;
    fn rows(p: &Path) -> Result<Rows, Error> {
        let text = std::fs::read_to_string(p).map_err(|error| Error::Io { context: format!("run {}", p.display()), error })?;
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let v = makepad_strict_json::parse(line.as_bytes()).map_err(|e| Error::Parse { file: p.display().to_string(), line: i as u32 + 1, message: e.into() })?;
            if let (Some(id), Some(solved)) = (v.get("id").and_then(Value::as_str), v.get("solved").and_then(Value::as_bool)) {
                let ranks = v.get("ranks").and_then(Value::as_arr).map(|r| r.iter().map(Value::as_i64).collect()).unwrap_or_default();
                let key = format!("{id}@{}", v.get("circle").and_then(Value::as_str).unwrap_or(""));
                out.push((key, solved, ranks));
            }
        }
        Ok(out)
    }
    let (old, new) = (rows(a)?, rows(b)?);
    let (mut wins, mut losses, mut moved) = (Vec::new(), Vec::new(), Vec::new());
    for (id, solved, ranks) in &new {
        match old.iter().find(|(o, _, _)| o == id) {
            None => moved.push(format!("new   {id}")),
            Some((_, was, old_ranks)) if was != solved => {
                (if *solved { &mut wins } else { &mut losses }).push(format!("{} {id} {old_ranks:?} -> {ranks:?}", if *solved { "win  " } else { "loss " }))
            }
            Some((_, _, old_ranks)) if old_ranks != ranks => moved.push(format!("rank  {id} {old_ranks:?} -> {ranks:?}")),
            _ => {}
        }
    }
    for (id, _, _) in &old {
        if !new.iter().any(|(n, _, _)| n == id) {
            moved.push(format!("gone  {id}"));
        }
    }
    Ok([wins, losses, moved].concat().join("\n"))
}

#[cfg(test)]
mod tests {
    use super::score::tests::{block, hit, q};
    use super::*;
    use crate::open::fakes::{opener, FakeRepo};
    use crate::{FindOutput, Kind, Opened, Record};
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Answers each query from a table; records which circles each call was opened with.
    struct TableSearch {
        answers: HashMap<String, FindOutput>,
        calls: RefCell<Vec<(String, Vec<String>)>>,
    }

    impl Search for TableSearch {
        fn find(&self, opened: &Opened, query: &Query) -> Result<FindOutput, Error> {
            let w = query.words.join(" ");
            self.calls.borrow_mut().push((w.clone(), opened.open.iter().map(|c| c.name.0.clone()).collect()));
            Ok(self.answers.get(&w).cloned().unwrap_or_default())
        }
        fn resolve(&self, _: &Opened, _: &crate::Handle) -> Result<Vec<crate::Located>, Error> {
            Ok(Vec::new())
        }
        fn refs(&self, _: &Opened, _: &str, _: bool) -> Result<crate::RefsOutput, Error> {
            Err(Error::Git("not in this test".into()))
        }
    }

    struct LenRender;
    impl Render for LenRender {
        fn text(&self, out: &Output) -> String {
            match out {
                Output::Find(f) => "x".repeat(10 * f.blocks.iter().map(|b| b.hits.len()).sum::<usize>()),
                _ => String::new(),
            }
        }
        fn records(&self, _: &Output) -> Vec<Record> {
            Vec::new()
        }
        fn json_lines(&self, _: &[Record]) -> String {
            String::new()
        }
    }

    fn path(p: &str, line: Option<u32>) -> GoldTarget {
        GoldTarget::Path { path: p.into(), line, end: None, alt_lines: Vec::new() }
    }

    #[test]
    fn a_run_groups_skips_gates_and_saves() {
        let root = std::env::temp_dir().join(format!("repo-index-eval-{}", std::process::id())).join("makepad");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
        std::fs::create_dir_all(&root).unwrap();
        let mut repo = FakeRepo::new(&root);
        repo.rev(&["9a76551d7", "HEAD", "refs/remotes/upstream/work"], 1, &[("a.rs", "fn a() {}\n")]);
        let (mut circles, _) = opener(repo, &root.join("none.conf"));
        let mut answers = HashMap::new();
        answers.insert("alpha".into(), FindOutput { blocks: vec![block("makepad", vec![hit("a.rs", 1, Kind::Fn, "a")], false)], pointers: vec![] });
        answers.insert("beta".into(), FindOutput { blocks: vec![block("makepad", vec![hit("b.rs", 1, Kind::Fn, "b")], true)], pointers: vec![] });
        let search = TableSearch { answers, calls: RefCell::new(Vec::new()) };
        let mut verify_repo = FakeRepo::new(&root);
        let mut eval = Eval { circles: &mut circles, search: &search, render: &LenRender, repo: &mut verify_repo, root: root.clone() };
        let mut pre = Question { precondition: Some(Precondition::Unconfigured(CircleName("makepad:history".into()))), ..q("pre", &["makepad"], vec![(2, path("a.rs", None))]) };
        pre.query = "alpha".into();
        let questions = vec![
            Question { query: "alpha".into(), ..q("w1", &["makepad"], vec![(2, path("a.rs", Some(1)))]) },
            Question { query: "beta".into(), ..q("w2", &["makepad"], vec![(2, path("a.rs", None))]) },
            Question { query: "x".into(), ..q("n05", &["notes:nope"], vec![(2, GoldTarget::Error("unconfigured circle".into()))]) },
            Question { query: "x".into(), ..q("loc", &["notes:topic"], vec![(2, path("a.md", None))]) },
            pre,
        ];
        let runs = root.parent().unwrap().join("runs");
        let opts = EvalOpts { mode: EvalMode::Retrieval, sets: vec![], circles: vec![], k: 10, json: false, save: Some(runs.join("one.jsonl")) };
        let report = eval.run(&questions, &opts).unwrap();
        assert_eq!(search.calls.borrow().len(), 2, "w1 and w2 share one open of makepad");
        let skipped: Vec<&str> = report.skipped.iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(skipped, ["loc", "pre"]);
        let solved: Vec<(&str, bool)> = report.rows.iter().map(|r| (r.id.as_str(), r.solved)).collect();
        assert_eq!(solved, [("n05", true), ("w1", true), ("w2", false)]);
        assert_eq!(report.failures, ["w2: false absence (absent claimed, gold exists)"]);
        assert!(report_text(&report, &|_| "where".into()).contains("FAIL w2"));
        // a later run that ranks worse fails against the saved one
        let mut worse = report.clone();
        for r in &mut worse.rows {
            r.ranks_of_rel2 = vec![None];
            r.solved = false;
        }
        let prev = read_saved(&runs.join("one.jsonl")).unwrap();
        assert!(!regressions(&summarize(&worse), &prev).is_empty());
        assert!(regressions(&summarize(&report), &prev).is_empty());
        write_run(&runs.join("two.jsonl"), &worse).unwrap();
        let d = diff_runs(&runs.join("one.jsonl"), &runs.join("two.jsonl")).unwrap();
        assert!(d.contains("loss  w1@makepad"), "{d}");
        assert!(report_json(&report).lines().count() >= 5);
    }

    #[test]
    fn agent_mode_is_not_run_here() {
        let root = std::env::temp_dir().join(format!("repo-index-eval-agent-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let (mut circles, _) = opener(FakeRepo::new(&root), &root.join("none.conf"));
        let search = TableSearch { answers: HashMap::new(), calls: RefCell::new(Vec::new()) };
        let mut r = FakeRepo::new(&root);
        let mut eval = Eval { circles: &mut circles, search: &search, render: &LenRender, repo: &mut r, root: root.clone() };
        let opts = EvalOpts { mode: EvalMode::Agent, sets: vec![], circles: vec![], k: 10, json: false, save: None };
        let report = eval.run(&[q("a", &["makepad"], vec![(2, path("a", None))])], &opts).unwrap();
        assert!(report.rows.is_empty() && report.failures.len() == 1);
    }
}
