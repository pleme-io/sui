//! The axum OCI `/v2/` registry server: [`AppState`], [`build_router`], and the
//! endpoint handlers (end-1 … end-14).
//!
//! Every handler returns `Result<impl IntoResponse, OciError>`, so a failure is
//! ALWAYS a typed OCI wire error (code + status + JSON body), never a fake 200
//! and never a bare panic. Deferred behaviors return a typed
//! [`OciError::Unsupported`] — the endpoints are all wired, and any not fully
//! spec-complete degrade to a typed error, never a silent success.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, RawQuery, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;

use sui_compat::hash::HashAlgorithm;

use crate::config::RegistryConfig;
use crate::digest::{is_valid_name, Digest, Reference};
use crate::error::OciError;
use crate::oci::{Descriptor, ManifestView, ReferrersIndex, TagsList};
use crate::route::OciRoute;
use crate::store::{Referrer, RegistryStore, StoreError, StoredManifest};
use crate::upload::{parse_content_range, UploadError, UploadSessions};

/// The OCI API version header value every `/v2/` response advertises.
const API_VERSION: &str = "registry/2.0";

/// Widen a byte length to `u64` without a lossy cast (satisfies the pedantic
/// cast lint on every target; a length never truncates).
#[inline]
fn len_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Shared state for all handlers.
#[derive(Clone)]
pub struct AppState {
    /// The typed storage seam (mem or sui-cache-backed).
    pub store: Arc<dyn RegistryStore>,
    /// In-flight blob-upload sessions.
    pub uploads: Arc<UploadSessions>,
    /// Server configuration.
    pub config: RegistryConfig,
}

impl AppState {
    /// Construct app state around a store, with fresh upload sessions.
    #[must_use]
    pub fn new(store: Arc<dyn RegistryStore>, config: RegistryConfig) -> Self {
        Self {
            store,
            uploads: Arc::new(UploadSessions::new()),
            config,
        }
    }
}

/// A store-layer error is a server-internal failure, not a client error — it
/// becomes a bare 500 at the edge (never a fake OCI success code that would
/// mislead a client into thinking its request was malformed).
fn store_err(e: StoreError) -> Response {
    tracing::error!("registry store error: {e}");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// Build the OCI `/v2/` router.
///
/// OCI repository names contain `/`, which axum's single-segment `{param}`
/// cannot capture — so the router is one catch-all `/v2/{*path}` whose tail is
/// parsed by [`OciRoute`] and dispatched by `(method, route)`. The base
/// endpoint `GET /v2/` is registered explicitly (its tail is empty). The body
/// limit is disabled by default (image layers exceed axum's 2 MiB default);
/// `config.max_body_bytes` re-imposes a ceiling when set.
#[must_use]
pub fn build_router(state: AppState) -> Router {
    let body_layer = match state.config.max_body_bytes {
        Some(n) => DefaultBodyLimit::max(n),
        None => DefaultBodyLimit::disable(),
    };
    Router::new()
        // end-1: the base endpoint (empty tail).
        .route("/v2/", get(api_version))
        // Every other OCI path — the name may span multiple `/` segments.
        .route("/v2/{*path}", any(dispatch))
        .layer(body_layer)
        .with_state(state)
}

/// Parse a raw query string into a small key→value map (last value wins).
///
/// A pure helper so the query-typed structs stay simple; OCI query params are
/// all single-valued scalars.
fn parse_query(raw: Option<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(raw) = raw else { return map };
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(
            percent_decode(k),
            percent_decode(v),
        );
    }
    map
}

