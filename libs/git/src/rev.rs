//! Revision resolution, as `git rev-parse --verify` reads a revision:
//!
//! - a full object id, or an abbreviated one (4 or more hex digits) that
//!   names exactly one object across loose objects, packs and alternates;
//! - a ref name by git's lookup rules: the name itself (`HEAD`,
//!   `FETCH_HEAD`, `refs/...`), then `refs/<name>`, `refs/tags/<name>`,
//!   `refs/heads/<name>`, `refs/remotes/<name>` and
//!   `refs/remotes/<name>/HEAD`, through loose refs and `packed-refs`,
//!   following symbolic refs;
//! - `@` for `HEAD`, and `main-worktree/<ref>` and `worktrees/<id>/<ref>`
//!   for another worktree's `HEAD` and private refs;
//! - the suffixes `~N`, `^N` and `^{}`, `^{commit}`, `^{tree}`,
//!   `^{blob}`, `^{tag}`, `^{object}`, in any sequence.
//!
//! A linked worktree resolves its own `HEAD` and private refs, and shares
//! every other ref through the common dir.
//!
//! The object a revision ends at must exist: unlike git, a full id or a ref
//! naming a missing object is an error.
//!
//! Precedence is git's: a full id is an id; otherwise a name that matches a
//! ref is that ref (the first rule that matches), and only a name that
//! matches no ref is read as an abbreviated id. Before `~`, `^N` and
//! `^{commit}` an abbreviated id counts only commit candidates (and tags of
//! commits), and before `^{tree}` only tree-ish ones, as git disambiguates.
//! Anything else that names more than one object is
//! [`GitError::AmbiguousObjectId`], listing every candidate.
//!
//! Reflogs (`@{...}`), ranges (`..`), `<rev>:<path>`, `:/text` and
//! `^{/text}` are refused as unsupported, never approximated. A failure is
//! [`GitError::BadRevision`] naming the step that failed, except I/O and
//! corrupt-object errors, which pass through unchanged.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::GitError;
use crate::object::{Object, ObjectKind};
use crate::oid::{ObjectId, ObjectIdPrefix};
use crate::pack::PackLookup;
use crate::refs::{self, RefTarget};
use crate::repo::Repository;

/// The symbolic-ref depth git follows before giving up.
const MAX_SYMREF_DEPTH: usize = 5;

/// What the step after a name needs, which also narrows the candidates of
/// an abbreviated id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Want {
    Any,
    Commit,
    Tree,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    /// `~N`: the Nth first-parent ancestor.
    Ancestor(u32),
    /// `^N`: the Nth parent; `^0` is the commit itself.
    Parent(u32),
    /// `^{}`: peel tags.
    PeelTags,
    /// `^{object}`: the object itself.
    Object,
    /// `^{commit}`, `^{tree}`, `^{blob}`, `^{tag}`.
    Peel(ObjectKind),
}

impl Step {
    fn want(self) -> Want {
        match self {
            Step::Ancestor(_) | Step::Parent(_) | Step::Peel(ObjectKind::Commit) => Want::Commit,
            Step::Peel(ObjectKind::Tree) => Want::Tree,
            _ => Want::Any,
        }
    }
}

/// A revision split into its name and its steps; each step keeps the text
/// of the revision up to and including it, for errors.
struct Parsed<'a> {
    name: &'a str,
    steps: Vec<(Step, &'a str)>,
}

fn bad(rev: &str, reason: impl Into<String>) -> GitError {
    GitError::BadRevision { rev: rev.to_string(), reason: reason.into() }
}

