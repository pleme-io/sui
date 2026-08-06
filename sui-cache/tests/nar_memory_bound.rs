//! **The measured gate on NAR peak memory.**
//!
//! sui OOMKilled six times in one day on camelot-eks (exit 137, cgroup OOM, not
//! node pressure) because the `StorageBackend` NAR verbs took and returned owned
//! `Vec<u8>`/`&[u8]`: streaming was not expressible, so every NAR was fully
//! resident while it was written or served, and a Postgres L2 `INSERT` of one
//! measured 12.712 s. With a 6 GiB pod limit the peak was set by the largest NAR
//! in flight and nothing bounded it.
//!
//! This test pushes a **256 MiB** NAR through the real `PUT /nar/…` handler over
//! a real three-tier `TieredBackend` and asserts the peak heap held while doing
//! it stays under a budget far below the NAR's own size.
//!
//! # What is measured, precisely
//!
//! A counting `GlobalAlloc` tracks **live heap bytes** (allocated minus freed)
//! and its high-water mark. That is a *proxy* for peak RSS, not peak RSS:
//!
//! - **Counted:** every `Vec`, `Bytes`, `BytesMut`, `String` and box the process
//!   holds at once — which is exactly where a resident NAR lives, and exactly
//!   what the old signature forced.
//! - **Not counted:** allocator fragmentation and unreturned arenas, thread
//!   stacks, `mmap`'d pages, and the kernel page cache behind the spool file.
//!   Peak RSS is therefore somewhat *higher* than this number in absolute terms;
//!   what the gate proves is that the peak does not **scale with NAR size**,
//!   which is the property that was violated.
//!
//! Portable peak-RSS is not available on both Linux and macOS without a
//! platform crate, so this is the honest instrument that runs everywhere the
//! suite runs, and it is labelled as a proxy rather than dressed up as RSS.
//!
//! # The instrument proves itself
//!
//! Per the repo's INSTRUMENT RULE — *before trusting a green, prove the
//! instrument can represent the failure it exists to catch* — this file runs a
//! **positive control** first: the same 256 MiB stream collected into one
//! buffer, asserted to **exceed** the budget. A meter that always read zero
//! would fail there. The gate's green is only meaningful because the control's
//! red is.
//!
//! One test, one file, on purpose: cargo gives each `tests/*.rs` its own binary,
//! and a single test in it means nothing else is allocating on another thread
//! while the meter is running.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use futures::{StreamExt, stream};
use sui_cache::config::{BackendConfig, CacheConfig};
use sui_cache::{AppState, LocalStorage, StorageBackend, TieredBackend, build_router};
use tower::ServiceExt;

// ───────────────────────────────────────────────────────────────────────────
// The meter
// ───────────────────────────────────────────────────────────────────────────

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct CountingAlloc;

// SAFETY: every method forwards to `System`, unchanged, and only adds relaxed
// atomic bookkeeping around it. No pointer is created, altered or consumed by
// this wrapper, so the `GlobalAlloc` contract is exactly `System`'s.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            note_alloc(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            note_alloc(new_size);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            note_alloc(layout.size());
        }
        p
    }
}

fn note_alloc(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Run `f` and return the high-water heap growth **above the level live when it
/// started**, in bytes.
async fn peak_growth<F, Fut, T>(f: F) -> (usize, T)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f().await;
    let peak = PEAK.load(Ordering::Relaxed);
    (peak.saturating_sub(base), out)
}

// ───────────────────────────────────────────────────────────────────────────
// Fixtures
// ───────────────────────────────────────────────────────────────────────────

/// The NAR size under test. Big enough that a resident copy is unmissable
/// against the budget, small enough that the disk writes stay quick.
const NAR_BYTES: usize = 256 * 1024 * 1024;

/// Wire frame size, matching the order of magnitude hyper hands a body in.
const FRAME_BYTES: usize = 64 * 1024;

/// The budget. The streaming path's expected peak is a couple of
/// `NAR_CHUNK_BYTES` (4 MiB) chunks plus wire frames — call it under 16 MiB —
/// so 64 MiB is ~4x headroom against noise while still being **4x below** the
/// NAR itself. Any regression that makes the peak scale with NAR size lands far
/// outside it.
const PEAK_BUDGET: usize = 64 * 1024 * 1024;

/// A synthetic NAR as a **stream**, never as a buffer.
///
/// Materializing 256 MiB to feed the test would itself be 256 MiB of counted
/// heap and would swamp the very thing being measured. Each frame is generated,
/// consumed and dropped.
fn synthetic_nar(total: usize) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    stream::unfold(0usize, move |sent| async move {
        if sent >= total {
            return None;
        }
        let n = (total - sent).min(FRAME_BYTES);
        // A position-derived byte so a mis-ordered or spliced write is visible
        // in the checksum, not just in the length.
        let fill = (sent / FRAME_BYTES % 251) as u8;
        Some((Ok(Bytes::from(vec![fill; n])), sent + n))
    })
}

