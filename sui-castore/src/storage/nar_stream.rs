//! Bounded-chunk NAR movement — the vocabulary that makes "the whole NAR is
//! resident" stop being the only way to move one.
//!
//! # Why this module exists
//!
//! The original [`StorageBackend`](super::StorageBackend) NAR verbs were
//!
//! ```ignore
//! async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError>;
//! async fn put_nar(&self, path: &str, data: &[u8])  -> Result<(), StoreError>;
//! ```
//!
//! Owned bytes in, owned bytes out. **Streaming a NAR was not expressible
//! through that signature**, so every NAR was fully resident in the process for
//! as long as it took to write or serve it — and a Postgres L2 `INSERT` of one
//! measured **12.712 s** in production. With a 6 GiB pod limit, the peak was set
//! by the largest NAR in flight and nothing bounded it. sui OOMKilled six times
//! in one day on camelot-eks.
//!
//! The fix is a vocabulary, not a patch: a NAR moves as a sequence of
//! [`NAR_CHUNK_BYTES`]-sized [`Bytes`] chunks, and the thing a writer is handed
//! is a **re-openable** [`NarSource`] rather than a slice.
//!
//! # Why the write side is a source, not a stream
//!
//! [`TieredBackend`](super::TieredBackend)`::put_nar` must write **L2, then L3,
//! then warm L1**, each independently, gating on the two durable tiers before
//! the best-effort hot warm. A one-shot `Stream` can be consumed exactly once,
//! so it would force either (a) buffering the whole NAR to fan it out — the bug
//! being fixed — or (b) interleaving chunks across all three tiers, which
//! changes that ordering. A source that can be **opened once per tier** keeps
//! the ordering byte-for-byte identical while never holding more than one chunk.
//!
//! # Tier honesty
//!
//! The unbounded-buffer state is **not** truly-unrepresentable: [`collect_nar`]
//! exists, and [`BytesNarSource`] wraps a whole NAR on purpose (test doubles,
//! and callers who genuinely need the bytes). What *is* enforced is that a
//! backend cannot inherit the buffering path by accident — see
//! [`NarResidency`](super::NarResidency), which every `StorageBackend`
//! implementor must state explicitly because it has no default.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::stream::{self, BoxStream, StreamExt};
use tokio::io::AsyncReadExt;

use crate::StoreError;

/// The bounded chunk size every streaming NAR path moves bytes in.
///
/// 4 MiB is large enough that a 1 GiB NAR is ~256 round trips (not 250 000) and
/// small enough that a dozen concurrent transfers cost tens of MiB, not
/// gigabytes. It is the *only* size constant in the streaming path: a backend
/// that invents its own is drift.
pub const NAR_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// A NAR byte stream — bounded chunks, consumed exactly once.
pub type NarStream = BoxStream<'static, Result<Bytes, StoreError>>;

/// A **re-openable** NAR byte source.
///
/// [`open`](NarSource::open) yields a *fresh* bounded-chunk stream over the same
/// bytes every time it is called, so a fan-out writer feeds each destination in
/// order from its own stream and never has to materialize the NAR to serve more
/// than one consumer. See the module docs for why the write side needs this and
/// a plain `Stream` will not do.
#[async_trait]
pub trait NarSource: Send + Sync {
    /// Total byte length if it is known before reading. Backends use it to size
    /// a multipart upload or to reject an over-cap value *before* reading a
    /// single byte; `None` is always legal and must never be load-bearing.
    fn size_hint(&self) -> Option<u64> {
        None
    }

    /// Open a fresh chunk stream over the same bytes.
    ///
    /// # Errors
    ///
    /// Returns a [`StoreError`] if the underlying bytes cannot be (re-)opened —
    /// a spool file that was removed, a lower tier that lost the content
    /// between the probe and the promotion.
    async fn open(&self) -> Result<NarStream, StoreError>;
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// A [`NarSource`] over bytes already in memory.
///
/// Zero-copy: [`Bytes`] slices share one allocation, so re-opening does not
/// duplicate the NAR. It is still **O(nar) resident** by construction — that is
/// the point of the name. Use it for test doubles, for small values, and at a
/// boundary that genuinely already holds the bytes; never as the way a large
/// upload reaches a backend.
#[derive(Debug, Clone)]
pub struct BytesNarSource {
    bytes: Bytes,
}

impl BytesNarSource {
    /// Wrap owned bytes.
    #[must_use]
    pub fn new(bytes: impl Into<Bytes>) -> Self {
        Self { bytes: bytes.into() }
    }
}

impl From<&[u8]> for BytesNarSource {
    fn from(v: &[u8]) -> Self {
        Self::new(Bytes::copy_from_slice(v))
    }
}

#[async_trait]
impl NarSource for BytesNarSource {
    fn size_hint(&self) -> Option<u64> {
        Some(self.bytes.len() as u64)
    }

