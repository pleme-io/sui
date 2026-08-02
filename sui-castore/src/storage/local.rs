//! Local filesystem storage backend.
//!
//! Layout:
//! ```text
//! <root>/
//!   <hash>.narinfo          -- text narinfo metadata
//!   nar/
//!     <narhash>.nar.xz      -- compressed NAR blobs
//!   nar-refs/
//!     nar/<narhash>.nar.xz/
//!       <hash>              -- empty file: "this store hash advertises that NAR"
//! ```
//!
//! `nar-refs/` mirrors the NAR key space one level down, one empty file per
//! reverse edge (see [`nar_refs`](super::nar_refs)). A directory listing *is*
//! the referrer set, so a lookup is one `read_dir` of a directory holding as
//! many entries as there are referrers — normally exactly one. Recording an edge
//! is a blind `create`, so two writers cannot lose each other's edge.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures::StreamExt;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use super::nar_refs::{NarRefIndex, NarRefScan};
use super::nar_stream::{self, NarSource, NarStream};
use super::{NarResidency, StorageBackend};
use crate::StoreError;

/// Filesystem-backed binary cache storage.
#[derive(Debug, Clone)]
pub struct LocalStorage {
    /// Root directory for all cache data.
    root: PathBuf,
}

impl LocalStorage {
    /// Create a new local storage backend rooted at `path`.
    ///
    /// The directory structure is created lazily on first write.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { root: path.into() }
    }

    /// Return the root path.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Ensure a directory exists.
    async fn ensure_dir(&self, path: &Path) -> Result<(), StoreError> {
        if !path.exists() {
            fs::create_dir_all(path).await.map_err(StoreError::Io)?;
        }
        Ok(())
    }

    /// Path to a narinfo file.
    fn narinfo_path(&self, hash: &str) -> PathBuf {
        self.root.join(format!("{hash}.narinfo"))
    }

    /// Path to a NAR blob. The `nar_path` is a relative path like
    /// `nar/xyz.nar.xz`.
    fn nar_blob_path(&self, nar_path: &str) -> PathBuf {
        self.root.join(nar_path)
    }

    /// Directory holding one empty file per narinfo advertising `nar_path`.
    fn nar_ref_dir(&self, nar_path: &str) -> PathBuf {
        self.root.join(NarRefScan { nar_path }.to_string())
    }

    /// A unique scratch path beside `final_path`, for the write-then-rename in
    /// [`put_nar_stream`](StorageBackend::put_nar_stream).
    ///
    /// Unique **per write, not per key**: two pods (or two tasks) racing to push
    /// the same content-addressed key is the normal case, and a shared temp name
    /// would have them interleave chunks into one file and rename a spliced NAR
    /// into place. Process id + a monotonic counter makes that unrepresentable
    /// without a lock. Beside the target, never in `/tmp`, so the rename stays
    /// on one filesystem and therefore atomic.
    fn temp_sibling(final_path: &Path) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let mut name = final_path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{pid}.{n}.tmp"));
        final_path.with_file_name(name)
    }
}

