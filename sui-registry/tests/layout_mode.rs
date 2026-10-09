//! porto's read-only OCI-image-layout mode, proven end to end.
//!
//! A fixture layout is built on disk the way a Nix derivation emits one
//! (`oci-layout`, `index.json` with `org.opencontainers.image.ref.name`,
//! `blobs/sha256/*`) around a REAL Helm chart tarball and Helm media types.
//! The registry is then driven the way `helm pull` / `oras pull` drive it:
//! catalog, tags/list, manifest by tag and by digest (GET + HEAD), blob fetch,
//! referrers. Every write verb is refused with `DENIED`; conflicting mounts
//! and corrupted store paths are refused, never served.
//!
//! The gold-standard test (`helm_pull_round_trips_the_chart`) runs the real
//! `helm` binary against a real TCP listener. It is `#[ignore]`d because the
//! CI runner is not guaranteed to have `helm` — run it with
//! `cargo test -p sui-registry --test layout_mode -- --ignored`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use sui_compat::hash::HashAlgorithm;
use sui_registry::config::{BlobVerification, LayoutMount, RegistryConfig};
use sui_registry::digest::Digest;
use sui_registry::server::{build_router, AppState};
use sui_registry::store::{LayoutError, LayoutStore, MemStore, OverlayStore, RegistryStore};

const HELM_CONFIG: &str = "application/vnd.cncf.helm.config.v1+json";
const HELM_CHART: &str = "application/vnd.cncf.helm.chart.content.v1.tar+gzip";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const REPO: &str = "pleme-io/charts";

/// A descriptor of a blob written into a fixture layout.
#[derive(Clone)]
struct Blob {
    digest: String,
    size: usize,
    bytes: Vec<u8>,
}

fn write_blob(layout: &Path, bytes: &[u8]) -> Blob {
    let d = Digest::of(HashAlgorithm::Sha256, bytes);
    let dir = layout.join("blobs").join("sha256");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(d.hex()), bytes).unwrap();
    Blob {
        digest: d.to_wire(),
        size: bytes.len(),
        bytes: bytes.to_vec(),
    }
}

/// A real Helm chart `.tgz` (`<name>/Chart.yaml` + `values.yaml`).
fn chart_tgz(name: &str, version: &str) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    {
        let mut tar = tar::Builder::new(&mut gz);
        let files = [
            (
                format!("{name}/Chart.yaml"),
                format!("apiVersion: v2\nname: {name}\nversion: {version}\ntype: application\n"),
            ),
            (format!("{name}/values.yaml"), "replicas: 1\n".to_string()),
        ];
        for (path, body) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            tar.append_data(&mut header, path, body.as_bytes()).unwrap();
        }
        tar.finish().unwrap();
    }
    gz.flush().unwrap();
    gz.finish().unwrap()
}

/// One chart version written into `layout`; returns (manifest, chart) blobs.
fn add_chart(layout: &Path, name: &str, version: &str) -> (Blob, Blob) {
    let chart = write_blob(layout, &chart_tgz(name, version));
    let config = write_blob(
        layout,
        serde_json::to_vec(&json!({"apiVersion": "v2", "name": name, "version": version}))
            .unwrap()
            .as_slice(),
    );
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST,
        "config": {"mediaType": HELM_CONFIG, "digest": config.digest, "size": config.size},
        "layers": [{"mediaType": HELM_CHART, "digest": chart.digest, "size": chart.size}],
    });
    let manifest = write_blob(layout, &serde_json::to_vec(&manifest).unwrap());
    (manifest, chart)
}

/// A signature-shaped referrer of `subject`, written into `layout`.
fn add_referrer(layout: &Path, subject: &Blob) -> Blob {
    let empty = write_blob(layout, b"{}");
    let sig = json!({
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST,
        "artifactType": "application/vnd.dev.cosign.artifact.sig.v1+json",
        "config": {"mediaType": "application/vnd.oci.empty.v1+json", "digest": empty.digest, "size": empty.size},
        "layers": [],
        "subject": {"mediaType": OCI_MANIFEST, "digest": subject.digest, "size": subject.size},
    });
    write_blob(layout, &serde_json::to_vec(&sig).unwrap())
}

/// Write `oci-layout` + `index.json` naming `entries` as (manifest, ref.name).
fn finish_layout(layout: &Path, entries: &[(&Blob, Option<&str>)]) {
    std::fs::write(layout.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
    let manifests: Vec<Value> = entries
        .iter()
        .map(|(m, ref_name)| {
            let mut d = json!({"mediaType": OCI_MANIFEST, "digest": m.digest, "size": m.size});
            if let Some(r) = ref_name {
                d["annotations"] = json!({"org.opencontainers.image.ref.name": r});
            }
            d
        })
        .collect();
    let index = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": manifests,
    });
    std::fs::write(layout.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
}

