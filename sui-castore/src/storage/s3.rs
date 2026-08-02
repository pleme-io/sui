//! S3-compatible object storage backend.
//!
//! Uses `object_store` crate — works with AWS S3, CloudFlare R2, MinIO,
//! RustFS, Backblaze B2, and any S3-compatible endpoint.
//!
//! Breathable by design: S3 provides infinite elasticity.
//! Combined with redb for ephemeral local metadata index.

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::{ObjectStore, WriteMultipart};
use tracing::{debug, warn};

use super::nar_refs::{referrer_of, NarRefIndex, NarRefKey, NarRefScan};
use super::nar_stream::{self, NarSource, NarStream};
use super::{NarResidency, StorageBackend};
use crate::StoreError;

/// How many multipart parts may be in flight at once.
///
/// The peak this backend can reach is
/// `(1 + S3_MAX_INFLIGHT_PARTS) * S3_PART_BYTES` plus one source chunk — a
/// constant, not a function of NAR size. Two is enough to keep the pipe full
/// without turning "bounded" into "bounded by something large".
const S3_MAX_INFLIGHT_PARTS: usize = 2;

/// Multipart part size. **5 MiB is S3's minimum for a non-final part** — a
/// smaller value makes real S3 reject the upload, so this is not a free knob and
/// deliberately does not reuse [`NAR_CHUNK_BYTES`](super::NAR_CHUNK_BYTES) (4 MiB).
const S3_PART_BYTES: usize = 5 * 1024 * 1024;

/// S3-compatible object storage backend.
pub struct S3Storage {
    store: Box<dyn ObjectStore>,
    bucket: String,
    region: String,
    endpoint: Option<String>,
}

impl S3Storage {
    /// Create a new S3 storage backend.
    ///
    /// Uses AWS default credential chain (IRSA, env vars, instance profile).
    /// Set `endpoint` for non-AWS S3-compatible services (MinIO, RustFS, R2).
    pub fn new(bucket: String, region: String, endpoint: Option<String>) -> Result<Self, StoreError> {
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&bucket)
            .with_region(&region);

        if let Some(ep) = &endpoint {
            builder = builder.with_endpoint(ep).with_allow_http(true);
        }

        let store = builder
            .build()
            .map_err(|e| StoreError::Io(std::io::Error::other(format!("S3 init failed: {e}"))))?;

        Ok(Self {
            store: Box::new(store),
            bucket,
            region,
            endpoint,
        })
    }

    /// Back this backend by an in-process object store.
    ///
    /// `object_store`'s `InMemory` implements the same [`ObjectStore`] trait a
    /// live S3 does, so the key layout, the `LIST`-driven reverse index and the
    /// delete semantics are exercised for real rather than asserted about. What
    /// it does **not** prove is anything S3-specific — multipart minimums,
    /// eventual consistency, IAM — so it is a unit seam, not an S3 integration
    /// test.
    #[cfg(test)]
    #[must_use]
    fn in_memory() -> Self {
        Self {
            store: Box::new(object_store::memory::InMemory::new()),
            bucket: "in-memory".to_string(),
            region: "none".to_string(),
            endpoint: None,
        }
    }

    /// Return the bucket name.
    #[must_use]
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Return the region.
    #[must_use]
    pub fn region(&self) -> &str {
        &self.region
    }

    /// Return the custom endpoint, if any.
    #[must_use]
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// Delete one object, treating "already gone" as success.
    ///
    /// Every delete on this backend is idempotent by contract: a GC that
    /// re-reaps a key it already reaped must not fail the run.
    async fn delete_object(&self, path: &Path) -> Result<(), StoreError> {
        match self.store.delete(path).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(StoreError::Io(std::io::Error::other(format!("S3 delete: {e}")))),
        }
    }
}

