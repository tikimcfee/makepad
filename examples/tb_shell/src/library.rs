//! Live `ui/lib` (`base.splash` + system pieces in `/usr/share/tinybot/ui/lib/`),
//! user reusable Splash library (`~/.local/share/tb-shell/lib/`), saved agent screens
//! (`~/.local/share/tb-shell/screens/`), and 60-second crash-loop guard.

use makepad_micro_serde::{DeJson, JsonValue, SerJson};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_UI_ROOT: &str = "/usr/share/tinybot/ui";

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn data_root() -> PathBuf {
    if let Some(d) = env_nonempty("TB_SHELL_DATA_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = env_nonempty("XDG_DATA_HOME") {
        return PathBuf::from(d).join("tb-shell");
    }
    if let Some(home) = env_nonempty("HOME") {
        return PathBuf::from(home).join(".local/share/tb-shell");
    }
    PathBuf::from("/tmp/tb-shell-data")
}

/// Resolves a live UI subdirectory (`views`, `trusted`, `lib`):
/// 1. Explicit `env_var` if set;
/// 2. `$TB_SHELL_UI_DIR/<sub_name>` or `/usr/share/tinybot/ui/<sub_name>` if that directory exists;
/// 3. `<bundle_or_exe_dir>/<sub_name>` fallback.
pub fn resolve_ui_subdir(env_var: &str, sub_name: &str) -> Option<PathBuf> {
    if let Some(custom) = env_nonempty(env_var) {
        return Some(PathBuf::from(custom));
    }
    let ui_root = env_nonempty("TB_SHELL_UI_DIR").unwrap_or_else(|| DEFAULT_UI_ROOT.to_string());
    let live = PathBuf::from(ui_root).join(sub_name);
    if live.is_dir() {
        return Some(live);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let direct = parent.join(sub_name);
            if direct.is_dir() {
                return Some(direct);
            }
            if let Some(grand) = parent.parent() {
                let bundled = grand.join(sub_name);
                if bundled.is_dir() {
                    return Some(bundled);
                }
            }
        }
    }
    None
}

pub fn valid_piece_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else { return false };
    name.len() <= 64
        && name != "base"
        && (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// True if `ident` appears as a whole identifier token in `text`.
pub fn ident_in(text: &str, ident: &str) -> bool {
    if ident.is_empty() {
        return false;
    }
    let b = text.as_bytes();
    let is_id = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut at = 0;
    while let Some(off) = text[at..].find(ident) {
        let i = at + off;
        let end = i + ident.len();
        let before_ok = i == 0 || !is_id(b[i - 1]);
        let after_ok = end >= b.len() || !is_id(b[end]);
        if before_ok && after_ok {
            return true;
        }
        at = i + 1;
    }
    false
}

/// Extracts a one-line description from line 1 of a Splash library file (`// ...`).
pub fn first_line_desc(body: &str) -> String {
    let first = body.lines().next().unwrap_or("").trim();
    if let Some(rest) = first.strip_prefix("//") {
        rest.trim().to_string()
    } else if let Some(rest) = first.strip_prefix("/*").and_then(|s| s.strip_suffix("*/")) {
        rest.trim().to_string()
    } else {
        String::new()
    }
}

fn ensure_trailing_newline(s: &str) -> String {
    if s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

fn atomic_write(dir: &Path, filename: &str, content: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".{filename}.tmp.{}", std::process::id()));
    let dst = dir.join(filename);
    std::fs::write(&tmp, content).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &dst).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {}: {e}", dst.display())
    })
}

#[derive(Default)]
pub struct StorePersistence {
    pub lib_dir: PathBuf,
    pub screens_dir: PathBuf,
    pub workspaces_dir: PathBuf,
    /// Active `base.splash` + any extra `/usr/share/tinybot/ui/lib/*.splash` pieces.
    pub sys_base_source: String,
    /// Sorted user pieces `(name, normalized_source_with_trailing_newline)` from `~/.local/share/tb-shell/lib/`.
    pub pieces: Vec<(String, String)>,
    /// Last-used unix epoch seconds per piece name.
    pub used: HashMap<String, u64>,
    /// Optional human title per saved screen name.
    pub titles: HashMap<String, String>,
}

