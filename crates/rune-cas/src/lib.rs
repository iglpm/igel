//! # rune-cas
//!
//! Content-Addressable Store for Rune build artifacts.
//! Enables artifact reuse across projects and machines.

pub mod gc;
pub mod remote;
pub mod store;
pub mod usage;

pub use store::CasStore;
