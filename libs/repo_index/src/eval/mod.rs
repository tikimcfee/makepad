//! The eval harness ([`crate::Harness`]): questions grouped by (revision, circles), each group
//! indexed at its pinned revision; retrieval metrics per circle; `verify-gold`; the leak check
//! (no chunk under an exclusion, in the base or the overlay).
//!
//! A run opens each group's circles once through [`crate::Circles::open`], asks every question
//! as a `find`, renders the output to measure its bytes, and scores it (`score.rs`). Hard gates
//! become [`crate::EvalReport::failures`]: containment, a false absence, an unconfigured circle
//! that answers instead of failing, and a metric worse than the last saved run by more than its
//! tolerance. Questions on circles not configured here, and those whose precondition does not
//! hold, are skipped and listed. Agent mode is run by an agent through a
//! logging wrapper, not by this harness.

pub mod leak;
pub mod load;
pub mod pointer;
pub mod score;
pub mod verify;

use crate::{
    AbsentScope,
    circles, CircleExpr, CircleManifest, CircleName, Circles, Error, EvalMode, EvalOpts, EvalReport, GoldDrift, GoldTarget,
    Harness, Mode, Output, Precondition, Query, Question, QuestionRow, QuestionSet, Render, Repo, RevSpec, Search,
};
use makepad_core_util::clock::monotonic_now;
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

/// A call and its wall time in milliseconds.
fn timed<T>(f: impl FnOnce() -> T) -> (T, f32) {
    let t = monotonic_now();
    let r = f();
    (r, ((monotonic_now() - t) * 1000.0) as f32)
}

/// Questions sharing one (revision, circles) open.
type Groups<'q> = Vec<((RevSpec, Vec<CircleName>), Vec<&'q Question>)>;

/// What every group of one run shares.
struct RunCtx<'a> {
    manifest: &'a CircleManifest,
    configured: &'a [CircleName],
    opts: &'a EvalOpts,
    /// Questions whose pinned notes changed: asked for containment only.
    stale: &'a [String],
}

impl Eval<'_> {
    /// Whether each `path:`, `package:` and `fn` absence scope of the group holds in the tree
    /// circle of the harness's repo at `rev` (worktree changes included at the worktree): one
    /// pass for the group. A scope that cannot be judged is a failure of its questions, and does
    /// not hold.
    fn scopes_held(&mut self, manifest: &CircleManifest, rev: &RevSpec, group: &[&Question], report: &mut EvalReport) -> Vec<(AbsentScope, bool)> {
        let mut scopes: Vec<AbsentScope> = Vec::new();
        let mut asking: Vec<&str> = Vec::new();
        for q in group {
            for g in &q.gold {
                if let GoldTarget::Absent { scope: Some(s @ (AbsentScope::Path(_) | AbsentScope::Package(_) | AbsentScope::Fn(_))), semantic: false, .. } = &g.target {
                    if !scopes.contains(s) {
                        scopes.push(s.clone());
                    }
                    if !asking.contains(&q.id.as_str()) {
                        asking.push(&q.id);
                    }
                }
            }
        }
        if scopes.is_empty() {
            return Vec::new();
        }
        let tree = manifest.defs.iter().find_map(|d| match &d.body {
            crate::CircleBody::Base(b @ crate::BaseCircle { source: crate::BaseSource::Tree { root }, .. }) if crate::open::same_dir(root, &self.root) => Some(b),
            _ => None,
        });
        let judged = match tree {
            Some(def) => verify::scopes_hold(self.repo, def, rev, &scopes),
            None => Err(Error::UnknownCircle(format!("a tree circle on {}", self.root.display()))),
        };
        match judged {
            Ok(held) => scopes.into_iter().zip(held).collect(),
            Err(e) => {
                for id in asking {
                    report.failures.push(format!("{id}: absence scopes not judged at {}: {e}", rev_text(rev)));
                }
                Vec::new()
            }
        }
    }

    fn run_group(
        &mut self,
        ctx: &RunCtx,
        rev: &RevSpec,
        circles_of: &[CircleName],
        group: &[&Question],
        report: &mut EvalReport,
        claims: &mut Vec<verify::Claim>,
    ) {
        let RunCtx { manifest, configured, opts, stale } = *ctx;
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
        let held = self.scopes_held(manifest, rev, group, report);
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
            let scored = score::score(q, &out, opts.k, bytes, ms, configured, &held);
            if scored.forbidden > 0 {
                report.failures.push(format!("{}: containment: {} forbidden records ranked", q.id, scored.forbidden));
            }
            // a stale question keeps its containment gate (it never depends on the gold's
            // counts); its scores, pointer counts and absence check wait for fresh gold
            if stale.contains(&q.id) {
                continue;
            }
            for (circle, term) in scored.absent_claims {
                claims.push(verify::Claim { id: q.id.clone(), circle, rev: rev.clone(), term });
            }
            if scored.rows.is_empty() {
                report.failures.push(format!("{}: no block for {:?}", q.id, names(&q.circles)));
            }
            report.rows.extend(scored.rows);
        }
    }
}