impl StorePersistence {
    pub fn load(compiled_base: &str) -> Self {
        let root = data_root();
        let lib_dir = env_nonempty("TB_SHELL_LIB_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("lib"));
        let screens_dir = env_nonempty("TB_SHELL_SCREENS_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("screens"));
        let workspaces_dir = env_nonempty("TB_SHELL_WORKSPACES_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("workspaces"));
        let mut me = Self {
            lib_dir,
            screens_dir,
            workspaces_dir,
            sys_base_source: ensure_trailing_newline(compiled_base),
            pieces: Vec::new(),
            used: HashMap::new(),
            titles: HashMap::new(),
        };
        me.reload_all(compiled_base);
        me.load_used();
        me.load_titles();
        me
    }

    /// Reloads `base.splash`, system `/usr/share/tinybot/ui/lib/*.splash`, and user `~/.local/share/tb-shell/lib/*.splash`.
    pub fn reload_all(&mut self, compiled_base: &str) {
        let ui_lib = resolve_ui_subdir("TB_SHELL_UI_LIB", "lib");
        let base_text = env_nonempty("TB_SHELL_BASE")
            .and_then(|p| std::fs::read_to_string(p).ok())
            .or_else(|| ui_lib.as_ref().and_then(|d| std::fs::read_to_string(d.join("base.splash")).ok()))
            .unwrap_or_else(|| compiled_base.to_string());
        let mut sys_src = ensure_trailing_newline(&base_text);

        if let Some(dir) = ui_lib {
            let mut sys_paths: Vec<PathBuf> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().map_or(false, |x| x == "splash"))
                .collect();
            sys_paths.sort();
            for p in sys_paths {
                let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
                if stem == "base" {
                    continue;
                }
                if let Ok(body) = std::fs::read_to_string(&p) {
                    sys_src.push_str(&ensure_trailing_newline(&body));
                }
            }
        }
        self.sys_base_source = sys_src;
        self.reload_lib();
    }

    pub fn reload_lib(&mut self) {
        self.pieces.clear();
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&self.lib_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |x| x == "splash"))
            .collect();
        paths.sort();
        for p in paths {
            let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
            if !valid_piece_name(&stem) {
                continue;
            }
            if let Ok(body) = std::fs::read_to_string(&p) {
                self.pieces.push((stem, ensure_trailing_newline(&body)));
            }
        }
    }

