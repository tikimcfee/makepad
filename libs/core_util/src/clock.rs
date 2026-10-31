//! The one std clock below the platform layer. Code that cannot ask `Cx`
//! (makepad-git, the repo index, benches) reads time here; on wasm it reads
//! the same host clocks the platform does.

/// Seconds on a monotonic clock with an arbitrary origin.
pub fn monotonic_now() -> f64 {
    native_or_host::monotonic_now()
}

/// Nanoseconds since the Unix epoch; 0 for a clock set before it.
pub fn unix_nanos() -> u128 {
    native_or_host::unix_nanos()
}

// The std clocks trap on wasm, which is why clippy.toml disallows them. This
// module is compiled only for native targets, the one place they are safe.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
mod native_or_host {
    use std::sync::OnceLock;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    pub(super) fn monotonic_now() -> f64 {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_secs_f64()
    }

    pub(super) fn unix_nanos() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    }
}

#[cfg(target_arch = "wasm32")]
mod native_or_host {
    #[link(wasm_import_module = "env")]
    extern "C" {
        fn js_monotonic_now() -> f64;
        fn js_time_now() -> f64;
    }

    pub(super) fn monotonic_now() -> f64 {
        unsafe { js_monotonic_now() }
    }

    pub(super) fn unix_nanos() -> u128 {
        (unsafe { js_time_now() }.max(0.0) * 1_000_000_000.0) as u128
    }
}
