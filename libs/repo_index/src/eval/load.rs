//! Question files (EVAL §1.3): one JSON object per line. Every key a line holds is read; an
//! unknown key or a malformed value is a parse error naming the file and line, so a typo never
//! drops gold silently.

use crate::{
    AbsentScope, Anchor, CircleManifest, CircleName, Error, Gold, GoldTarget, Lines, PointerUnit, Precondition, Question,
    QuestionSet, RevSpec,
};
use makepad_strict_json::Value;
use std::path::Path;

/// The shipped set, relative to the repo root.
pub const SHIPPED: &str = "libs/repo_index/eval/questions.jsonl";

struct At<'a> {
    file: &'a str,
    line: u32,
}

impl At<'_> {
    fn err(&self, message: impl Into<String>) -> Error {
        Error::Parse { file: self.file.to_string(), line: self.line, message: message.into() }
    }
}

fn str_of<'v>(v: &'v Value, key: &str, at: &At) -> Result<&'v str, Error> {
    v.get(key).and_then(Value::as_str).ok_or_else(|| at.err(format!("`{key}` must be a string")))
}

fn opt_str(v: &Value, key: &str, at: &At) -> Result<Option<String>, Error> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Str(s)) => Ok(Some(s.clone())),
        Some(_) => Err(at.err(format!("`{key}` must be a string"))),
    }
}

fn opt_u32(v: &Value, key: &str, at: &At) -> Result<Option<u32>, Error> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Int(i)) if *i >= 0 && *i <= u32::MAX as i64 => Ok(Some(*i as u32)),
        Some(_) => Err(at.err(format!("`{key}` must be a non-negative integer"))),
    }
}

fn strings(v: &Value, key: &str, at: &At) -> Result<Vec<String>, Error> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Arr(items)) => items
            .iter()
            .map(|i| i.as_str().map(str::to_string).ok_or_else(|| at.err(format!("`{key}` must hold strings"))))
            .collect(),
        Some(_) => Err(at.err(format!("`{key}` must be an array of strings"))),
    }
}

fn keys(v: &Value) -> Vec<&str> {
    match v {
        Value::Obj(pairs) => pairs.iter().map(|(k, _)| k.as_str()).collect(),
        _ => Vec::new(),
    }
}

fn check_keys(v: &Value, allowed: &[&str], what: &str, at: &At) -> Result<(), Error> {
    match keys(v).into_iter().find(|k| !allowed.contains(k)) {
        Some(k) => Err(at.err(format!("unknown key `{k}` in {what}"))),
        None => Ok(()),
    }
}

fn lines_list(v: &Value, at: &At) -> Result<Vec<Lines>, Error> {
    match v.get("alt_lines") {
        None => Ok(Vec::new()),
        Some(Value::Arr(items)) => items
            .iter()
            .map(|r| match r.as_arr() {
                Some([Value::Int(a), Value::Int(b)]) if *a > 0 && b >= a => Ok(Lines { start: *a as u32, end: *b as u32 }),
                _ => Err(at.err("`alt_lines` holds [start, end] pairs")),
            })
            .collect(),
        Some(_) => Err(at.err("`alt_lines` must be an array")),
    }
}

/// An absence scope: `path:<p>`, `package:<name>`, `fn <name>`, a configured circle, else text.
pub fn absent_scope(s: &str, manifest: &CircleManifest) -> AbsentScope {
    if let Some(p) = s.strip_prefix("path:") {
        AbsentScope::Path(p.to_string())
    } else if let Some(p) = s.strip_prefix("package:") {
        AbsentScope::Package(p.to_string())
    } else if let Some(f) = s.strip_prefix("fn ") {
        AbsentScope::Fn(f.trim().to_string())
    } else if manifest.defs.iter().any(|d| d.name.0 == s) {
        AbsentScope::Circle(CircleName(s.to_string()))
    } else {
        AbsentScope::Text(s.to_string())
    }
}