/// Minimal percent-decoding for query values (`%XX` + `+`→space). OCI query
/// values are digests / tags / artifact-types — a small ASCII surface. Kept
/// dependency-free; a malformed `%XX` is left verbatim (never a panic).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// The single catch-all dispatcher: parse the `/v2/` tail into a typed
/// [`OciRoute`] and route by `(method, route)`. Every arm delegates to a
/// handler that owns exactly one endpoint's logic; an unmatched
/// `(method, route)` is a typed OCI error, never a bodiless framework 404.
#[allow(clippy::too_many_lines)]
async fn dispatch(
    State(state): State<AppState>,
    method: Method,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let q = parse_query(query.as_deref());
    match OciRoute::parse(&path) {
        OciRoute::Blob { name, digest } => match method {
            Method::GET => get_blob(&state, &name, &digest).await,
            Method::HEAD => head_blob(&state, &name, &digest).await,
            Method::DELETE => delete_blob(&state, &name, &digest).await,
            _ => method_not_allowed(),
        },
        OciRoute::UploadStart { name } => match method {
            Method::POST => start_upload(&state, &name, &q, &body).await,
            _ => method_not_allowed(),
        },
        OciRoute::Upload { name, uuid } => match method {
            Method::GET => upload_status(&state, &name, &uuid).await,
            Method::PATCH => patch_upload(&state, &name, &uuid, &headers, &body).await,
            Method::PUT => finalize_upload(&state, &name, &uuid, &q, &body).await,
            Method::DELETE => cancel_upload(&state, &uuid).await,
            _ => method_not_allowed(),
        },
        OciRoute::Manifest { name, reference } => match method {
            Method::GET => get_manifest(&state, &name, &reference).await,
            Method::HEAD => head_manifest(&state, &name, &reference).await,
            Method::PUT => put_manifest(&state, &name, &reference, &headers, &body).await,
            Method::DELETE => delete_manifest(&state, &name, &reference).await,
            _ => method_not_allowed(),
        },
        OciRoute::Tags { name } => match method {
            Method::GET => list_tags(&state, &name, &q).await,
            _ => method_not_allowed(),
        },
        OciRoute::Referrers { name, digest } => match method {
            Method::GET => get_referrers(&state, &name, &digest, &q).await,
            _ => method_not_allowed(),
        },
        OciRoute::Unknown => {
            OciError::NameUnknown("no such registry endpoint".to_string()).into_response()
        }
    }
}

/// A method not allowed on an otherwise-valid resource is a typed UNSUPPORTED
/// (the spec's `405`-shaped case for a v2 registry), never a silent success.
fn method_not_allowed() -> Response {
    let mut resp = OciError::Unsupported("method not allowed on this resource".to_string())
        .into_response();
    *resp.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    resp
}

// ─────────────────────────── end-1 ───────────────────────────

/// `GET /v2/` — the OCI base endpoint. 200 + the API-version header.
async fn api_version() -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        "docker-distribution-api-version",
        HeaderValue::from_static(API_VERSION),
    );
    (StatusCode::OK, headers).into_response()
}

// ─────────────────────────── helpers ───────────────────────────

/// Validate a repository name at the border, or NAME_UNKNOWN.
fn checked_name(name: &str) -> Result<&str, OciError> {
    if is_valid_name(name) {
        Ok(name)
    } else {
        Err(OciError::NameUnknown(format!("invalid repository name: {name}")))
    }
}

/// The `Docker-Content-Digest` header value for a digest.
fn content_digest_header(d: &Digest) -> HeaderValue {
    // to_wire() is `sha256:<hex>` — always valid header bytes.
    HeaderValue::from_str(&d.to_wire())
        .unwrap_or_else(|_| HeaderValue::from_static("sha256:0"))
}

// ─────────────────────────── end-2: blobs ───────────────────────────

/// `GET /v2/<name>/blobs/<digest>` — download a blob (end-2).
async fn get_blob(state: &AppState, name: &str, digest: &str) -> Response {
    if let Err(e) = checked_name(name) {
        return e.into_response();
    }
    let d = match Digest::parse(digest) {
        Ok(d) => d,
        Err(e) => return OciError::from(e).into_response(),
    };
    match state.store.get_blob(&d).await {
        Ok(Some(bytes)) => {
            let mut headers = HeaderMap::new();
            headers.insert("docker-content-digest", content_digest_header(&d));
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            (StatusCode::OK, headers, bytes).into_response()
        }
        Ok(None) => OciError::BlobUnknown(d.to_wire()).into_response(),
        Err(e) => store_err(e),
    }
}