/// The standard fixture: one layout carrying `demo:0.1.0` + a referrer of it.
struct Fixture {
    _dir: tempfile::TempDir,
    layout: PathBuf,
    manifest: Blob,
    chart: Blob,
    referrer: Blob,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let layout = dir.path().join("charts");
    let (manifest, chart) = add_chart(&layout, "demo", "0.1.0");
    let referrer = add_referrer(&layout, &manifest);
    finish_layout(&layout, &[(&manifest, Some("demo:0.1.0")), (&referrer, None)]);
    Fixture {
        _dir: dir,
        layout,
        manifest,
        chart,
        referrer,
    }
}

fn mount(layout: &Path) -> LayoutMount {
    LayoutMount {
        repository: REPO.to_string(),
        layout: layout.to_path_buf(),
        verify: BlobVerification::Eager,
    }
}

fn app(store: Arc<dyn RegistryStore>) -> Router {
    build_router(AppState::new(store, RegistryConfig::bare()))
}

fn layout_app(mounts: &[LayoutMount]) -> Router {
    let layouts = LayoutStore::load(mounts).unwrap();
    app(Arc::new(OverlayStore::new(layouts, None)))
}

async fn send(app: &Router, method: &str, uri: &str, body: Vec<u8>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let resp = app
        .clone()
        .oneshot(Request::builder().method(method).uri(uri).body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, headers, bytes)
}

fn header<'a>(h: &'a axum::http::HeaderMap, name: &str) -> &'a str {
    h.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

// ─────────────────────────── the read surface ───────────────────────────

#[tokio::test]
async fn catalog_and_tags_come_from_index_json() {
    let f = fixture();
    let app = layout_app(&[mount(&f.layout)]);

    let (status, _, body) = send(&app, "GET", "/v2/_catalog", vec![]).await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["repositories"], json!(["pleme-io/charts", "pleme-io/charts/demo"]));

    let (status, _, body) = send(&app, "GET", "/v2/pleme-io/charts/demo/tags/list", vec![]).await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body, json!({"name": "pleme-io/charts/demo", "tags": ["0.1.0"]}));
}

#[tokio::test]
async fn manifest_by_tag_and_by_digest_carry_type_and_digest() {
    let f = fixture();
    let app = layout_app(&[mount(&f.layout)]);
    let by_digest = format!("/v2/pleme-io/charts/demo/manifests/{}", f.manifest.digest);

    for uri in ["/v2/pleme-io/charts/demo/manifests/0.1.0", by_digest.as_str()] {
        let (status, h, body) = send(&app, "GET", uri, vec![]).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(header(&h, "content-type"), OCI_MANIFEST);
        assert_eq!(header(&h, "docker-content-digest"), f.manifest.digest);
        assert_eq!(body, f.manifest.bytes);

        let (status, h, body) = send(&app, "HEAD", uri, vec![]).await;
        assert_eq!(status, StatusCode::OK, "HEAD {uri}");
        assert_eq!(header(&h, "content-length"), f.manifest.size.to_string());
        assert_eq!(header(&h, "docker-content-digest"), f.manifest.digest);
        assert!(body.is_empty());
    }

    let (status, _, body) = send(&app, "GET", "/v2/pleme-io/charts/demo/manifests/9.9.9", vec![]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(String::from_utf8_lossy(&body).contains("MANIFEST_UNKNOWN"));
}

#[tokio::test]
async fn blobs_are_served_with_their_digest() {
    let f = fixture();
    let app = layout_app(&[mount(&f.layout)]);
    let uri = format!("/v2/pleme-io/charts/demo/blobs/{}", f.chart.digest);
    let (status, h, body) = send(&app, "GET", &uri, vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&h, "docker-content-digest"), f.chart.digest);
    assert_eq!(body, f.chart.bytes);

    let (status, h, _) = send(&app, "HEAD", &uri, vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&h, "content-length"), f.chart.size.to_string());

    let absent = Digest::of(HashAlgorithm::Sha256, b"not in the layout").to_wire();
    let (status, _, _) =
        send(&app, "GET", &format!("/v2/pleme-io/charts/demo/blobs/{absent}"), vec![]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn referrers_are_derived_from_subject_fields() {
    let f = fixture();
    let app = layout_app(&[mount(&f.layout)]);
    // The untagged referrer lives in the mount's base repository.
    let uri = format!("/v2/pleme-io/charts/referrers/{}", f.manifest.digest);
    let (status, h, body) = send(&app, "GET", &uri, vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&h, "content-type"), "application/vnd.oci.image.index.v1+json");
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["manifests"][0]["digest"], json!(f.referrer.digest));
    assert_eq!(
        body["manifests"][0]["artifactType"],
        json!("application/vnd.dev.cosign.artifact.sig.v1+json")
    );
}

