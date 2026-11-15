//! `TB_SHELL_SPLIT=N`: where the time of one client update goes, for the next N
//! client `set`s. An update runs from its `set` to the next one, so its record
//! holds everything it caused, the frame on the render thread included.
//!
//! Clock (UI thread, wall): the request's JSON parse, the merge, the `__assign`
//! call (the script parses the values and assigns them to `state`), and the
//! `__render` call (it asks for `on_render`; Makepad runs that later, after the
//! app's handler returns, in its post-dispatch script tasks).
//!
//! UI thread CPU (its own schedstat, ns), per event kind (signal, draw, next
//! frame, other): inside the app's handler, and after it until the app is
//! entered again ("after": Makepad's post-dispatch work, where the deferred
//! `on_render` runs and its result is reloaded into the widgets, and, after a
//! draw, the frame's hand-off to the renderer). Whole-process CPU per thread
//! group from /proc/self/task/*/schedstat: UI, GL (`makepad-gl-rend*`, the
//! driver's threads included), other. The cost of taking that snapshot is
//! measured and taken off the UI figures.

use makepad_widgets::*;
use std::time::Instant;

extern "C" {
    fn gettid() -> i32;
    fn clock_gettime(clock: i32, ts: *mut [i64; 2]) -> i32;
}
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;

const KINDS: [&str; 4] = ["signal", "draw", "nextframe", "other"];

#[derive(Default, Clone)]
struct Rec {
    named: Vec<(&'static str, f64)>,
    tainted: bool,
    parse: f64,
    merge: f64,
    assign: f64,
    render: f64,
    walk: f64,
    warm: f64,
    drain: f64,
    probe: f64,
    count: [f64; 4],
    inside: [f64; 4],
    after: [f64; 4],
    ui: f64,
    gl: f64,
    other: f64,
}

impl Rec {
    fn add(&mut self, r: &Rec) {
        for (k, v) in &r.named {
            match self.named.iter_mut().find(|(n, _)| n == k) {
                Some(e) => e.1 += v,
                None => self.named.push((k, *v)),
            }
        }
        self.parse += r.parse;
        self.merge += r.merge;
        self.assign += r.assign;
        self.render += r.render;
        self.walk += r.walk;
        self.warm += r.warm;
        self.drain += r.drain;
        self.probe += r.probe;
        for k in 0..4 {
            self.count[k] += r.count[k];
            self.inside[k] += r.inside[k];
            self.after[k] += r.after[k];
        }
        self.ui += r.ui;
        self.gl += r.gl;
        self.other += r.other;
    }
}

#[derive(Default)]
pub struct Split {
    left: u32,
    ui_tid: i32,
    cur: Option<(Rec, [u64; 3])>,
    entered: u64,
    left_app: Option<(usize, u64)>,
    sum: Rec,
    n: u32,
}

fn ns_of(path: &std::path::Path) -> u64 {
    std::fs::read_to_string(path).ok().and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok())).unwrap_or(0)
}

/// This thread's ns on CPU, exact (schedstat is only brought up to date when the
/// thread is switched out, so it reads stale from inside a running thread).
fn self_ns() -> u64 {
    let mut ts = [0i64; 2];
    unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts[0] as u64 * 1_000_000_000 + ts[1] as u64
}

/// (ui, gl, other) ns on CPU so far.
fn cpu(ui_tid: i32) -> [u64; 3] {
    let mut out = [0u64; 3];
    for entry in std::fs::read_dir("/proc/self/task").into_iter().flatten().flatten() {
        let path = entry.path();
        let tid: i32 = entry.file_name().to_string_lossy().parse().unwrap_or(0);
        let ns = ns_of(&path.join("schedstat"));
        let comm = std::fs::read_to_string(path.join("comm")).unwrap_or_default();
        let i = if tid == ui_tid { 0 } else if comm.starts_with("makepad-gl-rend") { 1 } else { 2 };
        out[i] += ns;
    }
    out
}

fn kind(event: &Event) -> usize {
    match event {
        Event::Signal => 0,
        Event::Draw(_) => 1,
        Event::NextFrame(_) => 2,
        _ => 3,
    }
}

impl Split {
    pub fn from_env() -> Split {
        let left = std::env::var("TB_SHELL_SPLIT").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        Split { left, ui_tid: unsafe { gettid() }, ..Default::default() }
    }

    pub fn on(&self) -> bool {
        self.left > 0 || self.cur.is_some()
    }

