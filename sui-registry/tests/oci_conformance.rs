//! porto's `== spec` proof — the sui-parity analog for the OCI Distribution
//! Spec v1.1.
//!
//! Every implemented endpoint is driven against a registry app via
//! `tower::ServiceExt::oneshot`, asserting the spec behaviors. This is the
//! forcing-function that makes "porto is a conformant registry" a mechanical
//! test result, not an assertion: a regression on any behavior below goes red.
//!
//! ## The two-store matrix
//!
//! The whole conformance matrix runs against BOTH backing stores:
//!
//! - [`MemStore`] — the pure in-memory reference store, zero real I/O.
//! - [`SuiCacheStore`] — the PRODUCTION store over a real
//!   [`sui_castore::StorageBackend`] (a `LocalStorage` rooted in a
//!   tempdir). This exercises the empty-object tombstone / delete / immutability
//!   semantics of the sui-cache path — the exact behaviors a store-specific
//!   handler bug could differ on, and which the MemStore path alone would leave
//!   unproven.
//!
//! A single set of behavior functions (`async fn <behavior>(app: Router)`) is
//! defined once; two harness `#[tokio::test]`s (`mem_store_conformance` /
//! `sui_cache_store_conformance`) drive EVERY behavior through EVERY store, so a
//! new behavior is proven on both stores by construction and can never be added
//! to only one path.
//!
//! The behaviors proven:
//! - end-1 the API-version handshake
//! - end-4b monolithic upload with digest-verify at the border
//! - end-4a/5/6 the chunked upload FSM + finalize digest-verify
//! - blob immutability at its content-address
//! - a tag is a MUTABLE pointer; a digest reference is immutable
//! - the referrers reverse-index returns the signing manifest (lacre seam)
//! - `DIGEST_INVALID` on a malformed digest; `MANIFEST_UNKNOWN` on a miss
//! - cross-repo mount dedups a shared blob
//! - tags-list pagination
//! - blob/manifest/upload DELETE (and the deleted-is-tombstoned read-back)

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

use sui_castore::{LocalStorage, StorageBackend};
use sui_compat::hash::HashAlgorithm;
use sui_registry::config::RegistryConfig;
use sui_registry::digest::Digest;
use sui_registry::server::{build_router, AppState};
use sui_registry::store::{MemStore, SuiCacheStore};

/// A registry app plus an optional live tempdir guard.
///
/// The `SuiCacheStore` path is backed by files under a [`TempDir`]; the guard
/// must outlive every request the app serves (dropping it removes the backing
/// files). The `MemStore` path carries `None`.
struct TestApp {
    router: Router,
    _guard: Option<TempDir>,
}

impl TestApp {
    fn router(&self) -> Router {
        self.router.clone()
    }
}

/// A fresh registry app over an in-memory [`MemStore`].
fn mem_app() -> TestApp {
    let store = Arc::new(MemStore::new());
    let state = AppState::new(store, RegistryConfig::bare());
    TestApp {
        router: build_router(state),
        _guard: None,
    }
}

/// A fresh registry app over a real [`SuiCacheStore`] on a tempdir-backed
/// [`LocalStorage`] — the production storage path.
fn sui_cache_app() -> TestApp {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(dir.path().to_path_buf()));
    let store = Arc::new(SuiCacheStore::new(backend));
    let state = AppState::new(store, RegistryConfig::bare());
    TestApp {
        router: build_router(state),
        _guard: Some(dir),
    }
}

async fn body_bytes(resp: axum::http::Response<Body>) -> Vec<u8> {
    resp.into_body().collect().await.unwrap().to_bytes().to_vec()
}