/// `HEAD /v2/<name>/blobs/<digest>` — blob existence + Content-Length (end-2).
async fn head_blob(state: &AppState, name: &str, digest: &str) -> Response {
    if let Err(e) = checked_name(name) {
        return e.into_response();
    }
    let d = match Digest::parse(digest) {
        Ok(d) => d,
        Err(e) => return OciError::from(e).into_response(),
    };
    match state.store.get_blob(&d).await {
        Ok(Some(bytes)) => {
            let mut headers = HeaderMap::new();
            headers.insert("docker-content-digest", content_digest_header(&d));
            headers.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from(len_u64(bytes.len())),
            );
            (StatusCode::OK, headers).into_response()
        }
        Ok(None) => OciError::BlobUnknown(d.to_wire()).into_response(),
        Err(e) => store_err(e),
    }
}

/// `DELETE /v2/<name>/blobs/<digest>` — delete a blob (end-10).
async fn delete_blob(state: &AppState, name: &str, digest: &str) -> Response {
    if let Err(e) = checked_name(name) {
        return e.into_response();
    }
    let d = match Digest::parse(digest) {
        Ok(d) => d,
        Err(e) => return OciError::from(e).into_response(),
    };
    // The spec requires the blob to exist for a 202; a missing blob is
    // BLOB_UNKNOWN.
    match state.store.has_blob(&d).await {
        Ok(true) => match state.store.delete_blob(&d).await {
            Ok(()) => StatusCode::ACCEPTED.into_response(),
            Err(e) => store_err(e),
        },
        Ok(false) => OciError::BlobUnknown(d.to_wire()).into_response(),
        Err(e) => store_err(e),
    }
}

// ─────────────────────────── end-4: start upload ───────────────────────────

/// `POST /v2/<name>/blobs/uploads/` — start an upload (end-4a), or a monolithic
/// upload with `?digest=` (end-4b), or a cross-repo mount `?mount=&from=`
/// (end-11).
async fn start_upload(
    state: &AppState,
    name: &str,
    q: &HashMap<String, String>,
    body: &Bytes,
) -> Response {
    let name = match checked_name(name) {
        Ok(n) => n.to_string(),
        Err(e) => return e.into_response(),
    };

    // end-11: cross-repo mount. If the mount digest already exists in the
    // store, this is a pure pointer op — return 201 Created immediately (the
    // content is shared by content-address; no bytes move).
    if let Some(mount) = q.get("mount") {
        let d = match Digest::parse(mount) {
            Ok(d) => d,
            Err(e) => return OciError::from(e).into_response(),
        };
        // `from` is advisory in a content-addressed store — the blob is global
        // by digest, so if we have it, the mount succeeds regardless of source.
        let _ = q.get("from");
        match state.store.has_blob(&d).await {
            Ok(true) => return created_blob(&name, &d),
            Ok(false) => {
                // Cannot mount an absent blob — fall through to a normal upload
                // session (the spec says respond 202 with an upload location).
                let id = state.uploads.start(&name);
                return upload_accepted(&name, &id.to_string(), 0);
            }
            Err(e) => return store_err(e),
        }
    }

    // end-4b: monolithic upload — `?digest=` present, body is the whole blob.
    if let Some(digest) = q.get("digest") {
        let claimed = match Digest::parse(digest) {
            Ok(d) => d,
            Err(e) => return OciError::from(e).into_response(),
        };
        // Verify the body against the claimed digest at the border.
        let computed = Digest::of(claimed.algorithm(), body);
        if computed != claimed {
            return OciError::DigestInvalid(format!(
                "monolithic upload body hashes to {computed}, not claimed {claimed}"
            ))
            .into_response();
        }
        return match state.store.put_blob(&claimed, body).await {
            Ok(()) => created_blob(&name, &claimed),
            Err(e) => store_err(e),
        };
    }

    // end-4a: plain start — allocate a session, return 202 + Location.
    let id = state.uploads.start(&name);
    upload_accepted(&name, &id.to_string(), 0)
}

/// A `201 Created` blob response with the canonical Location + digest headers.
fn created_blob(name: &str, d: &Digest) -> Response {
    let mut headers = HeaderMap::new();
    if let Ok(loc) = HeaderValue::from_str(&crate::oci::BlobLocation { name, digest: d }.to_string()) {
        headers.insert(header::LOCATION, loc);
    }
    headers.insert("docker-content-digest", content_digest_header(d));
    (StatusCode::CREATED, headers).into_response()
}

