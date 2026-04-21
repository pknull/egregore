//! BlobStore — content-addressed file storage keyed by SHA-256 hash.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Metadata about a stored blob.
#[derive(Debug, Clone, Serialize)]
pub struct BlobInfo {
    pub hash: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// Content-addressed blob store backed by the filesystem.
///
/// Blobs are stored at `{base_dir}/{hash[0:2]}/{hash}`. The two-character
/// prefix prevents any single directory from growing too large.
#[derive(Clone)]
pub struct BlobStore {
    base_dir: PathBuf,
}

impl BlobStore {
    /// Create a new BlobStore rooted at `{data_dir}/blobs`.
    pub fn new(data_dir: &Path) -> Self {
        let base_dir = data_dir.join("blobs");
        Self { base_dir }
    }

    /// Store a blob and return its metadata.
    ///
    /// Content-addressed storage is idempotent: storing the same bytes
    /// twice returns the same hash without writing again.
    pub fn put(&self, data: &[u8]) -> std::io::Result<BlobInfo> {
        let hash = hex_sha256(data);
        let path = self.blob_path(&hash);

        if path.exists() {
            // Already stored (content-addressed = idempotent)
            return Ok(BlobInfo {
                hash,
                size: data.len() as u64,
                created_at: file_created_at(&path),
            });
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, data)?;

        Ok(BlobInfo {
            hash,
            size: data.len() as u64,
            created_at: file_created_at(&path),
        })
    }

    /// Read a blob by hash. Returns `None` if not found.
    pub fn get(&self, hash: &str) -> std::io::Result<Option<Vec<u8>>> {
        if !is_valid_hash(hash) {
            return Ok(None);
        }
        let path = self.blob_path(hash);
        if path.exists() {
            Ok(Some(std::fs::read(&path)?))
        } else {
            Ok(None)
        }
    }

    /// Check whether a blob exists.
    pub fn has(&self, hash: &str) -> bool {
        if !is_valid_hash(hash) {
            return false;
        }
        self.blob_path(hash).exists()
    }

    /// Get blob metadata without reading the content.
    pub fn info(&self, hash: &str) -> std::io::Result<Option<BlobInfo>> {
        if !is_valid_hash(hash) {
            return Ok(None);
        }
        let path = self.blob_path(hash);
        if path.exists() {
            let meta = std::fs::metadata(&path)?;
            Ok(Some(BlobInfo {
                hash: hash.to_string(),
                size: meta.len(),
                created_at: file_created_at(&path),
            }))
        } else {
            Ok(None)
        }
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        let prefix = &hash[..2.min(hash.len())];
        self.base_dir.join(prefix).join(hash)
    }
}

/// Compute the hex-encoded SHA-256 of `data`.
fn hex_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    hex::encode(result)
}

/// Basic validation: a hash should be 64 hex characters.
fn is_valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit())
}

/// Best-effort creation timestamp from file metadata.
fn file_created_at(path: &Path) -> Option<String> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok().or_else(|| m.created().ok()))
        .map(|t| {
            let dt: chrono::DateTime<chrono::Utc> = t.into();
            dt.to_rfc3339()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_get_blob() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let data = b"hello world";
        let info = store.put(data).unwrap();

        assert_eq!(info.size, 11);
        assert_eq!(info.hash.len(), 64);

        let retrieved = store.get(&info.hash).unwrap().unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn put_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let data = b"same content";
        let info1 = store.put(data).unwrap();
        let info2 = store.put(data).unwrap();

        assert_eq!(info1.hash, info2.hash);
        assert_eq!(info1.size, info2.size);
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let fake_hash = "a".repeat(64);
        let result = store.get(&fake_hash).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn has_returns_correct_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let fake_hash = "b".repeat(64);
        assert!(!store.has(&fake_hash));

        let info = store.put(b"exists").unwrap();
        assert!(store.has(&info.hash));
    }

    #[test]
    fn info_returns_correct_size() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let data = b"size check";
        let put_info = store.put(data).unwrap();
        let get_info = store.info(&put_info.hash).unwrap().unwrap();

        assert_eq!(get_info.hash, put_info.hash);
        assert_eq!(get_info.size, data.len() as u64);
        assert!(get_info.created_at.is_some());
    }

    #[test]
    fn info_nonexistent_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let fake_hash = "c".repeat(64);
        assert!(store.info(&fake_hash).unwrap().is_none());
    }

    #[test]
    fn rejects_invalid_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        // Too short
        assert!(store.get("abc").unwrap().is_none());
        assert!(!store.has("abc"));

        // Non-hex
        let bad = "g".repeat(64);
        assert!(store.get(&bad).unwrap().is_none());
    }

    #[test]
    fn blob_path_uses_prefix_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());

        let data = b"prefix test";
        let info = store.put(data).unwrap();

        let expected_prefix = &info.hash[..2];
        let blob_file = store.blob_path(&info.hash);
        assert!(blob_file.parent().unwrap().ends_with(expected_prefix));
    }
}