#[async_trait]
impl StorageBackend for LocalStorage {
    async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, StoreError> {
        let path = self.narinfo_path(hash);
        match fs::read_to_string(&path).await {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    async fn put_narinfo_record(&self, hash: &str, content: &str) -> Result<(), StoreError> {
        self.ensure_dir(&self.root).await?;
        let path = self.narinfo_path(hash);
        fs::write(&path, content).await.map_err(StoreError::Io)
    }

    async fn delete_narinfo_record(&self, hash: &str) -> Result<(), StoreError> {
        match fs::remove_file(self.narinfo_path(hash)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    async fn delete_nar_record(&self, nar_path: &str) -> Result<(), StoreError> {
        match fs::remove_file(self.nar_blob_path(nar_path)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    fn nar_ref_index(&self) -> &dyn NarRefIndex {
        self
    }

    async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        // ONE code path: the whole-value verb is the streaming verb drained.
        // Two independent readers would be two chances to diverge.
        match self.get_nar_stream(path).await? {
            Some(s) => Ok(Some(nar_stream::collect_nar(s, None).await?)),
            None => Ok(None),
        }
    }

    async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), StoreError> {
        self.put_nar_stream(path, &nar_stream::BytesNarSource::from(data)).await
    }

    /// **O(chunk).** Reads and writes go through a bounded buffer; the file's
    /// size never appears in this process's heap.
    fn nar_residency(&self) -> NarResidency {
        NarResidency::Streaming
    }

    async fn get_nar_stream(&self, path: &str) -> Result<Option<NarStream>, StoreError> {
        let full = self.nar_blob_path(path);
        match fs::File::open(&full).await {
            Ok(f) => Ok(Some(nar_stream::file_stream(f))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    /// Write chunk-by-chunk **through a temp file, then rename**.
    ///
    /// The rename is not tidiness: a streamed write is no longer atomic the way
    /// a single `write(2)` of a whole buffer was, so a crash (or an `ENOSPC`
    /// three chunks in — the exact live failure on the full tmpfs) would
    /// otherwise leave a *truncated* NAR at the real path, and a truncated NAR
    /// is silent corruption, strictly worse than the OOM being fixed. Writing
    /// aside and renaming means a partial write leaves nothing: the next read is
    /// a clean miss and the client rebuilds.
    async fn put_nar_stream(&self, path: &str, src: &dyn NarSource) -> Result<(), StoreError> {
        let full = self.nar_blob_path(path);
        if let Some(parent) = full.parent() {
            self.ensure_dir(parent).await?;
        }
        let tmp = Self::temp_sibling(&full);

        // Anything that leaves this block early must not leave the temp file
        // behind, so the result is captured and the cleanup runs unconditionally.
        let write = async {
            let mut f = fs::File::create(&tmp).await.map_err(StoreError::Io)?;
            let mut stream = src.open().await?;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                f.write_all(&chunk).await.map_err(StoreError::Io)?;
            }
            f.flush().await.map_err(StoreError::Io)?;
            drop(f);
            fs::rename(&tmp, &full).await.map_err(StoreError::Io)
        }
        .await;

        if write.is_err() {
            let _ = fs::remove_file(&tmp).await;
        }
        write
    }

    async fn list_narinfos(&self) -> Result<Vec<String>, StoreError> {
        let mut hashes = Vec::new();
        if !self.root.exists() {
            return Ok(hashes);
        }
        let mut entries = fs::read_dir(&self.root).await.map_err(StoreError::Io)?;
        while let Some(entry) = entries.next_entry().await.map_err(StoreError::Io)? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(hash) = name.strip_suffix(".narinfo") {
                hashes.push(hash.to_string());
            }
        }
        Ok(hashes)
    }

    /// Complete L3 wipe: remove the entire cache directory (narinfos + the
    /// `nar/` blob subtree), reclaiming NAR bytes a per-hash `delete` cannot
    /// reach. The directory is re-created lazily on the next `put`. Returns
    /// the narinfo count removed.
    async fn wipe_all(&self) -> Result<usize, StoreError> {
        let n = self.list_narinfos().await?.len();
        if self.root.exists() {
            fs::remove_dir_all(&self.root).await.map_err(StoreError::Io)?;
        }
        Ok(n)
    }
}

/// The reverse index as a directory tree: one empty file per edge.
///
/// `record` is a blind `create` of a path that names its own content, so it is
/// idempotent and two writers racing on the same edge simply write the same
/// empty file. Nothing here reads a set to write it back, which is why
/// concurrent narinfo pushes cannot lose an edge.
#[async_trait]
impl NarRefIndex for LocalStorage {
    async fn record(&self, nar_path: &str, hash: &str) -> Result<(), StoreError> {
        let dir = self.nar_ref_dir(nar_path);
        self.ensure_dir(&dir).await?;
        fs::write(dir.join(hash), b"").await.map_err(StoreError::Io)
    }

    async fn forget(&self, nar_path: &str, hash: &str) -> Result<(), StoreError> {
        let dir = self.nar_ref_dir(nar_path);
        match fs::remove_file(dir.join(hash)).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(StoreError::Io(e)),
        }
        // Tidy the now-possibly-empty directory. `remove_dir` fails on a
        // non-empty directory, which is exactly the "another referrer is still
        // here" case, so the error is the answer and is discarded.
        let _ = fs::remove_dir(&dir).await;
        Ok(())
    }

    async fn referrers(&self, nar_path: &str) -> Result<Vec<String>, StoreError> {
        let dir = self.nar_ref_dir(nar_path);
        let mut entries = match fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StoreError::Io(e)),
        };
        let mut hashes = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(StoreError::Io)? {
            hashes.push(entry.file_name().to_string_lossy().into_owned());
        }
        hashes.sort();
        Ok(hashes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_missing_narinfo_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let result = storage.get_narinfo("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn put_and_get_narinfo() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let content = "StorePath: /nix/store/abc-hello\nURL: nar/abc.nar.xz\nCompression: xz\nFileHash: sha256:aaa\nFileSize: 100\nNarHash: sha256:bbb\nNarSize: 200\nReferences: \n";
        storage.put_narinfo("abc", content).await.unwrap();
        let retrieved = storage.get_narinfo("abc").await.unwrap().unwrap();
        assert_eq!(retrieved, content);
    }

    #[tokio::test]
    async fn get_missing_nar_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let result = storage.get_nar("nar/missing.nar.xz").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn put_and_get_nar() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let data = b"fake nar data";
        storage.put_nar("nar/abc.nar.xz", data).await.unwrap();
        let retrieved = storage.get_nar("nar/abc.nar.xz").await.unwrap().unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn list_narinfos_empty() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let hashes = storage.list_narinfos().await.unwrap();
        assert!(hashes.is_empty());
    }

    #[tokio::test]
    async fn list_narinfos_returns_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_narinfo("aaa", "content1").await.unwrap();
        storage.put_narinfo("bbb", "content2").await.unwrap();
        let mut hashes = storage.list_narinfos().await.unwrap();
        hashes.sort();
        assert_eq!(hashes, vec!["aaa", "bbb"]);
    }

    #[tokio::test]
    async fn list_narinfos_ignores_non_narinfo_files() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_narinfo("abc", "content").await.unwrap();
        // Write a non-narinfo file.
        fs::write(dir.path().join("readme.txt"), "hello")
            .await
            .unwrap();
        let hashes = storage.list_narinfos().await.unwrap();
        assert_eq!(hashes, vec!["abc"]);
    }

    #[tokio::test]
    async fn list_narinfos_on_nonexistent_dir() {
        let storage = LocalStorage::new("/tmp/sui-castore-test-nonexistent-dir-12345");
        let hashes = storage.list_narinfos().await.unwrap();
        assert!(hashes.is_empty());
    }

    #[tokio::test]
    async fn delete_removes_narinfo_and_nar() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());