fn parse(rev: &str) -> Result<Parsed<'_>, GitError> {
    if rev.is_empty() {
        return Err(bad(rev, "empty revision"));
    }
    if rev.contains("@{") {
        return Err(bad(rev, "reflog syntax (@{...}) is not supported"));
    }
    if rev.contains("..") {
        return Err(bad(rev, "ranges (..) are not supported; resolve each end"));
    }
    if rev.contains(':') {
        return Err(bad(rev, "<rev>:<path> and :/text are not supported"));
    }
    if rev.starts_with('-') {
        return Err(bad(rev, "a revision cannot start with '-'"));
    }
    let end = rev.find(['~', '^']).unwrap_or(rev.len());
    let name = &rev[..end];
    if name.is_empty() {
        return Err(bad(rev, "no name before the first '~' or '^'"));
    }
    let bytes = rev.as_bytes();
    let mut steps = Vec::new();
    let mut i = end;
    while i < bytes.len() {
        let op = bytes[i];
        i += 1;
        let step = if op == b'^' && bytes.get(i) == Some(&b'{') {
            let close = rev[i..]
                .find('}')
                .ok_or_else(|| bad(rev, format!("unclosed '^{{' in {:?}", &rev[..i + 1])))?;
            let inner = &rev[i + 1..i + close];
            i += close + 1;
            match inner {
                "" => Step::PeelTags,
                "object" => Step::Object,
                "commit" => Step::Peel(ObjectKind::Commit),
                "tree" => Step::Peel(ObjectKind::Tree),
                "blob" => Step::Peel(ObjectKind::Blob),
                "tag" => Step::Peel(ObjectKind::Tag),
                _ if inner.starts_with('/') => {
                    return Err(bad(rev, "^{/text} message search is not supported"))
                }
                _ => return Err(bad(rev, format!("unknown peel type ^{{{inner}}}"))),
            }
        } else if op == b'~' || op == b'^' {
            let digits = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
            let n = if digits == 0 {
                1
            } else {
                rev[i..i + digits].parse::<u32>().map_err(|_| {
                    bad(rev, format!("count {} is too large", &rev[i..i + digits]))
                })?
            };
            i += digits;
            if op == b'~' {
                Step::Ancestor(n)
            } else {
                Step::Parent(n)
            }
        } else {
            return Err(bad(
                rev,
                format!("unexpected {:?} after {:?}", op as char, &rev[..i - 1]),
            ));
        };
        steps.push((step, &rev[..i]));
    }
    Ok(Parsed { name, steps })
}

/// Why `name` is not a ref name git would accept (`git check-ref-format`
/// with one-level names allowed), or `None`. Ref names become paths under
/// the git dir, so this also keeps them inside it.
fn ref_format_error(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("empty ref name".into());
    }
    if name == "@" {
        return Some("'@' alone is not a ref name".into());
    }
    if let Some(c) = name
        .chars()
        .find(|c| c.is_ascii_control() || " ~^:?*[\\".contains(*c))
    {
        return Some(format!("ref name contains {c:?}"));
    }
    if name.contains("..") || name.contains("@{") {
        return Some("ref name contains '..' or '@{'".into());
    }
    if name.ends_with('.') || name.ends_with('/') || name.starts_with('/') {
        return Some("ref name starts with '/' or ends with '/' or '.'".into());
    }
    for component in name.split('/') {
        if component.is_empty() || component.starts_with('.') || component.ends_with(".lock") {
            return Some(format!(
                "ref name component {component:?} is empty, starts with '.' or ends with '.lock'"
            ));
        }
    }
    None
}

/// `HEAD`, `FETCH_HEAD`, `ORIG_HEAD`, …: names git reads straight from the
/// git dir.
fn is_root_ref_syntax(name: &str) -> bool {
    name.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
}

fn is_full_hex(name: &str) -> bool {
    name.len() == 40 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn tag_target(oid: &ObjectId, tag: &Object) -> Result<ObjectId, GitError> {
    let first = tag.data.split(|b| *b == b'\n').next().unwrap_or_default();
    first
        .strip_prefix(b"object ")
        .and_then(|hex| std::str::from_utf8(hex).ok())
        .ok_or_else(|| GitError::InvalidObject(format!("tag {oid}: no object header")))
        .and_then(ObjectId::from_hex)
}

fn loose_with_prefix(
    objects_dir: &Path,
    prefix: &ObjectIdPrefix,
    out: &mut Vec<ObjectId>,
) -> Result<(), GitError> {
    let hex = prefix.to_hex();
    let entries = match std::fs::read_dir(objects_dir.join(&hex[..2])) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(GitError::Io(e)),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        // Only object files: 38 hex digits. Temporary files are skipped.
        let Some(rest) = name.to_str().filter(|n| n.len() == 38 && n.bytes().all(|b| b.is_ascii_hexdigit())) else {
            continue;
        };
        let oid = ObjectId::from_hex(&format!("{}{}", &hex[..2], rest.to_ascii_lowercase()))?;
        if prefix.matches(&oid) {
            out.push(oid);
        }
    }
    Ok(())
}

impl Repository {
    /// Resolve `rev` to an object id, as `git rev-parse --verify` does
    /// (see the module docs for the syntax). The object must exist.
    pub fn rev_parse(&mut self, rev: &str) -> Result<ObjectId, GitError> {
        let parsed = parse(rev)?;
        let want = parsed.steps.first().map_or(Want::Any, |(step, _)| step.want());
        let mut oid = self.resolve_name(rev, parsed.name, want)?;
        let mut shallow: Option<HashSet<ObjectId>> = None;
        let mut before = parsed.name;
        for (step, upto) in parsed.steps {
            oid = self.apply_step(rev, before, upto, oid, step, &mut shallow)?;
            before = upto;
        }
        match self.has_object(&oid)? {
            true => Ok(oid),
            false => Err(bad(rev, format!("{rev} names {oid}, which does not exist"))),
        }
    }