/// Order-sensitive checksum computed over a stream, holding one frame at a time.
fn fold_checksum(acc: u64, chunk: &[u8]) -> u64 {
    chunk.iter().fold(acc, |a, b| {
        a.wrapping_mul(1_000_003).wrapping_add(u64::from(*b))
    })
}

fn expected_checksum(total: usize) -> u64 {
    let mut acc = 0u64;
    let mut sent = 0usize;
    while sent < total {
        let n = (total - sent).min(FRAME_BYTES);
        let fill = (sent / FRAME_BYTES % 251) as u8;
        for _ in 0..n {
            acc = acc.wrapping_mul(1_000_003).wrapping_add(u64::from(fill));
        }
        sent += n;
    }
    acc
}

/// The production shape: three tiers behind the resolver, write-through.
fn tiered_router(root: &std::path::Path) -> axum::Router {
    let l1: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(root.join("l1")));
    let l2: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(root.join("l2")));
    let l3: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(root.join("l3")));
    let storage: Arc<dyn StorageBackend> = Arc::new(TieredBackend::new(l1, l2, l3));
    let config = CacheConfig {
        listen: "127.0.0.1:0".to_string(),
        backend: BackendConfig::Local {
            path: root.to_path_buf(),
        },
        priority: 40,
        want_mass_query: true,
        store_dir: "/nix/store".to_string(),
        signing_key: None,
        require_sigs: false,
        ..CacheConfig::default()
    };
    build_router(AppState {
        storage,
        config,
        signer: None,
    })
}

// ───────────────────────────────────────────────────────────────────────────
// The gate
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn a_large_nar_moves_through_the_cache_without_becoming_resident() {
    // ── 1. POSITIVE CONTROL: prove the meter can see a resident NAR ────────
    //
    // The same stream, collected the way the OLD signature forced. If this does
    // NOT blow the budget, the meter is broken and every green below is
    // worthless.
    let (control_peak, control_len) = peak_growth(|| async {
        let mut s = Box::pin(synthetic_nar(NAR_BYTES));
        let mut buf: Vec<u8> = Vec::new();
        while let Some(c) = s.next().await {
            buf.extend_from_slice(&c.unwrap());
        }
        buf.len()
    })
    .await;
    assert_eq!(control_len, NAR_BYTES);
    assert!(
        control_peak > PEAK_BUDGET,
        "POSITIVE CONTROL FAILED: collecting {NAR_BYTES} bytes registered a peak of \
         {control_peak}, under the {PEAK_BUDGET}-byte budget. The allocator meter is not \
         measuring what it claims to, so the gate below proves nothing. Fix the meter \
         before trusting any result from this file.",
    );

    // ── 2. THE GATE: the same NAR through the real handler ─────────────────
    let dir = tempfile::tempdir().unwrap();
    let app = tiered_router(dir.path());

    let (write_peak, status) = peak_growth(|| async {
        let req = Request::builder()
            .method("PUT")
            .uri("/nar/big.nar.xz")
            .body(Body::from_stream(synthetic_nar(NAR_BYTES)))
            .unwrap();
        app.clone().oneshot(req).await.unwrap().status()
    })
    .await;
    assert_eq!(status, StatusCode::OK, "the upload must actually be stored");

    assert!(
        write_peak <= PEAK_BUDGET,
        "WRITE PATH REGRESSED: peak heap grew {write_peak} bytes moving a {NAR_BYTES}-byte \
         NAR, over the {PEAK_BUDGET}-byte budget. A peak that tracks NAR size means \
         something on the ingest path is materializing the whole blob again — check that \
         the handler still streams the body and that every tier still declares \
         NarResidency::Streaming.",
    );

    // ── 3. The read path, same budget ──────────────────────────────────────
    //
    // …and the bytes must be correct. A write path that stored nothing would
    // sail through a memory bound; the checksum is what makes the gate mean
    // "moved 256 MiB cheaply" rather than "did nothing cheaply".
    let (read_peak, (len, checksum)) = peak_growth(|| async {
        let req = Request::builder()
            .method("GET")
            .uri("/nar/big.nar.xz")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let mut body = resp.into_body().into_data_stream();
        let mut len = 0usize;
        let mut sum = 0u64;
        while let Some(c) = body.next().await {
            let c = c.unwrap();
            len += c.len();
            sum = fold_checksum(sum, &c);
        }
        (len, sum)
    })
    .await;

    assert_eq!(len, NAR_BYTES, "the served NAR must be byte-complete");
    assert_eq!(
        checksum,
        expected_checksum(NAR_BYTES),
        "the served NAR must be byte-identical — a chunked write that reordered or \
         spliced would show up here and nowhere else",
    );
    assert!(
        read_peak <= PEAK_BUDGET,
        "READ PATH REGRESSED: peak heap grew {read_peak} bytes serving a {NAR_BYTES}-byte \
         NAR, over the {PEAK_BUDGET}-byte budget.",
    );

    // Reported so a run's real numbers are visible with `--nocapture`, not just
    // a pass/fail. The control number is the meter's own calibration.
    println!(
        "nar={NAR_BYTES} budget={PEAK_BUDGET} control_peak={control_peak} \
         write_peak={write_peak} read_peak={read_peak}",
    );
}
