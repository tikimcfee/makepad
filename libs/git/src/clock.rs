//! Phase timing for the read stats, on makepad-core-util's clock: this crate
//! sits below the platform layer, so it cannot ask `Cx`.

use makepad_core_util::clock::monotonic_now;

/// Elapsed wall time of one measured phase, in the milliseconds the stats
/// report.
#[derive(Clone, Copy)]
pub(crate) struct Stopwatch(f64);

impl Stopwatch {
    pub(crate) fn start() -> Self {
        Self(monotonic_now())
    }

    pub(crate) fn elapsed_ms(&self) -> f64 {
        (monotonic_now() - self.0) * 1000.0
    }
}
