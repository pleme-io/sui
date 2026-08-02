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

    async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), StoreError> {
        let path = Path::from(format!("{hash}.narinfo"));
        self.store
            .put(&path, Bytes::from(content.to_string()).into())
            .await
            .map_err(|e| StoreError::Io(std::io::Error::other(format!("S3 put: {e}"))))?;
        debug!(hash = %hash, "Stored narinfo in S3");
        Ok(())
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

    async fn delete(&self, hash: &str) -> Result<(), StoreError> {
        // Delete narinfo
        let narinfo_path = Path::from(format!("{hash}.narinfo"));
        if let Err(e) = self.store.delete(&narinfo_path).await {
            warn!(hash = %hash, error = %e, "Failed to delete narinfo from S3");
        }

        // Try to delete NAR blob (common path patterns)
        for ext in &["nar.xz", "nar.zst", "nar"] {
            let nar_path = Path::from(format!("nar/{hash}.{ext}"));
            let _ = self.store.delete(&nar_path).await;
        }

        debug!(hash = %hash, "Deleted from S3");
        Ok(())
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
}
