//! [`SuiCacheStore`] — the production [`RegistryStore`] over sui-cache.
//!
//! This is the "naturalize(zot)" payload: OCI blobs and manifests become
//! content-addressed objects in sui-cache's
//! [`StorageBackend`](sui_castore::StorageBackend), reusing the SAME
//! durable store that holds Nix NARs. An OCI blob is stored via `put_nar` under
//! a stable key derived from its digest; a manifest is stored the same way plus
//! a sidecar object carrying its media-type. Content-addressed dedup across the
//! whole store is inherited for free — a blob shared by two repos is one object.
//!
//! **What is NOT yet durable (a named seam, honestly):** the tag pointer map
//! and the referrers reverse-index are mutable, per-repo structures that do not
//! fit the content-addressed NAR key model. They live here in a
//! `Mutex<HashMap>` for M0. The durable destination is **sui-store** (a typed
//! Postgres/redb table keyed by `(name, tag)` / `(name, subject)`); a pod
//! restart loses the pointers today. The blob/manifest *bytes* are durable
//! immediately (they are in the backend); only the mutable pointers are the
//! in-memory interim. This is the exact `pending` seam the durable-store phase
//! closes — it is stated, not hidden.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use sui_castore::StorageBackend;

use crate::digest::Digest;

use super::{Referrer, RegistryStore, StoreError, StoredManifest, TagPage};

/// Per-repository mutable pointer state (the non-content-addressed part).
///
/// DESTINATION: a typed sui-store table, not this in-memory map. See the
/// module docs.
#[derive(Default)]
struct RepoPointers {
    /// tag → manifest digest-wire.
    tags: BTreeMap<String, String>,
    /// subject digest-wire → referring descriptors.
    referrers: BTreeMap<String, Vec<Referrer>>,
    /// The set of manifest digest-wires known in this repo, so `get_manifest`
    /// can distinguish "this blob is a manifest here" from "some other blob".
    /// The media-type is carried in the sidecar object (see `manifest_meta_key`).
    manifest_present: BTreeMap<String, ()>,
}

/// OCI registry store backed by sui-cache's content-addressed store.
pub struct SuiCacheStore {
    backend: Arc<dyn StorageBackend>,
    /// The mutable pointer index. DESTINATION: sui-store.
    pointers: Mutex<BTreeMap<String, RepoPointers>>,
}

impl SuiCacheStore {
    /// Wrap a sui-cache [`StorageBackend`] as an OCI registry store.
    #[must_use]
    pub fn new(backend: Arc<dyn StorageBackend>) -> Self {
        Self {
            backend,
            pointers: Mutex::new(BTreeMap::new()),
        }
    }

    /// The blob object key: OCI blobs live under an `oci/blobs/` prefix in the
    /// shared store, keyed by the digest wire string. The `.nar`-shaped path
    /// slots into `get_nar`/`put_nar` (which key by arbitrary relative path).
    fn blob_key(digest: &Digest) -> String {
        format!("oci/blobs/{}/{}", digest.algorithm(), digest.hex())
    }

    /// The manifest object key (bytes).
    fn manifest_key(name: &str, digest: &Digest) -> String {
        format!("oci/manifests/{name}/{}/{}", digest.algorithm(), digest.hex())
    }

    /// The manifest sidecar key holding the stored media-type (a tiny object so
    /// a `GET`/`HEAD` returns the exact Content-Type the client `PUT`).
    fn manifest_meta_key(name: &str, digest: &Digest) -> String {
        format!(
            "oci/manifests/{name}/{}/{}.media-type",
            digest.algorithm(),
            digest.hex()
        )
    }
}

#[async_trait]
impl RegistryStore for SuiCacheStore {
    async fn get_blob(&self, digest: &Digest) -> Result<Option<Vec<u8>>, StoreError> {
        // An empty object is a delete-tombstone (see `delete_blob`: the
        // content-addressed backend owns GC separately, so a delete overwrites
        // with empty bytes). Treat empty-as-absent, exactly like `get_manifest`,
        // so a deleted blob does not read back present-but-empty.
        Ok(self
            .backend
            .get_nar(&Self::blob_key(digest))
            .await?
            .filter(|bytes| !bytes.is_empty()))
    }

    async fn has_blob(&self, digest: &Digest) -> Result<bool, StoreError> {
        // Empty == tombstoned == absent (matches `get_blob`/`get_manifest`), so a
        // deleted blob is never falsely cross-mounted with zero content.
        Ok(self
            .backend
            .get_nar(&Self::blob_key(digest))
            .await?
            .is_some_and(|bytes| !bytes.is_empty()))
    }

    async fn put_blob(&self, digest: &Digest, bytes: &[u8]) -> Result<(), StoreError> {
        self.backend.put_nar(&Self::blob_key(digest), bytes).await?;
        Ok(())
    }

    async fn delete_blob(&self, digest: &Digest) -> Result<(), StoreError> {
        // The backend keys NAR bytes by path; put an empty object is not a
        // delete, so we rely on the backend's own delete where available. The
        // StorageBackend trait deletes by store-path hash, which does not map
        // to a NAR path key — so a blob delete is a best-effort overwrite with
        // empty bytes for M0 (the content-addressed store treats GC as a
        // separate durable concern). Recorded as an honest interim.
        self.backend.put_nar(&Self::blob_key(digest), &[]).await?;
        Ok(())
    }