/// The content stamp of every notes circle configured here, by name.
pub fn notes_stamps(manifest: &CircleManifest) -> Result<Vec<(CircleName, u64)>, Error> {
    use crate::{BaseCircle, BaseSource, CircleBody};
    let mut out = Vec::new();
    for d in &manifest.defs {
        if let CircleBody::Base(def @ BaseCircle { source: BaseSource::Notes { sources }, .. }) = &d.body {
            out.push((d.name.clone(), crate::open::notes_stamp(sources, def)?));
        }
    }
    Ok(out)
}

/// Why a question is stale: a pinned notes circle whose content stamp differs now. A pinned
/// circle not configured here leaves the question to the configuration check.
fn stale_pin(manifest: &CircleManifest, q: &Question) -> Result<Option<String>, Error> {
    if q.pins.is_empty() {
        return Ok(None);
    }
    let now = notes_stamps(manifest)?;
    for (c, pinned) in &q.pins {
        match now.iter().find(|(n, _)| n == c) {
            Some((_, stamp)) if stamp != pinned => {
                return Ok(Some(format!("{} changed since its gold: pinned {pinned:016x}, now {stamp:016x}", c.0)))
            }
            Some(_) => {}
            None => return Ok(Some(format!("{} is pinned but not a notes circle configured here", c.0))),
        }
    }
    Ok(None)
}

