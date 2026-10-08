//! Registry indexes, package downloads and the registry cache.

pub mod api;
pub mod credential;
pub mod download;
pub mod git;
pub mod git_index;
pub mod http;
pub mod index;
pub mod source;

pub use download::{CrateDownloader, download_and_extract_crate, download_and_extract_grouped};
pub use index::{CrateIndex, IndexDependency, IndexRecord, SparseIndexClient};