    async fn get_manifest(
        &self,
        name: &str,
        digest: &Digest,
    ) -> Result<Option<StoredManifest>, StoreError> {
        let Some(bytes) = self.backend.get_nar(&Self::manifest_key(name, digest)).await? else {
            return Ok(None);
        };
        // The manifest bytes could be an empty-tombstone from delete; treat
        // empty as absent.
        if bytes.is_empty() {
            return Ok(None);
        }
        let media_type = self
            .backend
            .get_nar(&Self::manifest_meta_key(name, digest))
            .await?
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".to_string());
        Ok(Some(StoredManifest { bytes, media_type }))
    }

    async fn put_manifest(
        &self,
        name: &str,
        digest: &Digest,
        manifest: &StoredManifest,
    ) -> Result<(), StoreError> {
        self.backend
            .put_nar(&Self::manifest_key(name, digest), &manifest.bytes)
            .await?;
        self.backend
            .put_nar(
                &Self::manifest_meta_key(name, digest),
                manifest.media_type.as_bytes(),
            )
            .await?;
        let mut pointers = self.pointers.lock().map_err(poisoned)?;
        pointers
            .entry(name.to_string())
            .or_default()
            .manifest_present
            .insert(digest.to_wire(), ());
        Ok(())
    }

    async fn delete_manifest(&self, name: &str, digest: &Digest) -> Result<(), StoreError> {
        // Tombstone the bytes (empty) — the content-addressed store owns real
        // reclamation as a GC concern.
        self.backend.put_nar(&Self::manifest_key(name, digest), &[]).await?;
        let mut pointers = self.pointers.lock().map_err(poisoned)?;
        if let Some(repo) = pointers.get_mut(name) {
            let wire = digest.to_wire();
            repo.manifest_present.remove(&wire);
            repo.tags.retain(|_, target| *target != wire);
        }
        Ok(())
    }

    async fn put_tag(&self, name: &str, tag: &str, digest: &Digest) -> Result<(), StoreError> {
        let mut pointers = self.pointers.lock().map_err(poisoned)?;
        pointers
            .entry(name.to_string())
            .or_default()
            .tags
            .insert(tag.to_string(), digest.to_wire());
        Ok(())
    }

    async fn resolve_tag(&self, name: &str, tag: &str) -> Result<Option<Digest>, StoreError> {
        let pointers = self.pointers.lock().map_err(poisoned)?;
        let Some(wire) = pointers.get(name).and_then(|r| r.tags.get(tag)) else {
            return Ok(None);
        };
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
        let pointers = self.pointers.lock().map_err(poisoned)?;
        let Some(repo) = pointers.get(name) else {
            return Ok(TagPage { tags: Vec::new(), next_last: None });
        };
        let mut all: Vec<String> = repo.tags.keys().cloned().collect();
        if let Some(last) = last {
            all.retain(|t| t.as_str() > last);
        }
        let (page, more) = match n {
            Some(limit) if all.len() > limit => {
                (all.into_iter().take(limit).collect::<Vec<_>>(), true)
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
        let mut pointers = self.pointers.lock().map_err(poisoned)?;
        let entry = pointers
            .entry(name.to_string())
            .or_default()
            .referrers
            .entry(subject.to_wire())
            .or_default();
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
        let pointers = self.pointers.lock().map_err(poisoned)?;
        let list = pointers
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

fn poisoned<T>(_: std::sync::PoisonError<T>) -> StoreError {
    StoreError::Backend("registry pointer lock poisoned".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_castore::LocalStorage;
    use sui_compat::hash::HashAlgorithm;

    fn store() -> (SuiCacheStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        // Return the TempDir alongside so the test keeps it alive for its
        // duration (dropping it removes the backing files). LocalStorage takes
        // an owned PathBuf.
        let backend: Arc<dyn StorageBackend> =
            Arc::new(LocalStorage::new(dir.path().to_path_buf()));
        (SuiCacheStore::new(backend), dir)
    }

    #[tokio::test]
    async fn blob_put_get_roundtrip_over_sui_cache() {
        let (s, _dir) = store();
        let bytes = b"layer bytes";
        let d = Digest::of(HashAlgorithm::Sha256, bytes);
        assert!(!s.has_blob(&d).await.unwrap());
        s.put_blob(&d, bytes).await.unwrap();
        assert!(s.has_blob(&d).await.unwrap());
        assert_eq!(s.get_blob(&d).await.unwrap().unwrap(), bytes);
    }

    #[tokio::test]
    async fn manifest_put_get_preserves_media_type() {
        let (s, _dir) = store();
        let m = StoredManifest {
            bytes: b"{}".to_vec(),
            media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
        };
        let d = Digest::of(HashAlgorithm::Sha256, &m.bytes);
        s.put_manifest("lib/x", &d, &m).await.unwrap();
        let got = s.get_manifest("lib/x", &d).await.unwrap().unwrap();
        assert_eq!(got, m);
    }

    #[tokio::test]
    async fn tag_is_a_mutable_pointer() {
        let (s, _dir) = store();
        let d1 = Digest::of(HashAlgorithm::Sha256, b"one");
        let d2 = Digest::of(HashAlgorithm::Sha256, b"two");
        s.put_tag("lib/x", "latest", &d1).await.unwrap();
        assert_eq!(s.resolve_tag("lib/x", "latest").await.unwrap().unwrap(), d1);
        s.put_tag("lib/x", "latest", &d2).await.unwrap();
        assert_eq!(s.resolve_tag("lib/x", "latest").await.unwrap().unwrap(), d2);
    }
}