#[tokio::test]
async fn tags_survive_a_restart_because_they_are_the_layout() {
    let f = fixture();
    for _ in 0..2 {
        let app = layout_app(&[mount(&f.layout)]);
        let (status, _, _) =
            send(&app, "GET", "/v2/pleme-io/charts/demo/manifests/0.1.0", vec![]).await;
        assert_eq!(status, StatusCode::OK);
    }
}

// ─────────────────────────── writes are refused ───────────────────────────

#[tokio::test]
async fn every_write_verb_is_denied() {
    let f = fixture();
    let app = layout_app(&[mount(&f.layout)]);
    let manifest_uri = "/v2/pleme-io/charts/demo/manifests/0.1.0".to_string();
    let blob_uri = format!("/v2/pleme-io/charts/demo/blobs/{}", f.chart.digest);
    let digest = &f.chart.digest;
    let writes = [
        ("PUT", manifest_uri.clone(), f.manifest.bytes.clone()),
        ("DELETE", manifest_uri, vec![]),
        ("DELETE", blob_uri, vec![]),
        ("POST", "/v2/pleme-io/charts/demo/blobs/uploads/".to_string(), vec![]),
        (
            "POST",
            format!("/v2/pleme-io/charts/demo/blobs/uploads/?digest={digest}"),
            f.chart.bytes.clone(),
        ),
        ("PATCH", "/v2/pleme-io/charts/demo/blobs/uploads/x".to_string(), b"x".to_vec()),
        ("PUT", format!("/v2/pleme-io/charts/demo/blobs/uploads/x?digest={digest}"), vec![]),
        // A brand-new repository: no writable backend exists at all.
        ("POST", "/v2/someone/else/blobs/uploads/".to_string(), vec![]),
    ];
    for (method, uri, body) in writes {
        let (status, _, body) = send(&app, method, &uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["errors"][0]["code"], "DENIED", "{method} {uri}");
    }
    // Still intact afterwards.
    let (status, _, _) = send(&app, "GET", "/v2/pleme-io/charts/demo/manifests/0.1.0", vec![]).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_writable_backend_serves_other_repos_but_never_shadows_a_layout() {
    let f = fixture();
    let layouts = LayoutStore::load(&[mount(&f.layout)]).unwrap();
    let app = app(Arc::new(OverlayStore::new(layouts, Some(Arc::new(MemStore::new())))));

    let pushed = b"pushed".to_vec();
    let d = Digest::of(HashAlgorithm::Sha256, &pushed).to_wire();
    let (status, _, _) =
        send(&app, "POST", &format!("/v2/team/app/blobs/uploads/?digest={d}"), pushed).await;
    assert_eq!(status, StatusCode::CREATED, "a non-layout repo is writable");
    let (status, _, _) = send(&app, "PUT", "/v2/team/app/manifests/v1", f.manifest.bytes.clone()).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _, _) = send(
        &app,
        "PUT",
        "/v2/pleme-io/charts/demo/manifests/0.1.0",
        f.manifest.bytes.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a layout repo stays read-only");

    let (_, _, body) = send(&app, "GET", "/v2/_catalog", vec![]).await;
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        body["repositories"],
        json!(["pleme-io/charts", "pleme-io/charts/demo", "team/app"])
    );
}

// ─────────────────────────── mounts compose, conflicts refuse ───────────────────────────

#[tokio::test]
async fn two_layouts_contribute_to_one_repository() {
    let f = fixture();
    let dir = tempfile::tempdir().unwrap();
    let second = dir.path().join("more");
    let (m2, _) = add_chart(&second, "demo", "0.2.0");
    // The same demo:0.1.0 again (identical digest) is agreement, not conflict.
    let (m1_again, _) = add_chart(&second, "demo", "0.1.0");
    assert_eq!(m1_again.digest, f.manifest.digest);
    finish_layout(&second, &[(&m2, Some("demo:0.2.0")), (&m1_again, Some("demo:0.1.0"))]);

    let app = layout_app(&[mount(&f.layout), mount(&second)]);
    let (_, _, body) = send(&app, "GET", "/v2/pleme-io/charts/demo/tags/list", vec![]).await;
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["tags"], json!(["0.1.0", "0.2.0"]));
}