    fn load_used(&mut self) {
        let Ok(text) = std::fs::read_to_string(self.lib_dir.join(".used")) else { return };
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                if let Ok(ts) = v.trim().parse::<u64>() {
                    self.used.insert(k.trim().to_string(), ts);
                }
            }
        }
    }

    fn save_used(&self) {
        if !self.lib_dir.exists() && self.used.is_empty() {
            return;
        }
        let mut keys: Vec<&String> = self.used.keys().collect();
        keys.sort();
        let text: String = keys.into_iter().map(|k| format!("{k}={}\n", self.used[k])).collect();
        let _ = atomic_write(&self.lib_dir, ".used", &text);
    }

    fn load_titles(&mut self) {
        let Ok(text) = std::fs::read_to_string(self.screens_dir.join(".titles.json")) else { return };
        if let Ok(JsonValue::Object(map)) = JsonValue::deserialize_json(&text) {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    self.titles.insert(k, s.to_string());
                }
            }
        }
    }

    fn save_titles(&self) {
        if !self.screens_dir.exists() && self.titles.is_empty() {
            return;
        }
        let map: HashMap<String, JsonValue> = self.titles.iter().map(|(k, v)| (k.clone(), JsonValue::String(v.clone()))).collect();
        let text = JsonValue::Object(map).serialize_json();
        let _ = atomic_write(&self.screens_dir, ".titles.json", &text);
    }

    pub fn base_and_lib_source(&self) -> String {
        let mut out = self.sys_base_source.clone();
        for (_, src) in &self.pieces {
            out.push_str(src);
        }
        out
    }

    pub fn base_and_lib_except(&self, skip: &str) -> String {
        let mut out = self.sys_base_source.clone();
        for (name, src) in &self.pieces {
            if name != skip {
                out.push_str(src);
            }
        }
        out
    }

    pub fn base_and_lib_lines(&self) -> usize {
        self.sys_base_source.lines().count() + self.pieces.iter().map(|(_, src)| src.lines().count()).sum::<usize>()
    }

    pub fn record_usage(&mut self, body: &str) {
        let now = now_unix();
        let mut dirty = false;
        for (name, _) in &self.pieces {
            if ident_in(body, name) {
                self.used.insert(name.clone(), now);
                dirty = true;
            }
        }
        if dirty {
            self.save_used();
        }
    }

    pub fn put_piece(&mut self, name: &str, body: &str) -> Result<(), String> {
        if !valid_piece_name(name) {
            return Err("a library piece name is 1..64 of A-Z a-z 0-9 _ - (starting with a letter or _, not \"base\")".into());
        }
        let norm = ensure_trailing_newline(body);
        atomic_write(&self.lib_dir, &format!("{name}.splash"), &norm)?;
        match self.pieces.iter_mut().find(|(n, _)| n == name) {
            Some((_, slot)) => *slot = norm,
            None => {
                self.pieces.push((name.to_string(), norm));
                self.pieces.sort_by(|a, b| a.0.cmp(&b.0));
            }
        }
        Ok(())
    }

    pub fn rm_piece(&mut self, name: &str) -> Result<(), String> {
        let path = self.lib_dir.join(format!("{name}.splash"));
        if !path.exists() {
            return Err(format!("no library piece {name}"));
        }
        std::fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
        self.pieces.retain(|(n, _)| n != name);
        self.used.remove(name);
        self.save_used();
        Ok(())
    }

    pub fn cat_piece(&self, name: &str) -> Option<&str> {
        self.pieces.iter().find(|(n, _)| n == name).map(|(_, s)| s.as_str())
    }

    pub fn ls_pieces(&self, unused_days: Option<u64>) -> Vec<JsonValue> {
        let now = now_unix();
        let mut out = Vec::new();
        for (name, src) in &self.pieces {
            let last = self.used.get(name).copied();
            let age_secs = last.map(|t| now.saturating_sub(t));
            if let Some(days) = unused_days {
                let threshold = days.saturating_mul(86_400);
                let is_unused = match age_secs {
                    None => true,
                    Some(age) => age >= threshold,
                };
                if !is_unused {
                    continue;
                }
            }
            let mut m = HashMap::new();
            m.insert("name".into(), JsonValue::String(name.clone()));
            m.insert("description".into(), JsonValue::String(first_line_desc(src)));
            m.insert("last_used".into(), last.map_or(JsonValue::Null, |t| JsonValue::U64(t)));
            m.insert("age_secs".into(), age_secs.map_or(JsonValue::Null, |a| JsonValue::U64(a)));
            out.push(JsonValue::Object(m));
        }
        out
    }

    pub fn save_screen(&mut self, surface: &str, body: &str, title: Option<&str>) -> Result<(), String> {
        let norm = ensure_trailing_newline(body);
        atomic_write(&self.screens_dir, &format!("{surface}.splash"), &norm)?;
        if let Some(t) = title.filter(|t| !t.trim().is_empty()) {
            self.titles.insert(surface.to_string(), t.trim().to_string());
            self.save_titles();
        }
        Ok(())
    }

    pub fn unsave_screen(&mut self, surface: &str) -> Result<bool, String> {
        let path = self.screens_dir.join(format!("{surface}.splash"));
        let existed = path.exists();
        if existed {
            std::fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
        }
        if self.titles.remove(surface).is_some() {
            self.save_titles();
        }
        Ok(existed)
    }

    pub fn is_saved(&self, surface: &str) -> bool {
        self.screens_dir.join(format!("{surface}.splash")).exists()
    }

    pub fn title_of(&self, surface: &str) -> Option<&str> {
        self.titles.get(surface).map(String::as_str)
    }

    pub fn saved_screen_names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.screens_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |x| x == "splash"))
            .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
            .collect();
        names.sort();
        names
    }

    pub fn load_saved_screens(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for name in self.saved_screen_names() {
            let p = self.screens_dir.join(format!("{name}.splash"));
            if let Ok(body) = std::fs::read_to_string(&p) {
                out.push((name, body));
            }
        }
        out
    }

    pub fn save_workspace(&self, name: &str, title: Option<&str>, stage: &JsonValue) -> Result<(), String> {
        let mut m = HashMap::new();
        m.insert("name".to_string(), JsonValue::String(name.to_string()));
        m.insert(
            "title".to_string(),
            JsonValue::String(title.filter(|s| !s.trim().is_empty()).unwrap_or(name).trim().to_string()),
        );
        m.insert("stage".to_string(), stage.clone());
        let text = JsonValue::Object(m).serialize_json() + "\n";
        atomic_write(&self.workspaces_dir, &format!("{name}.json"), &text)
    }

    /// The floating layers (`stage float`) outlive a restart: one file beside the workspaces.
    pub fn save_layers(&self, layers: &JsonValue) -> Result<(), String> {
        let dir = self.workspaces_dir.parent().unwrap_or(&self.workspaces_dir);
        atomic_write(dir, "layers.json", &(layers.serialize_json() + "\n"))
    }

    /// `stage headers on|off`, kept across restarts (absent: off).
    pub fn save_headers(&self, on: bool) {
        let dir = self.workspaces_dir.parent().unwrap_or(&self.workspaces_dir);
        let _ = atomic_write(dir, "stage-headers", if on { "on\n" } else { "off\n" });
    }

    pub fn load_headers(&self) -> bool {
        let dir = self.workspaces_dir.parent().unwrap_or(&self.workspaces_dir);
        std::fs::read_to_string(dir.join("stage-headers")).map_or(false, |t| t.trim() == "on")
    }

    pub fn load_layers(&self) -> Option<JsonValue> {
        let dir = self.workspaces_dir.parent().unwrap_or(&self.workspaces_dir);
        let text = std::fs::read_to_string(dir.join("layers.json")).ok()?;
        JsonValue::deserialize_json(text.trim()).ok()
    }

    pub fn unsave_workspace(&self, name: &str) -> Result<bool, String> {
        let path = self.workspaces_dir.join(format!("{name}.json"));
        let existed = path.exists();
        if existed {
            std::fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
        }
        Ok(existed)
    }

    pub fn load_workspace(&self, name: &str) -> Option<JsonValue> {
        let path = self.workspaces_dir.join(format!("{name}.json"));
        let text = std::fs::read_to_string(path).ok()?;
        let parsed = JsonValue::deserialize_json(text.trim()).ok()?;
        parsed.get("stage").cloned()
    }

    pub fn ls_workspaces(&self) -> Vec<JsonValue> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&self.workspaces_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |x| x == "json"))
            .collect();
        paths.sort();
        let mut out = Vec::new();
        for p in paths {
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            let Ok(parsed) = JsonValue::deserialize_json(text.trim()) else { continue };
            let Some(name) = parsed.get("name").and_then(|v| v.as_str()) else { continue };
            let title = parsed.get("title").and_then(|v| v.as_str()).unwrap_or(name);
            let Some(st) = parsed.get("stage") else { continue };
            let mut item = HashMap::new();
            item.insert("name".into(), JsonValue::String(name.to_string()));
            item.insert("title".into(), JsonValue::String(title.to_string()));
            item.insert("mode".into(), st.get("mode").cloned().unwrap_or_else(|| "stack".into()));
            item.insert("slots".into(), st.get("slots").cloned().unwrap_or_else(|| JsonValue::Array(vec![])));
            out.push(JsonValue::Object(item));
        }
        out
    }
}

