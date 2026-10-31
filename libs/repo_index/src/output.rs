//! Text and `--json` output ([`crate::Render`]) for every verb's [`crate::Output`], within the
//! `find` byte budget ([`crate::FIND_P90_BYTES`]).
//!
//! Text is plain, no ANSI. `find` prints, per circle block: a header line (circle, revision,
//! shown of total, definitions and uses), hits grouped by package (each hit one line of at most
//! [`crate::MAX_LINE_CHARS`]: `handle  kind  name — gist`), a `+N more` line, and under an
//! absence the nearest hits; then one `also:` line of pointers. `--json` is one record per
//! line ([`crate::Record`]), every field always present.

use makepad_strict_json::{obj, s, Value};

use crate::query::handle::format as handle_text;
use crate::{
    CircleBlock, CircleName, CircleStatus, CrateView, Field, FindOutput, Hit, Origin, Output, Pointer, PointerUnit,
    Record, RefsOutput, Render, ShowOutput, StaleFinding, StaleKind, StaleOutput, StatusOutput, UseSummary,
    MAX_GIST_CHARS, MAX_LINE_CHARS,
};

/// A chunk's gist: the first line of its doc text, cut at the end of its first sentence and at
/// [`MAX_GIST_CHARS`] characters. Never generated; empty when there is no doc. The store keeps
/// this as [`crate::ChunkView::gist`].
pub fn gist(doc: &str) -> &str {
    let line = doc.trim_start().lines().next().unwrap_or("").trim_end();
    let line = match line.find(". ") {
        Some(end) => &line[..end + 1],
        None => line,
    };
    match line.char_indices().nth(MAX_GIST_CHARS) {
        Some((cut, _)) => &line[..cut],
        None => line,
    }
}

/// The [`Render`] implementation.
#[derive(Clone, Copy, Debug, Default)]
pub struct Renderer;

impl Render for Renderer {
    fn text(&self, out: &Output) -> String {
        let mut t = String::new();
        match out {
            Output::Find(f) => find_text(&mut t, f),
            Output::Show(sh) => show_text(&mut t, sh),
            Output::Crate(c) => crate_text(&mut t, c),
            Output::Refs(r) => refs_text(&mut t, r),
            Output::Stale(st) => stale_text(&mut t, st),
            Output::Status(st) => status_text(&mut t, st),
        }
        t
    }

    fn records(&self, out: &Output) -> Vec<Record> {
        match out {
            Output::Find(f) => {
                let mut r = Vec::new();
                for b in &f.blocks {
                    r.push(Record::Header {
                        circle: b.circle.clone(),
                        rev: b.rev.clone(),
                        total: b.total,
                        definitions: b.definitions,
                        uses: b.uses,
                        absent: b.absent,
                    });
                    for (hits, nearest) in [(&b.hits, false), (&b.nearest, true)] {
                        r.extend(hits.iter().map(|h| Record::Hit { circle: b.circle.clone(), rev: b.rev.clone(), hit: Box::new(h.clone()), nearest }));
                    }
                }
                r.extend(f.pointers.iter().cloned().map(Record::Pointer));
                r
            }
            Output::Show(sh) => vec![Record::Show(sh.clone())],
            Output::Crate(c) => vec![Record::Crate(c.clone())],
            Output::Refs(rf) => {
                let mut r = vec![Record::Refs { circle: rf.circle.clone(), rev: rf.rev.clone(), ident: rf.ident.clone(), uses: rf.uses.clone() }];
                for list in [&rf.defs, &rf.reexports, &rf.use_hits, &rf.other_defs] {
                    r.extend(list.iter().map(|h| Record::Hit { circle: rf.circle.clone(), rev: rf.rev.clone(), hit: Box::new(h.clone()), nearest: false }));
                }
                r
            }
            Output::Stale(st) => st.findings.iter().cloned().map(Record::Stale).collect(),
            Output::Status(st) => st.circles.iter().cloned().map(Record::Circle).collect(),
        }
    }