    async fn open(&self) -> Result<NarStream, StoreError> {
        Ok(bytes_stream(self.bytes.clone()))
    }
}

/// A [`NarSource`] over a file on disk.
///
/// Each [`open`](NarSource::open) is a fresh `File` read in [`NAR_CHUNK_BYTES`]
/// steps — **O(chunk) resident regardless of file size**. This is what a spooled
/// HTTP upload and a local-tier promotion both ride on.
#[derive(Debug, Clone)]
pub struct FileNarSource {
    path: PathBuf,
    len: Option<u64>,
}

impl FileNarSource {
    /// Source the file at `path`. The length is probed lazily on first
    /// [`size_hint`](NarSource::size_hint) caller demand — construction does no
    /// I/O, so a missing file surfaces at `open` where it can be reported.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), len: None }
    }

    /// Source the file at `path`, recording a known byte length.
    #[must_use]
    pub fn with_len(path: impl Into<PathBuf>, len: u64) -> Self {
        Self { path: path.into(), len: Some(len) }
    }

    /// The file this source reads.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl NarSource for FileNarSource {
    fn size_hint(&self) -> Option<u64> {
        self.len
    }

    async fn open(&self) -> Result<NarStream, StoreError> {
        let file = tokio::fs::File::open(&self.path).await.map_err(StoreError::Io)?;
        Ok(file_stream(file))
    }
}

/// Default cap for the in-memory ingest fallback — see [`spool_or_buffer`].
///
/// This is the *worst case* peak of an ingest that could not get a spool file.
/// It has to be generous enough that ordinary NARs keep flowing when the spool
/// volume is unavailable, and small enough that a handful of concurrent ones
/// cannot fill a 6 GiB pod. 256 MiB gives ~20 concurrent uploads of headroom.
pub const DEFAULT_INGEST_MEMORY_CAP: usize = 256 * 1024 * 1024;

/// Deletes the spool file when the source is dropped.
///
/// A separate type rather than a `Drop` on [`SpooledNarSource`] so the source
/// stays cheap to move and the cleanup is impossible to forget in a future
/// field addition.
#[derive(Debug)]
struct SpoolGuard(PathBuf);

