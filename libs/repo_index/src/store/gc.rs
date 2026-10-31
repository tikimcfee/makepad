//! Removes bases no live worktree's merge-base names (keeping the newest for upstream) and
//! segments no recent manifest references, after a grace period; a delete that fails is
//! retried later and logged. A notes circle removed from `circles.conf` loses its segments (and
//! the record text they keep) with it.
//!
//! It sweeps one repo's `repo-<id>/` and the machine's notes circles (`<circle>/` at the root),
//! never another repo's directory: that repo's live worktrees are not known here.
//!
//! Per circle directory:
//! - a base not in `live_bases` goes once its last use (`.used`, else its pointer's time) is
//!   older than the grace period: pointer first, so no reader is sent to a missing segment;
//! - manifests: the newest valid one and those younger than the grace period stay;
//! - a segment no kept pointer or manifest names goes once it is older than the grace period
//!   (a segment written but not yet committed is young);
//! - tmp files of crashed writers and unheld lock files go after the grace period;
//! - a manifest circle not in `live_circles` (no longer configured) goes whole, segments and
//!   record text included, once its newest manifest is older than the grace period.
//!
//! On Windows a mapped file cannot be deleted; the failure is logged, reported as deferred,
//! and the next gc tries again (nothing ever renames over it).

use super::manifest;
use super::{age, base_name, io_err, log, FsStore};
use crate::{CircleName, Error, GcReport, RepoId, SegmentKey};
use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn run(store: &FsStore, repo: RepoId, live_bases: &[SegmentKey], live_circles: &[(Option<RepoId>, CircleName)]) -> Result<GcReport, Error> {
    let live: HashSet<String> = live_bases.iter().map(base_name).collect();
    let configured: HashSet<PathBuf> = live_circles.iter().map(|(r, c)| store.circle_dir(*r, c)).collect::<Result<_, _>>()?;
    let mut report = GcReport::default();
    for dir in circle_dirs(store.root(), repo)? {
        let manifests = manifest::numbers(&dir)?;
        if !manifests.is_empty() && !configured.contains(&dir) {
            drop_circle(&dir, manifests[0], store.grace(), &mut report)?;
        } else {
            sweep(&dir, &live, store.grace(), &mut report)?;
        }
    }
    Ok(report)
}

/// A manifest circle no longer configured: everything the store wrote there, newest manifest
/// last (a reader never finds a manifest naming removed segments for long), then the directory.
fn drop_circle(dir: &Path, newest: u64, grace: Duration, report: &mut GcReport) -> Result<(), Error> {
    let newest_path = manifest::path_of(dir, newest);
    if !older(&newest_path, grace) {
        report.deferred.push((dir.to_path_buf(), format!("not configured; newest manifest {}: within the grace period", age_text(&newest_path))));
        return Ok(());
    }
    let mut all_gone = true;
    for (name, path, is_dir) in list(dir)? {
        let ours = name.starts_with("manifest-") || (name.starts_with("seg-") && name.ends_with(".mpx")) || (name.starts_with("tmp-") && name.ends_with(".tmp"));
        if is_dir || !ours || path == newest_path {
            all_gone &= !is_dir && ours;
            continue;
        }
        all_gone &= remove(&path, report);
    }
    if all_gone && remove(&newest_path, report) {
        if let Err(e) = fs::remove_dir(dir) {
            log(&format!("could not remove {} ({e}); it holds files the index did not write", dir.display()));
            report.deferred.push((dir.to_path_buf(), format!("remove failed: {e}")));
        } else {
            report.removed.push(dir.to_path_buf());
        }
    }
    Ok(())
}

fn list(dir: &Path) -> Result<Vec<(String, PathBuf, bool)>, Error> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(dir, "list", e)),
    };
    let mut out = Vec::new();
    for e in rd {
        let e = e.map_err(|e| io_err(dir, "list", e))?;
        let is_dir = e.file_type().map_err(|err| io_err(&e.path(), "stat", err))?.is_dir();
        if let Ok(name) = e.file_name().into_string() {
            out.push((name, e.path(), is_dir));
        }
    }
    out.sort();
    Ok(out)
}