async fn body_json(resp: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = body_bytes(resp).await;
    serde_json::from_slice(&bytes).unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn head(uri: &str) -> Request<Body> {
    Request::builder().method("HEAD").uri(uri).body(Body::empty()).unwrap()
}

fn post_empty(uri: &str) -> Request<Body> {
    Request::builder().method("POST").uri(uri).body(Body::empty()).unwrap()
}

fn put_body(uri: &str, ct: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", ct)
        .body(Body::from(body))
        .unwrap()
}

fn patch_body(uri: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(uri)
        .body(Body::from(body))
        .unwrap()
}

fn delete(uri: &str) -> Request<Body> {
    Request::builder().method("DELETE").uri(uri).body(Body::empty()).unwrap()
}

// ───────────────────────── the two-store harness ─────────────────────────

/// The full conformance matrix.
///
/// Each behavior gets a FRESH app (built by `$fresh`) so state never bleeds
/// across behaviors — the exact-equality assertions (tags list, referrers
/// index) depend on a clean registry, just as the per-`#[test]` isolation of
/// the original suite did. `$fresh` is a `fn() -> TestApp`, so the same matrix
/// runs identically over [`mem_app`] and [`sui_cache_app`]. Adding a behavior
/// here is the only edit needed for it to run on BOTH stores — the matrix
/// cannot drift to a single-store proof.
async fn run_matrix(fresh: fn() -> TestApp) {
    // Each behavior runs against a freshly-built app, bound to a local so its
    // tempdir guard (SuiCacheStore path) outlives the request — a dropped guard
    // would delete the backing files mid-behavior. `run` names that pattern
    // once: build → keep alive → drive.
    async fn run<F, Fut>(fresh: fn() -> TestApp, behavior: F)
    where
        F: FnOnce(Router) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let app = fresh();
        behavior(app.router()).await;
        // `app` (and its tempdir guard) drops here, after the behavior.
    }

    run(fresh, end_1_api_version_handshake).await;
    run(fresh, end_4b_monolithic_upload_verifies_digest_and_stores).await;
    run(fresh, end_4b_monolithic_upload_wrong_digest_is_digest_invalid).await;
    run(fresh, end_4a_5_6_chunked_upload_finalize_verifies_digest).await;
    run(fresh, end_5_out_of_order_chunk_is_blob_upload_invalid).await;
    run(fresh, end_6_finalize_wrong_digest_is_digest_invalid).await;
    run(fresh, end_13_14_upload_status_and_cancel).await;
    run(fresh, blob_is_immutable_at_its_content_address).await;
    run(fresh, end_7_tag_is_a_mutable_pointer_end_3_get_by_tag_and_digest).await;
    run(fresh, end_3_manifest_unknown_on_missing_tag).await;
    run(fresh, head_manifest_returns_digest_and_length).await;
    run(fresh, end_7a_12_referrers_index_returns_the_signing_manifest).await;
    run(fresh, malformed_digest_is_digest_invalid).await;
    run(fresh, blob_unknown_on_missing_blob).await;
    run(fresh, end_11_cross_repo_mount_dedups_shared_blob).await;
    run(fresh, end_11_mount_of_unknown_blob_falls_back_to_upload_session).await;
    run(fresh, end_8_tags_list_and_pagination).await;
    run(fresh, end_9_delete_manifest).await;
    run(fresh, end_10_delete_blob).await;
    run(fresh, put_manifest_by_digest_mismatch_is_digest_invalid).await;
    // Store-path tombstone semantics: prove a deleted object reads back ABSENT,
    // not present-but-empty — the SuiCacheStore empty-object tombstone contract,
    // observable identically from MemStore's remove-the-entry delete.
    run(fresh, deleted_blob_reads_back_absent).await;
    run(fresh, deleted_manifest_reads_back_absent).await;
}

#[tokio::test]
async fn mem_store_conformance() {
    run_matrix(mem_app).await;
}

#[tokio::test]
async fn sui_cache_store_conformance() {
    run_matrix(sui_cache_app).await;
}

// ─────────────────────────── end-1 ───────────────────────────

async fn end_1_api_version_handshake(app: Router) {
    let resp = app.oneshot(get("/v2/")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("docker-distribution-api-version")
            .unwrap(),
        "registry/2.0"
    );
}

// ────────────────── end-4b: monolithic upload + digest-verify ──────────────────