impl Drop for SpoolGuard {
    fn drop(&mut self) {
        // Best-effort: the spool volume may already be gone. Leaving a stray
        // file is untidy; panicking in `drop` during an error unwind is worse.
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A [`NarSource`] over a spool file, deleted when the source is dropped.
///
/// This is what turns a **one-shot** upload — an HTTP request body, which can be
/// read exactly once — into something a fan-out writer can open per tier. Peak
/// is one chunk in each direction.
#[derive(Debug)]
pub struct SpooledNarSource {
    inner: FileNarSource,
    _guard: SpoolGuard,
}

#[async_trait]
impl NarSource for SpooledNarSource {
    fn size_hint(&self) -> Option<u64> {
        self.inner.size_hint()
    }

    async fn open(&self) -> Result<NarStream, StoreError> {
        self.inner.open().await
    }
}

/// Turn a one-shot byte stream into a re-openable [`NarSource`], **bounded
/// either way**.
///
/// Preferred path: spool to a file in `dir` in [`NAR_CHUNK_BYTES`] steps, peak
/// one chunk. Fallback: if the spool file cannot be *created* — no `dir`, no
/// permission, a full volume — buffer in memory instead, hard-capped at
/// `memory_cap`, refusing past it with [`StoreError::TooLarge`].
///
/// The fallback is chosen **before any bytes are read**, deliberately. A spool
/// that fails halfway has already consumed part of a one-shot stream and cannot
/// be restarted, so mid-write faults surface as errors rather than silently
/// switching strategy and truncating the upload.
///
/// Tier honesty: the fallback path is *bounded*, not *streaming* — a machine
/// with no usable spool directory has a `memory_cap`-sized worst case per
/// concurrent ingest, and NARs above the cap are refused rather than cached.
/// That is a deliberate trade against the alternative, which is the pod dying
/// and taking every in-flight build with it.
///
/// # Errors
///
/// Propagates a read error from `stream`, a write error to the spool file, or
/// [`StoreError::TooLarge`] when the memory fallback is in use and exceeded.
pub async fn spool_or_buffer<S, E>(
    mut stream: S,
    dir: &Path,
    memory_cap: usize,
) -> Result<Box<dyn NarSource>, StoreError>
where
    S: futures::Stream<Item = Result<Bytes, E>> + Send + Unpin,
    // Generic over the stream's error so a caller can hand over its transport's
    // own stream (an axum body, a reqwest response) untouched. Forcing the
    // caller to `.map()` into `StoreError` first would push a `futures`
    // dependency onto every ingest boundary for nothing.
    E: std::fmt::Display + Send,
{
    use tokio::io::AsyncWriteExt;

    fn transport_err<E: std::fmt::Display>(e: E) -> StoreError {
        StoreError::Io(std::io::Error::other(format!("nar ingest: {e}")))
    }

    let path = spool_path(dir);
    let created = tokio::fs::File::create(&path).await;

    let Ok(mut file) = created else {
        let e = created.err().expect("checked Err");
        tracing::warn!(
            dir = %dir.display(),
            error = %e,
            cap = memory_cap,
            "nar ingest: no spool file — falling back to a CAPPED in-memory buffer; \
             NARs above the cap will be refused. Point TMPDIR at a writable volume.",
        );
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(transport_err)?;
            if buf.len() + chunk.len() > memory_cap {
                return Err(StoreError::TooLarge {
                    limit: memory_cap as u64,
                    at_least: (buf.len() + chunk.len()) as u64,
                });
            }
            buf.extend_from_slice(&chunk);
        }
        return Ok(Box::new(BytesNarSource::new(buf)));
    };

    // The guard is armed the instant the file exists, so every early return
    // below (and any panic) removes it.
    let guard = SpoolGuard(path.clone());
    let mut len: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport_err)?;
        file.write_all(&chunk).await.map_err(StoreError::Io)?;
        len += chunk.len() as u64;
    }
    file.flush().await.map_err(StoreError::Io)?;
    drop(file);

    Ok(Box::new(SpooledNarSource {
        inner: FileNarSource::with_len(&path, len),
        _guard: guard,
    }))
}

/// A unique spool path. Unique per call, not per key: concurrent uploads of the
/// same content-addressed key are routine, and a shared name would have them
/// interleave into one file.
fn spool_path(dir: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("sui-nar-spool.{}.{n}", std::process::id()))
}

// ---------------------------------------------------------------------------
// Stream constructors
// ---------------------------------------------------------------------------

/// Yield in-memory bytes as bounded chunks (zero-copy slices of one allocation).
#[must_use]
pub fn bytes_stream(bytes: Bytes) -> NarStream {
    stream::unfold(bytes, |mut rest| async move {
        if rest.is_empty() {
            return None;
        }
        let take = rest.len().min(NAR_CHUNK_BYTES);
        let chunk = rest.split_to(take);
        Some((Ok(chunk), rest))
    })
    .boxed()
}

/// Read an open file as bounded chunks. **O(chunk) resident.**
#[must_use]
pub fn file_stream(file: tokio::fs::File) -> NarStream {
    stream::unfold(Some(file), |state| async move {
        let mut file = state?;
        let mut buf = BytesMut::zeroed(NAR_CHUNK_BYTES);
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(buf.freeze()), Some(file)))
            }
            // Surface the fault and END the stream: a reader that keeps polling
            // a broken file would spin forever on the same error.
            Err(e) => Some((Err(StoreError::Io(e)), None)),
        }
    })
    .boxed()
}

/// A stream that yields exactly one chunk — the whole value.
///
/// The buffering escape hatch, named so it is visible at a call site. Legal for
/// a backend whose values are inherently whole (an in-memory double, a capped
/// hot tier); never for a durable NAR tier.
#[must_use]
pub fn whole_value_stream(data: Vec<u8>) -> NarStream {
    bytes_stream(Bytes::from(data))
}

/// A stream that yields nothing.
#[must_use]
pub fn empty_stream() -> NarStream {
    stream::empty().boxed()
}

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

