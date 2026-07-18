//! [`MemStore`] — the pure in-memory [`RegistryStore`] impl.
//!
//! The testability contract made concrete: every blob, manifest, tag, and
//! referrer edge lives in a `Mutex`-guarded map. No filesystem, no network —
//! the whole OCI surface proves green against this in the conformance test.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::digest::Digest;

use super::{Referrer, RegistryStore, StoreError, StoredManifest, TagPage};

/// A repository's tag map and referrers reverse-index.
#[derive(Default)]
struct Repo {
    /// digest-wire → stored manifest.
    manifests: BTreeMap<String, StoredManifest>,
    /// tag → manifest digest-wire (the mutable pointers).
    tags: BTreeMap<String, String>,
    /// subject digest-wire → referring-manifest descriptors.
    referrers: BTreeMap<String, Vec<Referrer>>,
}

/// In-memory registry store. Cheap to clone-share via `Arc` in `AppState`.
#[derive(Default)]
pub struct MemStore {
    /// Global content-addressed blob pool (digest-wire → bytes). Blobs are
    /// cross-repo by content-address, which is exactly what makes cross-repo
    /// mount (end-11) a pointer op.
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Per-repository manifest/tag/referrer state.
    repos: Mutex<BTreeMap<String, Repo>>,
}

impl MemStore {
    /// A fresh empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RegistryStore for MemStore {
    async fn get_blob(&self, digest: &Digest) -> Result<Option<Vec<u8>>, StoreError> {
        let blobs = self.blobs.lock().map_err(poisoned)?;
        Ok(blobs.get(&digest.to_wire()).cloned())
    }

    async fn has_blob(&self, digest: &Digest) -> Result<bool, StoreError> {
        let blobs = self.blobs.lock().map_err(poisoned)?;
        Ok(blobs.contains_key(&digest.to_wire()))
    }

    async fn put_blob(&self, digest: &Digest, bytes: &[u8]) -> Result<(), StoreError> {
        let mut blobs = self.blobs.lock().map_err(poisoned)?;
        // Immutable at address: idempotent insert (same digest ⇒ same bytes).
        blobs.entry(digest.to_wire()).or_insert_with(|| bytes.to_vec());
        Ok(())
    }

    async fn delete_blob(&self, digest: &Digest) -> Result<(), StoreError> {
        let mut blobs = self.blobs.lock().map_err(poisoned)?;
        blobs.remove(&digest.to_wire());
        Ok(())
    }

    async fn get_manifest(
        &self,
        name: &str,
        digest: &Digest,
    ) -> Result<Option<StoredManifest>, StoreError> {
        let repos = self.repos.lock().map_err(poisoned)?;
        Ok(repos
            .get(name)
            .and_then(|r| r.manifests.get(&digest.to_wire()).cloned()))
    }

    async fn put_manifest(
        &self,
        name: &str,
        digest: &Digest,
        manifest: &StoredManifest,
    ) -> Result<(), StoreError> {
        let mut repos = self.repos.lock().map_err(poisoned)?;
        repos
            .entry(name.to_string())
            .or_default()
            .manifests
            .insert(digest.to_wire(), manifest.clone());
        Ok(())
    }

    async fn delete_manifest(&self, name: &str, digest: &Digest) -> Result<(), StoreError> {
        let mut repos = self.repos.lock().map_err(poisoned)?;
        if let Some(repo) = repos.get_mut(name) {
            let wire = digest.to_wire();
            repo.manifests.remove(&wire);
            // A manifest delete also drops every tag that pointed at it.
            repo.tags.retain(|_, target| *target != wire);
        }
        Ok(())
    }

    async fn put_tag(&self, name: &str, tag: &str, digest: &Digest) -> Result<(), StoreError> {
        let mut repos = self.repos.lock().map_err(poisoned)?;
        repos
            .entry(name.to_string())
            .or_default()
            .tags
            .insert(tag.to_string(), digest.to_wire());
        Ok(())
    }

    async fn resolve_tag(&self, name: &str, tag: &str) -> Result<Option<Digest>, StoreError> {
        let repos = self.repos.lock().map_err(poisoned)?;
        let Some(wire) = repos.get(name).and_then(|r| r.tags.get(tag)) else {
            return Ok(None);
        };
        // The stored pointer was written from a well-formed Digest, so parse
        // cannot fail here; a parse error would be a corrupt-store invariant
        // break surfaced as a typed StoreError, never a panic.
        Digest::parse(wire)
            .map(Some)
            .map_err(|e| StoreError::Backend(format!("corrupt tag pointer: {e}")))
    }

    async fn list_tags(
        &self,
        name: &str,
        n: Option<usize>,
        last: Option<&str>,
    ) -> Result<TagPage, StoreError> {
        let repos = self.repos.lock().map_err(poisoned)?;
        let Some(repo) = repos.get(name) else {
            // OCI: an existing-but-empty tag list is `{"tags":[]}` — but an
            // unknown repo is NAME_UNKNOWN. We signal "unknown repo" with an
            // empty page + a sentinel the handler distinguishes via a separate
            // existence check; here we return the empty page and let the
            // handler's name check own NAME_UNKNOWN.
            return Ok(TagPage { tags: Vec::new(), next_last: None });
        };
        // BTreeMap keys are already lexically ordered — the OCI ordering.
        let mut all: Vec<String> = repo.tags.keys().cloned().collect();
        if let Some(last) = last {
            all.retain(|t| t.as_str() > last);
        }
        let (page, more) = match n {
            Some(limit) if all.len() > limit => {
                let page: Vec<String> = all.into_iter().take(limit).collect();
                let more = true;
                (page, more)
            }
            _ => (all, false),
        };
        let next_last = if more { page.last().cloned() } else { None };
        Ok(TagPage { tags: page, next_last })
    }

    async fn add_referrer(
        &self,
        name: &str,
        subject: &Digest,
        referrer: &Referrer,
    ) -> Result<(), StoreError> {
        let mut repos = self.repos.lock().map_err(poisoned)?;
        let entry = repos
            .entry(name.to_string())
            .or_default()
            .referrers
            .entry(subject.to_wire())
            .or_default();
        // Idempotent per referrer digest.
        if !entry.iter().any(|r| r.digest == referrer.digest) {
            entry.push(referrer.clone());
        }
        Ok(())
    }

    async fn list_referrers(
        &self,
        name: &str,
        subject: &Digest,
        artifact_type: Option<&str>,
    ) -> Result<Vec<Referrer>, StoreError> {
        let repos = self.repos.lock().map_err(poisoned)?;
        let list = repos
            .get(name)
            .and_then(|r| r.referrers.get(&subject.to_wire()))
            .cloned()
            .unwrap_or_default();
        Ok(match artifact_type {
            Some(want) => list
                .into_iter()
                .filter(|r| r.artifact_type.as_deref() == Some(want))
                .collect(),
            None => list,
        })
    }
}

/// A poisoned lock is a process-internal invariant break, not a client error —
/// surface it as a typed backend error rather than propagating the panic.
fn poisoned<T>(_: std::sync::PoisonError<T>) -> StoreError {
    StoreError::Backend("registry store lock poisoned".to_string())
}
