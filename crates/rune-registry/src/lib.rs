//! # rune-registry
//!
//! Fast crates.io Sparse Index client, parallel `.crate` package downloader,
//! tarball decompression & extraction, and registry cache management for Rune.

pub mod download;
pub mod index;

pub use download::{CrateDownloader, download_and_extract_crate};
pub use index::{CrateIndex, IndexDependency, IndexRecord, SparseIndexClient};
