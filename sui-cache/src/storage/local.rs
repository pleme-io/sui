//! Local filesystem storage backend.
//!
//! Layout:
//! ```text
//! <root>/
//!   <hash>.narinfo          -- text narinfo metadata
//!   nar/
//!     <hash>.nar.xz         -- compressed NAR blobs
//! ```

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::fs;

use super::StorageBackend;
use crate::CacheError;

/// Filesystem-backed binary cache storage.
#[derive(Debug, Clone)]
pub struct LocalStorage {
    /// Root directory for all cache data.
    root: PathBuf,
}

impl LocalStorage {
    /// Create a new local storage backend rooted at `path`.
    ///
    /// The directory structure is created lazily on first write.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { root: path.into() }
    }

    /// Return the root path.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Ensure a directory exists.
    async fn ensure_dir(&self, path: &Path) -> Result<(), CacheError> {
        if !path.exists() {
            fs::create_dir_all(path).await.map_err(CacheError::Io)?;
        }
        Ok(())
    }

    /// Path to a narinfo file.
    fn narinfo_path(&self, hash: &str) -> PathBuf {
        self.root.join(format!("{hash}.narinfo"))
    }

    /// Path to a NAR blob. The `nar_path` is a relative path like `nar/xyz.nar.xz`.
    fn nar_blob_path(&self, nar_path: &str) -> PathBuf {
        self.root.join(nar_path)
    }
}

#[async_trait]
impl StorageBackend for LocalStorage {
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, CacheError> {
        let path = self.narinfo_path(hash);
        match fs::read_to_string(&path).await {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CacheError::Io(e)),
        }
    }

    async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), CacheError> {
        self.ensure_dir(&self.root).await?;
        let path = self.narinfo_path(hash);
        fs::write(&path, content).await.map_err(CacheError::Io)
    }

    async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, CacheError> {
        let full = self.nar_blob_path(path);
        match fs::read(&full).await {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CacheError::Io(e)),
        }
    }

    async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), CacheError> {
        let full = self.nar_blob_path(path);
        if let Some(parent) = full.parent() {
            self.ensure_dir(parent).await?;
        }
        fs::write(&full, data).await.map_err(CacheError::Io)
    }

    async fn delete(&self, hash: &str) -> Result<(), CacheError> {
        // Read narinfo to find the NAR blob path, then delete both.
        let narinfo_path = self.narinfo_path(hash);
        if narinfo_path.exists() {
            // Try to parse the narinfo to find the NAR URL.
            if let Ok(content) = fs::read_to_string(&narinfo_path).await {
                if let Ok(info) = sui_compat::narinfo::NarInfo::parse(&content) {
                    let nar_path = self.nar_blob_path(&info.url);
                    let _ = fs::remove_file(&nar_path).await;
                }
            }
            fs::remove_file(&narinfo_path)
                .await
                .map_err(CacheError::Io)?;
        }
        Ok(())
    }

    async fn list_narinfos(&self) -> Result<Vec<String>, CacheError> {
        let mut hashes = Vec::new();
        if !self.root.exists() {
            return Ok(hashes);
        }
        let mut entries = fs::read_dir(&self.root).await.map_err(CacheError::Io)?;
        while let Some(entry) = entries.next_entry().await.map_err(CacheError::Io)? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(hash) = name.strip_suffix(".narinfo") {
                hashes.push(hash.to_string());
            }
        }
        Ok(hashes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_missing_narinfo_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let result = storage.get_narinfo("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn put_and_get_narinfo() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let content = "StorePath: /nix/store/abc-hello\nURL: nar/abc.nar.xz\nCompression: xz\nFileHash: sha256:aaa\nFileSize: 100\nNarHash: sha256:bbb\nNarSize: 200\nReferences: \n";
        storage.put_narinfo("abc", content).await.unwrap();
        let retrieved = storage.get_narinfo("abc").await.unwrap().unwrap();
        assert_eq!(retrieved, content);
    }

    #[tokio::test]
    async fn get_missing_nar_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let result = storage.get_nar("nar/missing.nar.xz").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn put_and_get_nar() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let data = b"fake nar data";
        storage.put_nar("nar/abc.nar.xz", data).await.unwrap();
        let retrieved = storage.get_nar("nar/abc.nar.xz").await.unwrap().unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn list_narinfos_empty() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let hashes = storage.list_narinfos().await.unwrap();
        assert!(hashes.is_empty());
    }

    #[tokio::test]
    async fn list_narinfos_returns_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_narinfo("aaa", "content1").await.unwrap();
        storage.put_narinfo("bbb", "content2").await.unwrap();
        let mut hashes = storage.list_narinfos().await.unwrap();
        hashes.sort();
        assert_eq!(hashes, vec!["aaa", "bbb"]);
    }

    #[tokio::test]
    async fn list_narinfos_ignores_non_narinfo_files() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_narinfo("abc", "content").await.unwrap();
        // Write a non-narinfo file.
        fs::write(dir.path().join("readme.txt"), "hello")
            .await
            .unwrap();
        let hashes = storage.list_narinfos().await.unwrap();
        assert_eq!(hashes, vec!["abc"]);
    }

    #[tokio::test]
    async fn list_narinfos_on_nonexistent_dir() {
        let storage = LocalStorage::new("/tmp/sui-cache-test-nonexistent-dir-12345");
        let hashes = storage.list_narinfos().await.unwrap();
        assert!(hashes.is_empty());
    }

    #[tokio::test]
    async fn delete_removes_narinfo_and_nar() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());

        let narinfo = "StorePath: /nix/store/xyz-hello\nURL: nar/xyz.nar.xz\nCompression: xz\nFileHash: sha256:aaa\nFileSize: 100\nNarHash: sha256:bbb\nNarSize: 200\nReferences: \n";
        storage.put_narinfo("xyz", narinfo).await.unwrap();
        storage.put_nar("nar/xyz.nar.xz", b"nar data").await.unwrap();

        // Verify both exist.
        assert!(storage.get_narinfo("xyz").await.unwrap().is_some());
        assert!(storage.get_nar("nar/xyz.nar.xz").await.unwrap().is_some());

        // Delete.
        storage.delete("xyz").await.unwrap();

        // Both should be gone.
        assert!(storage.get_narinfo("xyz").await.unwrap().is_none());
        assert!(storage.get_nar("nar/xyz.nar.xz").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        // Should not error.
        storage.delete("nonexistent").await.unwrap();
    }

    #[tokio::test]
    async fn root_accessor() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        assert_eq!(storage.root(), dir.path());
    }

    #[tokio::test]
    async fn put_narinfo_creates_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("cache");
        let storage = LocalStorage::new(&nested);
        storage.put_narinfo("test", "content").await.unwrap();
        assert!(nested.join("test.narinfo").exists());
    }

    #[tokio::test]
    async fn put_nar_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_nar("nar/deep/path.nar.xz", b"data").await.unwrap();
        assert!(dir.path().join("nar/deep/path.nar.xz").exists());
    }

    #[tokio::test]
    async fn overwrite_narinfo() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_narinfo("hash", "version1").await.unwrap();
        storage.put_narinfo("hash", "version2").await.unwrap();
        let content = storage.get_narinfo("hash").await.unwrap().unwrap();
        assert_eq!(content, "version2");
    }

    #[tokio::test]
    async fn overwrite_nar() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_nar("nar/x.nar.xz", b"old").await.unwrap();
        storage.put_nar("nar/x.nar.xz", b"new").await.unwrap();
        let data = storage.get_nar("nar/x.nar.xz").await.unwrap().unwrap();
        assert_eq!(data, b"new");
    }
}