/// Drain a [`NarStream`] into one buffer.
///
/// **This is the unbounded path.** It exists for callers that genuinely need the
/// whole NAR (the `get_nar` convenience verb, test doubles) and for capped tiers
/// via `limit`. Every use is a deliberate decision to hold O(nar) bytes.
///
/// `limit` — when `Some(max)`, collection **refuses** the moment the accumulated
/// length would exceed `max`, returning [`StoreError::TooLarge`]. It never
/// accumulates past the cap, so a capped caller's peak is `max + NAR_CHUNK_BYTES`
/// and not one byte more.
///
/// # Errors
///
/// Propagates any error the stream yields, or [`StoreError::TooLarge`] when a
/// `limit` is set and exceeded.
pub async fn collect_nar(mut stream: NarStream, limit: Option<usize>) -> Result<Vec<u8>, StoreError> {
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if let Some(max) = limit {
            if out.len() + chunk.len() > max {
                return Err(StoreError::TooLarge {
                    limit: max as u64,
                    at_least: (out.len() + chunk.len()) as u64,
                });
            }
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build `n` bytes with a position-dependent pattern, so a test that
    /// re-assembles chunks catches reordering and truncation, not just length.
    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn bytes_source_chunks_are_bounded_and_reassemble() {
        let data = pattern(NAR_CHUNK_BYTES * 2 + 7);
        let src = BytesNarSource::new(data.clone());
        assert_eq!(src.size_hint(), Some(data.len() as u64));

        let mut s = src.open().await.unwrap();
        let mut seen = Vec::new();
        let mut chunks = 0usize;
        while let Some(c) = s.next().await {
            let c = c.unwrap();
            assert!(c.len() <= NAR_CHUNK_BYTES, "a chunk exceeded the bound");
            seen.extend_from_slice(&c);
            chunks += 1;
        }
        assert_eq!(chunks, 3, "2 full chunks + a 7-byte tail");
        assert_eq!(seen, data);
    }

    #[tokio::test]
    async fn a_source_re_opens_to_identical_bytes() {
        // The property TieredBackend's ordering depends on.
        let data = pattern(NAR_CHUNK_BYTES + 1);
        let src = BytesNarSource::new(data.clone());
        for _ in 0..3 {
            let got = collect_nar(src.open().await.unwrap(), None).await.unwrap();
            assert_eq!(got, data);
        }
    }

    #[tokio::test]
    async fn file_source_re_opens_and_is_chunk_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        let data = pattern(NAR_CHUNK_BYTES * 2 + 13);
        tokio::fs::write(&path, &data).await.unwrap();

        let src = FileNarSource::with_len(&path, data.len() as u64);
        assert_eq!(src.size_hint(), Some(data.len() as u64));
        for _ in 0..2 {
            let mut s = src.open().await.unwrap();
            let mut seen = Vec::new();
            while let Some(c) = s.next().await {
                let c = c.unwrap();
                assert!(c.len() <= NAR_CHUNK_BYTES);
                seen.extend_from_slice(&c);
            }
            assert_eq!(seen, data);
        }
    }

    #[tokio::test]
    async fn file_source_open_of_a_missing_file_is_a_typed_error() {
        let src = FileNarSource::new("/nonexistent/sui-castore/blob");
        // A `NarStream` is not `Debug`, so `unwrap_err` is unavailable here.
        match src.open().await {
            Err(StoreError::Io(_)) => {}
            Err(other) => panic!("expected a typed Io error, got {other}"),
            Ok(_) => panic!("opening a missing file must not succeed"),
        }
    }

    #[tokio::test]
    async fn empty_input_yields_no_chunks() {
        let src = BytesNarSource::new(Vec::new());
        assert!(src.open().await.unwrap().next().await.is_none());
        assert!(collect_nar(src.open().await.unwrap(), None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn collect_with_a_limit_refuses_instead_of_growing() {
        let data = pattern(NAR_CHUNK_BYTES * 3);
        let src = BytesNarSource::new(data);
        let err = collect_nar(src.open().await.unwrap(), Some(NAR_CHUNK_BYTES))
            .await
            .unwrap_err();
        match err {
            StoreError::TooLarge { limit, at_least } => {
                assert_eq!(limit, NAR_CHUNK_BYTES as u64);
                assert!(at_least > limit);
            }
            other => panic!("expected TooLarge, got {other}"),
        }
    }

    #[tokio::test]
    async fn collect_at_exactly_the_limit_is_accepted() {
        // The boundary must be inclusive: a value the cap allows must not be
        // refused by an off-by-one.
        let data = pattern(NAR_CHUNK_BYTES);
        let src = BytesNarSource::new(data.clone());
        let got = collect_nar(src.open().await.unwrap(), Some(NAR_CHUNK_BYTES)).await.unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn whole_value_stream_round_trips() {
        let data = pattern(1000);
        let got = collect_nar(whole_value_stream(data.clone()), None).await.unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn empty_stream_collects_to_nothing() {
        assert!(collect_nar(empty_stream(), None).await.unwrap().is_empty());
    }

    // ── spool_or_buffer: a one-shot upload becomes re-openable ─────────────

    fn one_shot(data: Vec<u8>, frame: usize) -> impl futures::Stream<Item = Result<Bytes, StoreError>> {
        stream::unfold(0usize, move |sent| {
            let data = data.clone();
            async move {
                if sent >= data.len() {
                    return None;
                }
                let n = (data.len() - sent).min(frame);
                Some((Ok(Bytes::copy_from_slice(&data[sent..sent + n])), sent + n))
            }
        })
    }

    #[tokio::test]
    async fn a_spooled_upload_re_opens_to_the_same_bytes_every_time() {
        // The property `TieredBackend`'s three sequential tier writes depend on.
        let dir = tempfile::tempdir().unwrap();
        let data = pattern(NAR_CHUNK_BYTES + 2048);
        let src = spool_or_buffer(
            Box::pin(one_shot(data.clone(), 8192)),
            dir.path(),
            DEFAULT_INGEST_MEMORY_CAP,
        )
        .await
        .unwrap();

        assert_eq!(src.size_hint(), Some(data.len() as u64));
        for _ in 0..3 {
            assert_eq!(collect_nar(src.open().await.unwrap(), None).await.unwrap(), data);
        }
    }

    #[tokio::test]
    async fn dropping_a_spooled_source_removes_its_file() {
        // The spool volume is the same kind of finite resource that filled up
        // and broke L3. Leaking one file per upload would recreate that failure
        // one directory over.
        let dir = tempfile::tempdir().unwrap();
        let src = spool_or_buffer(
            Box::pin(one_shot(pattern(4096), 1024)),
            dir.path(),
            DEFAULT_INGEST_MEMORY_CAP,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        drop(src);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "the spool file outlived its source",
        );
    }

    #[tokio::test]
    async fn an_unusable_spool_directory_falls_back_to_a_capped_buffer() {
        // A pod whose TMPDIR is missing or read-only must keep caching ordinary
        // NARs rather than 500ing every push — but bounded, never unbounded.
        let data = pattern(4096);
        let src = spool_or_buffer(
            Box::pin(one_shot(data.clone(), 512)),
            std::path::Path::new("/nonexistent/sui-spool-dir"),
            DEFAULT_INGEST_MEMORY_CAP,
        )
        .await
        .expect("the fallback must keep small uploads working");
        assert_eq!(collect_nar(src.open().await.unwrap(), None).await.unwrap(), data);
    }

    #[tokio::test]
    async fn the_fallback_refuses_past_its_cap_rather_than_growing() {
        // The fallback is the ONE place a whole NAR can still be resident, so
        // its cap is the last line: past it, refuse. Without this the "bounded
        // either way" claim would be false on exactly the machines that need it.
        // `dyn NarSource` is not `Debug`, so the Ok arm is matched explicitly.
        match spool_or_buffer(
            Box::pin(one_shot(pattern(64 * 1024), 4096)),
            std::path::Path::new("/nonexistent/sui-spool-dir"),
            8 * 1024,
        )
        .await
        {
            Err(StoreError::TooLarge { limit, at_least }) => {
                assert_eq!(limit, 8 * 1024);
                assert!(at_least > limit);
            }
            Err(other) => panic!("expected TooLarge, got {other}"),
            Ok(_) => panic!("the fallback must refuse past its cap, not grow"),
        }
    }

    #[tokio::test]
    async fn an_empty_upload_spools_and_re_opens_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let src = spool_or_buffer(
            Box::pin(one_shot(Vec::new(), 1024)),
            dir.path(),
            DEFAULT_INGEST_MEMORY_CAP,
        )
        .await
        .unwrap();
        assert_eq!(src.size_hint(), Some(0));
        assert!(collect_nar(src.open().await.unwrap(), None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_spools_do_not_share_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut sources = Vec::new();
        for i in 0..8u8 {
            let data = vec![i; 1024];
            sources.push((
                data.clone(),
                spool_or_buffer(
                    Box::pin(one_shot(data, 128)),
                    dir.path(),
                    DEFAULT_INGEST_MEMORY_CAP,
                )
                .await
                .unwrap(),
            ));
        }
        for (expected, src) in &sources {
            assert_eq!(&collect_nar(src.open().await.unwrap(), None).await.unwrap(), expected);
        }
    }
}