/// Tracks unclean exits across restarts in `$XDG_RUNTIME_DIR` so >= 2 crashes within
/// 60 seconds skip saved agent screens and report them in the strip.
pub struct CrashGuard;

impl CrashGuard {
    fn runtime_file(suffix: &str) -> Option<PathBuf> {
        env_nonempty("TB_SHELL_RUNTIME_DIR")
            .or_else(|| env_nonempty("XDG_RUNTIME_DIR"))
            .map(|d| PathBuf::from(d).join(format!("tb-shell.{suffix}")))
    }

    pub fn check_on_start() -> (bool, usize) {
        let Some(running) = Self::runtime_file("running") else { return (false, 0) };
        let Some(crashes_file) = Self::runtime_file("crashes") else { return (false, 0) };
        let now = now_unix();
        let prev_unclean = running.exists();
        let mut stamps: Vec<u64> = std::fs::read_to_string(&crashes_file)
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|s| s.parse::<u64>().ok())
            .filter(|&t| now.saturating_sub(t) <= 60)
            .collect();
        if prev_unclean {
            stamps.push(now);
        }
        let body = stamps.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(" ");
        let _ = std::fs::write(&crashes_file, body);
        let _ = std::fs::write(&running, format!("{}", std::process::id()));
        (prev_unclean, stamps.len())
    }

    pub fn mark_clean_shutdown() {
        if let Some(running) = Self::runtime_file("running") {
            let _ = std::fs::remove_file(running);
        }
    }
}