async fn end_4b_monolithic_upload_verifies_digest_and_stores(app: Router) {
    let blob = b"the layer bytes".to_vec();
    let d = Digest::of(HashAlgorithm::Sha256, &blob);

    // Monolithic upload (end-4b): POST uploads/?digest=<d> with the whole body.
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v2/lib/x/blobs/uploads/?digest={}", d.to_wire()))
        .body(Body::from(blob.clone()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "monolithic upload → 201");
    assert_eq!(
        resp.headers().get("docker-content-digest").unwrap(),
        &d.to_wire()
    );

    // The blob is now retrievable at its digest (end-2 GET).
    let resp = app
        .clone()
        .oneshot(get(&format!("/v2/lib/x/blobs/{}", d.to_wire())))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, blob);
}

async fn end_4b_monolithic_upload_wrong_digest_is_digest_invalid(app: Router) {
    let blob = b"real bytes".to_vec();
    let wrong = Digest::of(HashAlgorithm::Sha256, b"other bytes");

    let req = Request::builder()
        .method("POST")
        .uri(format!("/v2/lib/x/blobs/uploads/?digest={}", wrong.to_wire()))
        .body(Body::from(blob))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["errors"][0]["code"], "DIGEST_INVALID");
}

// ────────────────── end-4a/5/6: chunked upload FSM ──────────────────

async fn end_4a_5_6_chunked_upload_finalize_verifies_digest(app: Router) {
    // end-4a: start.
    let resp = app.clone().oneshot(post_empty("/v2/lib/x/blobs/uploads/")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let location = resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // end-5: two chunks (Content-Range).
    let chunk1 = b"hello ".to_vec();
    let chunk2 = b"world".to_vec();
    let req = Request::builder()
        .method("PATCH")
        .uri(&location)
        .header("content-range", "0-5")
        .body(Body::from(chunk1.clone()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(resp.headers().get("range").unwrap(), "0-5");

    let req = Request::builder()
        .method("PATCH")
        .uri(&location)
        .header("content-range", "6-10")
        .body(Body::from(chunk2.clone()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(resp.headers().get("range").unwrap(), "0-10");

    // end-6: finalize with the correct digest.
    let full = b"hello world".to_vec();
    let d = Digest::of(HashAlgorithm::Sha256, &full);
    let req = Request::builder()
        .method("PUT")
        .uri(format!("{location}?digest={}", d.to_wire()))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Blob is stored at its address.
    let resp = app
        .oneshot(get(&format!("/v2/lib/x/blobs/{}", d.to_wire())))
        .await
        .unwrap();
    assert_eq!(body_bytes(resp).await, full);
}

async fn end_5_out_of_order_chunk_is_blob_upload_invalid(app: Router) {
    let resp = app.clone().oneshot(post_empty("/v2/lib/x/blobs/uploads/")).await.unwrap();
    let location = resp.headers().get("location").unwrap().to_str().unwrap().to_string();

    // First chunk ok.
    let req = Request::builder()
        .method("PATCH")
        .uri(&location)
        .header("content-range", "0-2")
        .body(Body::from(b"abc".to_vec()))
        .unwrap();
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::ACCEPTED);

    // Second chunk claims a gap (starts at 5, session at 3).
    let req = Request::builder()
        .method("PATCH")
        .uri(&location)
        .header("content-range", "5-7")
        .body(Body::from(b"xyz".to_vec()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["errors"][0]["code"], "BLOB_UPLOAD_INVALID");
}

async fn end_6_finalize_wrong_digest_is_digest_invalid(app: Router) {
    let resp = app.clone().oneshot(post_empty("/v2/lib/x/blobs/uploads/")).await.unwrap();
    let location = resp.headers().get("location").unwrap().to_str().unwrap().to_string();

    let req = patch_body(&location, b"real bytes".to_vec());
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::ACCEPTED);

    let wrong = Digest::of(HashAlgorithm::Sha256, b"nope");
    let req = Request::builder()
        .method("PUT")
        .uri(format!("{location}?digest={}", wrong.to_wire()))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["errors"][0]["code"], "DIGEST_INVALID");
}

async fn end_13_14_upload_status_and_cancel(app: Router) {
    let resp = app.clone().oneshot(post_empty("/v2/lib/x/blobs/uploads/")).await.unwrap();
    let location = resp.headers().get("location").unwrap().to_str().unwrap().to_string();

    let req = patch_body(&location, b"12345".to_vec());
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::ACCEPTED);

    // end-13: status → 204 + Range 0-4.
    let resp = app.clone().oneshot(get(&location)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(resp.headers().get("range").unwrap(), "0-4");

    // end-14: cancel → 204, then status is unknown.
    let resp = app.clone().oneshot(delete(&location)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = app.oneshot(get(&location)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["errors"][0]["code"], "BLOB_UPLOAD_UNKNOWN");
}

// ────────────────── blob immutability at content-address ──────────────────

async fn blob_is_immutable_at_its_content_address(app: Router) {
    let blob = b"immutable".to_vec();
    let d = Digest::of(HashAlgorithm::Sha256, &blob);

    // Store once via monolithic upload.
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v2/lib/x/blobs/uploads/?digest={}", d.to_wire()))
        .body(Body::from(blob.clone()))
        .unwrap();
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::CREATED);

    // Re-store the SAME digest+bytes is idempotent (still 201, same bytes).
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v2/lib/x/blobs/uploads/?digest={}", d.to_wire()))
        .body(Body::from(blob.clone()))
        .unwrap();
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::CREATED);

    let resp = app.oneshot(get(&format!("/v2/lib/x/blobs/{}", d.to_wire()))).await.unwrap();
    assert_eq!(body_bytes(resp).await, blob, "bytes at an address never change");
}

// ────────────────── end-3/7: manifest tag pointer + digest immutability ──────

/// A minimal image manifest referencing a config blob (no subject).
fn image_manifest(config_digest: &Digest) -> Vec<u8> {
    // Typed JSON via serde_json::json! — never a hand-built string.
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest.to_wire(),
            "size": 2
        },
        "layers": []
    }))
    .unwrap()
}