fn gold(v: &Value, manifest: &CircleManifest, at: &At) -> Result<Gold, Error> {
    const COMMON: &[&str] = &["rel", "symbol", "requires"];
    let rel = match v.get("rel") {
        Some(Value::Int(r)) if (0..=2).contains(r) => *r as u8,
        _ => return Err(at.err("a gold item needs `rel` 0, 1 or 2")),
    };
    let symbol = match v.get("symbol") {
        None => None,
        Some(s) => {
            check_keys(s, &["def", "heading", "text"], "a symbol", at)?;
            Some(if let Some(d) = s.get("def").and_then(Value::as_str) {
                Anchor::Def(d.to_string())
            } else if let Some(h) = s.get("heading").and_then(Value::as_str) {
                Anchor::Heading(h.to_string())
            } else if let Some(t) = s.get("text").and_then(Value::as_str) {
                Anchor::Text(t.to_string())
            } else {
                return Err(at.err("a symbol is {\"def\"|\"heading\"|\"text\": \"..\"}"));
            })
        }
    };
    let requires = opt_str(v, "requires", at)?.map(CircleName);
    fn with(extra: &[&'static str]) -> Vec<&'static str> {
        [COMMON, extra].concat()
    }
    let target = if let Some(p) = v.get("path").or(v.get("note")) {
        let is_note = v.get("note").is_some();
        check_keys(v, &with(&[if is_note { "note" } else { "path" }, "line", "end", "alt_lines"]), "a path item", at)?;
        let path = p.as_str().ok_or_else(|| at.err("a path must be a string"))?.to_string();
        let (line, end, alt_lines) = (opt_u32(v, "line", at)?, opt_u32(v, "end", at)?, lines_list(v, at)?);
        if is_note {
            GoldTarget::Note { path, line, end, alt_lines }
        } else {
            GoldTarget::Path { path, line, end, alt_lines }
        }
    } else if v.get("package").is_some() {
        check_keys(v, &with(&["package"]), "a package item", at)?;
        GoldTarget::Package(str_of(v, "package", at)?.to_string())
    } else if v.get("commit").is_some() {
        check_keys(v, &with(&["commit", "alt", "via"]), "a commit item", at)?;
        let via = opt_str(v, "via", at)?;
        if via.as_deref().is_some_and(|x| x != "pointer") {
            return Err(at.err("`via` is \"pointer\" or absent"));
        }
        GoldTarget::Commit { hex: str_of(v, "commit", at)?.to_string(), alt: strings(v, "alt", at)?, via_pointer: via.is_some() }
    } else if v.get("dependents").is_some() {
        check_keys(v, &with(&["dependents", "scope"]), "a dependents item", at)?;
        GoldTarget::Dependents { names: strings(v, "dependents", at)?, scope: opt_str(v, "scope", at)? }
    } else if v.get("absent").is_some() {
        check_keys(v, &with(&["absent", "scope", "semantic", "term"]), "an absent item", at)?;
        if v.get("absent") != Some(&Value::Bool(true)) {
            return Err(at.err("`absent` is true"));
        }
        GoldTarget::Absent {
            scope: opt_str(v, "scope", at)?.map(|s| absent_scope(&s, manifest)),
            semantic: v.get("semantic").and_then(Value::as_bool).unwrap_or(false),
            term: opt_str(v, "term", at)?,
        }
    } else if v.get("pointer").is_some() {
        check_keys(v, &with(&["pointer", "count", "unit"]), "a pointer item", at)?;
        let unit = match opt_str(v, "unit", at)?.as_deref() {
            None | Some("blobs") => PointerUnit::Blobs,
            Some("chunks") => PointerUnit::Chunks,
            Some("commits") => PointerUnit::Commits,
            Some("files") => PointerUnit::Files,
            Some(u) => return Err(at.err(format!("unknown pointer unit `{u}`"))),
        };
        GoldTarget::Pointer {
            circle: CircleName(str_of(v, "pointer", at)?.to_string()),
            count: opt_u32(v, "count", at)?.map(u64::from),
            unit,
        }
    } else if v.get("error").is_some() {
        check_keys(v, &with(&["error"]), "an error item", at)?;
        GoldTarget::Error(str_of(v, "error", at)?.to_string())
    } else {
        return Err(at.err(format!("a gold item without a target: {:?}", keys(v))));
    };
    Ok(Gold { rel, symbol, requires, target })
}

/// One question line.
pub fn question(text: &str, set: &QuestionSet, manifest: &CircleManifest, file: &str, line: u32) -> Result<Question, Error> {
    let at = At { file, line };
    let v = makepad_strict_json::parse(text.as_bytes()).map_err(|e| at.err(format!("JSON: {e}")))?;
    check_keys(
        &v,
        &[
            "id", "query", "agent_prompt", "rev", "precondition", "circles", "type", "subtype", "difficulty", "gold", "distractors",
            "forbid", "notes", "source",
        ],
        "a question",
        &at,
    )?;
    let rev = match str_of(&v, "rev", &at)? {
        "tip" => RevSpec::Pinned("HEAD".into()),
        r => RevSpec::Pinned(r.to_string()),
    };
    let precondition = match v.get("precondition") {
        None => None,
        Some(p) => {
            check_keys(p, &["unconfigured"], "a precondition", &at)?;
            Some(Precondition::Unconfigured(CircleName(str_of(p, "unconfigured", &at)?.to_string())))
        }
    };
    let items = |key: &str| -> Result<Vec<Gold>, Error> {
        match v.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Arr(a)) => a.iter().map(|g| gold(g, manifest, &at)).collect(),
            Some(_) => Err(at.err(format!("`{key}` must be an array"))),
        }
    };
    let circles: Vec<CircleName> = strings(&v, "circles", &at)?.into_iter().map(CircleName).collect();
    if circles.is_empty() {
        return Err(at.err("`circles` names at least one circle"));
    }
    let gold = items("gold")?;
    if gold.is_empty() {
        return Err(at.err("a question needs gold"));
    }
    Ok(Question {
        id: str_of(&v, "id", &at)?.to_string(),
        set: set.clone(),
        query: str_of(&v, "query", &at)?.to_string(),
        agent_prompt: opt_str(&v, "agent_prompt", &at)?,
        precondition,
        rev,
        circles,
        qtype: str_of(&v, "type", &at)?.to_string(),
        subtype: str_of(&v, "subtype", &at)?.to_string(),
        difficulty: str_of(&v, "difficulty", &at)?.to_string(),
        gold,
        distractors: items("distractors")?,
        forbid: strings(&v, "forbid", &at)?,
        notes: opt_str(&v, "notes", &at)?.unwrap_or_default(),
        source: opt_str(&v, "source", &at)?.unwrap_or_default(),
    })
}