/// A `202 Accepted` upload-session response with Location + Range + upload-uuid.
fn upload_accepted(name: &str, id: &str, upper: u64) -> Response {
    let mut headers = HeaderMap::new();
    if let Ok(loc) = HeaderValue::from_str(&crate::oci::UploadLocation { name, id }.to_string()) {
        headers.insert(header::LOCATION, loc);
    }
    if let Ok(id_hv) = HeaderValue::from_str(id) {
        headers.insert("docker-upload-uuid", id_hv);
    }
    // Range is `0-<upper>` where upper is the last written byte offset. For a
    // fresh session (upper == 0 bytes), the spec's convention is `0-0`.
    if let Ok(range) = HeaderValue::from_str(&crate::oci::ByteRange { end: upper }.to_string()) {
        headers.insert(header::RANGE, range);
    }
    (StatusCode::ACCEPTED, headers).into_response()
}

// ─────────────────────────── end-5: chunk ───────────────────────────

/// `PATCH /v2/<name>/blobs/uploads/<uuid>` — append a chunk (end-5).
async fn patch_upload(
    state: &AppState,
    name: &str,
    uuid: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    let name = match checked_name(name) {
        Ok(n) => n.to_string(),
        Err(e) => return e.into_response(),
    };
    // Determine the chunk's declared start offset. With a Content-Range header,
    // it's the range start; without, it's the session's current offset
    // (a streaming PATCH appends at the tail).
    let declared_start = match headers.get(header::CONTENT_RANGE) {
        Some(hv) => {
            let Ok(raw) = hv.to_str() else {
                return OciError::BlobUploadInvalid("non-ascii Content-Range".to_string())
                    .into_response();
            };
            match parse_content_range(raw) {
                Some((start, _end)) => Some(start),
                None => {
                    return OciError::BlobUploadInvalid(format!(
                        "malformed Content-Range: {raw}"
                    ))
                    .into_response();
                }
            }
        }
        None => None,
    };

    let start = match declared_start {
        Some(s) => s,
        None => match state.uploads.offset(uuid) {
            Ok(off) => off,
            Err(e) => return upload_err(e),
        },
    };

    match state.uploads.patch(uuid, start, body) {
        Ok(new_len) => {
            let mut resp_headers = HeaderMap::new();
            if let Ok(loc) =
                HeaderValue::from_str(&crate::oci::UploadLocation { name: &name, id: uuid }.to_string())
            {
                resp_headers.insert(header::LOCATION, loc);
            }
            if let Ok(id_hv) = HeaderValue::from_str(uuid) {
                resp_headers.insert("docker-upload-uuid", id_hv);
            }
            // Range echo: 0-(new_len - 1) is the inclusive last-byte offset.
            if let Ok(range) =
                HeaderValue::from_str(&crate::oci::ByteRange { end: new_len.saturating_sub(1) }.to_string())
            {
                resp_headers.insert(header::RANGE, range);
            }
            (StatusCode::ACCEPTED, resp_headers).into_response()
        }
        Err(e) => upload_err(e),
    }
}

/// Map an upload FSM error to its typed OCI wire code.
fn upload_err(e: UploadError) -> Response {
    match e {
        UploadError::UnknownSession(id) => OciError::BlobUploadUnknown(id).into_response(),
        UploadError::OutOfOrder { have, got } => OciError::BlobUploadInvalid(format!(
            "out-of-order chunk: session at {have}, chunk starts at {got}"
        ))
        .into_response(),
        UploadError::DigestMismatch { computed, claimed } => OciError::DigestInvalid(format!(
            "finalize digest mismatch: computed {computed}, claimed {claimed}"
        ))
        .into_response(),
    }
}

// ─────────────────────────── end-6: finalize ───────────────────────────