async fn end_7_tag_is_a_mutable_pointer_end_3_get_by_tag_and_digest(app: Router) {
    let cfg = Digest::of(HashAlgorithm::Sha256, b"{}");
    let manifest = image_manifest(&cfg);
    let mdigest = Digest::of(HashAlgorithm::Sha256, &manifest);

    // end-7: PUT manifest by tag `latest`.
    let req = put_body(
        "/v2/lib/x/manifests/latest",
        "application/vnd.oci.image.manifest.v1+json",
        manifest.clone(),
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(resp.headers().get("docker-content-digest").unwrap(), &mdigest.to_wire());

    // end-3: GET by tag returns the manifest bytes + digest header + media-type.
    let resp = app.clone().oneshot(get("/v2/lib/x/manifests/latest")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("docker-content-digest").unwrap(), &mdigest.to_wire());
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/vnd.oci.image.manifest.v1+json"
    );
    assert_eq!(body_bytes(resp).await, manifest);

    // end-3: GET by digest returns the same manifest (digest is immutable).
    let resp = app
        .clone()
        .oneshot(get(&format!("/v2/lib/x/manifests/{}", mdigest.to_wire())))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, manifest);

    // Re-point `latest` at a DIFFERENT manifest (the pointer is mutable).
    let manifest2 = image_manifest(&Digest::of(HashAlgorithm::Sha256, b"different-cfg"));
    let mdigest2 = Digest::of(HashAlgorithm::Sha256, &manifest2);
    let req = put_body(
        "/v2/lib/x/manifests/latest",
        "application/vnd.oci.image.manifest.v1+json",
        manifest2.clone(),
    );
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::CREATED);
    let resp = app.oneshot(get("/v2/lib/x/manifests/latest")).await.unwrap();
    assert_eq!(resp.headers().get("docker-content-digest").unwrap(), &mdigest2.to_wire());
}

async fn end_3_manifest_unknown_on_missing_tag(app: Router) {
    let resp = app.oneshot(get("/v2/lib/x/manifests/nope")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["errors"][0]["code"], "MANIFEST_UNKNOWN");
}

