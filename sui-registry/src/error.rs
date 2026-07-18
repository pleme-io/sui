//! The typed OCI error surface: [`OciError`] → wire code + HTTP status + body.
//!
//! The OCI Distribution Spec defines a fixed set of error codes and a JSON
//! error body of the shape `{"errors":[{"code":..,"message":..,"detail":..}]}`.
//! Every failure path in porto returns a typed [`OciError`]; the wire JSON is
//! produced by a `serde::Serialize` struct ([`ErrorBody`]), never by a
//! `format!()` of JSON (TYPED EMISSION). The HTTP status is a total function of
//! the variant, so a handler *cannot* return the wrong status for a code.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::digest::DigestError;

/// A typed OCI Distribution error.
///
/// The variant set is the spec's error-code set; each variant carries the
/// operator-facing detail string. `Display`/`code()`/`status()` are total —
/// there is no "unknown" fall-through that could leak a bare 500.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OciError {
    /// `MANIFEST_UNKNOWN` — manifest not found (name/reference).
    #[error("manifest unknown: {0}")]
    ManifestUnknown(String),
    /// `BLOB_UNKNOWN` — referenced blob is unknown to the registry.
    #[error("blob unknown: {0}")]
    BlobUnknown(String),
    /// `DIGEST_INVALID` — provided digest did not match the uploaded content,
    /// or was syntactically malformed.
    #[error("digest invalid: {0}")]
    DigestInvalid(String),
    /// `MANIFEST_INVALID` — manifest bytes/media-type could not be accepted.
    #[error("manifest invalid: {0}")]
    ManifestInvalid(String),
    /// `BLOB_UPLOAD_UNKNOWN` — upload session id is unknown.
    #[error("blob upload unknown: {0}")]
    BlobUploadUnknown(String),
    /// `BLOB_UPLOAD_INVALID` — an upload chunk was malformed / out of order.
    #[error("blob upload invalid: {0}")]
    BlobUploadInvalid(String),
    /// `NAME_UNKNOWN` — repository name is not known / not valid.
    #[error("name unknown: {0}")]
    NameUnknown(String),
    /// `SIZE_INVALID` — the provided length did not match the content.
    #[error("size invalid: {0}")]
    SizeInvalid(String),
    /// `UNSUPPORTED` — the operation is unsupported (a deferred endpoint that
    /// intentionally never fakes a 200).
    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl OciError {
    /// The OCI wire code string (the `code` field of an error object).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            OciError::ManifestUnknown(_) => "MANIFEST_UNKNOWN",
            OciError::BlobUnknown(_) => "BLOB_UNKNOWN",
            OciError::DigestInvalid(_) => "DIGEST_INVALID",
            OciError::ManifestInvalid(_) => "MANIFEST_INVALID",
            OciError::BlobUploadUnknown(_) => "BLOB_UPLOAD_UNKNOWN",
            OciError::BlobUploadInvalid(_) => "BLOB_UPLOAD_INVALID",
            OciError::NameUnknown(_) => "NAME_UNKNOWN",
            OciError::SizeInvalid(_) => "SIZE_INVALID",
            OciError::Unsupported(_) => "UNSUPPORTED",
        }
    }

    /// The HTTP status the spec assigns this code.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            OciError::ManifestUnknown(_)
            | OciError::BlobUnknown(_)
            | OciError::BlobUploadUnknown(_)
            | OciError::NameUnknown(_) => StatusCode::NOT_FOUND,
            OciError::DigestInvalid(_)
            | OciError::ManifestInvalid(_)
            | OciError::BlobUploadInvalid(_)
            | OciError::SizeInvalid(_) => StatusCode::BAD_REQUEST,
            // The spec returns 400 for an unsupported request against a v2
            // registry (not 501) — a client must be able to distinguish a
            // real registry that refuses the op from a non-registry host.
            OciError::Unsupported(_) => StatusCode::BAD_REQUEST,
        }
    }

    /// The operator-facing detail message (the `message` field).
    #[must_use]
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// A malformed content-address anywhere in the request path/query is a
/// `DIGEST_INVALID` on the wire — the parse-at-boundary rejection surfaces as
/// exactly one typed OCI code.
impl From<DigestError> for OciError {
    fn from(e: DigestError) -> Self {
        OciError::DigestInvalid(e.to_string())
    }
}

/// One error object inside the OCI error body.
#[derive(Debug, Serialize)]
struct ErrorObject {
    code: &'static str,
    message: String,
}

/// The OCI error JSON body: `{"errors":[{code,message}]}`.
///
/// A `Serialize` struct — the wire JSON is produced by `serde_json`, never by
/// a hand-built string.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    errors: Vec<ErrorObject>,
}

impl ErrorBody {
    fn single(err: &OciError) -> Self {
        Self {
            errors: vec![ErrorObject {
                code: err.code(),
                message: err.message(),
            }],
        }
    }
}

impl IntoResponse for OciError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = ErrorBody::single(&self);
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable() {
        assert_eq!(OciError::ManifestUnknown("x".into()).code(), "MANIFEST_UNKNOWN");
        assert_eq!(OciError::BlobUnknown("x".into()).code(), "BLOB_UNKNOWN");
        assert_eq!(OciError::DigestInvalid("x".into()).code(), "DIGEST_INVALID");
        assert_eq!(OciError::Unsupported("x".into()).code(), "UNSUPPORTED");
    }

    #[test]
    fn statuses_match_spec() {
        assert_eq!(OciError::ManifestUnknown("x".into()).status(), StatusCode::NOT_FOUND);
        assert_eq!(OciError::BlobUnknown("x".into()).status(), StatusCode::NOT_FOUND);
        assert_eq!(OciError::DigestInvalid("x".into()).status(), StatusCode::BAD_REQUEST);
        assert_eq!(OciError::SizeInvalid("x".into()).status(), StatusCode::BAD_REQUEST);
        assert_eq!(OciError::Unsupported("x".into()).status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn body_serializes_to_oci_shape() {
        let body = ErrorBody::single(&OciError::ManifestUnknown("no such tag".into()));
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["errors"][0]["code"], "MANIFEST_UNKNOWN");
        assert!(json["errors"][0]["message"].as_str().unwrap().contains("no such tag"));
    }

    #[test]
    fn digest_error_maps_to_digest_invalid() {
        let oci: OciError = DigestError::MalformedHex.into();
        assert_eq!(oci.code(), "DIGEST_INVALID");
        assert_eq!(oci.status(), StatusCode::BAD_REQUEST);
    }
}
