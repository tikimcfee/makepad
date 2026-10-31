//! Top-level definitions of `.splash` files (`let`, `fn`, `mod.x.Y =`, `name =`): a definition
//! runs from its line to the next one at the top level (blank lines at its end dropped).

use crate::{Lines, Span};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplashDef {
    pub name: String,
    /// The definition's first line, trimmed.
    pub sig: String,
    pub text: String,
    pub span: Span,
    pub lines: Lines,
}

/// The byte length of the identifier `s` starts with (0 when it starts with none).
fn ident_len(s: &str) -> usize {
    if !s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') {
        return 0;
    }
    s.char_indices().take_while(|(_, c)| c.is_alphanumeric() || *c == '_').map(|(i, c)| i + c.len_utf8()).last().unwrap_or(0)
}

/// The name a top-level line defines, if it defines one.
fn defines(line: &str) -> Option<String> {
    if line.starts_with([' ', '\t']) {
        return None;
    }
    for kw in ["pub let ", "let ", "pub fn ", "fn "] {
        if let Some(rest) = line.strip_prefix(kw) {
            let n = ident_len(rest);
            return (n > 0).then(|| rest[..n].to_string());
        }
    }
    // `mod.a.b.Name = ...`, `Name = ...`, `Name := ...`, `Name +: ...`
    let n = line
        .char_indices()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_' || *c == '.')
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    if n == 0 || !line.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') {
        return None;
    }
    let name = &line[..n];
    let rest = line[n..].trim_start();
    let assigns = (rest.starts_with('=') && !rest.starts_with("==")) || rest.starts_with(":=") || rest.starts_with("+:");
    (assigns && name != "use").then(|| name.to_string())
}

pub fn defs(src: &str) -> Vec<SplashDef> {
    let mut starts = vec![0usize];
    starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
    let line_at = |i: usize| -> &str { src[starts[i]..].split('\n').next().unwrap_or("").trim_end_matches('\r') };
    let count = if src.ends_with('\n') { starts.len() - 1 } else { starts.len() };
    let mut found: Vec<(usize, String)> = Vec::new();
    for i in 0..count {
        if let Some(name) = defines(line_at(i)) {
            found.push((i, name));
        }
    }
    let mut out = Vec::new();
    for (k, (first, name)) in found.iter().enumerate() {
        let mut last = found.get(k + 1).map_or(count, |n| n.0) - 1;
        while last > *first && line_at(last).trim().is_empty() {
            last -= 1;
        }
        let lo = starts[*first];
        let hi = (starts[last] + line_at(last).len()).min(src.len());
        out.push(SplashDef {
            name: name.clone(),
            sig: line_at(*first).trim().to_string(),
            text: src[lo..hi].to_string(),
            span: Span { lo: lo as u32, hi: hi as u32 },
            lines: Lines { start: *first as u32 + 1, end: last as u32 + 1 },
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_demo() {
        let d = defs(include_str!("../../fixture/ui/demo.splash"));
        assert_eq!(d.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), vec!["GizmoView", "spin_label"]);
        assert_eq!(d[0].sig, "let GizmoView = View{");
        assert_eq!(d[0].lines, Lines { start: 3, end: 5 });
        assert_eq!(d[1].lines, Lines { start: 7, end: 9 });
    }

    #[test]
    fn module_paths_and_non_defs() {
        let d = defs("use mod.std.*\nmod.widgets.Button = Base{}\nTheme := {}\nx == y\n  let inner = 1\nci.step(\"a\", fn(){})\n");
        assert_eq!(d.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), vec!["mod.widgets.Button", "Theme"]);
    }
}
