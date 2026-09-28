//! # rune-common
//!
//! Shared types, error definitions, and utilities for the Rune build system.
//!
//! This crate provides the foundational types used across all Rune crates:
//! - [`CrateId`] — unique identification for a crate
//! - [`Edition`], [`CrateType`], [`Profile`] — compilation parameters
//! - [`TargetTriple`] — platform targeting
//! - [`hash`] — fast content hashing utilities (xxhash3 + blake3)
//! - [`RuneError`] — shared error types

pub mod cargo_config;
pub mod cfgexpr;
pub mod config;
pub mod error;
pub mod fsutil;
pub mod hash;
pub mod manifest_file;
pub mod net;
pub mod settings;
pub mod sysinfo;
pub mod time;
pub mod timing;
pub mod toolchain;
pub mod types;

pub use config::RuneConfig;
pub use error::{RuneError, RuneResult};
pub use settings::Settings;
pub use types::*;
