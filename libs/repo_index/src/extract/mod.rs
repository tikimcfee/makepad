//! Chunks from a base circle's sources ([`crate::Extract`]): Rust items, Cargo crate cards,
//! Markdown sections, splash definitions, commits and notes, all filtered by the circle's
//! exclusions before anything is read.

pub mod cargo;
pub mod git;
pub mod history;
pub mod markdown;
pub mod notes;
pub mod rust;
pub mod splash;