/// Every question of a file; ids are unique within it.
pub fn file(path: &Path, set: &QuestionSet, manifest: &CircleManifest) -> Result<Vec<Question>, Error> {
    let text = std::fs::read_to_string(path).map_err(|error| Error::Io { context: format!("questions {}", path.display()), error })?;
    let name = path.display().to_string();
    let mut out: Vec<Question> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let q = question(line, set, manifest, &name, i as u32 + 1)?;
        if out.iter().any(|x| x.id == q.id) {
            return Err(Error::Parse { file: name, line: i as u32 + 1, message: format!("question id `{}` repeats", q.id) });
        }
        out.push(q);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn manifest() -> CircleManifest {
        crate::circles::manifest_for(Path::new("/r/makepad"), None, Path::new("/h")).unwrap()
    }

    #[test]
    fn every_gold_form_parses() {
        let line = r##"{"id":"q1","query":"KeyCode enum","agent_prompt":"hint","rev":"9a76551d7","precondition":{"unconfigured":"commercial"},"circles":["makepad","makepad:history"],"type":"where","subtype":"ident","difficulty":"medium","gold":[{"path":"a/b.rs","line":36,"end":40,"alt_lines":[[50,60]],"rel":2,"symbol":{"def":"enum KeyCode"}},{"note":"topic/x.md","line":3,"rel":1,"symbol":{"heading":"# x"},"requires":"notes:topic"},{"package":"pkg","rel":1},{"commit":"abc1234","alt":["def5678"],"via":"pointer","rel":1},{"dependents":[],"scope":"public tree","rel":2},{"absent":true,"scope":"path:apps/x","rel":1},{"absent":true,"scope":"fn unit_here","rel":1},{"absent":true,"scope":"makepad","semantic":true,"rel":1,"term":"glyph"},{"pointer":"notes:topic","count":9,"unit":"blobs","rel":2},{"error":"unconfigured circle","rel":2}],"distractors":[{"path":"c.rs","line":5,"rel":0,"symbol":{"text":"x"}}],"forbid":["notes:*"],"notes":"n","source":"s"}"##;
        let q = question(line, &QuestionSet::Shipped, &manifest(), "f", 1).unwrap();
        assert_eq!(q.rev, RevSpec::Pinned("9a76551d7".into()));
        assert_eq!(q.precondition, Some(Precondition::Unconfigured(CircleName("commercial".into()))));
        assert_eq!(q.gold.len(), 10);
        assert!(matches!(&q.gold[0].target, GoldTarget::Path { line: Some(36), end: Some(40), alt_lines, .. } if alt_lines == &[Lines { start: 50, end: 60 }]));
        assert_eq!(q.gold[0].symbol, Some(Anchor::Def("enum KeyCode".into())));
        assert_eq!(q.gold[1].requires, Some(CircleName("notes:topic".into())));
        assert!(matches!(&q.gold[3].target, GoldTarget::Commit { via_pointer: true, alt, .. } if alt == &["def5678"]));
        assert!(matches!(&q.gold[5].target, GoldTarget::Absent { scope: Some(AbsentScope::Path(p)), semantic: false, term: None } if p == "apps/x"));
        assert!(matches!(&q.gold[6].target, GoldTarget::Absent { scope: Some(AbsentScope::Fn(f)), .. } if f == "unit_here"));
        assert!(matches!(&q.gold[7].target, GoldTarget::Absent { scope: Some(AbsentScope::Circle(_)), semantic: true, term: Some(t) } if t == "glyph"));
        assert!(matches!(&q.gold[8].target, GoldTarget::Pointer { count: Some(9), unit: PointerUnit::Blobs, .. }));
        assert_eq!(q.distractors[0].rel, 0);
        let tip = line.replace("\"9a76551d7\"", "\"tip\"");
        assert_eq!(question(&tip, &QuestionSet::Shipped, &manifest(), "f", 1).unwrap().rev, RevSpec::Pinned("HEAD".into()));
    }

    #[test]
    fn typos_are_errors_with_the_line() {
        let ok = r#"{"id":"q","query":"x","rev":"tip","circles":["makepad"],"type":"where","subtype":"ident","difficulty":"easy","gold":[{"path":"a","rel":2}]}"#;
        assert!(question(ok, &QuestionSet::Shipped, &manifest(), "f", 1).is_ok());
        for (bad, needle) in [
            (ok.replace("\"query\"", "\"qery\""), "unknown key `qery`"),
            (ok.replace("\"rel\":2", "\"rel\":2,\"lien\":3"), "unknown key `lien`"),
            (ok.replace("\"rel\":2", "\"rel\":5"), "`rel`"),
            (ok.replace("[{\"path\":\"a\",\"rel\":2}]", "[]"), "needs gold"),
            (ok.replace("[\"makepad\"]", "[]"), "at least one"),
            (ok.replace("{\"path\":\"a\",\"rel\":2}", "{\"pointer\":\"n\",\"unit\":\"bytes\",\"rel\":2}"), "unknown pointer unit"),
        ] {
            match question(&bad, &QuestionSet::Shipped, &manifest(), "q.jsonl", 7) {
                Err(Error::Parse { file, line: 7, message }) => {
                    assert_eq!(file, "q.jsonl");
                    assert!(message.contains(needle), "{bad}: {message}");
                }
                other => panic!("{bad}: {other:?}"),
            }
        }
    }

    #[test]
    fn the_shipped_set_parses_whole() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("eval/questions.jsonl");
        let qs = file(&path, &QuestionSet::Shipped, &manifest()).unwrap();
        assert!(qs.len() >= 50, "{}", qs.len());
        assert!(qs.iter().all(|q| matches!(&q.rev, RevSpec::Pinned(_))));
        assert!(qs.iter().any(|q| q.subtype == "self-exclusion" && q.rev == RevSpec::Pinned("HEAD".into())));
        assert!(qs.iter().all(|q| q.circles.iter().all(|c| !c.0.starts_with("notes:") || q.gold.iter().any(|g| matches!(g.target, GoldTarget::Error(_))))));
    }
}