        let narinfo = "StorePath: /nix/store/xyz-hello\nURL: nar/xyz.nar.xz\nCompression: xz\nFileHash: sha256:aaa\nFileSize: 100\nNarHash: sha256:bbb\nNarSize: 200\nReferences: \n";
        storage.put_narinfo("xyz", narinfo).await.unwrap();
        storage.put_nar("nar/xyz.nar.xz", b"nar data").await.unwrap();

        assert!(storage.get_narinfo("xyz").await.unwrap().is_some());
        assert!(storage.get_nar("nar/xyz.nar.xz").await.unwrap().is_some());

        storage.delete("xyz").await.unwrap();

        assert!(storage.get_narinfo("xyz").await.unwrap().is_none());
        assert!(storage.get_nar("nar/xyz.nar.xz").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.delete("nonexistent").await.unwrap();
    }

    // ── the on-disk reverse index ──────────────────────────────────────────

    /// A narinfo for a store path advertising `url`.
    fn narinfo_for(url: &str) -> String {
        format!(
            "StorePath: /nix/store/pkg\nURL: {url}\nCompression: xz\nFileHash: sha256:aaa\n\
             FileSize: 100\nNarHash: sha256:bbb\nNarSize: 200\nReferences: \n"
        )
    }

    /// The edge is a file whose path names the pair, so writing it twice is
    /// writing the same file twice and a listing is the referrer set.
    #[tokio::test]
    async fn the_index_is_a_directory_of_edge_files() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_narinfo("pathA", &narinfo_for("nar/shared.nar.xz")).await.unwrap();
        storage.put_narinfo("pathB", &narinfo_for("nar/shared.nar.xz")).await.unwrap();

        assert!(dir.path().join("nar-refs/nar/shared.nar.xz/pathA").exists());
        assert!(dir.path().join("nar-refs/nar/shared.nar.xz/pathB").exists());
        assert_eq!(
            storage.nar_ref_index().referrers("nar/shared.nar.xz").await.unwrap(),
            vec!["pathA".to_string(), "pathB".to_string()],
        );

