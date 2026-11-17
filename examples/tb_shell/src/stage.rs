//! General N-surface `Stage` model: `stack`, `grid`, and `deck` modes across `1..N` slots,
//! with `pinned` (and `ephemeral`) slots that persist across `front()` navigation.

use makepad_micro_serde::JsonValue;
use std::collections::{BTreeSet, HashMap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageMode {
    Stack,
    Grid,
    Deck,
}

impl StageMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stack => "stack",
            Self::Grid => "grid",
            Self::Deck => "deck",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "grid" => Self::Grid,
            "deck" | "tabs" => Self::Deck,
            _ => Self::Stack,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotMode {
    Fill,
    Compact,
    Header,
}

impl SlotMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fill => "fill",
            Self::Compact => "compact",
            Self::Header => "header",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "compact" | "fit" => Self::Compact,
            "header" | "collapsed" => Self::Header,
            _ => Self::Fill,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StageSlot {
    pub surface: String,
    pub mode: SlotMode,
    pub weight: f64,
    pub px: Option<f64>,
    pub pinned: bool,
    pub ephemeral: bool,
}

impl StageSlot {
    pub fn fill(surface: impl Into<String>) -> Self {
        Self {
            surface: surface.into(),
            mode: SlotMode::Fill,
            weight: 1.0,
            px: None,
            pinned: false,
            ephemeral: false,
        }
    }

    /// Parses a CLI `<spec>` like `"chat"`, `"voice:compact"`, `"battery:header"`,
    /// `"voice:168px"`, `"voice:168p"`, or `"chat:2w"`.
    pub fn parse_spec(spec: &str) -> Option<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        let mut parts = spec.split(':');
        let surface = parts.next()?.trim();
        if surface.is_empty() {
            return None;
        }
        let mut slot = Self::fill(surface);
        for tok in parts {
            let t = tok.trim();
            if t.is_empty() {
                continue;
            }
            if t == "compact" || t == "fit" {
                slot.mode = SlotMode::Compact;
            } else if t == "header" {
                slot.mode = SlotMode::Header;
            } else if t == "fill" {
                slot.mode = SlotMode::Fill;
            } else if t == "pin" || t == "pinned" {
                slot.pinned = true;
            } else if t == "ephemeral" {
                slot.ephemeral = true;
            } else if let Some(num) = t.strip_suffix("px").or_else(|| t.strip_suffix('p')) {
                if let Ok(px) = num.parse::<f64>() {
                    slot.px = Some(px.clamp(36.0, 720.0));
                    if slot.mode == SlotMode::Fill {
                        slot.mode = SlotMode::Compact;
                    }
                }
            } else if let Some(num) = t.strip_suffix('w') {
                if let Ok(w) = num.parse::<f64>() {
                    slot.weight = w.clamp(0.1, 20.0);
                    slot.mode = SlotMode::Fill;
                }
            } else if let Ok(px) = t.parse::<f64>() {
                slot.px = Some(px.clamp(36.0, 720.0));
                slot.mode = SlotMode::Compact;
            }
        }
        Some(slot)
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        if let Some(s) = v.as_str() {
            return Self::parse_spec(s);
        }
        let surface = v.get("surface")?.as_str()?.trim().to_string();
        if surface.is_empty() {
            return None;
        }
        let mode = v.get("mode").and_then(|m| m.as_str()).map(SlotMode::from_str).unwrap_or(SlotMode::Fill);
        let weight = json_num(v.get("weight")).unwrap_or(1.0).clamp(0.1, 20.0);
        let px = json_num(v.get("px")).or_else(|| json_num(v.get("size"))).map(|p| p.clamp(36.0, 720.0));
        let pinned = v.get("pinned") == Some(&JsonValue::Bool(true));
        let ephemeral = v.get("ephemeral") == Some(&JsonValue::Bool(true));
        Some(Self { surface, mode, weight, px, pinned, ephemeral })
    }

    pub fn to_json(&self) -> JsonValue {
        let mut m = HashMap::new();
        m.insert("surface".to_string(), JsonValue::String(self.surface.clone()));
        m.insert("mode".to_string(), JsonValue::String(self.mode.as_str().into()));
        m.insert("weight".to_string(), JsonValue::F64(self.weight));
        if let Some(px) = self.px {
            m.insert("px".to_string(), JsonValue::F64(px));
        }
        if self.pinned {
            m.insert("pinned".to_string(), JsonValue::Bool(true));
        }
        if self.ephemeral {
            m.insert("ephemeral".to_string(), JsonValue::Bool(true));
        }
        JsonValue::Object(m)
    }

    /// The `slot.mode` string exposed inside the Splash isolate (`"full"`, `"compact"`, `"grid"`).
    pub fn prelude_mode(&self, stage_mode: StageMode, total_slots: usize) -> &'static str {
        match stage_mode {
            StageMode::Grid if total_slots > 1 => "grid",
            StageMode::Deck => "full",
            _ => match self.mode {
                SlotMode::Compact => "compact",
                SlotMode::Header => "compact",
                SlotMode::Fill => "full",
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stage {
    pub mode: StageMode,
    pub focus: String,
    pub slots: Vec<StageSlot>,
}

impl Default for Stage {
    fn default() -> Self {
        Self {
            mode: StageMode::Stack,
            focus: String::new(),
            slots: Vec::new(),
        }
    }
}

impl Stage {
    pub fn to_json(&self) -> JsonValue {
        let mut m = HashMap::new();
        m.insert("mode".to_string(), JsonValue::String(self.mode.as_str().into()));
        m.insert("focus".to_string(), JsonValue::String(self.focus.clone()));
        m.insert("slots".to_string(), JsonValue::Array(self.slots.iter().map(StageSlot::to_json).collect()));
        JsonValue::Object(m)
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let mode = v.get("mode").and_then(|m| m.as_str()).map(StageMode::from_str).unwrap_or(StageMode::Stack);
        let slots_arr = v.get("slots")?.as_array()?;
        let mut slots: Vec<StageSlot> = Vec::new();
        for item in slots_arr {
            if let Some(slot) = StageSlot::from_json(item) {
                if !slots.iter().any(|s| s.surface == slot.surface) {
                    slots.push(slot);
                }
            }
        }
        if slots.is_empty() {
            return None;
        }
        let focus = v
            .get("focus")
            .and_then(|f| f.as_str())
            .filter(|f| slots.iter().any(|s| s.surface == *f))
            .map(str::to_string)
            .unwrap_or_else(|| slots.iter().find(|s| !s.pinned).or(slots.first()).unwrap().surface.clone());
        Some(Self { mode, focus, slots })
    }

    /// Surfaces actively drawn right now (empty when `panel_off`, excludes `Header`-collapsed
    /// slots and non-focused `Deck` tabs).
    pub fn visible_surfaces(&self, panel_off: bool) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        if panel_off || self.slots.is_empty() {
            return set;
        }
        match self.mode {
            StageMode::Deck => {
                let active = self
                    .slots
                    .iter()
                    .find(|s| s.surface == self.focus)
                    .or_else(|| self.slots.first());
                if let Some(s) = active {
                    if s.mode != SlotMode::Header {
                        set.insert(s.surface.clone());
                    }
                }
            }
            StageMode::Stack | StageMode::Grid => {
                for s in &self.slots {
                    if s.mode != SlotMode::Header {
                        set.insert(s.surface.clone());
                    }
                }
            }
        }
        set
    }

    /// Applies legacy `front = name` navigation: keeps all `pinned: true` slots in place and
    /// replaces unpinned slots with `{surface: name, mode: Fill}`.
    pub fn apply_front(&mut self, name: &str) {
        if name.is_empty() {
            self.slots.retain(|s| s.pinned);
            self.focus = self.slots.first().map(|s| s.surface.clone()).unwrap_or_default();
            return;
        }
        if self.slots.iter().any(|s| s.surface == name && s.pinned) {
            self.focus = name.to_string();
            return;
        }
        let mut pinned_top: Vec<StageSlot> = self.slots.iter().filter(|s| s.pinned).cloned().collect();
        pinned_top.push(StageSlot::fill(name));
        if self.mode == StageMode::Grid {
            self.mode = StageMode::Stack;
        }
        self.slots = pinned_top;
        self.focus = name.to_string();
    }

    pub fn solo(&mut self, name: &str) {
        self.mode = StageMode::Stack;
        let mut pinned_top: Vec<StageSlot> = self
            .slots
            .iter()
            .filter(|s| s.pinned && s.surface != name)
            .cloned()
            .collect();
        pinned_top.push(StageSlot::fill(name));
        self.slots = pinned_top;
        self.focus = name.to_string();
    }

    pub fn ensure_valid_focus(&mut self, fallback_home: bool) {
        if self.slots.is_empty() && fallback_home {
            self.slots.push(StageSlot::fill("home"));
        }
        if !self.slots.iter().any(|s| s.surface == self.focus) {
            self.focus = self
                .slots
                .iter()
                .find(|s| !s.pinned)
                .or_else(|| self.slots.first())
                .map(|s| s.surface.clone())
                .unwrap_or_default();
        }
    }

    pub fn remove_surface(&mut self, name: &str, fallback_home: bool) -> bool {
        let before = self.slots.len();
        self.slots.retain(|s| s.surface != name);
        let removed = self.slots.len() != before;
        if removed {
            self.ensure_valid_focus(fallback_home);
        }
        removed
    }

    pub fn extra_slot_count(&self) -> usize {
        self.slots.len().saturating_sub(1)
    }
}

pub fn json_num(v: Option<&JsonValue>) -> Option<f64> {
    match v? {
        JsonValue::F64(f) => Some(*f),
        JsonValue::I64(n) => Some(*n as f64),
        JsonValue::U64(n) => Some(*n as f64),
        JsonValue::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}
