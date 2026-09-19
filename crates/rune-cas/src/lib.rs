//! # rune-cas
//!
//! Content-Addressable Store for Rune build artifacts.
//! Enables artifact reuse across projects and machines.

pub mod gc;
pub mod store;

pub use store::CasStore;
