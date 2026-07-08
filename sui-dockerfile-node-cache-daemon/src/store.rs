//! The local L0 disk store — the injectable seam this crate's server
//! logic reads/writes through. Never touched directly by tests; tests
//! use [`MockLocalCacheStore`] the same way `sui-dockerfile-wrapper`'s
//! tests use `MockCacheBackend`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::protocol::CachedArtifact;
use crate::DaemonError;

/// Abstraction over the node-local content-addressed cache directory.
/// Content hashes are opaque strings (the same `content_hash` a
/// `DockerfileGraph` node already carries); this trait does not
/// interpret them beyond using them as a lookup key.
#[async_trait]
pub trait LocalCacheStore: Send + Sync {
    async fn get(&self, content_hash: &str) -> Result<Option<CachedArtifact>, DaemonError>;
    async fn put(&self, content_hash: &str, artifact: &CachedArtifact) -> Result<(), DaemonError>;
}

/// Real disk-backed implementation. A `DaemonSet` deployment backs
/// `root` with a `hostPath` volume so entries survive pod restarts —
/// see the `sui-dockerfile-node-cache-daemon` Helm chart values.
///
/// Layout: `<root>/<first 2 hex chars of hash>/<hash>.json`, mirroring
/// the classic two-level fan-out used by Nix store paths and most
/// content-addressed caches, so a single directory never accumulates
/// millions of entries.
pub struct RealLocalCacheStore {
    root: PathBuf,
}

impl RealLocalCacheStore {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn entry_path(&self, content_hash: &str) -> PathBuf {
        let shard = if content_hash.len() >= 2 { &content_hash[..2] } else { "00" };
        self.root.join(shard).join(format!("{content_hash}.json"))
    }
}

#[async_trait]
impl LocalCacheStore for RealLocalCacheStore {
    async fn get(&self, content_hash: &str) -> Result<Option<CachedArtifact>, DaemonError> {
        let path = self.entry_path(content_hash);
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let artifact: CachedArtifact =
                    serde_json::from_slice(&bytes).map_err(|e| DaemonError::Protocol(e.to_string()))?;
                Ok(Some(artifact))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(DaemonError::Io(e)),
        }
    }

    async fn put(&self, content_hash: &str, artifact: &CachedArtifact) -> Result<(), DaemonError> {
        let path = self.entry_path(content_hash);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let body = serde_json::to_vec(artifact).map_err(|e| DaemonError::Protocol(e.to_string()))?;
        // Write to a temp file then rename, so a crash mid-write never
        // leaves a truncated/corrupt entry for a later `Get` to trip
        // over — same atomic-publish discipline the rest of sui's
        // storage backends use.
        let tmp_path = path.with_extension("json.tmp");
        tokio::fs::write(&tmp_path, &body).await?;
        tokio::fs::rename(&tmp_path, &path).await?;
        Ok(())
    }
}

/// In-memory mock for unit tests. Never touches a filesystem.
#[derive(Default)]
pub struct MockLocalCacheStore {
    entries: Mutex<BTreeMap<String, CachedArtifact>>,
    /// Number of `get` calls observed — lets tests assert a second
    /// `Get` for the same hash is served without re-touching anything.
    pub get_calls: Mutex<usize>,
}

impl MockLocalCacheStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// # Panics
    ///
    /// Panics only if the internal mutex is poisoned (a prior panic
    /// while holding the lock) — never in normal test use.
    #[must_use]
    pub fn with_entry(self, content_hash: &str, artifact: CachedArtifact) -> Self {
        self.entries.lock().expect("mock mutex poisoned").insert(content_hash.to_string(), artifact);
        self
    }

    /// Snapshot of hashes currently present — used by tests to assert
    /// a remote-fetched entry was actually persisted locally.
    ///
    /// # Panics
    ///
    /// Panics only if the internal mutex is poisoned (a prior panic
    /// while holding the lock) — never in normal test use.
    #[must_use]
    pub fn contains(&self, content_hash: &str) -> bool {
        self.entries.lock().expect("mock mutex poisoned").contains_key(content_hash)
    }
}

#[async_trait]
impl LocalCacheStore for MockLocalCacheStore {
    async fn get(&self, content_hash: &str) -> Result<Option<CachedArtifact>, DaemonError> {
        *self.get_calls.lock().expect("mock mutex poisoned") += 1;
        Ok(self.entries.lock().expect("mock mutex poisoned").get(content_hash).cloned())
    }

    async fn put(&self, content_hash: &str, artifact: &CachedArtifact) -> Result<(), DaemonError> {
        self.entries
            .lock()
            .expect("mock mutex poisoned")
            .insert(content_hash.to_string(), artifact.clone());
        Ok(())
    }
}

/// Best-effort helper: ensure a root directory exists before serving.
/// Kept as a free function (not folded into `RealLocalCacheStore::new`)
/// so construction stays infallible and synchronous; callers await
/// this once at daemon startup.
///
/// # Errors
///
/// Propagates any I/O failure creating the directory.
pub async fn ensure_root_exists(root: &Path) -> Result<(), DaemonError> {
    tokio::fs::create_dir_all(root).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn real_store_put_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = RealLocalCacheStore::new(dir.path().to_path_buf());
        let artifact = CachedArtifact { image_ref: "example/image:v1".to_string() };
        store.put("deadbeef", &artifact).await.unwrap();
        let got = store.get("deadbeef").await.unwrap();
        assert_eq!(got, Some(artifact));
    }

    #[tokio::test]
    async fn real_store_miss_returns_none_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = RealLocalCacheStore::new(dir.path().to_path_buf());
        let got = store.get("nope").await.unwrap();
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn mock_store_get_calls_are_counted() {
        let store = MockLocalCacheStore::new();
        store.get("x").await.unwrap();
        store.get("x").await.unwrap();
        assert_eq!(*store.get_calls.lock().unwrap(), 2);
    }
}
