//! Content-addressed blob store.
//!
//! Blobs are stored on disk by SHA-256 hash with a two-character prefix
//! directory to avoid flat-directory performance issues.
//! Storage path: `{data_dir}/blobs/{hash[0:2]}/{hash}`

mod store;

pub use store::{BlobInfo, BlobStore};
