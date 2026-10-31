//! Small std-only primitives with exactly one implementation each.
//!
//! - [`clock`]: monotonic and wall-clock time for code below the platform
//!   layer; the one place native code reads the std clocks.
//! - [`sha256`]: streaming SHA-256 with hardware acceleration where the CPU
//!   has it (content digests, download verification, run identities).
//! - [`netif`]: this machine's IPv4 interfaces and gateway candidates.
//! - [`udp`]: a UDP socket bound with address and port reuse, so several
//!   processes on one machine can listen for the same LAN beacons.
//!
//! Nothing here knows about assets, flows, models or any application.

pub mod clock;
pub mod netif;
pub mod sha256;
pub mod udp;
