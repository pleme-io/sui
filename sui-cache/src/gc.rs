//! Garbage collection for the binary cache.
//!
//! Walks all narinfo entries, keeps those in the roots set,
//! and deletes the rest.

use std::collections::HashSet;

use crate::storage::StorageBackend;
use crate::CacheError;

/// Result of a garbage collection run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcResult {
    /// Number of store paths deleted.
    pub paths_deleted: usize,
    /// Total bytes freed (estimated from narinfo FileSize + narinfo text).
    pub bytes_freed: u64,
}

impl std::fmt::Display for GcResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GC: {} paths deleted, {} bytes freed",
            self.paths_deleted, self.bytes_freed
        )
    }
}

/// Run garbage collection on the cache.
///
/// Enumerates all narinfo hashes in the storage backend, keeps
/// those present in `roots`, and deletes the rest.
///
/// `roots` contains the 32-character store path hashes that should be kept.
pub async fn collect_garbage(
    storage: &dyn StorageBackend,
    roots: &[String],
) -> Result<GcResult, CacheError> {
    let all = storage.list_narinfos().await?;
    let keep: HashSet<&str> = roots.iter().map(String::as_str).collect();

    let mut result = GcResult::default();

    for hash in &all {
        if !keep.contains(hash.as_str()) {
            // Try to read the narinfo to estimate freed bytes.
            if let Ok(Some(content)) = storage.get_narinfo(hash).await {
                if let Ok(info) = sui_compat::narinfo::NarInfo::parse(&content) {
                    result.bytes_freed += info.file_size;
                }
                // Also account for the narinfo text itself.
                result.bytes_freed += content.len() as u64;
            }
            storage.delete(hash).await?;
            result.paths_deleted += 1;
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::local::LocalStorage;

    fn make_narinfo(hash: &str, file_size: u64) -> String {
        format!(
            "StorePath: /nix/store/{hash}-pkg\n\
             URL: nar/{hash}.nar.xz\n\
             Compression: xz\n\
             FileHash: sha256:aaaa\n\
             FileSize: {file_size}\n\
             NarHash: sha256:bbbb\n\
             NarSize: 5000\n\
             References: \n"
        )
    }

    #[tokio::test]
    async fn gc_empty_cache() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let result = collect_garbage(&storage, &[]).await.unwrap();
        assert_eq!(result.paths_deleted, 0);
        assert_eq!(result.bytes_freed, 0);
    }

    #[tokio::test]
    async fn gc_keeps_roots() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());

        storage.put_narinfo("aaa", &make_narinfo("aaa", 100)).await.unwrap();
        storage.put_narinfo("bbb", &make_narinfo("bbb", 200)).await.unwrap();

        let roots = vec!["aaa".to_string(), "bbb".to_string()];
        let result = collect_garbage(&storage, &roots).await.unwrap();

        assert_eq!(result.paths_deleted, 0);
        // Both should still exist.
        assert!(storage.get_narinfo("aaa").await.unwrap().is_some());
        assert!(storage.get_narinfo("bbb").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn gc_deletes_non_roots() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());

        storage.put_narinfo("keep", &make_narinfo("keep", 100)).await.unwrap();
        storage.put_narinfo("drop", &make_narinfo("drop", 500)).await.unwrap();
        storage.put_nar("nar/drop.nar.xz", b"nar data").await.unwrap();

        let roots = vec!["keep".to_string()];
        let result = collect_garbage(&storage, &roots).await.unwrap();

        assert_eq!(result.paths_deleted, 1);
        assert!(result.bytes_freed > 0);

        // "keep" should still exist, "drop" should be gone.
        assert!(storage.get_narinfo("keep").await.unwrap().is_some());
        assert!(storage.get_narinfo("drop").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn gc_deletes_all_when_no_roots() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());

        storage.put_narinfo("aaa", &make_narinfo("aaa", 100)).await.unwrap();
        storage.put_narinfo("bbb", &make_narinfo("bbb", 200)).await.unwrap();
        storage.put_narinfo("ccc", &make_narinfo("ccc", 300)).await.unwrap();

        let result = collect_garbage(&storage, &[]).await.unwrap();

        assert_eq!(result.paths_deleted, 3);
        assert!(storage.list_narinfos().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn gc_result_display() {
        let result = GcResult {
            paths_deleted: 5,
            bytes_freed: 1024,
        };
        let s = format!("{result}");
        assert!(s.contains("5"));
        assert!(s.contains("1024"));
    }

    #[tokio::test]
    async fn gc_accounts_for_file_size_and_narinfo_text() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());

        let narinfo_text = make_narinfo("abc", 1000);
        storage.put_narinfo("abc", &narinfo_text).await.unwrap();

        let result = collect_garbage(&storage, &[]).await.unwrap();
        // Should include FileSize (1000) + narinfo text length.
        assert_eq!(result.bytes_freed, 1000 + narinfo_text.len() as u64);
    }
}