    fn json_lines(&self, records: &[Record]) -> String {
        let mut out = String::new();
        for r in records {
            record_json(r).write_into(&mut out);
            out.push('\n');
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Text

/// One line: newlines and tabs as spaces, cut to `max` characters with `…`.
fn clip(text: &str, max: usize) -> String {
    let flat: String = text.chars().map(|c| if c == '\n' || c == '\t' || c == '\r' { ' ' } else { c }).collect();
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn push_line(t: &mut String, line: &str) {
    t.push_str(&clip(line, MAX_LINE_CHARS));
    t.push('\n');
}

fn at_rev(circle: &CircleName, rev: &str) -> String {
    if rev.is_empty() {
        circle.0.clone()
    } else {
        format!("{} @{rev}", circle.0)
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `handle  kind  name — gist`, with the signature when there is no gist, and marks for an
/// overlay hit, a local-only commit and collapsed copies.
fn hit_line(h: &Hit) -> String {
    let mut line = format!("{}  {}  {}", handle_text(&h.handle), h.kind.as_str(), h.name);
    if !h.gist.is_empty() {
        line.push_str(" — ");
        line.push_str(&h.gist);
    } else if !h.sig.is_empty() && h.sig != h.name {
        line.push_str("  ");
        line.push_str(&h.sig);
    }
    if h.overlay {
        line.push_str(" [worktree]");
    }
    if h.local {
        line.push_str(" [local]");
    }
    if !h.also_at.is_empty() {
        line.push_str(&format!(" (+{} {})", h.also_at.len(), if h.also_at.len() == 1 { "copy" } else { "copies" }));
    }
    line
}

fn block_text(t: &mut String, b: &CircleBlock) {
    let counts = format!("{}, {}", plural(b.definitions, "definition", "definitions"), plural(b.uses, "use", "uses"));
    if b.absent {
        let at = if b.rev.is_empty() { String::new() } else { format!(" at {}", b.rev) };
        push_line(t, &format!("{} · absent: {counts}{at}", at_rev(&b.circle, &b.rev)));
    } else {
        push_line(t, &format!("{} · {} of {} · {counts}", at_rev(&b.circle, &b.rev), b.hits.len(), b.total));
    }
    let mut group: Option<&str> = None;
    for h in &b.hits {
        if group != Some(h.package.as_str()) {
            push_line(t, if h.package.is_empty() { "(no package)" } else { &h.package });
            group = Some(h.package.as_str());
        }
        push_line(t, &format!("  {}", hit_line(h)));
    }
    if b.total > b.hits.len() as u64 && !b.hits.is_empty() {
        push_line(t, &format!("  +{} more · narrow with --crate, --kind, --path or -n", b.total - b.hits.len() as u64));
    }
    if !b.nearest.is_empty() {
        push_line(t, "nearest:");
        for h in &b.nearest {
            push_line(t, &format!("  {}", hit_line(h)));
        }
    }
}

fn pointer_text(p: &Pointer) -> String {
    match (&p.circle, &p.branch) {
        (_, Some(branch)) => {
            let mut s = format!(
                "branch {branch}: {} in {} not upstream",
                plural(p.count, "file", "files"),
                plural(p.commits, "commit", "commits")
            );
            if !p.sample.is_empty() {
                s.push_str(&format!(" ({})", p.sample));
            }
            s
        }
        (Some(c), None) => {
            let unit = match p.unit {
                PointerUnit::Blobs | PointerUnit::Files => ("file", "files"),
                PointerUnit::Chunks => ("hit", "hits"),
                PointerUnit::Commits => ("commit", "commits"),
            };
            format!("{} {}", c.0, plural(p.count, unit.0, unit.1))
        }
        (None, None) => plural(p.count, "record", "records"),
    }
}

fn find_text(t: &mut String, f: &FindOutput) {
    for b in &f.blocks {
        block_text(t, b);
    }
    if !f.pointers.is_empty() {
        let parts: Vec<String> = f.pointers.iter().map(pointer_text).collect();
        push_line(t, &format!("also: {}", parts.join(" · ")));
    }
}

fn uses_line(u: &UseSummary) -> String {
    let dirs: Vec<String> = u.by_dir.iter().take(5).map(|(d, n)| format!("{d} {n}")).collect();
    let mut s = format!("uses {} in {}", u.total, plural(u.files, "file", "files"));
    if !dirs.is_empty() {
        s.push_str(&format!("; by dir: {}", dirs.join(" · ")));
        if u.by_dir.len() > 5 {
            s.push_str(" · …");
        }
    }
    s
}

fn show_text(t: &mut String, sh: &ShowOutput) {
    let h = handle_text(&sh.handle);
    let pkg = if sh.package.is_empty() { String::new() } else { format!(" · {}", sh.package) };
    push_line(t, &format!("{h}{pkg} · {} {} · {}", sh.kind.as_str(), sh.name, at_rev(&sh.circle, &sh.rev)));
    for line in &sh.text {
        t.push_str(line);
        t.push('\n');
    }
    if sh.more > 0 {
        push_line(t, &format!("… {} more {} (show {h} --all)", sh.more, if sh.more == 1 { "line" } else { "lines" }));
    }
    if let Some(u) = &sh.uses {
        push_line(t, &uses_line(u));
    }
    if !sh.cited_by.is_empty() {
        let c: Vec<String> = sh.cited_by.iter().map(|(c, n)| format!("{} ({n})", c.0)).collect();
        push_line(t, &format!("cited by {}", c.join(" · ")));
    }
    if !sh.also_at.is_empty() {
        push_line(t, &format!("also at: {}", sh.also_at.join(", ")));
    }
}

/// Words wrapped into lines of at most [`MAX_LINE_CHARS`], each after `indent`.
fn wrap(t: &mut String, first: &str, words: &[String], indent: &str) {
    let mut line = first.to_string();
    for w in words {
        if line.chars().count() + 1 + w.chars().count() > MAX_LINE_CHARS {
            push_line(t, &line);
            line = indent.to_string();
        }
        if !line.trim().is_empty() {
            line.push(' ');
        }
        line.push_str(w);
    }
    push_line(t, &line);
}

fn crate_text(t: &mut String, c: &CrateView) {
    let k = &c.card;
    let targets = if k.targets.is_empty() { String::new() } else { format!(" · {}", k.targets.join(", ")) };
    push_line(t, &format!("{}  {} · {} .rs{targets} · {}", c.name, k.dir, k.rs_files, at_rev(&c.circle, &c.rev)));
    if !k.description.is_empty() {
        wrap(t, " ", &k.description.split_whitespace().map(|w| w.to_string()).collect::<Vec<_>>(), "  ");
    }
    let mut deps: Vec<String> = Vec::new();
    let mut cfgs: Vec<&str> = Vec::new();
    for (_, cfg) in &k.deps {
        if !cfgs.contains(&cfg.as_str()) {
            cfgs.push(cfg);
        }
    }
    for cfg in cfgs {
        if !cfg.is_empty() {
            deps.push(format!("· {cfg}:"));
        }
        deps.extend(k.deps.iter().filter(|(_, c)| c == cfg).map(|(n, _)| n.clone()));
    }
    wrap(t, &format!("deps (path) {}:", k.deps.len()), &deps, "  ");
    let mut used = format!("used by {}", k.dependents.len());
    if k.dependents.is_empty() {
        used.push_str(" (public tree)");
    } else {
        used.push(':');
    }
    if k.private_members {
        used.push_str(" · private members may use it");
    }
    wrap(t, &used, &k.dependents, "  ");
    if !k.apps.is_empty() {
        wrap(t, &format!("apps {}:", k.apps.len()), &k.apps, "  ");
    }
    if !k.modules.is_empty() {
        wrap(t, &format!("modules {}:", k.modules.len()), &k.modules, "  ");
    }
}

fn refs_text(t: &mut String, r: &RefsOutput) {
    push_line(t, &format!("{} · refs {} (by name, not resolved)", at_rev(&r.circle, &r.rev), r.ident));
    if r.defs.is_empty() {
        push_line(t, "def  none");
    }
    for h in &r.defs {
        push_line(t, &format!("def  {} · {}", hit_line(h), h.package));
    }
    for h in &r.reexports {
        push_line(t, &format!("re-export  {}", hit_line(h)));
    }
    push_line(t, &uses_line(&r.uses));
    for h in &r.use_hits {
        push_line(t, &format!("  {}", hit_line(h)));
    }
    if !r.other_defs.is_empty() {
        let o: Vec<String> = r.other_defs.iter().map(|h| handle_text(&h.handle)).collect();
        wrap(t, &format!("other defs named {}:", r.ident), &o, "  ");
    }
}

fn stale_name(k: StaleKind) -> &'static str {
    match k {
        StaleKind::Dangling => "dangling",
        StaleKind::Retired => "retired",
        StaleKind::Missing => "missing",
        StaleKind::Outside => "outside",
        StaleKind::Unavailable => "unavailable",
    }
}

fn stale_line(f: &StaleFinding) -> String {
    let at = if f.line > 0 { format!("{}:{}", f.path, f.line) } else { f.path.clone() };
    let mut s = format!("{:<11} {at}  {}  {}", stale_name(f.kind), f.name, f.detail);
    if !f.also.is_empty() {
        s.push_str(&format!(" (also {})", f.also.join(", ")));
    }
    s
}

fn stale_text(t: &mut String, st: &StaleOutput) {
    push_line(t, &format!("{} · {}", at_rev(&st.circle, &st.rev), plural(st.findings.len() as u64, "finding", "findings")));
    for f in &st.findings {
        push_line(t, &stale_line(f));
    }
}

fn origin_text(o: &Origin) -> String {
    match o {
        Origin::BuiltIn => String::from("built-in"),
        Origin::Conf { line } => format!("circles.conf:{line}"),
    }
}

fn status_text(t: &mut String, st: &StatusOutput) {
    for c in &st.circles {
        let state = if c.built {
            format!("{} · {} · overlay {}", plural(c.chunks, "chunk", "chunks"), plural(c.segments.len() as u64, "segment", "segments"), c.overlay_files)
        } else {
            String::from("not built")
        };
        let rev = if c.rev.is_empty() { String::new() } else { format!(" @{}", c.rev) };
        let note = if c.note.is_empty() { String::new() } else { format!(" · {}", c.note) };
        push_line(t, &format!("{}{rev} · {state} · {} · {}{note}", c.name.0, origin_text(&c.origin), c.source));
    }
}

// ---------------------------------------------------------------------------------------------
// JSON

fn field_name(f: Field) -> &'static str {
    match f {
        Field::Name => "name",
        Field::Path => "path",
        Field::Sig => "sig",
        Field::Doc => "doc",
        Field::Body => "body",
    }
}

fn unit_name(u: PointerUnit) -> &'static str {
    match u {
        PointerUnit::Blobs => "blobs",
        PointerUnit::Chunks => "chunks",
        PointerUnit::Commits => "commits",
        PointerUnit::Files => "files",
    }
}

fn int(n: u64) -> Value {
    Value::Int(n.min(i64::MAX as u64) as i64)
}

fn strs<'a>(items: impl IntoIterator<Item = &'a String>) -> Value {
    Value::Arr(items.into_iter().map(|x| s(x.as_str())).collect())
}

fn opt_str(o: Option<&str>) -> Value {
    o.map(s).unwrap_or(Value::Null)
}

fn uses_json(u: &UseSummary) -> Value {
    obj(vec![
        ("total", int(u.total)),
        ("files", int(u.files)),
        ("by_dir", Value::Arr(u.by_dir.iter().map(|(d, n)| obj(vec![("dir", s(d.as_str())), ("lines", int(*n))])).collect())),
    ])
}

fn hit_json(circle: &CircleName, rev: &str, h: &Hit, nearest: bool) -> Value {
    obj(vec![
        ("handle", s(handle_text(&h.handle))),
        ("circle", s(circle.0.as_str())),
        ("rev", s(rev)),
        ("kind", s(h.kind.as_str())),
        ("package", s(h.package.as_str())),
        ("path", s(h.path.as_str())),
        ("start", int(h.lines.start as u64)),
        ("end", int(h.lines.end as u64)),
        ("name", s(h.name.as_str())),
        ("sig", s(h.sig.as_str())),
        ("gist", s(h.gist.as_str())),
        ("score", Value::F64(((h.score as f64) * 1000.0).round() / 1000.0)),
        ("why", Value::Arr(h.why.iter().map(|w| s(format!("{}:{}", field_name(w.field), w.term))).collect())),
        ("also_at", strs(&h.also_at)),
        ("overlay", Value::Bool(h.overlay)),
        ("local", Value::Bool(h.local)),
        ("nearest", Value::Bool(nearest)),
    ])
}

fn record_json(r: &Record) -> Value {
    match r {
        Record::Header { circle, rev, total, definitions, uses, absent } => obj(vec![
            ("header", Value::Bool(true)),
            ("circle", s(circle.0.as_str())),
            ("rev", s(rev.as_str())),
            ("total", int(*total)),
            ("definitions", int(*definitions)),
            ("uses", int(*uses)),
            ("absent", Value::Bool(*absent)),
        ]),
        Record::Hit { circle, rev, hit, nearest } => hit_json(circle, rev, hit, *nearest),
        Record::Pointer(p) => obj(vec![
            ("pointer", Value::Bool(true)),
            ("circle", opt_str(p.circle.as_ref().map(|c| c.0.as_str()))),
            ("branch", opt_str(p.branch.as_deref())),
            ("count", int(p.count)),
            ("unit", s(unit_name(p.unit))),
            ("commits", int(p.commits)),
            ("sample", s(p.sample.as_str())),
        ]),
        Record::Show(sh) => obj(vec![
            ("show", Value::Bool(true)),
            ("circle", s(sh.circle.0.as_str())),
            ("rev", s(sh.rev.as_str())),
            ("handle", s(handle_text(&sh.handle))),
            ("kind", s(sh.kind.as_str())),
            ("package", s(sh.package.as_str())),
            ("path", s(sh.path.as_str())),
            ("start", int(sh.lines.start as u64)),
            ("end", int(sh.lines.end as u64)),
            ("name", s(sh.name.as_str())),
            ("text", strs(&sh.text)),
            ("more", int(sh.more as u64)),
            ("uses", sh.uses.as_ref().map(uses_json).unwrap_or(Value::Null)),
            ("cited_by", Value::Arr(sh.cited_by.iter().map(|(c, n)| obj(vec![("circle", s(c.0.as_str())), ("count", int(*n))])).collect())),
            ("also_at", strs(&sh.also_at)),
        ]),
        Record::Crate(c) => obj(vec![
            ("crate", Value::Bool(true)),
            ("circle", s(c.circle.0.as_str())),
            ("rev", s(c.rev.as_str())),
            ("name", s(c.name.as_str())),
            ("dir", s(c.card.dir.as_str())),
            ("description", s(c.card.description.as_str())),
            ("deps", Value::Arr(c.card.deps.iter().map(|(n, cfg)| obj(vec![("name", s(n.as_str())), ("cfg", s(cfg.as_str()))])).collect())),
            ("dependents", strs(&c.card.dependents)),
            ("apps", strs(&c.card.apps)),
            ("targets", strs(&c.card.targets)),
            ("rs_files", int(c.card.rs_files as u64)),
            ("modules", strs(&c.card.modules)),
            ("private_members", Value::Bool(c.card.private_members)),
        ]),
        Record::Refs { circle, rev, ident, uses } => obj(vec![
            ("refs", Value::Bool(true)),
            ("circle", s(circle.0.as_str())),
            ("rev", s(rev.as_str())),
            ("ident", s(ident.as_str())),
            ("uses", int(uses.total)),
            ("files", int(uses.files)),
            ("by_dir", uses_json(uses).get("by_dir").cloned().unwrap_or(Value::Arr(Vec::new()))),
        ]),
        Record::Stale(f) => obj(vec![
            ("stale", s(stale_name(f.kind))),
            ("path", s(f.path.as_str())),
            ("line", int(f.line as u64)),
            ("name", s(f.name.as_str())),
            ("detail", s(f.detail.as_str())),
            ("also", strs(&f.also)),
        ]),
        Record::Circle(c) => circle_json(c),
    }
}

fn circle_json(c: &CircleStatus) -> Value {
    obj(vec![
        ("circle_status", Value::Bool(true)),
        ("name", s(c.name.0.as_str())),
        ("origin", s(origin_text(&c.origin))),
        ("source", s(c.source.as_str())),
        ("rev", s(c.rev.as_str())),
        ("segments", Value::Arr(c.segments.iter().map(|id| s(id.0.as_str())).collect())),
        ("chunks", int(c.chunks)),
        ("overlay_files", int(c.overlay_files)),
        ("built", Value::Bool(c.built)),
        ("note", s(c.note.as_str())),
    ])
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CrateCard, Handle, Kind, Lines, SegmentId, Target, Why};

    fn hit(path: &str, name: &str, gist: &str) -> Hit {
        Hit {
            handle: Handle { circle: None, target: Target::Lines { path: path.into(), lines: Lines { start: 14, end: 17 } }, rev: None },
            kind: Kind::Method,
            package: "fixture-alpha".into(),
            path: path.into(),
            lines: Lines { start: 14, end: 17 },
            name: name.into(),
            sig: "pub fn spin(&mut self, step: u32)".into(),
            gist: gist.into(),
            score: 1.23456,
            why: vec![Why { field: Field::Name, term: "spin".into() }],
            also_at: Vec::new(),
            overlay: false,
            local: false,
        }
    }

    fn circle(n: &str) -> CircleName {
        CircleName(n.into())
    }

    fn every_field(json: &str, keys: &[&str]) {
        let v = makepad_strict_json::parse(json.as_bytes()).expect("json");
        for k in keys {
            assert!(v.get(k).is_some(), "{k} missing in {json}");
        }
    }

    #[test]
    fn gist_is_the_first_sentence_of_the_first_line() {
        assert_eq!(gist("Spins the gizmo. Then stops."), "Spins the gizmo.");
        assert_eq!(gist("\n  Turns per tick,\n at most."), "Turns per tick,");
        assert_eq!(gist(""), "");
        let long = "x".repeat(200);
        assert_eq!(gist(&long).chars().count(), MAX_GIST_CHARS);
        assert_eq!(gist(&"é".repeat(100)).chars().count(), MAX_GIST_CHARS);
    }

    #[test]
    fn hit_lines_are_clipped_and_marked() {
        let mut h = hit("alpha/src/lib.rs", "Gizmo::spin", &"y".repeat(80));
        h.handle.target = Target::Lines { path: "a/".repeat(40) + "lib.rs", lines: Lines { start: 1, end: 2 } };
        let text = Renderer.text(&Output::Find(FindOutput {
            blocks: vec![CircleBlock { circle: circle("makepad"), rev: "9a76551d7".into(), total: 4, definitions: 1, uses: 3, hits: vec![h], absent: false, nearest: Vec::new() }],
            pointers: Vec::new(),
        }));
        assert!(text.lines().all(|l| l.chars().count() <= MAX_LINE_CHARS), "{text}");
        assert!(text.starts_with("makepad @9a76551d7 · 1 of 4 · 1 definition, 3 uses\nfixture-alpha\n  a/a/"), "{text}");
        assert!(text.contains("+3 more · narrow with --crate, --kind, --path or -n"));
        let mut h = hit("alpha/src/lib.rs", "Gizmo::spin", "");
        h.overlay = true;
        h.also_at = vec!["b.rs:1".into()];
        assert_eq!(hit_line(&h), "alpha/src/lib.rs:14-17  method  Gizmo::spin  pub fn spin(&mut self, step: u32) [worktree] (+1 copy)");
    }

    fn show() -> ShowOutput {
        ShowOutput {
            circle: circle("makepad"),
            rev: "9a76551d7".into(),
            handle: Handle { circle: None, target: Target::Lines { path: "alpha/src/lib.rs".into(), lines: Lines { start: 14, end: 17 } }, rev: None },
            kind: Kind::Method,
            package: "fixture-alpha".into(),
            path: "alpha/src/lib.rs".into(),
            lines: Lines { start: 14, end: 17 },
            name: "Gizmo::spin".into(),
            text: vec!["    pub fn spin(&mut self, step: u32) {".into(), "    }".into()],
            more: 2,
            uses: Some(UseSummary { total: 3, files: 2, by_dir: vec![("alpha/src".into(), 2), ("docs".into(), 1)] }),
            cited_by: vec![(circle("notes:topic-a"), 2)],
            also_at: Vec::new(),
        }
    }

    #[test]
    fn show_text_and_json() {
        let out = Output::Show(Box::new(show()));
        let text = Renderer.text(&out);
        assert_eq!(
            text,
            "alpha/src/lib.rs:14-17 · fixture-alpha · method Gizmo::spin · makepad @9a76551d7\n    pub fn spin(&mut self, step: u32) {\n    }\n… 2 more lines (show alpha/src/lib.rs:14-17 --all)\nuses 3 in 2 files; by dir: alpha/src 2 · docs 1\ncited by notes:topic-a (2)\n"
        );
        every_field(Renderer.json_lines(&Renderer.records(&out)).trim_end(), &["show", "circle", "rev", "handle", "kind", "package", "path", "start", "end", "name", "text", "more", "uses", "cited_by", "also_at"]);
    }

    #[test]
    fn crate_text_and_json() {
        let card = CrateCard {
            dir: "alpha".into(),
            description: "Spinning gizmos for the index fixture".into(),
            deps: vec![("fixture-beta".into(), String::new()), ("fixture-vk".into(), "linux".into())],
            dependents: Vec::new(),
            apps: vec!["spinner".into()],
            targets: vec!["lib".into()],
            rs_files: 2,
            modules: vec!["tables".into()],
            private_members: true,
        };
        let out = Output::Crate(Box::new(CrateView { circle: circle("makepad"), rev: "9a76551d7".into(), name: "fixture-alpha".into(), card }));
        let text = Renderer.text(&out);
        assert_eq!(
            text,
            "fixture-alpha  alpha · 2 .rs · lib · makepad @9a76551d7\n Spinning gizmos for the index fixture\ndeps (path) 2: fixture-beta · linux: fixture-vk\nused by 0 (public tree) · private members may use it\napps 1: spinner\nmodules 1: tables\n"
        );
        every_field(Renderer.json_lines(&Renderer.records(&out)).trim_end(), &["crate", "circle", "rev", "name", "dir", "description", "deps", "dependents", "apps", "targets", "rs_files", "modules", "private_members"]);
    }

    #[test]
    fn refs_text_and_json() {
        let r = RefsOutput {
            circle: circle("makepad"),
            rev: "9a76551d7".into(),
            ident: "MAX_TURN".into(),
            defs: vec![hit("alpha/src/lib.rs", "MAX_TURN", "Turns per tick, at most.")],
            reexports: Vec::new(),
            uses: UseSummary { total: 2, files: 2, by_dir: vec![("alpha/src".into(), 1), ("docs".into(), 1)] },
            use_hits: Vec::new(),
            other_defs: vec![hit("beta/src/lib.rs", "max_turn", "")],
        };
        let out = Output::Refs(Box::new(r));
        let text = Renderer.text(&out);
        assert!(text.starts_with("makepad @9a76551d7 · refs MAX_TURN (by name, not resolved)\ndef  alpha/src/lib.rs:14-17  method  MAX_TURN — Turns per tick, at most. · fixture-alpha\n"), "{text}");
        assert!(text.contains("uses 2 in 2 files; by dir: alpha/src 1 · docs 1\nother defs named MAX_TURN: beta/src/lib.rs:14-17\n"), "{text}");
        let json = Renderer.json_lines(&Renderer.records(&out));
        let mut lines = json.lines();
        every_field(lines.next().unwrap(), &["refs", "circle", "rev", "ident", "uses", "files", "by_dir"]);
        assert_eq!(lines.count(), 2);
    }

    #[test]
    fn stale_and_status_text_and_json() {
        let st = Output::Stale(StaleOutput {
            circle: circle("makepad"),
            rev: "9a76551d7".into(),
            findings: vec![StaleFinding { kind: StaleKind::Dangling, path: "README.md".into(), line: 165, name: "makepad-example-ironfish".into(), detail: "moved to old/".into(), also: vec!["tools/android/README.md:23".into()] }],
        });
        assert_eq!(Renderer.text(&st), "makepad @9a76551d7 · 1 finding\ndangling    README.md:165  makepad-example-ironfish  moved to old/ (also tools/android/README.md:23)\n");
        every_field(Renderer.json_lines(&Renderer.records(&st)).trim_end(), &["stale", "path", "line", "name", "detail", "also"]);

        let status = Output::Status(StatusOutput {
            circles: vec![
                CircleStatus { name: circle("makepad"), origin: Origin::BuiltIn, source: "tree .".into(), rev: "9a76551d7".into(), segments: vec![SegmentId("seg-ab.mpx".into())], chunks: 20, overlay_files: 1, built: true, note: String::new() },
                CircleStatus { name: circle("notes:topic-a"), origin: Origin::Conf { line: 3 }, source: "notes topic-a".into(), rev: String::new(), segments: Vec::new(), chunks: 0, overlay_files: 0, built: false, note: "build with `makepad index build --in notes:topic-a`".into() },
            ],
        });
        assert_eq!(
            Renderer.text(&status),
            "makepad @9a76551d7 · 20 chunks · 1 segment · overlay 1 · built-in · tree .\nnotes:topic-a · not built · circles.conf:3 · notes topic-a · build with `makepad index build --in notes:topic-a`\n"
        );
        for line in Renderer.json_lines(&Renderer.records(&status)).lines() {
            every_field(line, &["circle_status", "name", "origin", "source", "rev", "segments", "chunks", "overlay_files", "built", "note"]);
        }
    }

    #[test]
    fn pointer_json_keeps_null_fields() {
        let p = Pointer { circle: Some(circle("notes:x")), branch: None, count: 3, unit: PointerUnit::Blobs, commits: 0, sample: String::new() };
        let json = Renderer.json_lines(&[Record::Pointer(p.clone())]);
        assert_eq!(json, "{\"pointer\":true,\"circle\":\"notes:x\",\"branch\":null,\"count\":3,\"unit\":\"blobs\",\"commits\":0,\"sample\":\"\"}\n");
        assert_eq!(pointer_text(&p), "notes:x 3 files");
    }
}
