//! Content-hash based dirty detection.

pub mod cache;
pub mod hasher;

pub use cache::FingerprintCache;
pub use hasher::{CrateFingerprint, FingerprintInputs};
