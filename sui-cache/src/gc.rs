//! Garbage collection for the binary cache.
//!
//! Walks all narinfo entries, keeps those in the roots set,
//! and deletes the rest.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;

use crate::CacheError;
use crate::LocalStorage;
use crate::StorageBackend;
use crate::advertised_nar_url;

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvictResult {
    pub evicted: Vec<String>,
    pub nars_evicted: usize,
    pub bytes_freed: u64,
    pub bytes_after: u64,
    pub failed: usize,
}

pub async fn evict_to_size(storage: &LocalStorage, max_bytes: NonZeroU64) -> Result<EvictResult, CacheError> {
    let usage = storage.usage().await?;
    let total = usage.total_bytes();
    let mut result = EvictResult { bytes_after: total, ..EvictResult::default() };
    if total <= max_bytes.get() {
        return Ok(result);
    }

    let mut advertisers: HashMap<String, Vec<(String, u64)>> = HashMap::new();
    for hash in storage.list_narinfos().await? {
        if let Some(text) = storage.get_narinfo(&hash).await?
            && let Some(nar_path) = advertised_nar_url(&text)
        {
            advertisers.entry(nar_path).or_default().push((hash, text.len() as u64));
        }
    }

    let mut nars = usage.nars;
    nars.sort_by(|a, b| a.last_used.cmp(&b.last_used).then_with(|| a.nar_path.cmp(&b.nar_path)));
    for nar in nars {
        if result.bytes_after <= max_bytes.get() {
            break;
        }
        let mut freed = 0;
        let mut kept_an_advertiser = false;
        for (hash, narinfo_bytes) in advertisers.remove(&nar.nar_path).unwrap_or_default() {
            match storage.delete(&hash).await {
                Ok(()) => {
                    freed += narinfo_bytes;
                    result.evicted.push(hash);
                }
                Err(e) => {
                    tracing::warn!(hash = %hash, error = %e, "evict: narinfo not removed, its NAR stays");
                    kept_an_advertiser = true;
                    result.failed += 1;
                }
            }
        }
        if !kept_an_advertiser {
            match storage.delete_nar_record(&nar.nar_path).await {
                Ok(()) => {
                    freed += nar.bytes;
                    result.nars_evicted += 1;
                }
                Err(e) => {
                    tracing::warn!(nar = %nar.nar_path, error = %e, "evict: NAR not removed");
                    result.failed += 1;
                }
            }
        }
        result.bytes_freed += freed;
        result.bytes_after = result.bytes_after.saturating_sub(freed);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalStorage;

    fn narinfo_for_nar(hash: &str, nar_path: &str) -> String {
        format!(
            "StorePath: /nix/store/{hash}-pkg\nURL: {nar_path}\nCompression: zstd\nFileHash: sha256:aaaa\nFileSize: 1000\nNarHash: sha256:bbbb\nNarSize: 5000\nReferences: \n"
        )
    }

    fn age(storage: &LocalStorage, nar_path: &str, seconds_ago: u64) {
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(seconds_ago);
        std::fs::File::open(storage.root().join(nar_path))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_accessed(then).set_modified(then))
            .unwrap();
    }

    async fn cached(storage: &LocalStorage, hash: &str, nar_path: &str, seconds_ago: u64) {
        storage.put_nar(nar_path, &[0u8; 1000]).await.unwrap();
        storage
            .put_narinfo(&sui_castore::AdmittedNarInfo::unchecked_fixture(hash, &narinfo_for_nar(hash, nar_path)))
            .await
            .unwrap();
        age(storage, nar_path, seconds_ago);
    }

    fn bound(bytes: u64) -> NonZeroU64 {
        NonZeroU64::new(bytes).unwrap()
    }

    fn present(storage: &LocalStorage, nar_path: &str) -> bool {
        storage.root().join(nar_path).exists()
    }

    #[tokio::test]
    async fn the_least_recently_used_nar_leaves_with_its_narinfo_until_the_cache_fits() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        cached(&storage, "old", "nar/old.nar.zst", 300).await;
        cached(&storage, "mid", "nar/mid.nar.zst", 200).await;
        cached(&storage, "new", "nar/new.nar.zst", 100).await;
        let max = storage.usage().await.unwrap().total_bytes() - 1;

        let result = evict_to_size(&storage, bound(max)).await.unwrap();

        assert_eq!(result.evicted, ["old"]);
        assert_eq!((result.nars_evicted, result.failed), (1, 0));
        assert!(result.bytes_after <= max, "{result:?}");
        assert_eq!(result.bytes_after, storage.usage().await.unwrap().total_bytes());
        assert!(storage.get_narinfo("old").await.unwrap().is_none());
        assert!(!present(&storage, "nar/old.nar.zst"));
        assert!(storage.get_narinfo("mid").await.unwrap().is_some() && present(&storage, "nar/mid.nar.zst"));
        assert!(storage.get_narinfo("new").await.unwrap().is_some() && present(&storage, "nar/new.nar.zst"));
    }

    #[tokio::test]
    async fn a_nar_that_was_read_outlives_one_written_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        cached(&storage, "read", "nar/read.nar.zst", 300).await;
        cached(&storage, "idle", "nar/idle.nar.zst", 200).await;
        assert!(storage.get_nar("nar/read.nar.zst").await.unwrap().is_some());
        let max = storage.usage().await.unwrap().total_bytes() - 1;

        let result = evict_to_size(&storage, bound(max)).await.unwrap();

        assert_eq!(result.evicted, ["idle"]);
        assert!(present(&storage, "nar/read.nar.zst"));
    }

    #[tokio::test]
    async fn a_shared_nar_leaves_with_every_narinfo_that_advertises_it() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        cached(&storage, "one", "nar/shared.nar.zst", 300).await;
        cached(&storage, "two", "nar/shared.nar.zst", 300).await;
        cached(&storage, "other", "nar/other.nar.zst", 100).await;
        let max = storage.usage().await.unwrap().total_bytes() - 1;

        let mut result = evict_to_size(&storage, bound(max)).await.unwrap();
        result.evicted.sort();

        assert_eq!(result.evicted, ["one", "two"]);
        assert!(!present(&storage, "nar/shared.nar.zst"));
        assert!(storage.get_narinfo("other").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_store_without_its_reverse_index_strands_no_narinfo() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        cached(&storage, "old", "nar/old.nar.zst", 300).await;
        cached(&storage, "new", "nar/new.nar.zst", 100).await;
        std::fs::remove_dir_all(dir.path().join("nar-refs")).unwrap();
        let max = storage.usage().await.unwrap().total_bytes() - 1;

        evict_to_size(&storage, bound(max)).await.unwrap();

        assert!(storage.get_narinfo("old").await.unwrap().is_none());
        for hash in storage.list_narinfos().await.unwrap() {
            let nar = storage.advertised_nar(&hash).await.unwrap().unwrap();
            assert!(present(&storage, &nar), "{hash} advertises {nar}, which is gone");
        }
    }

    #[tokio::test]
    async fn a_cache_inside_its_bound_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        cached(&storage, "old", "nar/old.nar.zst", 300).await;
        let total = storage.usage().await.unwrap().total_bytes();

        let result = evict_to_size(&storage, bound(total)).await.unwrap();

        assert_eq!(result, EvictResult { bytes_after: total, ..EvictResult::default() });
        assert!(present(&storage, "nar/old.nar.zst"));
    }

    #[tokio::test]
    async fn an_orphan_nar_counts_against_the_bound_and_a_scratch_file_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        cached(&storage, "new", "nar/new.nar.zst", 100).await;
        storage.put_nar("nar/orphan.nar.zst", &[0u8; 1000]).await.unwrap();
        age(&storage, "nar/orphan.nar.zst", 300);
        std::fs::write(dir.path().join("nar/x.nar.zst.1.2.tmp"), [0u8; 4096]).unwrap();
        let usage = storage.usage().await.unwrap();
        assert_eq!(usage.nars.len(), 2);

        let result = evict_to_size(&storage, bound(usage.total_bytes() - 1)).await.unwrap();

        assert_eq!((result.evicted.len(), result.nars_evicted), (0, 1));
        assert!(!present(&storage, "nar/orphan.nar.zst"));
        assert!(present(&storage, "nar/new.nar.zst"));
    }

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

        storage
            .put_narinfo(&sui_castore::AdmittedNarInfo::unchecked_fixture("aaa", &make_narinfo("aaa", 100)))
            .await
            .unwrap();
        storage
            .put_narinfo(&sui_castore::AdmittedNarInfo::unchecked_fixture("bbb", &make_narinfo("bbb", 200)))
            .await
            .unwrap();

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

        storage
            .put_narinfo(&sui_castore::AdmittedNarInfo::unchecked_fixture("keep", &make_narinfo("keep", 100)))
            .await
            .unwrap();
        storage
            .put_narinfo(&sui_castore::AdmittedNarInfo::unchecked_fixture("drop", &make_narinfo("drop", 500)))
            .await
            .unwrap();
        storage
            .put_nar("nar/drop.nar.xz", b"nar data")
            .await
            .unwrap();

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

        storage
            .put_narinfo(&sui_castore::AdmittedNarInfo::unchecked_fixture("aaa", &make_narinfo("aaa", 100)))
            .await
            .unwrap();
        storage
            .put_narinfo(&sui_castore::AdmittedNarInfo::unchecked_fixture("bbb", &make_narinfo("bbb", 200)))
            .await
            .unwrap();
        storage
            .put_narinfo(&sui_castore::AdmittedNarInfo::unchecked_fixture("ccc", &make_narinfo("ccc", 300)))
            .await
            .unwrap();

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
        storage.put_narinfo(&sui_castore::AdmittedNarInfo::unchecked_fixture("abc", &narinfo_text)).await.unwrap();

        let result = collect_garbage(&storage, &[]).await.unwrap();
        // Should include FileSize (1000) + narinfo text length.
        assert_eq!(result.bytes_freed, 1000 + narinfo_text.len() as u64);
    }
}