async fn head_manifest_returns_digest_and_length(app: Router) {
    let manifest = image_manifest(&Digest::of(HashAlgorithm::Sha256, b"{}"));
    let mdigest = Digest::of(HashAlgorithm::Sha256, &manifest);
    let req = put_body(
        "/v2/lib/x/manifests/v1",
        "application/vnd.oci.image.manifest.v1+json",
        manifest.clone(),
    );
    app.clone().oneshot(req).await.unwrap();

    let resp = app.oneshot(head("/v2/lib/x/manifests/v1")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("docker-content-digest").unwrap(), &mdigest.to_wire());
    assert_eq!(
        resp.headers().get("content-length").unwrap(),
        &manifest.len().to_string()
    );
}

// ────────────────── end-7a/12: referrers reverse-index (lacre seam) ──────────

/// A signing manifest that declares a `subject` — the lacre attestation shape.
fn signing_manifest(subject: &Digest) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "artifactType": "application/vnd.dev.cosign.artifact.sig.v1+json",
        "config": { "mediaType": "application/vnd.oci.empty.v1+json", "digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a", "size": 2 },
        "layers": [],
        "subject": {
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": subject.to_wire(),
            "size": 100
        }
    }))
    .unwrap()
}

async fn end_7a_12_referrers_index_returns_the_signing_manifest(app: Router) {
    // Push a subject image manifest.
    let image = image_manifest(&Digest::of(HashAlgorithm::Sha256, b"{}"));
    let image_digest = Digest::of(HashAlgorithm::Sha256, &image);
    let req = put_body(
        &format!("/v2/lib/x/manifests/{}", image_digest.to_wire()),
        "application/vnd.oci.image.manifest.v1+json",
        image.clone(),
    );
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::CREATED);

    // Push a signing manifest whose subject IS the image (end-7a).
    let sig = signing_manifest(&image_digest);
    let sig_digest = Digest::of(HashAlgorithm::Sha256, &sig);
    let req = put_body(
        &format!("/v2/lib/x/manifests/{}", sig_digest.to_wire()),
        "application/vnd.oci.image.manifest.v1+json",
        sig.clone(),
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    // end-7a: the OCI-Subject header echoes the subject digest.
    assert_eq!(resp.headers().get("oci-subject").unwrap(), &image_digest.to_wire());

    // end-12: the referrers index for the image contains the signing manifest.
    let resp = app
        .clone()
        .oneshot(get(&format!("/v2/lib/x/referrers/{}", image_digest.to_wire())))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/vnd.oci.image.index.v1+json"
    );
    let json = body_json(resp).await;
    assert_eq!(json["schemaVersion"], 2);
    let manifests = json["manifests"].as_array().unwrap();
    assert_eq!(manifests.len(), 1, "the one signing manifest refers to the image");
    assert_eq!(manifests[0]["digest"], sig_digest.to_wire());
    assert_eq!(
        manifests[0]["artifactType"],
        "application/vnd.dev.cosign.artifact.sig.v1+json"
    );

    // end-12b: filter by a NON-matching artifactType yields an empty index.
    let resp = app
        .oneshot(get(&format!(
            "/v2/lib/x/referrers/{}?artifactType=application/vnd.example.other",
            image_digest.to_wire()
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("oci-filters-applied").unwrap(), "artifactType");
    let json = body_json(resp).await;
    assert!(json["manifests"].as_array().unwrap().is_empty());
}

// ────────────────── DIGEST_INVALID / malformed inputs ──────────────────

async fn malformed_digest_is_digest_invalid(app: Router) {
    let resp = app.oneshot(get("/v2/lib/x/blobs/sha256:short")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["errors"][0]["code"], "DIGEST_INVALID");
}

async fn blob_unknown_on_missing_blob(app: Router) {
    let d = Digest::of(HashAlgorithm::Sha256, b"nonexistent");
    let resp = app.oneshot(get(&format!("/v2/lib/x/blobs/{}", d.to_wire()))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["errors"][0]["code"], "BLOB_UNKNOWN");
}

// ────────────────── end-11: cross-repo mount dedups ──────────────────

async fn end_11_cross_repo_mount_dedups_shared_blob(app: Router) {
    let blob = b"shared layer".to_vec();
    let d = Digest::of(HashAlgorithm::Sha256, &blob);

    // Store the blob in repo A.
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v2/repo/a/blobs/uploads/?digest={}", d.to_wire()))
        .body(Body::from(blob.clone()))
        .unwrap();
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::CREATED);

    // Mount it into repo B via ?mount=&from= — no bytes move; immediate 201.
    let req = post_empty(&format!(
        "/v2/repo/b/blobs/uploads/?mount={}&from=repo/a",
        d.to_wire()
    ));
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "mount of a known blob is a pointer op");
    assert_eq!(resp.headers().get("docker-content-digest").unwrap(), &d.to_wire());

    // The blob is now servable from repo B (same content-address, one copy).
    let resp = app.oneshot(get(&format!("/v2/repo/b/blobs/{}", d.to_wire()))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, blob);
}

