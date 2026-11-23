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

/// Where a floating layer sits in the stage viewport (below the strip).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Top,
    Center,
    Bottom,
}

impl Place {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Top => "top",
            Self::Center => "center",
            Self::Bottom => "bottom",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "center" | "middle" => Self::Center,
            "bottom" => Self::Bottom,
            _ => Self::Top,
        }
    }
}

/// A layer's width or height cap: the whole stage width, points, or a fraction (of the
/// stage width for `width`, of the WINDOW height for `max_h`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Extent {
    Fill,
    Px(f64),
    Frac(f64),
}

impl Extent {
    /// `"fill"`, `"48%"`, `"0.48"` (a fraction when <= 1), `"320"`, `"320px"`, or a number.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        if s == "fill" {
            return Some(Self::Fill);
        }
        if let Some(p) = s.strip_suffix('%') {
            return p.trim().parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| Self::Frac((f / 100.0).clamp(0.05, 1.0)));
        }
        let num = s.strip_suffix("px").unwrap_or(s).trim();
        let f = num.parse::<f64>().ok().filter(|f| f.is_finite())?;
        Some(Self::from_num(f))
    }

    fn from_num(f: f64) -> Self {
        if f <= 1.0 {
            Self::Frac(f.clamp(0.05, 1.0))
        } else {
            Self::Px(f.clamp(36.0, 4096.0))
        }
    }

    pub fn from_json(v: Option<&JsonValue>) -> Option<Self> {
        match v? {
            JsonValue::String(s) => Self::parse(s),
            other => json_num(Some(other)).filter(|f| f.is_finite()).map(Self::from_num),
        }
    }

    pub fn to_json(self) -> JsonValue {
        match self {
            Self::Fill => JsonValue::String("fill".into()),
            Self::Px(p) => JsonValue::F64(p),
            Self::Frac(f) => JsonValue::String(format!("{}%", (f * 1000.0).round() / 10.0)),
        }
    }

    pub fn resolve(self, full: f64) -> f64 {
        match self {
            Self::Fill => full,
            Self::Px(p) => p.min(full),
            Self::Frac(f) => full * f,
        }
    }
}

/// Which touches a layer takes: its drawn card only (the rest passes through to the
/// stage underneath), the whole stage viewport (a modal), or none (a HUD).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Card,
    All,
    None,
}

impl Hit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Card => "card",
            Self::All => "all",
            Self::None => "none",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "all" | "modal" => Self::All,
            "none" | "hud" => Self::None,
            _ => Self::Card,
        }
    }
}

/// Which edge a layer slides in from; `None` shows and hides it at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlideFrom {
    Top,
    Bottom,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Motion {
    pub from: SlideFrom,
    pub in_ms: u32,
    pub out_ms: u32,
}

impl Motion {
    pub const SLIDE: Self = Self { from: SlideFrom::Top, in_ms: 200, out_ms: 160 };
    pub const NONE: Self = Self { from: SlideFrom::None, in_ms: 0, out_ms: 0 };

    /// `"slide"` (from the layer's own edge: top for `Top`/`Center`, bottom for `Bottom`),
    /// `"slide-top"`, `"slide-bottom"`, `"none"`, or `{from, in_ms, out_ms}`.
    pub fn from_json(v: Option<&JsonValue>, place: Place) -> Self {
        let edge = if place == Place::Bottom { SlideFrom::Bottom } else { SlideFrom::Top };
        let from_str = |s: &str| match s {
            "none" | "off" | "" => SlideFrom::None,
            "top" | "slide-top" => SlideFrom::Top,
            "bottom" | "slide-bottom" => SlideFrom::Bottom,
            _ => edge,
        };
        match v {
            None => Self { from: edge, ..Self::SLIDE },
            Some(JsonValue::String(s)) => {
                let from = from_str(s.as_str());
                if from == SlideFrom::None { Self::NONE } else { Self { from, ..Self::SLIDE } }
            }
            Some(o @ JsonValue::Object(_)) => {
                let from = o.get("from").and_then(|f| f.as_str()).map(from_str).unwrap_or(edge);
                let ms = |k: &str, d: u32| json_num(o.get(k)).map(|f| f.clamp(0.0, 2000.0) as u32).unwrap_or(d);
                Self { from, in_ms: ms("in_ms", Self::SLIDE.in_ms), out_ms: ms("out_ms", Self::SLIDE.out_ms) }
            }
            Some(_) => Self { from: edge, ..Self::SLIDE },
        }
    }

    pub fn to_json(self) -> JsonValue {
        let mut m = HashMap::new();
        let from = match self.from {
            SlideFrom::Top => "top",
            SlideFrom::Bottom => "bottom",
            SlideFrom::None => "none",
        };
        m.insert("from".to_string(), JsonValue::String(from.into()));
        m.insert("in_ms".to_string(), JsonValue::F64(self.in_ms as f64));
        m.insert("out_ms".to_string(), JsonValue::F64(self.out_ms as f64));
        JsonValue::Object(m)
    }
}

