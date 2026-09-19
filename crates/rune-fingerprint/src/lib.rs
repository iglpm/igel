//! # rune-fingerprint
//!
//! Content-hash based dirty detection for the Rune build system.
//! Uses xxhash3 for fast file hashing with mtime as a cache optimization.

pub mod cache;
pub mod hasher;

pub use cache::FingerprintCache;
pub use hasher::{CrateFingerprint, FingerprintInputs};
