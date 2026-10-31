//! Spinning gizmos: a gizmo turns by a step each tick.

use fixture_beta::clamp_turn;

/// Turns per tick, at most.
pub const MAX_TURN: u32 = 8;

/// A gizmo that spins.
pub struct Gizmo {
    pub angle: u32,
}

impl Gizmo {
    /// Spins the gizmo by `step`, clamped to [`MAX_TURN`].
    pub fn spin(&mut self, step: u32) {
        self.angle = self.angle.wrapping_add(clamp_turn(step, MAX_TURN));
    }

    pub fn reset(&mut self) {
        self.angle = 0;
    }
}

/// Things that can be ticked.
pub trait Tick {
    fn tick(&mut self);
}

impl Tick for Gizmo {
    fn tick(&mut self) {
        self.spin(1);
    }
}

macro_rules! gizmo_count {
    ($($g:expr),*) => { 0 $(+ { let _ = $g; 1 })* };
}

pub fn count_two() -> u32 {
    gizmo_count!(1, 2)
}

pub mod tables;