/// `repo-<repo>/<circle>/` and the notes circles' `<circle>/` under the root.
fn circle_dirs(root: &Path, repo: RepoId) -> Result<Vec<PathBuf>, Error> {
    let mine = format!("repo-{:016x}", repo.0);
    let mut out = Vec::new();
    for (name, path, is_dir) in list(root)? {
        if !is_dir {
            continue;
        }
        if name == mine {
            out.extend(list(&path)?.into_iter().filter(|e| e.2).map(|e| e.1));
        } else if !name.starts_with("repo-") {
            out.push(path);
        }
    }
    Ok(out)
}

fn older(path: &Path, grace: Duration) -> bool {
    age(path).is_some_and(|a| a >= grace)
}

fn age_text(path: &Path) -> String {
    age(path).map_or("unknown age".into(), |a| format!("{}s old", a.as_secs()))
}

/// Removes a file; a failure (on Windows: still mapped or open) is logged and deferred to the
/// next gc.
fn remove(path: &Path, report: &mut GcReport) -> bool {
    match fs::remove_file(path) {
        Ok(()) => {
            report.removed.push(path.to_path_buf());
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            log(&format!("could not remove {} ({e}); the next gc retries", path.display()));
            report.deferred.push((path.to_path_buf(), format!("remove failed: {e}")));
            false
        }
    }
}

fn sweep(dir: &Path, live: &HashSet<String>, grace: Duration, report: &mut GcReport) -> Result<(), Error> {
    let files: Vec<(String, PathBuf)> = list(dir)?.into_iter().filter(|e| !e.2).map(|e| (e.0, e.1)).collect();
    let mut referenced: HashSet<String> = HashSet::new();

    // bases
    for (name, path) in files.iter().filter(|(n, _)| is_pointer(n)) {
        let used = dir.join(format!("{name}.used"));
        let last_use = [age(path), age(&used)].into_iter().flatten().min();
        let keep = live.contains(name) || last_use.is_none_or(|a| a < grace);
        if keep {
            if !live.contains(name) {
                report.deferred.push((path.clone(), format!("not live, last used {}s ago: within the grace period", last_use.map_or(0, |a| a.as_secs()))));
            }
            match fs::read_to_string(path) {
                Ok(id) => {
                    referenced.insert(id.trim().to_string());
                }
                Err(e) => log(&format!("could not read {} ({e}); its segment is kept by age only", path.display())),
            }
        } else if remove(path, report) {
            remove(&used, report);
        } else {
            // the pointer stays, so its segment does too
            if let Ok(id) = fs::read_to_string(path) {
                referenced.insert(id.trim().to_string());
            }
        }
    }

    // manifests
    let numbers = manifest::numbers(dir)?;
    if !numbers.is_empty() {
        let newest_valid = numbers.iter().copied().find(|&n| match fs::read_to_string(manifest::path_of(dir, n)) {
            Ok(t) => manifest::decode(&t).is_ok_and(|e| e.manifest.version == n),
            Err(_) => false,
        });
        for n in numbers {
            let path = manifest::path_of(dir, n);
            let young = !older(&path, grace);
            let keep = Some(n) == newest_valid || young || newest_valid.is_some_and(|v| n > v);
            if keep {
                if let Ok(entry) = fs::read_to_string(&path).map_err(|_| ()).and_then(|t| manifest::decode(&t).map_err(|_| ())) {
                    referenced.extend(entry.segments.into_iter().map(|s| s.id.0));
                }
            } else {
                remove(&path, report);
            }
        }
    }

    // segments, tmp files, locks
    for (name, path) in &files {
        if name.starts_with("seg-") && name.ends_with(".mpx") {
            if referenced.contains(name) {
                continue;
            }
            if older(path, grace) {
                remove(path, report);
            } else {
                report.deferred.push((path.clone(), format!("unreferenced, {}: within the grace period", age_text(path))));
            }
        } else if name.starts_with("tmp-") && name.ends_with(".tmp") {
            if older(path, grace) {
                remove(path, report);
            }
        } else if let Some(base) = name.strip_suffix(".lock") {
            if !older(path, grace) {
                continue;
            }
            let Ok(file) = File::options().read(true).write(true).open(path) else { continue };
            match file.try_lock() {
                Ok(()) => {
                    remove(path, report);
                }
                Err(_) => report.deferred.push((path.clone(), format!("held: {base} is being built"))),
            }
        } else if let Some(base) = name.strip_suffix(".used") {
            if !files.iter().any(|(n, _)| n == base) && older(path, grace) {
                remove(path, report);
            }
        }
    }
    Ok(())
}

fn is_pointer(name: &str) -> bool {
    name.strip_prefix("base-").is_some_and(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}