/// A surface floating above the stage body at `z` (higher on top), instead of in a slot.
#[derive(Clone, Debug, PartialEq)]
pub struct StageLayer {
    pub surface: String,
    pub z: i32,
    pub place: Place,
    /// left, top, right, bottom, in points, inside the stage viewport.
    pub inset: [f64; 4],
    pub width: Extent,
    pub max_h: Extent,
    pub hit: Hit,
    /// Store paths (`"voice.live"`); the layer is shown while ANY is truthy. Empty: shown
    /// unless hidden by hand (`stage layer NAME hide`).
    pub when: Vec<String>,
    /// Stays up this long after `when` turns false.
    pub hold_ms: u32,
    pub motion: Motion,
    /// Runtime, not saved: shown now (the shell animates towards it).
    pub shown: bool,
    /// Runtime, not saved: hidden but still animating out, so its surface is still drawn.
    pub leaving: bool,
}

impl StageLayer {
    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let surface = v.get("surface")?.as_str()?.trim().to_string();
        if surface.is_empty() {
            return None;
        }
        let place = v.get("place").and_then(|p| p.as_str()).map(Place::from_str).unwrap_or(Place::Top);
        let mut inset = [0.0; 4];
        match v.get("inset") {
            Some(JsonValue::Array(a)) => {
                for (i, x) in a.iter().take(4).enumerate() {
                    inset[i] = json_num(Some(x)).unwrap_or(0.0).clamp(0.0, 2000.0);
                }
            }
            Some(JsonValue::String(s)) => inset = parse_inset(s).unwrap_or(inset),
            Some(other) => inset = [json_num(Some(other)).unwrap_or(0.0).clamp(0.0, 2000.0); 4],
            None => {}
        }
        let when = match v.get("when") {
            Some(JsonValue::Array(a)) => a.iter().filter_map(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            Some(JsonValue::String(s)) => s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            _ => Vec::new(),
        };
        Some(Self {
            surface,
            z: json_num(v.get("z")).unwrap_or(0.0).clamp(-1000.0, 1000.0) as i32,
            place,
            inset,
            width: Extent::from_json(v.get("width")).unwrap_or(Extent::Fill),
            max_h: Extent::from_json(v.get("max_h")).unwrap_or(Extent::Frac(0.48)),
            hit: v.get("hit").and_then(|h| h.as_str()).map(Hit::from_str).unwrap_or(Hit::Card),
            shown: when.is_empty(),
            when,
            hold_ms: json_num(v.get("hold_ms")).unwrap_or(0.0).clamp(0.0, 60_000.0) as u32,
            motion: Motion::from_json(v.get("motion"), place),
            leaving: false,
        })
    }

    pub fn to_json(&self) -> JsonValue {
        let mut m = HashMap::new();
        m.insert("surface".to_string(), JsonValue::String(self.surface.clone()));
        m.insert("z".to_string(), JsonValue::F64(self.z as f64));
        m.insert("place".to_string(), JsonValue::String(self.place.as_str().into()));
        m.insert("inset".to_string(), JsonValue::Array(self.inset.iter().map(|f| JsonValue::F64(*f)).collect()));
        m.insert("width".to_string(), self.width.to_json());
        m.insert("max_h".to_string(), self.max_h.to_json());
        m.insert("hit".to_string(), JsonValue::String(self.hit.as_str().into()));
        m.insert("when".to_string(), JsonValue::Array(self.when.iter().map(|s| JsonValue::String(s.clone())).collect()));
        m.insert("hold_ms".to_string(), JsonValue::F64(self.hold_ms as f64));
        m.insert("motion".to_string(), self.motion.to_json());
        JsonValue::Object(m)
    }

    /// Drawn this frame: shown, or animating out.
    pub fn drawn(&self) -> bool {
        self.shown || self.leaving
    }

    /// Whether `when` holds against the store: any path truthy (true, a non-zero number,
    /// a non-empty string, array or object).
    pub fn when_holds(&self, store: &HashMap<String, JsonValue>) -> bool {
        self.when.iter().any(|p| truthy(store_path(store, p)))
    }

    /// Whether any of `touched` store keys is one `when` reads.
    pub fn when_reads_any(&self, touched: &[String]) -> bool {
        self.when.iter().any(|p| {
            let top = p.split('.').next().unwrap_or("");
            touched.iter().any(|k| k == top)
        })
    }
}

/// `"10,8,10,0"` (left, top, right, bottom), `"12"` (all four) or `"8,10"` (vertical, horizontal).
pub fn parse_inset(s: &str) -> Option<[f64; 4]> {
    let v: Vec<f64> = s.split(',').map(|x| x.trim().parse::<f64>()).collect::<Result<_, _>>().ok()?;
    let c = |f: f64| f.clamp(0.0, 2000.0);
    match v.as_slice() {
        [a] => Some([c(*a); 4]),
        [vert, hor] => Some([c(*hor), c(*vert), c(*hor), c(*vert)]),
        [l, t, r, b] => Some([c(*l), c(*t), c(*r), c(*b)]),
        _ => None,
    }
}