/// `PUT /v2/<name>/blobs/uploads/<uuid>?digest=<d>` — finalize (end-6).
async fn finalize_upload(
    state: &AppState,
    name: &str,
    uuid: &str,
    q: &HashMap<String, String>,
    body: &Bytes,
) -> Response {
    let name = match checked_name(name) {
        Ok(n) => n.to_string(),
        Err(e) => return e.into_response(),
    };
    let Some(digest) = q.get("digest") else {
        return OciError::DigestInvalid(
            "finalize requires ?digest=<content-address>".to_string(),
        )
        .into_response();
    };
    let claimed = match Digest::parse(digest) {
        Ok(d) => d,
        Err(e) => return OciError::from(e).into_response(),
    };
    let trailing = if body.is_empty() { None } else { Some(body.as_ref()) };
    match state.uploads.finalize(uuid, trailing, &claimed) {
        Ok(fin) => match state.store.put_blob(&fin.digest, &fin.bytes).await {
            Ok(()) => created_blob(&name, &fin.digest),
            Err(e) => store_err(e),
        },
        Err(e) => upload_err(e),
    }
}

// ─────────────────────────── end-13: status ───────────────────────────

/// `GET /v2/<name>/blobs/uploads/<uuid>` — upload status (end-13).
async fn upload_status(state: &AppState, name: &str, uuid: &str) -> Response {
    let name = match checked_name(name) {
        Ok(n) => n.to_string(),
        Err(e) => return e.into_response(),
    };
    match state.uploads.offset(uuid) {
        Ok(off) => {
            let mut headers = HeaderMap::new();
            if let Ok(loc) =
                HeaderValue::from_str(&crate::oci::UploadLocation { name: &name, id: uuid }.to_string())
            {
                headers.insert(header::LOCATION, loc);
            }
            if let Ok(range) =
                HeaderValue::from_str(&crate::oci::ByteRange { end: off.saturating_sub(1) }.to_string())
            {
                headers.insert(header::RANGE, range);
            }
            (StatusCode::NO_CONTENT, headers).into_response()
        }
        Err(e) => upload_err(e),
    }
}

// ─────────────────────────── end-14: cancel ───────────────────────────

/// `DELETE /v2/<name>/blobs/uploads/<uuid>` — cancel an upload (end-14).
async fn cancel_upload(state: &AppState, uuid: &str) -> Response {
    match state.uploads.cancel(uuid) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => upload_err(e),
    }
}

// ─────────────────────────── end-3/7/9: manifests ───────────────────────────

/// Resolve a `<reference>` to a concrete manifest digest within `name`.
async fn resolve_reference(
    state: &AppState,
    name: &str,
    reference: &str,
) -> Result<Option<Digest>, OciError> {
    match Reference::parse(reference)? {
        Reference::Digest(d) => Ok(Some(d)),
        Reference::Tag(tag) => {
            if !Reference::is_valid_tag(&tag) {
                return Err(OciError::ManifestUnknown(format!("invalid tag: {tag}")));
            }
            state
                .store
                .resolve_tag(name, &tag)
                .await
                .map_err(|e| OciError::ManifestUnknown(format!("tag resolution failed: {e}")))
        }
    }
}

/// `GET /v2/<name>/manifests/<reference>` — fetch a manifest (end-3).
async fn get_manifest(state: &AppState, name: &str, reference: &str) -> Response {
    let name = match checked_name(name) {
        Ok(n) => n.to_string(),
        Err(e) => return e.into_response(),
    };
    let digest = match resolve_reference(state, &name, reference).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            return OciError::ManifestUnknown(reference.to_string()).into_response();
        }
        Err(e) => return e.into_response(),
    };
    match state.store.get_manifest(&name, &digest).await {
        Ok(Some(m)) => {
            let mut headers = HeaderMap::new();
            headers.insert("docker-content-digest", content_digest_header(&digest));
            if let Ok(ct) = HeaderValue::from_str(&m.media_type) {
                headers.insert(header::CONTENT_TYPE, ct);
            }
            (StatusCode::OK, headers, m.bytes).into_response()
        }
        Ok(None) => OciError::ManifestUnknown(reference.to_string()).into_response(),
        Err(e) => store_err(e),
    }
}