#[async_trait]
impl StorageBackend for S3Storage {
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, StoreError> {
        let path = Path::from(format!("{hash}.narinfo"));
        match self.store.get(&path).await {
            Ok(result) => {
                let bytes = result
                    .bytes()
                    .await
                    .map_err(|e| StoreError::Io(std::io::Error::other(format!("S3 read: {e}"))))?;
                Ok(Some(
                    String::from_utf8(bytes.to_vec())
                        .map_err(|e| StoreError::NarInfo(format!("Invalid UTF-8: {e}")))?,
                ))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(StoreError::Io(std::io::Error::other(format!("S3 get: {e}")))),
        }
    }

    async fn put_narinfo_record(&self, hash: &str, content: &str) -> Result<(), StoreError> {
        let path = Path::from(format!("{hash}.narinfo"));
        self.store
            .put(&path, Bytes::from(content.to_string()).into())
            .await
            .map_err(|e| StoreError::Io(std::io::Error::other(format!("S3 put: {e}"))))?;
        debug!(hash = %hash, "Stored narinfo in S3");
        Ok(())
    }

    async fn delete_narinfo_record(&self, hash: &str) -> Result<(), StoreError> {
        self.delete_object(&Path::from(format!("{hash}.narinfo"))).await
    }

    async fn delete_nar_record(&self, nar_path: &str) -> Result<(), StoreError> {
        self.delete_object(&Path::from(nar_path)).await
    }

    fn nar_ref_index(&self) -> &dyn NarRefIndex {
        self
    }

    async fn get_nar(&self, nar_path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        // ONE code path: the whole-value verb is the streaming verb drained.
        match self.get_nar_stream(nar_path).await? {
            Some(s) => Ok(Some(nar_stream::collect_nar(s, None).await?)),
            None => Ok(None),
        }
    }

    async fn put_nar(&self, nar_path: &str, data: &[u8]) -> Result<(), StoreError> {
        self.put_nar_stream(nar_path, &nar_stream::BytesNarSource::from(data)).await
    }

    /// **O(constant).** Reads come off the object stream; writes go up as
    /// bounded multipart parts with at most [`S3_MAX_INFLIGHT_PARTS`] in flight.
    fn nar_residency(&self) -> NarResidency {
        NarResidency::Streaming
    }

    async fn get_nar_stream(&self, nar_path: &str) -> Result<Option<NarStream>, StoreError> {
        let path = Path::from(nar_path);
        match self.store.get(&path).await {
            Ok(result) => Ok(Some(
                result
                    .into_stream()
                    .map(|r| {
                        r.map_err(|e| {
                            StoreError::Io(std::io::Error::other(format!("S3 read: {e}")))
                        })
                    })
                    .boxed(),
            )),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(StoreError::Io(std::io::Error::other(format!("S3 get: {e}")))),
        }
    }

    /// Upload as a bounded multipart, aborting on any fault.
    ///
    /// The abort matters for the same reason the local tier renames: a
    /// half-finished multipart is not a truncated object (S3 only publishes on
    /// `complete`), but it *is* billable storage that lingers until a lifecycle
    /// rule reaps it. Aborting turns a failed push into nothing at all.
    async fn put_nar_stream(&self, nar_path: &str, src: &dyn NarSource) -> Result<(), StoreError> {
        let path = Path::from(nar_path);
        let upload = self
            .store
            .put_multipart(&path)
            .await
            .map_err(|e| StoreError::Io(std::io::Error::other(format!("S3 multipart init: {e}"))))?;
        let mut writer = WriteMultipart::new_with_chunk_size(upload, S3_PART_BYTES);

        let mut written: u64 = 0;
        let pump = async {
            let mut stream = src.open().await?;
            while let Some(chunk) = stream.next().await {
                let chunk: Bytes = chunk?;
                // Back-pressure BEFORE buffering the next part, so the number of
                // parts resident is capped rather than "however fast the source
                // produces". Without this, `write`/`put` spawn uploads eagerly
                // and a fast local source would queue the whole NAR in flight.
                writer.wait_for_capacity(S3_MAX_INFLIGHT_PARTS).await.map_err(|e| {
                    StoreError::Io(std::io::Error::other(format!("S3 multipart backpressure: {e}")))
                })?;
                written += chunk.len() as u64;
                writer.put(chunk);
            }
            Ok::<(), StoreError>(())
        }
        .await;

        match pump {
            Ok(()) => {
                writer.finish().await.map_err(|e| {
                    StoreError::Io(std::io::Error::other(format!("S3 multipart complete: {e}")))
                })?;
                debug!(path = %nar_path, size = written, "Stored NAR in S3 (multipart)");
                Ok(())
            }
            Err(e) => {
                if let Err(abort_err) = writer.abort().await {
                    warn!(
                        path = %nar_path, error = %abort_err,
                        "S3 multipart abort failed — orphaned parts may linger until a \
                         lifecycle rule reaps them",
                    );
                }
                Err(e)
            }
        }
    }

    async fn list_narinfos(&self) -> Result<Vec<String>, StoreError> {
        use futures::TryStreamExt;

        let prefix = Path::from("");
        let mut hashes = Vec::new();

        let mut list_stream = self.store.list(Some(&prefix));

        while let Some(meta) = list_stream
            .try_next()
            .await
            .map_err(|e| StoreError::Io(std::io::Error::other(format!("S3 list: {e}"))))?
        {
            let key = meta.location.to_string();
            if let Some(hash) = key.strip_suffix(".narinfo") {
                hashes.push(hash.to_string());
            }
        }

        debug!(count = hashes.len(), "Listed narinfos from S3");
        Ok(hashes)
    }
}

/// The reverse index as one zero-byte object per edge, under `nar-refs/`.
///
/// A `LIST` bounded by the edge prefix *is* the referrer set. Recording is a
/// blind `PUT` of a key that names its own content, so it is idempotent and two
/// concurrent pushes cannot lose an edge — which a read-modify-write of a
/// set-valued object could, and S3 gives no compare-and-swap to prevent it with.
#[async_trait]
impl NarRefIndex for S3Storage {
    async fn record(&self, nar_path: &str, hash: &str) -> Result<(), StoreError> {
        let path = Path::from(NarRefKey { nar_path, hash }.to_string());
        self.store
            .put(&path, Bytes::new().into())
            .await
            .map_err(|e| StoreError::Io(std::io::Error::other(format!("S3 put nar-ref: {e}"))))?;
        Ok(())
    }