    /// The ref a short name means, by git's lookup rules (see the module
    /// docs), as `(full name, id)`; `@` is `HEAD`. The full name is the ref
    /// the symbolic chain ends at, as `git rev-parse --symbolic-full-name`
    /// prints it:
    /// `HEAD` on `work` gives `refs/heads/work`; a detached `HEAD` gives
    /// `HEAD`. `Ok(None)` means no rule matched; a matching symbolic ref
    /// whose target is missing is an error.
    pub fn expand_ref(&self, name: &str) -> Result<Option<(String, ObjectId)>, GitError> {
        let name = if name == "@" { "HEAD" } else { name };
        if let Some(reason) = ref_format_error(name) {
            return Err(GitError::InvalidRef(format!("{name:?}: {reason}")));
        }
        let literal = name.starts_with("refs/")
            || is_root_ref_syntax(name)
            || name.starts_with("main-worktree/")
            || name.starts_with("worktrees/");
        let candidates = [
            literal.then(|| name.to_string()),
            Some(format!("refs/{name}")),
            Some(format!("refs/tags/{name}")),
            Some(format!("refs/heads/{name}")),
            Some(format!("refs/remotes/{name}")),
            Some(format!("refs/remotes/{name}/HEAD")),
        ];
        for full in candidates.into_iter().flatten() {
            if let Some(found) = self.read_ref_chain(&full)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// Every object whose id `prefix` abbreviates, across loose objects,
    /// packs and alternates, sorted and without duplicates.
    pub fn objects_with_prefix(
        &mut self,
        prefix: &ObjectIdPrefix,
    ) -> Result<Vec<ObjectId>, GitError> {
        self.ensure_object_sources()?;
        let mut found = Vec::new();
        for pack in self.packs.iter().flatten() {
            pack.find_prefix(prefix, &mut found);
        }
        let primary = self.common_dir.join("objects");
        for dir in std::iter::once(&primary).chain(self.alternates.iter().flatten()) {
            loose_with_prefix(dir, prefix, &mut found)?;
        }
        found.sort_unstable_by_key(|oid| *oid.as_bytes());
        found.dedup();
        Ok(found)
    }

    /// Whether the object exists, loose or packed, here or in an
    /// alternate. Nothing is inflated.
    pub fn has_object(&mut self, oid: &ObjectId) -> Result<bool, GitError> {
        self.ensure_object_sources()?;
        if self.packs.iter().flatten().any(|pack| pack.find_offset(oid).is_some()) {
            return Ok(true);
        }
        let (dir, file) = oid.loose_path_components();
        let primary = self.common_dir.join("objects");
        Ok(std::iter::once(&primary)
            .chain(self.alternates.iter().flatten())
            .any(|objects| objects.join(&dir).join(&file).is_file()))
    }

    /// Route a full ref name to the private dir it is read from:
    /// `main-worktree/<private ref>` and `worktrees/<id>/<private ref>`
    /// name another worktree's; anything else is this worktree's.
    fn ref_location<'a>(&self, full: &'a str) -> (PathBuf, &'a str) {
        if let Some(rest) = full.strip_prefix("main-worktree/") {
            if refs::is_private_ref(rest) {
                return (self.common_dir.clone(), rest);
            }
        }
        if let Some((id, rest)) = full.strip_prefix("worktrees/").and_then(|r| r.split_once('/')) {
            if refs::is_private_ref(rest) {
                return (self.common_dir.join("worktrees").join(id), rest);
            }
        }
        (self.git_dir.clone(), full)
    }

    /// Read `full` and follow its symbolic chain. `Ok(None)` when `full`
    /// does not exist; a dangling or over-deep chain is an error.
    fn read_ref_chain(&self, full: &str) -> Result<Option<(String, ObjectId)>, GitError> {
        let (git_dir, local) = self.ref_location(full);
        let Some(mut target) = refs::read_ref_in(&git_dir, &self.common_dir, local)? else {
            return Ok(None);
        };
        let mut name = full.to_string();
        for _ in 0..=MAX_SYMREF_DEPTH {
            let next = match target {
                RefTarget::Direct(oid) => return Ok(Some((name, oid))),
                RefTarget::Symbolic(next) => next,
            };
            if let Some(reason) = ref_format_error(&next) {
                return Err(GitError::InvalidRef(format!(
                    "{name} is a symbolic ref to {next:?}: {reason}"
                )));
            }
            target = refs::read_ref_in(&git_dir, &self.common_dir, &next)?.ok_or_else(|| {
                GitError::RefNotFound(format!("{next} (the target of symbolic ref {name})"))
            })?;
            name = next;
        }
        Err(GitError::InvalidRef(format!(
            "{full}: more than {MAX_SYMREF_DEPTH} levels of symbolic refs"
        )))
    }

    fn resolve_name(&mut self, rev: &str, name: &str, want: Want) -> Result<ObjectId, GitError> {
        if is_full_hex(name) {
            return ObjectId::from_hex(&name.to_ascii_lowercase());
        }
        let found = self.expand_ref(name).map_err(|e| match e {
            GitError::RefNotFound(what) | GitError::InvalidRef(what) => {
                bad(rev, format!("ref {name}: {what}"))
            }
            other => other,
        })?;
        if let Some((_, oid)) = found {
            return Ok(oid);
        }
        let is_hex = name.bytes().all(|b| b.is_ascii_hexdigit());
        if is_hex && name.len() >= ObjectIdPrefix::MIN_DIGITS {
            return self.resolve_prefix(rev, &ObjectIdPrefix::from_hex(name)?, want);
        }
        let mut reason = format!(
            "no ref named {name} (tried {}refs/{name}, refs/tags/{name}, refs/heads/{name}, \
             refs/remotes/{name}, refs/remotes/{name}/HEAD)",
            if name.starts_with("refs/") || is_root_ref_syntax(name) { format!("{name}, ") } else { String::new() }
        );
        if is_hex {
            reason.push_str(&format!(
                "; an abbreviated id needs at least {} hex digits",
                ObjectIdPrefix::MIN_DIGITS
            ));
        }
        Err(bad(rev, reason))
    }

    fn resolve_prefix(&mut self, rev: &str, prefix: &ObjectIdPrefix, want: Want) -> Result<ObjectId, GitError> {
        let found = self.objects_with_prefix(prefix)?;
        match found.len() {
            0 => return Err(bad(rev, format!("no ref named {prefix} and no object id starting with it"))),
            1 => return Ok(found[0]),
            _ => {}
        }
        let mut candidates = Vec::with_capacity(found.len());
        let mut fitting = Vec::new();
        for oid in found {
            let kind = self.read_object(&oid)?.kind;
            if want != Want::Any && self.peels_to(oid, want)? {
                fitting.push(oid);
            }
            candidates.push((oid, kind));
        }
        if fitting.len() == 1 {
            return Ok(fitting[0]);
        }
        Err(GitError::AmbiguousObjectId { prefix: prefix.to_hex(), candidates })
    }

    /// Whether `oid` is commit-ish (`Want::Commit`) or tree-ish
    /// (`Want::Tree`): itself, or a tag that peels to one.
    fn peels_to(&mut self, mut oid: ObjectId, want: Want) -> Result<bool, GitError> {
        loop {
            let object = self.read_object(&oid)?;
            match object.kind {
                ObjectKind::Tag => oid = tag_target(&oid, &object)?,
                ObjectKind::Commit => return Ok(true),
                ObjectKind::Tree => return Ok(want == Want::Tree),
                ObjectKind::Blob => return Ok(false),
            }
        }
    }

    /// Read an object a revision step reached; a missing one is a bad
    /// revision naming the step.
    fn read_step(&mut self, rev: &str, at: &str, oid: &ObjectId) -> Result<Object, GitError> {
        self.read_object(oid).map_err(|e| match e {
            GitError::ObjectNotFound(_) => bad(rev, format!("{at} names {oid}, which is missing")),
            other => other,
        })
    }

    /// Peel tags from `oid` until a `kind` object: a tree is also reached
    /// through a commit. Anything else is an error naming what was found.
    fn peel(&mut self, rev: &str, at: &str, mut oid: ObjectId, kind: Option<ObjectKind>) -> Result<ObjectId, GitError> {
        loop {
            let object = self.read_step(rev, at, &oid)?;
            if Some(object.kind) == kind {
                return Ok(oid);
            }
            match (object.kind, kind) {
                (ObjectKind::Tag, _) => oid = tag_target(&oid, &object)?,
                (_, None) => return Ok(oid),
                (ObjectKind::Commit, Some(ObjectKind::Tree)) => {
                    oid = crate::commit::parse_commit(&object.data)?.tree;
                }
                (found, Some(kind)) => {
                    return Err(bad(rev, format!(
                        "{at} is {} {oid}, which does not peel to a {}",
                        found.as_str(),
                        kind.as_str()
                    )));
                }
            }
        }
    }

    fn apply_step(
        &mut self,
        rev: &str,
        before: &str,
        upto: &str,
        oid: ObjectId,
        step: Step,
        shallow: &mut Option<HashSet<ObjectId>>,
    ) -> Result<ObjectId, GitError> {
        match step {
            Step::Object => Ok(oid),
            Step::PeelTags => self.peel(rev, before, oid, None),
            Step::Peel(ObjectKind::Tag) => match self.read_step(rev, before, &oid)?.kind {
                ObjectKind::Tag => Ok(oid),
                found => Err(bad(rev, format!("{before} is {} {oid}, not a tag", found.as_str()))),
            },
            Step::Peel(kind) => self.peel(rev, before, oid, Some(kind)),
            Step::Parent(0) => self.peel(rev, before, oid, Some(ObjectKind::Commit)),
            Step::Parent(n) => {
                let commit_oid = self.peel(rev, before, oid, Some(ObjectKind::Commit))?;
                let parents = self.parents(rev, before, &commit_oid, shallow)?;
                parents.get(n as usize - 1).copied().ok_or_else(|| {
                    bad(rev, format!(
                        "{upto}: commit {commit_oid} ({before}) has {} parent(s)",
                        parents.len()
                    ))
                })
            }
            Step::Ancestor(n) => {
                let mut current = self.peel(rev, before, oid, Some(ObjectKind::Commit))?;
                for walked in 0..n {
                    let parents = self.parents(rev, before, &current, shallow)?;
                    current = *parents.first().ok_or_else(|| {
                        bad(rev, format!(
                            "{upto}: {before}~{walked} is commit {current}, which has no parent{}",
                            if shallow.as_ref().is_some_and(|s| s.contains(&current)) {
                                " in this shallow clone"
                            } else {
                                ""
                            }
                        ))
                    })?;
                }
                Ok(current)
            }
        }
    }

    /// A commit's parents; a shallow boundary has none, as for git.
    fn parents(
        &mut self,
        rev: &str,
        at: &str,
        oid: &ObjectId,
        shallow: &mut Option<HashSet<ObjectId>>,
    ) -> Result<Vec<ObjectId>, GitError> {
        if shallow.is_none() {
            *shallow = Some(self.shallow_boundary()?);
        }
        if shallow.as_ref().is_some_and(|s| s.contains(oid)) {
            return Ok(Vec::new());
        }
        let object = self.read_step(rev, at, oid)?;
        Ok(crate::commit::parse_commit(&object.data)?.parents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_and_steps() {
        let parsed = parse("upstream/work~2^2^{tree}").unwrap();
        assert_eq!(parsed.name, "upstream/work");
        let steps: Vec<_> = parsed.steps.iter().map(|(s, t)| (*s, *t)).collect();
        assert_eq!(steps, vec![
            (Step::Ancestor(2), "upstream/work~2"),
            (Step::Parent(2), "upstream/work~2^2"),
            (Step::Peel(ObjectKind::Tree), "upstream/work~2^2^{tree}"),
        ]);
        let steps: Vec<_> = parse("v1^{}~^0^").unwrap().steps.iter().map(|(s, _)| *s).collect();
        assert_eq!(steps, vec![Step::PeelTags, Step::Ancestor(1), Step::Parent(0), Step::Parent(1)]);
    }

    #[test]
    fn refuses_unsupported_and_malformed_syntax() {
        for rev in [
            "", "~1", "HEAD@{1}", "@{-1}", "a..b", "HEAD:path", ":/msg", "HEAD^{/msg}",
            "HEAD^{unknown}", "HEAD^{tree", "HEAD~x", "-HEAD", "HEAD~99999999999",
        ] {
            assert!(matches!(parse(rev), Err(GitError::BadRevision { .. })), "{rev:?}");
        }
    }

    #[test]
    fn ref_name_rules() {
        for ok in ["HEAD", "main", "upstream/work", "refs/heads/a-b_c.d", "v1.0"] {
            assert_eq!(ref_format_error(ok), None, "{ok}");
        }
        for bad in ["", "@", "a..b", "/a", "a/", "a.", "a//b", ".a", "a/.b", "a.lock", "a b", "a*", "a\\b"] {
            assert!(ref_format_error(bad).is_some(), "{bad:?}");
        }
    }
}