/// Every base circle whose index a revision of the repo at `root` decides: its tree and history
/// circles, and every notes circle. Another repo's circles (`commercial`) are not opened at this
/// repo's revisions.
pub fn pointer_circles(manifest: &CircleManifest, root: &Path) -> Vec<CircleName> {
    use crate::{BaseCircle, BaseSource, CircleBody};
    let here = |r: &Path| std::fs::canonicalize(r).ok() == std::fs::canonicalize(root).ok();
    manifest
        .defs
        .iter()
        .filter(|d| match &d.body {
            CircleBody::Base(BaseCircle { source: BaseSource::Tree { root: r } | BaseSource::History { root: r, .. }, .. }) => here(r),
            CircleBody::Base(BaseCircle { source: BaseSource::Notes { .. }, .. }) => true,
            CircleBody::Derived(_) => false,
        })
        .map(|d| d.name.clone())
        .collect()
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
            report.failures.push("agent mode is run by an agent through a logging wrapper; the harness runs retrieval mode".into());
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
            if let Some(why) = stale_pin(&manifest, q)? {
                report.stale.push((q.id.clone(), why));
            }
            let key = (q.rev.clone(), q.circles.clone());
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, g)) => g.push(q),
                None => groups.push((key, vec![q])),
            }
        }
        // a pointer counts only circles that are built, so build every circle a pointer can name
        // first: the run is then a function of (revision, configuration), never of build state.
        // Each circle is built on its own; one that cannot be (a notes source missing here) is
        // one failure, and only the questions that point at it go without its pointer.
        let mut prebuilt: Vec<RevSpec> = Vec::new();
        for ((rev, _), _) in &groups {
            if prebuilt.contains(rev) {
                continue;
            }
            prebuilt.push(rev.clone());
            for c in pointer_circles(&manifest, &self.root) {
                if let Err(e) = self.circles.open(&manifest, std::slice::from_ref(&c), rev) {
                    let f = format!("circle {} cannot be built here: {e}", c.0);
                    if !report.failures.contains(&f) {
                        report.failures.push(f);
                    }
                }
            }
        }
        let stale: Vec<String> = report.stale.iter().map(|(id, _)| id.clone()).collect();
        let ctx = RunCtx { manifest: &manifest, configured: &configured, opts, stale: &stale };
        let mut claims: Vec<verify::Claim> = Vec::new();
        for ((rev, circles_of), group) in &groups {
            self.run_group(&ctx, rev, circles_of, group, &mut report, &mut claims);
        }
        // a false absence is a named absent term that does occur in its circle at the revision
        let counts = verify::occurrences(self.repo, &manifest, &claims)?;
        for (c, n) in claims.iter().zip(counts) {
            if n > 0 {
                report.failures.push(format!(
                    "{}: false absence: {} says `{}` is absent at {}, but {n} of its records hold it",
                    c.id,
                    c.circle.0,
                    c.term,
                    rev_text(&c.rev)
                ));
            }
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
    /// The share of questions with a distractor above their first rel-2 hit in some block.
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
        /// Questions with a distractor above their first rel-2 hit in some block, once each.
        distractor: Vec<String>,
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
                distractor: Vec::new(),
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
            if row.distractor_above && !a.distractor.contains(&row.id) {
                a.distractor.push(row.id.clone());
            }
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
                distractor_above: a.distractor.len() as f32 / a.ids.len().max(1) as f32,
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
    for (id, why) in &report.stale {
        t.push_str(&format!("stale {id}: {why}\n"));
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
    for (id, why) in &report.stale {
        obj(vec![("stale", s(id)), ("why", s(why))]).write_into(&mut out);
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

    /// The harness's own repo in a test: the same revisions as the circles' repo.
    fn mirror(root: &Path, files: &[(&str, &str)]) -> FakeRepo {
        let mut r = FakeRepo::new(root);
        r.rev(&["c0ffee123", "HEAD", "refs/remotes/upstream/work", "refs/heads/work"], 1, files);
        r
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
        repo.rev(&["c0ffee123", "HEAD", "refs/remotes/upstream/work"], 1, &[("a.rs", "fn a() {}\n")]);
        let (mut circles, _) = opener(repo, &root.join("none.conf"));
        let mut answers = HashMap::new();
        answers.insert("alpha".into(), FindOutput { blocks: vec![block("makepad", vec![hit("a.rs", 1, Kind::Fn, "a")], false)], pointers: vec![] });
        let mut wrong = block("makepad", vec![hit("b.rs", 1, Kind::Fn, "b")], true);
        wrong.absent_terms = vec!["a".into()]; // a.rs holds `a`: a false absence
        answers.insert("beta".into(), FindOutput { blocks: vec![wrong], pointers: vec![] });
        // real hits and a true absence together: no failure
        let mut truthful = block("makepad", vec![hit("a.rs", 1, Kind::Fn, "a")], true);
        truthful.absent_terms = vec!["zz_nowhere".into()];
        answers.insert("gamma".into(), FindOutput { blocks: vec![truthful], pointers: vec![] });
        let search = TableSearch { answers, calls: RefCell::new(Vec::new()) };
        let mut verify_repo = mirror(&root, &[("a.rs", "fn a() {}\n")]);
        let mut eval = Eval { circles: &mut circles, search: &search, render: &LenRender, repo: &mut verify_repo, root: root.clone() };
        let mut pre = Question { precondition: Some(Precondition::Unconfigured(CircleName("makepad:history".into()))), ..q("pre", &["makepad"], vec![(2, path("a.rs", None))]) };
        pre.query = "alpha".into();
        let questions = vec![
            Question { query: "alpha".into(), ..q("w1", &["makepad"], vec![(2, path("a.rs", Some(1)))]) },
            Question { query: "beta".into(), ..q("w2", &["makepad"], vec![(2, path("a.rs", None))]) },
            Question { query: "gamma".into(), ..q("w3", &["makepad"], vec![(2, path("a.rs", Some(1)))]) },
            Question { query: "x".into(), ..q("n05", &["notes:nope"], vec![(2, GoldTarget::Error("unconfigured circle".into()))]) },
            Question { query: "x".into(), ..q("loc", &["notes:topic"], vec![(2, path("a.md", None))]) },
            pre,
        ];
        let runs = root.parent().unwrap().join("runs");
        let opts = EvalOpts { mode: EvalMode::Retrieval, sets: vec![], circles: vec![], k: 10, json: false, save: Some(runs.join("one.jsonl")) };
        let report = eval.run(&questions, &opts).unwrap();
        assert_eq!(search.calls.borrow().len(), 3, "w1, w2 and w3 share one open of makepad");
        let skipped: Vec<&str> = report.skipped.iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(skipped, ["loc", "pre"]);
        let solved: Vec<(&str, bool)> = report.rows.iter().map(|r| (r.id.as_str(), r.solved)).collect();
        assert_eq!(solved, [("n05", true), ("w1", true), ("w2", false), ("w3", true)]);
        assert_eq!(report.failures, ["w2: false absence: makepad says `a` is absent at c0ffee123, but 1 of its records hold it"]);
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
    fn pointer_circles_are_built_before_scoring() {
        let top = std::env::temp_dir().join(format!("repo-index-eval-pre-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        let root = top.join("makepad");
        std::fs::create_dir_all(top.join("mail/t")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(top.join("mail/t/a.md"), "# a\n").unwrap();
        let conf = top.join("c.conf");
        std::fs::write(&conf, format!("notes:t = notes {}\n", top.join("mail/t").display())).unwrap();
        let mut repo = FakeRepo::new(&root);
        repo.rev(&["c0ffee123", "HEAD", "refs/remotes/upstream/work", "refs/heads/work"], 1, &[("a.rs", "fn a() {}\n")]);
        let (mut circles, store) = opener(repo, &conf);
        let search = TableSearch { answers: HashMap::new(), calls: RefCell::new(Vec::new()) };
        let others = RefCell::new(Vec::new());
        struct Spy<'a>(&'a TableSearch, &'a RefCell<Vec<Vec<String>>>);
        impl Search for Spy<'_> {
            fn find(&self, opened: &Opened, query: &Query) -> Result<FindOutput, Error> {
                self.1.borrow_mut().push(opened.others.iter().map(|c| c.name.0.clone()).collect());
                self.0.find(opened, query)
            }
            fn resolve(&self, o: &Opened, h: &crate::Handle) -> Result<Vec<crate::Located>, Error> {
                self.0.resolve(o, h)
            }
            fn refs(&self, o: &Opened, i: &str, u: bool) -> Result<crate::RefsOutput, Error> {
                self.0.refs(o, i, u)
            }
        }
        let spy = Spy(&search, &others);
        let mut r = mirror(&root, &[]);
        let mut eval = Eval { circles: &mut circles, search: &spy, render: &LenRender, repo: &mut r, root: root.clone() };
        let pointer = GoldTarget::Pointer { circle: CircleName("notes:t".into()), count: None, unit: crate::PointerUnit::Blobs };
        let opts = EvalOpts { mode: EvalMode::Retrieval, sets: vec![], circles: vec![], k: 10, json: false, save: None };
        eval.run(&[q("x", &["makepad"], vec![(2, pointer)])], &opts).unwrap();
        assert!(others.borrow()[0].contains(&"notes:t".to_string()), "{:?}", others.borrow());
        assert!(others.borrow()[0].contains(&"makepad:history".to_string()));
        assert!(store.state.borrow().manifests.len() >= 2);
    }

    #[test]
    fn path_package_and_fn_scopes_are_judged_against_the_circle_not_the_printed_hits() {
        let root = std::env::temp_dir().join(format!("repo-index-eval-scopes-{}", std::process::id())).join("makepad");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
        std::fs::create_dir_all(&root).unwrap();
        let files: &[(&str, &str)] = &[
            ("apps/still/main.rs", "fn here_still() {}\n"),
            ("libs/p/Cargo.toml", "[package]\nname = \"still-pkg\"\n"),
            ("libs/repo_index/fixture/Cargo.toml", "[package]\nname = \"fixture-only\"\n"),
        ];
        let mut repo = FakeRepo::new(&root);
        repo.rev(&["c0ffee123", "HEAD", "refs/remotes/upstream/work"], 1, files);
        let (mut circles, _) = opener(repo, &root.join("none.conf"));
        // the query prints nothing at all: only the circle can say what is absent
        let mut answers = HashMap::new();
        answers.insert("zz".into(), FindOutput { blocks: vec![block("makepad", vec![], false)], pointers: vec![] });
        let search = TableSearch { answers, calls: RefCell::new(Vec::new()) };
        let mut r = mirror(&root, files);
        let mut eval = Eval { circles: &mut circles, search: &search, render: &LenRender, repo: &mut r, root: root.clone() };
        let absent = |s: AbsentScope| GoldTarget::Absent { scope: Some(s), semantic: false, term: None };
        let one = |id: &str, s: AbsentScope| Question { query: "zz".into(), ..q(id, &["makepad"], vec![(2, absent(s))]) };
        let questions = vec![
            one("p_gone", AbsentScope::Path("apps/director".into())),
            one("p_here", AbsentScope::Path("apps/still".into())),
            one("p_suffix", AbsentScope::Path("main.rs".into())),
            one("k_gone", AbsentScope::Package("ironfish".into())),
            one("k_here", AbsentScope::Package("still-pkg".into())),
            one("k_excluded", AbsentScope::Package("fixture-only".into())), // only in the index's own data
            one("f_gone", AbsentScope::Fn("unit_here".into())),
            one("f_here", AbsentScope::Fn("here_still".into())),
        ];
        let opts = EvalOpts { mode: EvalMode::Retrieval, sets: vec![], circles: vec![], k: 10, json: false, save: None };
        let report = eval.run(&questions, &opts).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        let solved: Vec<(&str, bool)> = report.rows.iter().map(|r| (r.id.as_str(), r.solved)).collect();
        assert_eq!(
            solved,
            [("p_gone", true), ("p_here", false), ("p_suffix", false), ("k_gone", true), ("k_here", false), ("k_excluded", true), ("f_gone", true), ("f_here", false)]
        );
    }

    #[test]
    fn a_distractor_counts_once_per_question_whatever_its_blocks() {
        let row = |id: &str, circle: &str, above: bool| QuestionRow {
            id: id.into(),
            set: QuestionSet::Shipped,
            rev: "c0ffee123".into(),
            circle: CircleName(circle.into()),
            ranks_of_rel2: vec![Some(1)],
            ndcg: 1.0,
            distractor_above: above,
            bytes: 0,
            ms: 0.0,
            absent_said: false,
            forbidden_hits: 0,
            skipped_items: 0,
            solved: true,
        };
        // q1 has a distractor above its gold in both its blocks, q2 in neither
        let report = EvalReport {
            rows: vec![row("q1", "makepad", true), row("q1", "makepad:history", true), row("q2", "makepad", false)],
            ..EvalReport::default()
        };
        let s = summarize(&report);
        assert_eq!(s[0].questions, 2);
        assert!((s[0].distractor_above - 0.5).abs() < 1e-6, "{}", s[0].distractor_above);
    }

    #[test]
    fn a_circle_that_cannot_be_built_is_one_failure_not_every_question() {
        let top = std::env::temp_dir().join(format!("repo-index-eval-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        let root = top.join("makepad");
        std::fs::create_dir_all(&root).unwrap();
        let conf = top.join("c.conf");
        // a source that is a file, not a directory, cannot be listed (a missing one is just left out)
        std::fs::write(top.join("nowhere"), "x").unwrap();
        std::fs::write(&conf, format!("notes:gone = notes {}\n", top.join("nowhere").display())).unwrap();
        let mut repo = FakeRepo::new(&root);
        repo.rev(&["c0ffee123", "HEAD", "refs/remotes/upstream/work", "refs/heads/work"], 1, &[("a.rs", "fn a() {}\n")]);
        let (mut circles, _) = opener(repo, &conf);
        let mut answers = HashMap::new();
        answers.insert("alpha".into(), FindOutput { blocks: vec![block("makepad", vec![hit("a.rs", 1, Kind::Fn, "a")], false)], pointers: vec![] });
        let search = TableSearch { answers, calls: RefCell::new(Vec::new()) };
        let mut r = mirror(&root, &[]);
        let mut eval = Eval { circles: &mut circles, search: &search, render: &LenRender, repo: &mut r, root: root.clone() };
        let questions: Vec<Question> =
            ["w1", "w2"].iter().map(|id| Question { query: "alpha".into(), ..q(id, &["makepad"], vec![(2, path("a.rs", Some(1)))]) }).collect();
        let opts = EvalOpts { mode: EvalMode::Retrieval, sets: vec![], circles: vec![], k: 10, json: false, save: None };
        let report = eval.run(&questions, &opts).unwrap();
        assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
        assert!(report.failures[0].starts_with("circle notes:gone cannot be built here"), "{:?}", report.failures);
        assert!(report.rows.iter().all(|r| r.solved));
    }

    #[test]
    fn a_question_on_changed_notes_is_stale_not_failed() {
        let top = std::env::temp_dir().join(format!("repo-index-eval-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        let root = top.join("makepad");
        std::fs::create_dir_all(top.join("mail/t")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(top.join("mail/t/a.md"), "# a\n").unwrap();
        let conf = top.join("c.conf");
        std::fs::write(&conf, format!("notes:t = notes {}\n", top.join("mail/t").display())).unwrap();
        let mut repo = FakeRepo::new(&root);
        repo.rev(&["c0ffee123", "HEAD", "refs/remotes/upstream/work", "refs/heads/work"], 1, &[]);
        let (mut circles, _) = opener(repo, &conf);
        let mut answers = HashMap::new();
        answers.insert("x".to_string(), FindOutput { blocks: vec![block("makepad", vec![], true)], pointers: vec![] });
        let search = TableSearch { answers, calls: RefCell::new(Vec::new()) };
        let mut r = mirror(&root, &[]);
        let mut eval = Eval { circles: &mut circles, search: &search, render: &LenRender, repo: &mut r, root: root.clone() };
        let m = eval.circles.manifest().unwrap();
        let pinned = notes_stamps(&m).unwrap()[0].1;
        let pointer = GoldTarget::Pointer { circle: CircleName("notes:t".into()), count: Some(1), unit: crate::PointerUnit::Blobs };
        let question = Question { pins: vec![(CircleName("notes:t".into()), pinned)], ..q("n01", &["makepad"], vec![(2, pointer)]) };
        let opts = EvalOpts { mode: EvalMode::Retrieval, sets: vec![], circles: vec![], k: 10, json: false, save: None };
        let fresh = eval.run(std::slice::from_ref(&question), &opts).unwrap();
        assert!(fresh.stale.is_empty() && fresh.rows.len() == 1, "unchanged notes: scored");
        std::fs::write(top.join("mail/t/b.md"), "# b\n").unwrap();
        let later = eval.run(std::slice::from_ref(&question), &opts).unwrap();
        assert!(later.rows.is_empty() && later.failures.is_empty(), "stale: neither scored nor failed");
        assert_eq!(later.stale.len(), 1);
        // but its containment gate still holds: a forbidden record ranked is a failure
        let mut leaking = HashMap::new();
        leaking.insert("x".to_string(), FindOutput { blocks: vec![block("makepad", vec![hit("local/agent_state/port/g.rs", 1, Kind::Fn, "g")], false)], pointers: vec![] });
        let search2 = TableSearch { answers: leaking, calls: RefCell::new(Vec::new()) };
        let mut r2 = mirror(&root, &[]);
        let (mut circles2, _) = opener(
            {
                let mut repo = FakeRepo::new(&root);
                repo.rev(&["c0ffee123", "HEAD", "refs/remotes/upstream/work", "refs/heads/work"], 1, &[]);
                repo
            },
            &conf,
        );
        let mut eval2 = Eval { circles: &mut circles2, search: &search2, render: &LenRender, repo: &mut r2, root: root.clone() };
        let guarded = Question { forbid: vec!["local/agent_state/*".into()], ..question.clone() };
        let leak = eval2.run(&[guarded], &opts).unwrap();
        assert_eq!(leak.stale.len(), 1);
        assert!(leak.rows.is_empty(), "still not scored");
        assert_eq!(leak.failures, ["n01: containment: 1 forbidden records ranked"]);
        assert!(later.stale[0].1.starts_with(&format!("notes:t changed since its gold: pinned {pinned:016x}")), "{:?}", later.stale);
        assert!(report_text(&later, &|_| "circle".into()).contains("stale n01: notes:t changed"));
    }

    #[test]
    fn agent_mode_is_not_run_here() {
        let root = std::env::temp_dir().join(format!("repo-index-eval-agent-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let (mut circles, _) = opener(FakeRepo::new(&root), &root.join("none.conf"));
        let search = TableSearch { answers: HashMap::new(), calls: RefCell::new(Vec::new()) };
        let mut r = mirror(&root, &[]);
        let mut eval = Eval { circles: &mut circles, search: &search, render: &LenRender, repo: &mut r, root: root.clone() };
        let opts = EvalOpts { mode: EvalMode::Agent, sets: vec![], circles: vec![], k: 10, json: false, save: None };
        let report = eval.run(&[q("a", &["makepad"], vec![(2, path("a", None))])], &opts).unwrap();
        assert!(report.rows.is_empty() && report.failures.len() == 1);
    }
}