    async fn forget(&self, nar_path: &str, hash: &str) -> Result<(), StoreError> {
        self.delete_object(&Path::from(NarRefKey { nar_path, hash }.to_string())).await
    }

    async fn referrers(&self, nar_path: &str) -> Result<Vec<String>, StoreError> {
        use futures::TryStreamExt;

        let scan = NarRefScan { nar_path };
        let prefix = Path::from(scan.to_string());
        let mut list = self.store.list(Some(&prefix));
        let mut hashes = Vec::new();
        while let Some(meta) = list.try_next().await.map_err(|e| {
            StoreError::Io(std::io::Error::other(format!("S3 list nar-refs: {e}")))
        })? {
            let key = meta.location.to_string();
            if let Some(hash) = referrer_of(&scan, &key) {
                hashes.push(hash.to_string());
            }
        }
        hashes.sort();
        hashes.dedup();
        Ok(hashes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_storage_accessors() {
        let storage = S3Storage::new(
            "my-bucket".to_string(),
            "us-east-1".to_string(),
            Some("http://localhost:9000".to_string()),
        )
        .unwrap();
        assert_eq!(storage.bucket(), "my-bucket");
        assert_eq!(storage.region(), "us-east-1");
        assert_eq!(storage.endpoint(), Some("http://localhost:9000"));
    }

    #[test]
    fn s3_storage_no_endpoint() {
        // This may fail without valid AWS creds — skip in CI
        let result = S3Storage::new("bucket".to_string(), "eu-west-1".to_string(), None);
        // Just verify construction doesn't panic
        assert!(result.is_ok());
    }

    const NARINFO: &str = "StorePath: /nix/store/abc-hello\nURL: nar/narhash.nar.xz\n\
                           Compression: xz\nFileHash: sha256:aaa\nFileSize: 100\n\
                           NarHash: sha256:bbb\nNarSize: 200\nReferences: \n";
    const ADVERTISED: &str = "nar/narhash.nar.xz";

    /// `delete` takes the object the narinfo names, and leaves a
    /// store-hash-shaped key it never named.
    #[tokio::test]
    async fn delete_resolves_the_nar_from_the_narinfo_instead_of_guessing() {
        let s3 = S3Storage::in_memory();
        s3.put_narinfo("storehash", NARINFO).await.unwrap();
        s3.put_nar(ADVERTISED, b"the real nar").await.unwrap();
        s3.put_nar("nar/storehash.nar.zst", b"someone else's nar").await.unwrap();

        s3.delete("storehash").await.unwrap();

        assert!(s3.get_narinfo("storehash").await.unwrap().is_none());
        assert!(s3.get_nar(ADVERTISED).await.unwrap().is_none());
        assert_eq!(
            s3.get_nar("nar/storehash.nar.zst").await.unwrap().unwrap(),
            b"someone else's nar",
        );
    }

    /// The `LIST`-driven reverse index round-trips, and a co-referenced NAR
    /// survives the first delete.
    #[tokio::test]
    async fn the_object_index_holds_every_referrer() {
        let s3 = S3Storage::in_memory();
        s3.put_narinfo("pathA", NARINFO).await.unwrap();
        s3.put_narinfo("pathB", NARINFO).await.unwrap();
        s3.put_nar(ADVERTISED, b"shared").await.unwrap();
        assert_eq!(
            s3.nar_ref_index().referrers(ADVERTISED).await.unwrap(),
            vec!["pathA".to_string(), "pathB".to_string()],
        );

        s3.delete("pathA").await.unwrap();
        assert_eq!(
            s3.nar_ref_index().referrers(ADVERTISED).await.unwrap(),
            vec!["pathB".to_string()],
        );
        assert!(s3.get_nar(ADVERTISED).await.unwrap().is_some(), "pathB still advertises it");

        s3.delete("pathB").await.unwrap();
        assert!(s3.nar_ref_index().referrers(ADVERTISED).await.unwrap().is_empty());
        assert!(s3.get_nar(ADVERTISED).await.unwrap().is_none());
    }

    /// Edge objects live under `nar-refs/`, which must not be mistaken for a
    /// narinfo by the bucket-wide listing.
    #[tokio::test]
    async fn edge_objects_do_not_pollute_the_narinfo_listing() {
        let s3 = S3Storage::in_memory();
        s3.put_narinfo("storehash", NARINFO).await.unwrap();
        assert_eq!(s3.list_narinfos().await.unwrap(), vec!["storehash".to_string()]);
    }
}
