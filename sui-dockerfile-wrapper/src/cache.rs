//! The cache seam this crate consumes.
//!
//! This crate does **not** invent a new cache abstraction — it consumes
//! [`sui_cache::storage::StorageBackend`] exactly as `sui cache serve`
//! does. A node is "cached" when its `content_hash` has a narinfo entry;
//! the narinfo *content* is the image reference the node's cache write
//! produced, so a cache hit can pull the already-built image instead of
//! rebuilding it. Real callers construct the backend via
//! [`sui_cache::storage::build_backend`] against a
//! [`sui_cache::BackendConfig`] (Postgres L2 + Redis L1, per
//! `sui-supercacheci`); tests use [`MockCacheBackend`], an in-memory
//! implementation of the same trait — the identical shape the
//! `sui-store` crate's `TestStore` uses to prove `Store`'s default
//! methods without a real database.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use sui_cache::storage::StorageBackend;
use sui_cache::CacheError;

/// An in-memory [`StorageBackend`] for unit tests. Never touches a
/// filesystem, Redis, or Postgres — narinfo entries live in a
/// `Mutex<BTreeMap>` for the duration of the test.
#[derive(Default)]
pub struct MockCacheBackend {
    narinfos: Mutex<BTreeMap<String, String>>,
}

impl MockCacheBackend {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-populate a cache entry — the constructor tests use to set up a
    /// full or partial cache-hit fixture.
    ///
    /// # Panics
    ///
    /// Panics only if the internal mutex is poisoned (a prior panic
    /// while holding the lock) — never in normal test use.
    #[must_use]
    pub fn with_entry(self, hash: &str, image_ref: &str) -> Self {
        self.narinfos
            .lock()
            .expect("mock mutex poisoned")
            .insert(hash.to_string(), image_ref.to_string());
        self
    }
}

#[async_trait]
impl StorageBackend for MockCacheBackend {
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, CacheError> {
        Ok(self.narinfos.lock().expect("mock mutex poisoned").get(hash).cloned())
    }

    async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), CacheError> {
        self.narinfos
            .lock()
            .expect("mock mutex poisoned")
            .insert(hash.to_string(), content.to_string());
        Ok(())
    }

    async fn get_nar(&self, _path: &str) -> Result<Option<Vec<u8>>, CacheError> {
        Ok(None)
    }

    async fn put_nar(&self, _path: &str, _data: &[u8]) -> Result<(), CacheError> {
        Ok(())
    }

    async fn delete(&self, hash: &str) -> Result<(), CacheError> {
        self.narinfos.lock().expect("mock mutex poisoned").remove(hash);
        Ok(())
    }

    async fn list_narinfos(&self) -> Result<Vec<String>, CacheError> {
        Ok(self.narinfos.lock().expect("mock mutex poisoned").keys().cloned().collect())
    }
}
