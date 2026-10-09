//! [`OverlayStore`] — read-only layout repositories over an optional writable
//! store.
//!
//! A porto with `layouts` configured serves those repositories from a
//! [`LayoutStore`] and, if a writable `backend` is also configured, every other
//! repository from it. Routing is by repository name: a name some layout serves
//! is read-only, full stop — a push to it is refused even though a writable
//! backend exists, so a Nix-built chart can never be shadowed by a pushed one.
//! With no writable store, every repository is read-only.
//!
//! Blobs are content-addressed and global (porto's existing model), so a blob
//! read consults the layouts first and then the writable store; a blob delete
//! is refused when the blob belongs to a layout.

use std::sync::Arc;

use async_trait::async_trait;

use crate::digest::Digest;

use super::{
    LayoutStore, Referrer, RegistryStore, RepoAccess, StoreError, StoredManifest, TagPage,
};

/// See the module docs.
pub struct OverlayStore {
    layouts: LayoutStore,
    writable: Option<Arc<dyn RegistryStore>>,
}

impl OverlayStore {
    /// Serve `layouts` read-only, everything else from `writable` (if any).
    #[must_use]
    pub fn new(layouts: LayoutStore, writable: Option<Arc<dyn RegistryStore>>) -> Self {
        Self { layouts, writable }
    }

    /// The store that owns repository `name` for reads.
    fn reader(&self, name: &str) -> Option<&dyn RegistryStore> {
        if self.layouts.serves(name) {
            Some(&self.layouts)
        } else {
            self.writable.as_deref()
        }
    }

    /// The store that accepts writes to `name`, or the typed refusal.
    fn writer(&self, name: &str) -> Result<&dyn RegistryStore, StoreError> {
        if self.layouts.serves(name) {
            return Err(StoreError::ReadOnly(format!(
                "{name} is served from a read-only OCI image layout"
            )));
        }
        self.writable.as_deref().ok_or_else(|| {
            StoreError::ReadOnly(format!("{name}: this registry has no writable backend"))
        })
    }
}

#[async_trait]
impl RegistryStore for OverlayStore {
    fn access(&self, name: &str) -> RepoAccess {
        match self.writer(name) {
            Ok(w) => w.access(name),
            Err(_) => RepoAccess::ReadOnly,
        }
    }

    async fn list_repositories(&self) -> Result<Vec<String>, StoreError> {
        let mut all = self.layouts.list_repositories().await?;
        if let Some(w) = &self.writable {
            all.extend(w.list_repositories().await?);
        }
        all.sort();
        all.dedup();
        Ok(all)
    }

    async fn get_blob(&self, digest: &Digest) -> Result<Option<Vec<u8>>, StoreError> {
        if self.layouts.holds_blob(digest) {
            return self.layouts.get_blob(digest).await;
        }
        match &self.writable {
            Some(w) => w.get_blob(digest).await,
            None => Ok(None),
        }
    }

    async fn has_blob(&self, digest: &Digest) -> Result<bool, StoreError> {
        if self.layouts.has_blob(digest).await? {
            return Ok(true);
        }
        match &self.writable {
            Some(w) => w.has_blob(digest).await,
            None => Ok(false),
        }
    }

    async fn put_blob(&self, digest: &Digest, bytes: &[u8]) -> Result<(), StoreError> {
        match &self.writable {
            Some(w) => w.put_blob(digest, bytes).await,
            None => Err(StoreError::ReadOnly("no writable backend".to_string())),
        }
    }

    async fn delete_blob(&self, digest: &Digest) -> Result<(), StoreError> {
        if self.layouts.holds_blob(digest) {
            return Err(StoreError::ReadOnly(format!(
                "{digest} belongs to a read-only OCI image layout"
            )));
        }
        match &self.writable {
            Some(w) => w.delete_blob(digest).await,
            None => Err(StoreError::ReadOnly("no writable backend".to_string())),
        }
    }

    async fn get_manifest(
        &self,
        name: &str,
        digest: &Digest,
    ) -> Result<Option<StoredManifest>, StoreError> {
        match self.reader(name) {
            Some(s) => s.get_manifest(name, digest).await,
            None => Ok(None),
        }
    }

    async fn put_manifest(
        &self,
        name: &str,
        digest: &Digest,
        manifest: &StoredManifest,
    ) -> Result<(), StoreError> {
        self.writer(name)?.put_manifest(name, digest, manifest).await
    }

    async fn delete_manifest(&self, name: &str, digest: &Digest) -> Result<(), StoreError> {
        self.writer(name)?.delete_manifest(name, digest).await
    }

    async fn put_tag(&self, name: &str, tag: &str, digest: &Digest) -> Result<(), StoreError> {
        self.writer(name)?.put_tag(name, tag, digest).await
    }

    async fn resolve_tag(&self, name: &str, tag: &str) -> Result<Option<Digest>, StoreError> {
        match self.reader(name) {
            Some(s) => s.resolve_tag(name, tag).await,
            None => Ok(None),
        }
    }

    async fn list_tags(
        &self,
        name: &str,
        n: Option<usize>,
        last: Option<&str>,
    ) -> Result<TagPage, StoreError> {
        match self.reader(name) {
            Some(s) => s.list_tags(name, n, last).await,
            None => Ok(TagPage { tags: Vec::new(), next_last: None }),
        }
    }

    async fn add_referrer(
        &self,
        name: &str,
        subject: &Digest,
        referrer: &Referrer,
    ) -> Result<(), StoreError> {
        self.writer(name)?.add_referrer(name, subject, referrer).await
    }

    async fn list_referrers(
        &self,
        name: &str,
        subject: &Digest,
        artifact_type: Option<&str>,
    ) -> Result<Vec<Referrer>, StoreError> {
        match self.reader(name) {
            Some(s) => s.list_referrers(name, subject, artifact_type).await,
            None => Ok(Vec::new()),
        }
    }
}