async fn end_11_mount_of_unknown_blob_falls_back_to_upload_session(app: Router) {
    let d = Digest::of(HashAlgorithm::Sha256, b"not stored anywhere");
    let req = post_empty(&format!(
        "/v2/repo/b/blobs/uploads/?mount={}&from=repo/a",
        d.to_wire()
    ));
    let resp = app.oneshot(req).await.unwrap();
    // Spec: a mount that can't be satisfied returns 202 with an upload location.
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert!(resp.headers().get("location").is_some());
}

// ────────────────── end-8: tags list + pagination ──────────────────

async fn end_8_tags_list_and_pagination(app: Router) {
    let manifest = image_manifest(&Digest::of(HashAlgorithm::Sha256, b"{}"));

    // Push the same manifest under three tags.
    for tag in ["a", "b", "c"] {
        let req = put_body(
            &format!("/v2/lib/x/manifests/{tag}"),
            "application/vnd.oci.image.manifest.v1+json",
            manifest.clone(),
        );
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::CREATED);
    }

    // Full list.
    let resp = app.clone().oneshot(get("/v2/lib/x/tags/list")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["name"], "lib/x");
    assert_eq!(json["tags"], serde_json::json!(["a", "b", "c"]));

    // Paginated: ?n=2 returns [a, b] + a Link header for the next page.
    let resp = app.clone().oneshot(get("/v2/lib/x/tags/list?n=2")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let link = resp.headers().get("link").unwrap().to_str().unwrap().to_string();
    assert!(link.contains("last=b"), "Link header points at last=b: {link}");
    let json = body_json(resp).await;
    assert_eq!(json["tags"], serde_json::json!(["a", "b"]));

    // Continue: ?n=2&last=b returns [c], no more Link.
    let resp = app.oneshot(get("/v2/lib/x/tags/list?n=2&last=b")).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["tags"], serde_json::json!(["c"]));
}

// ────────────────── end-9/10: deletes ──────────────────

async fn end_9_delete_manifest(app: Router) {
    let manifest = image_manifest(&Digest::of(HashAlgorithm::Sha256, b"{}"));
    let mdigest = Digest::of(HashAlgorithm::Sha256, &manifest);
    let req = put_body(
        "/v2/lib/x/manifests/latest",
        "application/vnd.oci.image.manifest.v1+json",
        manifest,
    );
    app.clone().oneshot(req).await.unwrap();

    // DELETE by digest → 202.
    let resp = app
        .clone()
        .oneshot(delete(&format!("/v2/lib/x/manifests/{}", mdigest.to_wire())))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // GET by the (now-dangling) tag → MANIFEST_UNKNOWN (the delete dropped the tag).
    let resp = app.oneshot(get("/v2/lib/x/manifests/latest")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["errors"][0]["code"], "MANIFEST_UNKNOWN");
}