fn store_path<'a>(store: &'a HashMap<String, JsonValue>, path: &str) -> Option<&'a JsonValue> {
    let mut parts = path.split('.');
    let mut cur = store.get(parts.next()?)?;
    for p in parts {
        cur = cur.get(p)?;
    }
    Some(cur)
}

fn truthy(v: Option<&JsonValue>) -> bool {
    match v {
        None | Some(JsonValue::Null) => false,
        Some(JsonValue::Bool(b)) => *b,
        Some(JsonValue::String(s)) => !s.is_empty(),
        Some(JsonValue::Array(a)) => !a.is_empty(),
        Some(JsonValue::Object(o)) => !o.is_empty(),
        Some(other) => json_num(Some(other)).map_or(false, |f| f != 0.0),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stage {
    pub mode: StageMode,
    pub focus: String,
    pub slots: Vec<StageSlot>,
    /// Floating surfaces above the stage body, kept in ascending `z` (draw order).
    pub layers: Vec<StageLayer>,
}

impl Default for Stage {
    fn default() -> Self {
        Self {
            mode: StageMode::Stack,
            focus: String::new(),
            slots: Vec::new(),
            layers: Vec::new(),
        }
    }
}

impl Stage {
    pub fn to_json(&self) -> JsonValue {
        let mut m = HashMap::new();
        m.insert("mode".to_string(), JsonValue::String(self.mode.as_str().into()));
        m.insert("focus".to_string(), JsonValue::String(self.focus.clone()));
        m.insert("slots".to_string(), JsonValue::Array(self.slots.iter().map(StageSlot::to_json).collect()));
        if !self.layers.is_empty() {
            m.insert("layers".to_string(), JsonValue::Array(self.layers.iter().map(StageLayer::to_json).collect()));
        }
        JsonValue::Object(m)
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let mode = v.get("mode").and_then(|m| m.as_str()).map(StageMode::from_str).unwrap_or(StageMode::Stack);
        let mut layers: Vec<StageLayer> = Vec::new();
        for item in v.get("layers").and_then(|l| l.as_array()).map(|a| a.as_slice()).unwrap_or(&[]) {
            if let Some(layer) = StageLayer::from_json(item) {
                if !layers.iter().any(|l| l.surface == layer.surface) {
                    layers.push(layer);
                }
            }
        }
        layers.sort_by_key(|l| l.z);
        let slots_arr = v.get("slots")?.as_array()?;
        let mut slots: Vec<StageSlot> = Vec::new();
        for item in slots_arr {
            if let Some(slot) = StageSlot::from_json(item) {
                // A surface is a slot or a layer, never both.
                if !slots.iter().any(|s| s.surface == slot.surface) && !layers.iter().any(|l| l.surface == slot.surface) {
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
        Some(Self { mode, focus, slots, layers })
    }

    /// Surfaces actively drawn right now (empty when `panel_off`, excludes `Header`-collapsed
    /// slots and non-focused `Deck` tabs).
    pub fn visible_surfaces(&self, panel_off: bool) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        if panel_off {
            return set;
        }
        for l in &self.layers {
            if l.drawn() {
                set.insert(l.surface.clone());
            }
        }
        if self.slots.is_empty() {
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
        if self.is_floated(name) {
            // A floated surface has no slot; it shows by its layer, so focus does not move.
            return;
        }
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
        if self.is_floated(name) {
            return;
        }
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

    pub fn is_floated(&self, name: &str) -> bool {
        self.layers.iter().any(|l| l.surface == name)
    }

    pub fn layer(&self, name: &str) -> Option<&StageLayer> {
        self.layers.iter().find(|l| l.surface == name)
    }

    pub fn layer_mut(&mut self, name: &str) -> Option<&mut StageLayer> {
        self.layers.iter_mut().find(|l| l.surface == name)
    }

    /// Floats `layer`'s surface: takes it out of the slots and replaces any layer it had.
    /// A layer with `when` shows by the store (the caller re-evaluates it); one without keeps
    /// the shown state it had.
    pub fn float(&mut self, mut layer: StageLayer, fallback_home: bool) {
        let name = layer.surface.clone();
        if let Some(old) = self.layer(&name) {
            if layer.when.is_empty() && old.when.is_empty() {
                layer.shown = old.shown;
            }
            layer.leaving = old.leaving;
        }
        self.layers.retain(|l| l.surface != name);
        self.layers.push(layer);
        self.layers.sort_by_key(|l| l.z);
        if self.slots.iter().any(|s| s.surface == name) {
            self.remove_surface(&name, fallback_home);
        }
    }

    pub fn unfloat(&mut self, name: &str) -> bool {
        let before = self.layers.len();
        self.layers.retain(|l| l.surface != name);
        self.layers.len() != before
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

/// `TB_LAYER_MOTION=0` shows and hides every layer at once (the A/B switch for the slide).
pub fn motion_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !matches!(std::env::var("TB_LAYER_MOTION").as_deref(), Ok("0") | Ok("off") | Ok("false")))
}