#[test]
fn conflicting_tags_across_mounts_are_a_startup_error() {
    let f = fixture();
    let dir = tempfile::tempdir().unwrap();
    let other = dir.path().join("other");
    // A different chart body published under the same name:version.
    let (different, _) = add_chart(&other, "demo-fork", "0.1.0");
    finish_layout(&other, &[(&different, Some("demo:0.1.0"))]);

    let err = LayoutStore::load(&[mount(&f.layout), mount(&other)]).unwrap_err();
    match err {
        LayoutError::ConflictingTag(conflict) => {
            assert_eq!(conflict.repository, "pleme-io/charts/demo");
            assert_eq!(conflict.tag, "0.1.0");
            assert_ne!(conflict.first, conflict.second);
        }
        other => panic!("expected ConflictingTag, got {other}"),
    }
}

// ─────────────────────────── corruption is refused ───────────────────────────

fn corrupt(layout: &Path, blob: &Blob) {
    let path = layout.join("blobs/sha256").join(blob.digest.trim_start_matches("sha256:"));
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff; // same size, different content
    std::fs::write(&path, bytes).unwrap();
}

#[test]
fn a_corrupt_blob_fails_an_eager_load() {
    let f = fixture();
    corrupt(&f.layout, &f.chart);
    let err = LayoutStore::load(&[mount(&f.layout)]).unwrap_err();
    assert!(matches!(err, LayoutError::DigestMismatch { .. }), "got {err}");
}

#[test]
fn a_corrupt_manifest_fails_even_a_lazy_load() {
    let f = fixture();
    corrupt(&f.layout, &f.manifest);
    let lazy = LayoutMount {
        verify: BlobVerification::Lazy,
        ..mount(&f.layout)
    };
    let err = LayoutStore::load(&[lazy]).unwrap_err();
    assert!(matches!(err, LayoutError::DigestMismatch { .. }), "got {err}");
}

#[tokio::test]
async fn a_corrupt_blob_under_lazy_verification_is_refused_not_served() {
    let f = fixture();
    corrupt(&f.layout, &f.chart);
    let lazy = LayoutMount {
        verify: BlobVerification::Lazy,
        ..mount(&f.layout)
    };
    let app = layout_app(&[lazy]);
    let uri = format!("/v2/pleme-io/charts/demo/blobs/{}", f.chart.digest);
    for _ in 0..2 {
        // Twice: the cached verdict must keep refusing.
        let (status, _, body) = send(&app, "GET", &uri, vec![]).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.is_empty(), "no corrupt byte may reach the client");
    }
}

#[test]
fn a_size_lie_in_a_descriptor_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let layout = dir.path().join("l");
    let (mut manifest, _) = add_chart(&layout, "demo", "0.1.0");
    manifest.size += 1;
    finish_layout(&layout, &[(&manifest, Some("demo:0.1.0"))]);
    let err = LayoutStore::load(&[mount(&layout)]).unwrap_err();
    assert!(matches!(err, LayoutError::SizeMismatch { .. }), "got {err}");
}

#[test]
fn a_directory_that_is_not_a_layout_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let err = LayoutStore::load(&[mount(dir.path())]).unwrap_err();
    assert!(matches!(err, LayoutError::Io { .. }), "got {err}");
    std::fs::write(dir.path().join("oci-layout"), r#"{"imageLayoutVersion":"2.0.0"}"#).unwrap();
    let err = LayoutStore::load(&[mount(dir.path())]).unwrap_err();
    assert!(matches!(err, LayoutError::UnsupportedLayoutVersion { .. }), "got {err}");
}

// ─────────────────────────── the real client ───────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs `helm` on PATH; run with --ignored"]
async fn helm_pull_round_trips_the_chart() {
    let f = fixture();
    let layouts = LayoutStore::load(&[mount(&f.layout)]).unwrap();
    let router = app(Arc::new(OverlayStore::new(layouts, None)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let work = tempfile::tempdir().unwrap();
    let out = work.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let output = tokio::process::Command::new("helm")
        .args([
            "pull",
            &format!("oci://127.0.0.1:{port}/pleme-io/charts/demo"),
            "--version",
            "0.1.0",
            "--plain-http",
            "--destination",
        ])
        .arg(&out)
        // Isolate helm from the operator's real config/cache/credentials.
        .env("HELM_CACHE_HOME", work.path().join("cache"))
        .env("HELM_CONFIG_HOME", work.path().join("config"))
        .env("HELM_DATA_HOME", work.path().join("data"))
        .env("HELM_REGISTRY_CONFIG", work.path().join("registry.json"))
        .output()
        .await
        .expect("helm must be on PATH for this test");
    assert!(
        output.status.success(),
        "helm pull failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let pulled = std::fs::read(out.join("demo-0.1.0.tgz")).unwrap();
    assert_eq!(pulled, f.chart.bytes, "helm must receive the exact layout bytes");
}