    /// A client `set` arrives: the previous update ends here, a new one begins.
    pub fn begin(&mut self) {
        let before = self_ns();
        let now = cpu(self.ui_tid);
        let probe = self_ns() - before; // the snapshot's own cost, not the update's
        if let Some((mut r, c0)) = self.cur.take() {
            r.ui = (now[0] - c0[0]).saturating_sub(probe) as f64 / 1e3;
            r.gl = (now[1] - c0[1]) as f64 / 1e3;
            r.other = (now[2] - c0[2]) as f64 / 1e3;
            if r.tainted {
                log!("tb-split (not in the mean: it held a show) {}", line(&r, 1.0));
            } else {
                self.n += 1;
                self.sum.add(&r);
                log!("tb-split #{}: {}", self.n, line(&r, 1.0));
            }
            if self.left == 0 {
                log!("tb-split MEAN over {}: {}", self.n, line(&self.sum, self.n as f64));
            }
        }
        if self.left > 0 {
            self.left -= 1;
            let mut rec = Rec::default();
            rec.probe = probe as f64 / 1e3;
            self.cur = Some((rec, now));
        }
        // The snapshot was taken inside this handler: start its count after it.
        self.entered = self_ns();
    }

    /// The app's handler is entered.
    pub fn event_start(&mut self) {
        if !self.on() {
            return;
        }
        let now = self_ns();
        if let (Some((k, from)), Some((rec, _))) = (self.left_app.take(), self.cur.as_mut()) {
            rec.after[k] += (now - from) as f64 / 1e3;
        }
        self.entered = now;
    }

    /// The app's handler returns.
    pub fn event_end(&mut self, event: &Event) {
        if !self.on() {
            return;
        }
        let now = self_ns();
        let k = kind(event);
        if let Some((rec, _)) = self.cur.as_mut() {
            rec.count[k] += 1.0;
            rec.inside[k] += (now - self.entered) as f64 / 1e3;
        }
        self.left_app = Some((k, now));
    }

    /// The socket drain's wall time (it holds parse, merge, assign and the render call).
    pub fn add_drain(&mut self, dt: std::time::Duration) {
        if let Some((rec, _)) = self.cur.as_mut() {
            rec.drain += dt.as_secs_f64() * 1e6;
        }
    }

    /// The current record held something other than an update (a show): keep it out of the mean.
    pub fn taint(&mut self) {
        if let Some((rec, _)) = self.cur.as_mut() {
            rec.tainted = true;
        }
    }

    pub fn add_named(&mut self, name: &'static str, dt: std::time::Duration) {
        if let Some((rec, _)) = self.cur.as_mut() {
            let dt = dt.as_secs_f64() * 1e6;
            match rec.named.iter_mut().find(|(n, _)| *n == name) {
                Some(e) => e.1 += dt,
                None => rec.named.push((name, dt)),
            }
        }
    }

    pub fn timed<R>(&mut self, which: u8, f: impl FnOnce() -> R) -> R {
        if self.cur.is_none() {
            return f();
        }
        let t = Instant::now();
        let r = f();
        let dt = t.elapsed().as_secs_f64() * 1e6;
        if let Some((rec, _)) = self.cur.as_mut() {
            match which {
                0 => rec.parse += dt,
                1 => rec.merge += dt,
                2 => rec.assign += dt,
                3 => rec.render += dt,
                4 => rec.walk += dt,
                5 => rec.warm += dt,
                _ => {}
            }
        }
        r
    }
}

fn line(r: &Rec, n: f64) -> String {
    let mut s = format!(
        "clock: parse {:.0} merge {:.0} assign {:.0} render-call {:.0} tree-warm {:.0} walk(non-draw) {:.0} drain(holds parse..render-call and the probe {:.0}) {:.0} | ui cpu",
        r.parse / n, r.merge / n, r.assign / n, r.render / n, r.warm / n, r.walk / n, r.probe / n, r.drain / n
    );
    for (k, v) in &r.named {
        s += &format!(" {k} {:.0};", v / n);
    }
    let mut accounted = 0.0;
    for k in 0..4 {
        if r.count[k] > 0.0 {
            s += &format!(" {}x{:.1}: in {:.0} after {:.0};", KINDS[k], r.count[k] / n, r.inside[k] / n, r.after[k] / n);
            accounted += r.inside[k] + r.after[k];
        }
    }
    s += &format!(" unaccounted {:.0} = {:.0} | gl {:.0} other {:.0} us", (r.ui - accounted) / n, r.ui / n, r.gl / n, r.other / n);
    s
}
