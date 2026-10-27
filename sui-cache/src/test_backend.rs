//! A test backend that fails on demand, shared by the sweep tests (resign,
//! audit), so every per-entry failure path is exercised the same way.

use crate::{LocalStorage, StorageBackend};

/// A backend whose reads of one key, whose NAR reads and whose writes fail on
/// demand; everything else goes to a [`LocalStorage`].
pub(crate) struct Flaky {
    pub(crate) inner: LocalStorage,
    pub(crate) unreadable: Option<String>,
    pub(crate) nar_unreadable: bool,
    pub(crate) writes_fail: std::sync::atomic::AtomicBool,
}

impl Flaky {
    pub(crate) fn new(dir: &std::path::Path, unreadable: Option<&str>, nar_unreadable: bool) -> Self {
        Self {
            inner: LocalStorage::new(dir),
            unreadable: unreadable.map(str::to_string),
            nar_unreadable,
            writes_fail: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl StorageBackend for Flaky {
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, crate::CacheError> {
        if self.unreadable.as_deref() == Some(hash) {
            return Err(crate::CacheError::NarInfo("read failed".into()));
        }
        self.inner.get_narinfo(hash).await
    }
    async fn put_narinfo_record(&self, h: &str, c: &str, w: sui_castore::RecordWrite) -> Result<(), crate::CacheError> {
        if self.writes_fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::CacheError::NarInfo("write failed".into()));
        }
        self.inner.put_narinfo_record(h, c, w).await
    }
    async fn delete_narinfo_record(&self, h: &str) -> Result<(), crate::CacheError> {
        self.inner.delete_narinfo_record(h).await
    }
    async fn delete_nar_record(&self, p: &str) -> Result<(), crate::CacheError> {
        self.inner.delete_nar_record(p).await
    }
    fn nar_ref_index(&self) -> &dyn sui_castore::NarRefIndex {
        self.inner.nar_ref_index()
    }
    async fn get_nar(&self, p: &str) -> Result<Option<Vec<u8>>, crate::CacheError> {
        if self.nar_unreadable {
            return Err(crate::CacheError::NarInfo("nar read failed".into()));
        }
        self.inner.get_nar(p).await
    }
    async fn get_nar_stream(&self, p: &str) -> Result<Option<sui_castore::NarStream>, crate::CacheError> {
        if self.nar_unreadable {
            return Err(crate::CacheError::NarInfo("nar read failed".into()));
        }
        self.inner.get_nar_stream(p).await
    }
    async fn put_nar(&self, p: &str, d: &[u8]) -> Result<(), crate::CacheError> {
        self.inner.put_nar(p, d).await
    }
    fn nar_residency(&self) -> sui_castore::NarResidency {
        self.inner.nar_residency()
    }
    async fn list_narinfos(&self) -> Result<Vec<String>, crate::CacheError> {
        self.inner.list_narinfos().await
    }
}
