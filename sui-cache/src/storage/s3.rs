//! S3-compatible object storage backend (stub).
//!
//! This module will implement the `StorageBackend` trait using the S3 API
//! (PutObject, GetObject, HeadObject, ListObjectsV2, DeleteObject).
//! Compatible with AWS S3, CloudFlare R2, MinIO, Backblaze B2, and GCS
//! (via S3-compatible endpoint).
//!
//! For now, this is a placeholder that returns `NotImplemented` errors.

use async_trait::async_trait;

use super::StorageBackend;
use crate::CacheError;

/// S3-compatible object storage backend.
#[derive(Debug, Clone)]
pub struct S3Storage {
    bucket: String,
    region: String,
    endpoint: Option<String>,
}

impl S3Storage {
    /// Create a new S3 storage backend.
    #[must_use]
    pub fn new(bucket: String, region: String, endpoint: Option<String>) -> Self {
        Self {
            bucket,
            region,
            endpoint,
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
}

#[async_trait]
impl StorageBackend for S3Storage {
    async fn get_narinfo(&self, _hash: &str) -> Result<Option<String>, CacheError> {
        Err(CacheError::NotImplemented("S3 backend not yet implemented"))
    }

    async fn put_narinfo(&self, _hash: &str, _content: &str) -> Result<(), CacheError> {
        Err(CacheError::NotImplemented("S3 backend not yet implemented"))
    }

    async fn get_nar(&self, _path: &str) -> Result<Option<Vec<u8>>, CacheError> {
        Err(CacheError::NotImplemented("S3 backend not yet implemented"))
    }

    async fn put_nar(&self, _path: &str, _data: &[u8]) -> Result<(), CacheError> {
        Err(CacheError::NotImplemented("S3 backend not yet implemented"))
    }

    async fn delete(&self, _hash: &str) -> Result<(), CacheError> {
        Err(CacheError::NotImplemented("S3 backend not yet implemented"))
    }

    async fn list_narinfos(&self) -> Result<Vec<String>, CacheError> {
        Err(CacheError::NotImplemented("S3 backend not yet implemented"))
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
        );
        assert_eq!(storage.bucket(), "my-bucket");
        assert_eq!(storage.region(), "us-east-1");
        assert_eq!(storage.endpoint(), Some("http://localhost:9000"));
    }

    #[test]
    fn s3_storage_no_endpoint() {
        let storage = S3Storage::new("bucket".to_string(), "eu-west-1".to_string(), None);
        assert!(storage.endpoint().is_none());
    }

    #[tokio::test]
    async fn s3_get_narinfo_returns_not_implemented() {
        let storage = S3Storage::new("b".to_string(), "r".to_string(), None);
        let result = storage.get_narinfo("hash").await;
        assert!(result.is_err());
        assert!(matches!(result, Err(CacheError::NotImplemented(_))));
    }

    #[tokio::test]
    async fn s3_put_narinfo_returns_not_implemented() {
        let storage = S3Storage::new("b".to_string(), "r".to_string(), None);
        let result = storage.put_narinfo("hash", "content").await;
        assert!(matches!(result, Err(CacheError::NotImplemented(_))));
    }

    #[tokio::test]
    async fn s3_get_nar_returns_not_implemented() {
        let storage = S3Storage::new("b".to_string(), "r".to_string(), None);
        let result = storage.get_nar("nar/x.nar.xz").await;
        assert!(matches!(result, Err(CacheError::NotImplemented(_))));
    }

    #[tokio::test]
    async fn s3_put_nar_returns_not_implemented() {
        let storage = S3Storage::new("b".to_string(), "r".to_string(), None);
        let result = storage.put_nar("nar/x.nar.xz", b"data").await;
        assert!(matches!(result, Err(CacheError::NotImplemented(_))));
    }

    #[tokio::test]
    async fn s3_delete_returns_not_implemented() {
        let storage = S3Storage::new("b".to_string(), "r".to_string(), None);
        let result = storage.delete("hash").await;
        assert!(matches!(result, Err(CacheError::NotImplemented(_))));
    }

    #[tokio::test]
    async fn s3_list_returns_not_implemented() {
        let storage = S3Storage::new("b".to_string(), "r".to_string(), None);
        let result = storage.list_narinfos().await;
        assert!(matches!(result, Err(CacheError::NotImplemented(_))));
    }
}