/// `HEAD /v2/<name>/manifests/<reference>` — manifest existence (end-3).
async fn head_manifest(state: &AppState, name: &str, reference: &str) -> Response {
    let name = match checked_name(name) {
        Ok(n) => n.to_string(),
        Err(e) => return e.into_response(),
    };
    let digest = match resolve_reference(state, &name, reference).await {
        Ok(Some(d)) => d,
        Ok(None) => return OciError::ManifestUnknown(reference.to_string()).into_response(),
        Err(e) => return e.into_response(),
    };
    match state.store.get_manifest(&name, &digest).await {
        Ok(Some(m)) => {
            let mut headers = HeaderMap::new();
            headers.insert("docker-content-digest", content_digest_header(&digest));
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len_u64(m.bytes.len())));
            if let Ok(ct) = HeaderValue::from_str(&m.media_type) {
                headers.insert(header::CONTENT_TYPE, ct);
            }
            (StatusCode::OK, headers).into_response()
        }
        Ok(None) => OciError::ManifestUnknown(reference.to_string()).into_response(),
        Err(e) => store_err(e),
    }
}

/// `PUT /v2/<name>/manifests/<reference>` — store a manifest (end-7).
///
/// If the reference is a tag, the manifest is stored by its computed digest AND
/// the tag is pointed at it (end-7b multi-tag is just repeated PUTs). If the
/// manifest declares a `subject`, the referrers reverse-index is updated and an
/// `OCI-Subject` response header is returned (end-7a).
async fn put_manifest(
    state: &AppState,
    name: &str,
    reference: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    let name = match checked_name(name) {
        Ok(n) => n.to_string(),
        Err(e) => return e.into_response(),
    };
    if body.is_empty() {
        return OciError::ManifestInvalid("empty manifest body".to_string()).into_response();
    }
    // The manifest's own digest is the sha256 of its bytes (the OCI default
    // manifest digest algorithm).
    let digest = Digest::of(HashAlgorithm::Sha256, body);

    // If the reference is itself a digest, it MUST equal the computed digest.
    let reference_parsed = match Reference::parse(reference) {
        Ok(r) => r,
        Err(e) => return OciError::from(e).into_response(),
    };
    if let Reference::Digest(ref d) = reference_parsed {
        if d != &digest {
            return OciError::DigestInvalid(format!(
                "manifest body hashes to {digest}, not reference {d}"
            ))
            .into_response();
        }
    }

    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or(crate::oci::MEDIA_TYPE_OCI_MANIFEST)
        .to_string();

    let stored = StoredManifest {
        bytes: body.to_vec(),
        media_type: media_type.clone(),
    };
    if let Err(e) = state.store.put_manifest(&name, &digest, &stored).await {
        return store_err(e);
    }

    // Tag pointer (end-7b): if the reference is a tag, point it at this digest.
    if let Reference::Tag(ref tag) = reference_parsed {
        if !Reference::is_valid_tag(tag) {
            return OciError::ManifestInvalid(format!("invalid tag: {tag}")).into_response();
        }
        if let Err(e) = state.store.put_tag(&name, tag, &digest).await {
            return store_err(e);
        }
    }

    // Referrers index (end-7a): if the manifest declares a subject, record the
    // reverse edge and return the OCI-Subject header.
    let view = ManifestView::parse(body);
    let mut resp_headers = HeaderMap::new();
    if let Some(subject_desc) = view.subject.as_ref() {
        match Digest::parse(&subject_desc.digest) {
            Ok(subject) => {
                let referrer = Referrer {
                    digest: digest.clone(),
                    media_type: media_type.clone(),
                    artifact_type: view.effective_artifact_type(),
                    size: len_u64(body.len()),
                };
                if let Err(e) = state.store.add_referrer(&name, &subject, &referrer).await {
                    return store_err(e);
                }
                if let Ok(hv) = HeaderValue::from_str(&subject.to_wire()) {
                    resp_headers.insert("oci-subject", hv);
                }
            }
            Err(e) => {
                return OciError::ManifestInvalid(format!("bad subject digest: {e}"))
                    .into_response();
            }
        }
    }

    if let Ok(loc) =
        HeaderValue::from_str(&crate::oci::ManifestLocation { name: &name, digest: &digest }.to_string())
    {
        resp_headers.insert(header::LOCATION, loc);
    }
    resp_headers.insert("docker-content-digest", content_digest_header(&digest));
    (StatusCode::CREATED, resp_headers).into_response()
}

