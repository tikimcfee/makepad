//! Clock stamps for the panel's on/off lines, and with `TB_SHELL_PANEL_TRACE=1`
//! a trace of what the shell does around a panel change: every read of the
//! panel state file with its value, and for the ten seconds after a change, per
//! second, the app's events by kind, its draws and the store writes from bodies.
//! A second's counts are logged at the first event of a later second, so a
//! shell that stays silent logs nothing.

use makepad_widgets::*;

extern "C" {
    fn clock_gettime(clock: i32, ts: *mut [i64; 2]) -> i32;
}

fn clock_s(id: i32) -> f64 {
    let mut ts = [0i64; 2];
    unsafe { clock_gettime(id, &mut ts) };
    ts[0] as f64 + ts[1] as f64 / 1e9
}

/// CLOCK_MONOTONIC, CLOCK_BOOTTIME (counts suspend) and the wall clock.
pub fn stamp() -> String {
    format!("mono {:.3} boot {:.3} real {:.3}", clock_s(1), clock_s(7), clock_s(0))
}

#[derive(Default)]
pub struct PanelTrace {
    pub on: bool,
    change_mono: Option<f64>,
    second: i64,
    events: [u32; 4],
    draws: u32,
    sets: u32,
}

impl PanelTrace {
    pub fn from_env() -> PanelTrace {
        PanelTrace { on: std::env::var("TB_SHELL_PANEL_TRACE").map_or(false, |v| v == "1"), ..Default::default() }
    }

    pub fn read(&self, value: Option<&str>) {
        if self.on {
            log!("tb-panel read {:?} at {}", value, stamp());
        }
    }

    pub fn changed(&mut self) {
        self.flush();
        self.change_mono = Some(clock_s(1));
        self.second = 0;
    }

    fn bucket(&mut self) -> Option<i64> {
        let since = clock_s(1) - self.change_mono?;
        if since > 10.0 {
            self.flush();
            self.change_mono = None;
            return None;
        }
        let s = since.floor() as i64;
        if s != self.second {
            self.flush();
            self.second = s;
        }
        Some(s)
    }

    fn flush(&mut self) {
        if self.on && self.change_mono.is_some() && (self.events.iter().sum::<u32>() + self.draws + self.sets) > 0 {
            log!(
                "tb-panel second {} after the change: events signal {} draw {} nextframe {} other {}, body writes {}",
                self.second, self.events[0], self.events[1], self.events[2], self.events[3], self.sets
            );
        }
        self.events = [0; 4];
        self.draws = 0;
        self.sets = 0;
    }

    pub fn event(&mut self, event: &Event) {
        if !self.on || self.bucket().is_none() {
            return;
        }
        let k = match event {
            Event::Signal => 0,
            Event::Draw(_) => 1,
            Event::NextFrame(_) => 2,
            _ => 3,
        };
        self.events[k] += 1;
        if k == 1 {
            self.draws += 1;
        }
    }

    pub fn body_write(&mut self) {
        if self.on && self.bucket().is_some() {
            self.sets += 1;
        }
    }
}
