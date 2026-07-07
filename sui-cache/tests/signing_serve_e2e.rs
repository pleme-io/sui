//! End-to-end proof of the per-derivation cache-signing serve path.
//!
//! This is the "live-ish" proof for the super-cache-ci security layer's
//! first move (design §1, §10): it drives the REAL axum binary-cache router
//! (`build_router` over a signing `AppState`) via `tower::ServiceExt::oneshot`
//! — the same code path a `nix copy --to http://<sui>` write-through and a
//! consumer `GET` exercise — and asserts the three load-bearing properties:
//!
//! 1. The daemon SIGNS at ingest: an unsigned narinfo `PUT` comes back from
//!    `GET` carrying a `Sig:` that verifies against the daemon's public key
//!    (the key a consumer would put in `trusted-public-keys`).
//! 2. A consumer with the right trusted key ACCEPTS the served path.
//! 3. The poisoned-write case is REJECTED on consume: a path served WITHOUT a
//!    valid signature (the daemon had no key), and a signed path whose bytes
//!    are then TAMPERED, both fail the consumer's signature check — so trust
//!    of a poisoned durable-tier write is closed on the consume side.
//!
//! It uses only the local filesystem backend, so it needs no network,
//! Redis, or Postgres — the Environment-trait seam keeps it fast + hermetic.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use sui_cache::server::{build_router, AppState};
use sui_cache::signing::CacheSigner;
use sui_cache::storage::LocalStorage;
use sui_cache::{BackendConfig, CacheConfig, StorageBackend};
use sui_compat::narinfo::NarInfo;

/// An unsigned narinfo whose references are intentionally NOT in canonical
/// (sorted) order — this exercises the ref-ordering fix end-to-end.
const UNSIGNED_NARINFO: &str = "StorePath: /nix/store/e2e-hello\n\
     URL: nar/e2e.nar.xz\n\
     Compression: xz\n\
     FileHash: sha256:aaa\n\
     FileSize: 100\n\
     NarHash: sha256:bbb\n\
     NarSize: 200\n\
     References: /nix/store/zzz-b /nix/store/aaa-a /nix/store/mmm-c\n";

fn config(dir: &std::path::Path) -> CacheConfig {
    CacheConfig {
        listen: "127.0.0.1:0".to_string(),
        backend: BackendConfig::Local { path: dir.to_path_buf() },
        signing_key: None,
        priority: 40,
        want_mass_query: true,
        store_dir: "/nix/store".to_string(),
        require_sigs: false,
    }
}

async fn body_string(resp: axum::http::Response<Body>) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Consumer-side verify: does this narinfo carry a valid signature from one
/// of the trusted keys? Mirrors `BinaryCacheStore::verify_narinfo_signatures`
/// but lives here to keep the test in the sui-cache crate.
fn consumer_accepts(info: &NarInfo, trusted_pubkey: &str) -> bool {
    info.signatures.iter().any(|sig| {
        sui_cache::signing::verify_narinfo_signature(info, sig, trusted_pubkey)
            .unwrap_or(false)
    })
}

#[tokio::test]
async fn signing_daemon_serves_verifiable_signature_and_consumer_accepts() {
    let dir = tempfile::tempdir().unwrap();
    let storage: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(dir.path()));
    let signer = Arc::new(CacheSigner::generate("e2e-cache-1".to_string()));
    let trusted_pubkey = signer.public_key_string();

    let app = build_router(AppState {
        storage,
        config: config(dir.path()),
        signer: Some(signer),
    });

    // PUT an unsigned narinfo (write-through ingest).
    let put = Request::builder()
        .method("PUT")
        .uri("/e2e.narinfo")
        .body(Body::from(UNSIGNED_NARINFO))
        .unwrap();
    assert_eq!(app.clone().oneshot(put).await.unwrap().status(), StatusCode::OK);

    // GET it back — the daemon must have signed it at ingest.
    let get = Request::builder().uri("/e2e.narinfo").body(Body::empty()).unwrap();
    let served = body_string(app.oneshot(get).await.unwrap()).await;
    let info = NarInfo::parse(&served).unwrap();

    assert_eq!(info.signatures.len(), 1, "daemon must sign at ingest");
    assert!(info.signatures[0].starts_with("e2e-cache-1:"));

    // A consumer with the daemon's public key ACCEPTS the served path,
    // even though the references were unsorted at PUT time.
    assert!(
        consumer_accepts(&info, &trusted_pubkey),
        "consumer with the right key must accept the served signature",
    );
}

#[tokio::test]
async fn unsigned_daemon_serves_unsigned_and_requiring_consumer_rejects() {
    let dir = tempfile::tempdir().unwrap();
    let storage: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(dir.path()));

    // No signer configured — the legacy fail-open daemon.
    let app = build_router(AppState {
        storage,
        config: config(dir.path()),
        signer: None,
    });

    let put = Request::builder()
        .method("PUT")
        .uri("/e2e.narinfo")
        .body(Body::from(UNSIGNED_NARINFO))
        .unwrap();
    assert_eq!(app.clone().oneshot(put).await.unwrap().status(), StatusCode::OK);

    let get = Request::builder().uri("/e2e.narinfo").body(Body::empty()).unwrap();
    let served = body_string(app.oneshot(get).await.unwrap()).await;
    let info = NarInfo::parse(&served).unwrap();

    assert!(info.signatures.is_empty(), "unsigned daemon serves no Sig:");

    // A consumer that requires a signature from ANY trusted key rejects an
    // unsigned path — the poisoned-write-rejected posture (there is nothing
    // to verify, so acceptance is false).
    let some_trusted = CacheSigner::generate("consumer-trusts".to_string()).public_key_string();
    assert!(
        !consumer_accepts(&info, &some_trusted),
        "a require-sigs consumer must REJECT an unsigned served path",
    );
}

#[tokio::test]
async fn tampered_served_path_is_rejected_on_consume() {
    let dir = tempfile::tempdir().unwrap();
    let storage: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(dir.path()));
    let signer = Arc::new(CacheSigner::generate("e2e-cache-1".to_string()));
    let trusted_pubkey = signer.public_key_string();

    let app = build_router(AppState {
        storage,
        config: config(dir.path()),
        signer: Some(signer),
    });

    let put = Request::builder()
        .method("PUT")
        .uri("/e2e.narinfo")
        .body(Body::from(UNSIGNED_NARINFO))
        .unwrap();
    assert_eq!(app.clone().oneshot(put).await.unwrap().status(), StatusCode::OK);

    let get = Request::builder().uri("/e2e.narinfo").body(Body::empty()).unwrap();
    let served = body_string(app.oneshot(get).await.unwrap()).await;
    let mut info = NarInfo::parse(&served).unwrap();

    // Control: the honest served path verifies.
    assert!(consumer_accepts(&info, &trusted_pubkey), "control: honest path verifies");

    // Simulate a poisoned durable-tier write: an attacker mutates the bytes
    // the store path points at (nar_size) but cannot forge a new signature,
    // so the old Sig: rides along. The consumer must reject it.
    info.nar_size = 42_000_000;
    assert!(
        !consumer_accepts(&info, &trusted_pubkey),
        "a tampered served path must be REJECTED on consume",
    );
}