async fn end_10_delete_blob(app: Router) {
    let blob = b"deletable".to_vec();
    let d = Digest::of(HashAlgorithm::Sha256, &blob);
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v2/lib/x/blobs/uploads/?digest={}", d.to_wire()))
        .body(Body::from(blob))
        .unwrap();
    app.clone().oneshot(req).await.unwrap();

    // DELETE → 202.
    let resp = app
        .clone()
        .oneshot(delete(&format!("/v2/lib/x/blobs/{}", d.to_wire())))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Deleting an already-absent blob → BLOB_UNKNOWN.
    let d2 = Digest::of(HashAlgorithm::Sha256, b"never existed");
    let resp = app
        .oneshot(delete(&format!("/v2/lib/x/blobs/{}", d2.to_wire())))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["errors"][0]["code"], "BLOB_UNKNOWN");
}

// ────────────────── store-path tombstone semantics ──────────────────
//
// These prove that a DELETE makes the object read back ABSENT, not
// present-but-empty. On `SuiCacheStore` a delete is an empty-object tombstone
// over a content-addressed backend; the handler must treat empty-as-absent so a
// GET/HEAD after delete is BLOB_UNKNOWN/MANIFEST_UNKNOWN, never a fake 200 with
// zero bytes. `MemStore` deletes by removing the entry, so it proves the same
// observable contract from the other implementation — a behavior a single-store
// matrix would leave unproven on the production path.

async fn deleted_blob_reads_back_absent(app: Router) {
    let blob = b"tombstone me".to_vec();
    let d = Digest::of(HashAlgorithm::Sha256, &blob);

    // Store, then delete.
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v2/lib/x/blobs/uploads/?digest={}", d.to_wire()))
        .body(Body::from(blob))
        .unwrap();
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::CREATED);
    assert_eq!(
        app.clone()
            .oneshot(delete(&format!("/v2/lib/x/blobs/{}", d.to_wire())))
            .await
            .unwrap()
            .status(),
        StatusCode::ACCEPTED
    );

    // GET after delete → BLOB_UNKNOWN (absent, NOT a 200 with empty bytes).
    let resp = app
        .clone()
        .oneshot(get(&format!("/v2/lib/x/blobs/{}", d.to_wire())))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["errors"][0]["code"], "BLOB_UNKNOWN");

    // HEAD after delete → BLOB_UNKNOWN too (has_blob must see the tombstone).
    let resp = app
        .oneshot(head(&format!("/v2/lib/x/blobs/{}", d.to_wire())))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

async fn deleted_manifest_reads_back_absent(app: Router) {
    let manifest = image_manifest(&Digest::of(HashAlgorithm::Sha256, b"tombstone-cfg"));
    let mdigest = Digest::of(HashAlgorithm::Sha256, &manifest);

    // Store by digest, then delete by digest.
    let req = put_body(
        &format!("/v2/lib/x/manifests/{}", mdigest.to_wire()),
        "application/vnd.oci.image.manifest.v1+json",
        manifest,
    );
    assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::CREATED);
    assert_eq!(
        app.clone()
            .oneshot(delete(&format!("/v2/lib/x/manifests/{}", mdigest.to_wire())))
            .await
            .unwrap()
            .status(),
        StatusCode::ACCEPTED
    );

    // GET by digest after delete → MANIFEST_UNKNOWN (the empty-tombstone reads
    // as absent, not a 200 with a zero-length manifest).
    let resp = app
        .oneshot(get(&format!("/v2/lib/x/manifests/{}", mdigest.to_wire())))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["errors"][0]["code"], "MANIFEST_UNKNOWN");
}

// ────────────────── manifest digest-reference mismatch ──────────────────

async fn put_manifest_by_digest_mismatch_is_digest_invalid(app: Router) {
    let manifest = image_manifest(&Digest::of(HashAlgorithm::Sha256, b"{}"));
    // Reference a WRONG digest in the URL.
    let wrong = Digest::of(HashAlgorithm::Sha256, b"wrong");
    let req = put_body(
        &format!("/v2/lib/x/manifests/{}", wrong.to_wire()),
        "application/vnd.oci.image.manifest.v1+json",
        manifest,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["errors"][0]["code"], "DIGEST_INVALID");
}