/// `DELETE /v2/<name>/manifests/<reference>` — delete a manifest (end-9).
async fn delete_manifest(state: &AppState, name: &str, reference: &str) -> Response {
    let name = match checked_name(name) {
        Ok(n) => n.to_string(),
        Err(e) => return e.into_response(),
    };
    let digest = match resolve_reference(state, &name, reference).await {
        Ok(Some(d)) => d,
        Ok(None) => return OciError::ManifestUnknown(reference.to_string()).into_response(),
        Err(e) => return e.into_response(),
    };
    match state.store.get_manifest(&name, &digest).await {
        Ok(Some(_)) => match state.store.delete_manifest(&name, &digest).await {
            Ok(()) => StatusCode::ACCEPTED.into_response(),
            Err(e) => store_err(e),
        },
        Ok(None) => OciError::ManifestUnknown(reference.to_string()).into_response(),
        Err(e) => store_err(e),
    }
}

// ─────────────────────────── end-8: tags ───────────────────────────

/// `GET /v2/<name>/tags/list` — list tags with pagination (end-8).
async fn list_tags(state: &AppState, name: &str, q: &HashMap<String, String>) -> Response {
    let name = match checked_name(name) {
        Ok(n) => n.to_string(),
        Err(e) => return e.into_response(),
    };
    // `?n=` may be malformed; a non-numeric `n` is a BAD_REQUEST-shaped input,
    // but the spec treats an absent/invalid `n` as "no limit" — we take the
    // lenient path (no limit) rather than erroring, matching common registries.
    let n = q.get("n").and_then(|v| v.parse::<usize>().ok());
    let last = q.get("last").map(String::as_str);
    match state.store.list_tags(&name, n, last).await {
        Ok(page) => {
            let mut headers = HeaderMap::new();
            // Link header for the next page (RFC 5988), if there is one.
            if let Some(next_last) = &page.next_last {
                let link = crate::oci::LinkNext {
                    name: &name,
                    n,
                    last: next_last.as_str(),
                }
                .to_string();
                if let Ok(hv) = HeaderValue::from_str(&link) {
                    headers.insert(header::LINK, hv);
                }
            }
            let body = TagsList {
                name: name.clone(),
                tags: page.tags,
            };
            (StatusCode::OK, headers, axum::Json(body)).into_response()
        }
        Err(e) => store_err(e),
    }
}

// ─────────────────────────── end-12: referrers ───────────────────────────

/// `GET /v2/<name>/referrers/<digest>` — the referrers image-index (end-12).
async fn get_referrers(
    state: &AppState,
    name: &str,
    digest: &str,
    q: &HashMap<String, String>,
) -> Response {
    let name = match checked_name(name) {
        Ok(n) => n.to_string(),
        Err(e) => return e.into_response(),
    };
    let subject = match Digest::parse(digest) {
        Ok(d) => d,
        Err(e) => return OciError::from(e).into_response(),
    };
    let artifact_type = q.get("artifactType").map(String::as_str);
    match state.store.list_referrers(&name, &subject, artifact_type).await {
        Ok(referrers) => {
            let manifests: Vec<Descriptor> = referrers
                .into_iter()
                .map(|r| Descriptor {
                    media_type: r.media_type,
                    digest: r.digest.to_wire(),
                    size: r.size,
                    artifact_type: r.artifact_type,
                })
                .collect();
            let index = ReferrersIndex::new(manifests);
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(crate::oci::MEDIA_TYPE_OCI_INDEX),
            );
            // When a filter was applied, echo it in OCI-Filters-Applied.
            if artifact_type.is_some() {
                headers.insert(
                    "oci-filters-applied",
                    HeaderValue::from_static("artifactType"),
                );
            }
            (StatusCode::OK, headers, axum::Json(index)).into_response()
        }
        Err(e) => store_err(e),
    }
}

/// Start the registry server, binding and serving until shutdown.
///
/// # Errors
///
/// Returns an [`std::io::Error`] if binding or serving fails.
pub async fn serve(state: AppState) -> std::io::Result<()> {
    let listen = state.config.listen.clone();
    let app = build_router(state);
    tracing::info!("porto (sui-registry) listening on {listen}");
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    axum::serve(listener, app).await
}
