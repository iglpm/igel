//! # rune-registry
//!
//! Fast crates.io Sparse Index client, parallel `.crate` package downloader,
//! tarball decompression & extraction, and registry cache management for Rune.

pub mod download;
pub mod git;
pub mod index;
pub mod source;

pub use download::{CrateDownloader, download_and_extract_crate, download_and_extract_grouped};
pub use index::{CrateIndex, IndexDependency, IndexRecord, SparseIndexClient};