        // A re-push of the same narinfo is the same file, not a second edge.
        storage.put_narinfo("pathA", &narinfo_for("nar/shared.nar.xz")).await.unwrap();
        assert_eq!(
            storage.nar_ref_index().referrers("nar/shared.nar.xz").await.unwrap().len(),
            2,
        );
    }

    /// `nar-refs/` sits beside `nar/`, so it is neither a narinfo nor a NAR.
    #[tokio::test]
    async fn edge_files_do_not_pollute_the_narinfo_listing() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_narinfo("pathA", &narinfo_for("nar/shared.nar.xz")).await.unwrap();
        assert_eq!(storage.list_narinfos().await.unwrap(), vec!["pathA".to_string()]);
    }

    /// Forgetting the last edge removes the directory too, so the index does
    /// not accumulate one empty directory per NAR ever cached.
    #[tokio::test]
    async fn the_last_edge_takes_its_directory_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_narinfo("pathA", &narinfo_for("nar/x.nar.xz")).await.unwrap();
        assert!(dir.path().join("nar-refs/nar/x.nar.xz").exists());
        storage.delete("pathA").await.unwrap();
        assert!(!dir.path().join("nar-refs/nar/x.nar.xz").exists());
    }

    /// A `URL:` that would escape the cache root is refused at the write
    /// boundary, not sanitized at each of the several places it is used (a
    /// filesystem join here, a key elsewhere).
    #[tokio::test]
    async fn a_traversal_url_is_refused_rather_than_stored() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let evil = narinfo_for("../../escape.nar");
        let err = storage.put_narinfo("evil", &evil).await.unwrap_err();
        assert!(
            matches!(err, StoreError::NarInfo(ref m) if m.contains("unaddressable")),
            "expected a typed refusal, got {err:?}",
        );
        assert!(
            storage.get_narinfo("evil").await.unwrap().is_none(),
            "a refused narinfo must not be stored either",
        );
    }

    /// A store filled **before** the index existed has no edges. `reindex_nar_refs`
    /// rebuilds them from the narinfos already on disk, and it is idempotent.
    #[tokio::test]
    async fn reindex_rebuilds_edges_for_a_pre_index_store() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        // Write narinfos through the RECORD verb — exactly what a pre-index
        // binary did, since it had no index to maintain.
        storage.put_narinfo_record("pathA", &narinfo_for("nar/shared.nar.xz")).await.unwrap();
        storage.put_narinfo_record("pathB", &narinfo_for("nar/shared.nar.xz")).await.unwrap();
        assert!(
            storage.nar_ref_index().referrers("nar/shared.nar.xz").await.unwrap().is_empty(),
            "the fixture must actually start unindexed",
        );

        assert_eq!(storage.reindex_nar_refs().await.unwrap(), 2);
        assert_eq!(
            storage.nar_ref_index().referrers("nar/shared.nar.xz").await.unwrap(),
            vec!["pathA".to_string(), "pathB".to_string()],
        );

        // Idempotent: a second run records the same edges, not duplicates.
        assert_eq!(storage.reindex_nar_refs().await.unwrap(), 2);
        assert_eq!(
            storage.nar_ref_index().referrers("nar/shared.nar.xz").await.unwrap().len(),
            2,
        );
    }

    #[tokio::test]
    async fn root_accessor() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        assert_eq!(storage.root(), dir.path());
    }

    #[tokio::test]
    async fn put_narinfo_creates_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("cache");
        let storage = LocalStorage::new(&nested);
        storage.put_narinfo("test", "content").await.unwrap();
        assert!(nested.join("test.narinfo").exists());
    }

    #[tokio::test]
    async fn put_nar_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_nar("nar/deep/path.nar.xz", b"data").await.unwrap();
        assert!(dir.path().join("nar/deep/path.nar.xz").exists());
    }

    #[tokio::test]
    async fn overwrite_narinfo() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_narinfo("hash", "version1").await.unwrap();
        storage.put_narinfo("hash", "version2").await.unwrap();
        let content = storage.get_narinfo("hash").await.unwrap().unwrap();
        assert_eq!(content, "version2");
    }

    #[tokio::test]
    async fn overwrite_nar() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_nar("nar/x.nar.xz", b"old").await.unwrap();
        storage.put_nar("nar/x.nar.xz", b"new").await.unwrap();
        let data = storage.get_nar("nar/x.nar.xz").await.unwrap().unwrap();
        assert_eq!(data, b"new");
    }

    // ── streamed NAR I/O ───────────────────────────────────────────────────

    use super::nar_stream::{collect_nar, BytesNarSource, NarStream, NAR_CHUNK_BYTES};

    fn multi_chunk() -> Vec<u8> {
        (0..NAR_CHUNK_BYTES * 2 + 33).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn residency_is_streaming() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(LocalStorage::new(dir.path()).nar_residency(), NarResidency::Streaming);
    }

    #[tokio::test]
    async fn a_multi_chunk_nar_round_trips_and_every_chunk_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let nar = multi_chunk();
        storage
            .put_nar_stream("nar/big.nar.xz", &BytesNarSource::new(nar.clone()))
            .await
            .unwrap();

        let mut s = storage.get_nar_stream("nar/big.nar.xz").await.unwrap().unwrap();
        let mut seen = Vec::new();
        while let Some(c) = s.next().await {
            let c = c.unwrap();
            assert!(c.len() <= NAR_CHUNK_BYTES, "the read path handed out an unbounded chunk");
            seen.extend_from_slice(&c);
        }
        assert_eq!(seen, nar);
    }

    #[tokio::test]
    async fn a_streamed_write_leaves_no_scratch_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_nar("nar/x.nar.xz", b"bytes").await.unwrap();
        let mut entries = fs::read_dir(dir.path().join("nar")).await.unwrap();
        let mut names = Vec::new();
        while let Some(e) = entries.next_entry().await.unwrap() {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(names, vec!["x.nar.xz".to_string()], "a .tmp survived the rename");
    }

    /// A source whose stream fails partway — a client that hung up mid-upload,
    /// or a lower tier that died mid-promotion.
    struct FailingSource {
        good_bytes: usize,
    }

    #[async_trait]
    impl super::nar_stream::NarSource for FailingSource {
        async fn open(&self) -> Result<NarStream, StoreError> {
            let n = self.good_bytes;
            Ok(futures::stream::iter(vec![
                Ok(bytes::Bytes::from(vec![7u8; n])),
                Err(StoreError::Io(std::io::Error::other("upload died mid-stream"))),
            ])
            .boxed())
        }
    }

    #[tokio::test]
    async fn a_write_that_dies_mid_stream_publishes_nothing_at_all() {
        // A streamed write is not atomic the way a single whole-buffer `write`
        // was, so without the write-then-rename this would leave a TRUNCATED
        // NAR at the real path — silent corruption, strictly worse than the OOM
        // this change is against. Nothing must be published, and no scratch
        // file may survive.
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        let err = storage
            .put_nar_stream("nar/doomed.nar.xz", &FailingSource { good_bytes: 4096 })
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Io(_)));

        assert!(
            storage.get_nar("nar/doomed.nar.xz").await.unwrap().is_none(),
            "a half-written NAR must never be readable",
        );
        let mut entries = fs::read_dir(dir.path().join("nar")).await.unwrap();
        assert!(
            entries.next_entry().await.unwrap().is_none(),
            "the scratch file must be cleaned up on failure",
        );
    }

    #[tokio::test]
    async fn a_failed_rewrite_does_not_destroy_the_previous_value() {
        // The other half of write-then-rename: an existing good NAR must
        // survive a failed re-put rather than being truncated in place.
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new(dir.path());
        storage.put_nar("nar/x.nar.xz", b"the good bytes").await.unwrap();
        let _ = storage
            .put_nar_stream("nar/x.nar.xz", &FailingSource { good_bytes: 8 })
            .await;
        assert_eq!(
            storage.get_nar("nar/x.nar.xz").await.unwrap().unwrap(),
            b"the good bytes",
        );
    }

    #[tokio::test]
    async fn concurrent_writes_of_the_same_key_do_not_splice() {
        // Two pushes of the same content-addressed key race routinely. A shared
        // scratch name would let them interleave into one file and rename a
        // spliced NAR into place; per-write scratch names make that
        // unreachable.
        let dir = tempfile::tempdir().unwrap();
        let storage = std::sync::Arc::new(LocalStorage::new(dir.path()));
        let nar = multi_chunk();
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let s = std::sync::Arc::clone(&storage);
            let n = nar.clone();
            set.spawn(async move {
                s.put_nar_stream("nar/raced.nar.xz", &BytesNarSource::new(n)).await
            });
        }
        while let Some(r) = set.join_next().await {
            r.expect("task panicked").expect("write failed");
        }
        let got = collect_nar(
            storage.get_nar_stream("nar/raced.nar.xz").await.unwrap().unwrap(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(got, nar, "a raced write spliced the file");
    }
}
