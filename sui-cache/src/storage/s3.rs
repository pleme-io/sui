//! S3-compatible object storage backend.
//!
//! Uses `object_store` crate — works with AWS S3, CloudFlare R2, MinIO,
//! RustFS, Backblaze B2, and any S3-compatible endpoint.
//!
//! Breathable by design: S3 provides infinite elasticity.
//! Combined with redb for ephemeral local metadata index.

use async_trait::async_trait;
use bytes::Bytes;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::ObjectStore;
use tracing::{debug, warn};

use super::StorageBackend;
use crate::CacheError;

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
    pub fn new(bucket: String, region: String, endpoint: Option<String>) -> Result<Self, CacheError> {
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&bucket)
            .with_region(&region);

        if let Some(ep) = &endpoint {
            builder = builder.with_endpoint(ep).with_allow_http(true);
        }

        let store = builder
            .build()
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("S3 init failed: {e}"))))?;

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
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, CacheError> {
        let path = Path::from(format!("{hash}.narinfo"));
        match self.store.get(&path).await {
            Ok(result) => {
                let bytes = result
                    .bytes()
                    .await
                    .map_err(|e| CacheError::Io(std::io::Error::other(format!("S3 read: {e}"))))?;
                Ok(Some(
                    String::from_utf8(bytes.to_vec())
                        .map_err(|e| CacheError::NarInfo(format!("Invalid UTF-8: {e}")))?,
                ))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(CacheError::Io(std::io::Error::other(format!("S3 get: {e}")))),
        }
    }

    async fn put_narinfo(&self, hash: &str, content: &str) -> Result<(), CacheError> {
        let path = Path::from(format!("{hash}.narinfo"));
        self.store
            .put(&path, Bytes::from(content.to_string()).into())
            .await
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("S3 put: {e}"))))?;
        debug!(hash = %hash, "Stored narinfo in S3");
        Ok(())
    }

    async fn get_nar(&self, nar_path: &str) -> Result<Option<Vec<u8>>, CacheError> {
        let path = Path::from(nar_path);
        match self.store.get(&path).await {
            Ok(result) => {
                let bytes = result
                    .bytes()
                    .await
                    .map_err(|e| CacheError::Io(std::io::Error::other(format!("S3 read: {e}"))))?;
                Ok(Some(bytes.to_vec()))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(CacheError::Io(std::io::Error::other(format!("S3 get: {e}")))),
        }
    }

    async fn put_nar(&self, nar_path: &str, data: &[u8]) -> Result<(), CacheError> {
        let path = Path::from(nar_path);
        self.store
            .put(&path, Bytes::from(data.to_vec()).into())
            .await
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("S3 put: {e}"))))?;
        debug!(path = %nar_path, size = data.len(), "Stored NAR in S3");
        Ok(())
    }

    async fn delete(&self, hash: &str) -> Result<(), CacheError> {
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

    async fn list_narinfos(&self) -> Result<Vec<String>, CacheError> {
        use futures::TryStreamExt;

        let prefix = Path::from("");
        let mut hashes = Vec::new();

        let mut list_stream = self.store.list(Some(&prefix));

        while let Some(meta) = list_stream
            .try_next()
            .await
            .map_err(|e| CacheError::Io(std::io::Error::other(format!("S3 list: {e}"))))?
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
