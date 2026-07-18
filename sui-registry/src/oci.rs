//! Typed OCI wire structs (the subset porto reads/writes).
//!
//! These are `serde` structs, never hand-built JSON (TYPED EMISSION). porto
//! parses just enough of a manifest to extract its `subject` (for the
//! referrers index) and `artifactType`/config-media-type (for the referrers
//! filter), and *emits* the referrers response as a typed image-index.
//!
//! Media-type constants.
pub const MEDIA_TYPE_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const MEDIA_TYPE_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

use serde::{Deserialize, Serialize};

// ── Typed wire-value builders (TYPED EMISSION) ────────────────────────────
// `Location` / `Range` / `Link` header VALUES are wire syntax; each is rendered
// by a `Display` impl (write! is the sanctioned typed surface), never
// `format!()`d at the call site.
use std::fmt;

use crate::digest::Digest;

/// `Location` header for a stored blob: `/v2/{name}/blobs/{digest}`.
pub struct BlobLocation<'a> {
    pub name: &'a str,
    pub digest: &'a Digest,
}
impl fmt::Display for BlobLocation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/v2/{}/blobs/{}", self.name, self.digest.to_wire())
    }
}

/// `Location` header for a stored manifest: `/v2/{name}/manifests/{digest}`.
pub struct ManifestLocation<'a> {
    pub name: &'a str,
    pub digest: &'a Digest,
}
impl fmt::Display for ManifestLocation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/v2/{}/manifests/{}", self.name, self.digest.to_wire())
    }
}

/// `Location` header for an in-progress upload session:
/// `/v2/{name}/blobs/uploads/{id}`.
pub struct UploadLocation<'a> {
    pub name: &'a str,
    pub id: &'a str,
}
impl fmt::Display for UploadLocation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/v2/{}/blobs/uploads/{}", self.name, self.id)
    }
}

/// `Range` header for an upload offset — the inclusive `0-{end}` byte range
/// the OCI chunked-upload protocol echoes.
pub struct ByteRange {
    pub end: u64,
}
impl fmt::Display for ByteRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0-{}", self.end)
    }
}

/// RFC-5988 `Link` header for tags-list pagination:
/// `</v2/{name}/tags/list?[n={n}&]last={last}>; rel="next"`.
pub struct LinkNext<'a> {
    pub name: &'a str,
    pub n: Option<usize>,
    pub last: &'a str,
}
impl fmt::Display for LinkNext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "</v2/{}/tags/list?", self.name)?;
        if let Some(n) = self.n {
            write!(f, "n={n}&")?;
        }
        write!(f, "last={}>; rel=\"next\"", self.last)
    }
}

/// A content descriptor (the OCI `descriptor` shape). Only the fields porto
/// needs are modeled; unknown fields are ignored on read so we accept any
/// spec-conformant descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(rename = "artifactType", skip_serializing_if = "Option::is_none", default)]
    pub artifact_type: Option<String>,
}

/// The subset of a manifest porto inspects to build the referrers index.
///
/// A manifest that declares `subject: <descriptor>` is a *referrer* of that
/// subject (the seam where a lacre signature / SBOM / attestation attaches to
/// the image it describes). `config.mediaType` and top-level `artifactType`
/// feed the `?artifactType=` filter.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestView {
    #[serde(rename = "artifactType", default)]
    pub artifact_type: Option<String>,
    #[serde(default)]
    pub config: Option<ConfigDescriptor>,
    #[serde(default)]
    pub subject: Option<Descriptor>,
}

/// The `config` descriptor of an image manifest (its `mediaType` is the
/// artifact-type fallback per the OCI referrers rules).
#[derive(Debug, Clone, Deserialize)]
pub struct ConfigDescriptor {
    #[serde(rename = "mediaType", default)]
    pub media_type: Option<String>,
}

impl ManifestView {
    /// Parse the referrer-relevant view of a manifest, tolerantly: a manifest
    /// that is not JSON, or is JSON without these fields, yields a view with
    /// all-`None` (it simply declares no subject). This never errors — a
    /// non-referrer manifest is a legitimate, common case.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Self {
        serde_json::from_slice(bytes).unwrap_or(ManifestView {
            artifact_type: None,
            config: None,
            subject: None,
        })
    }

    /// The effective artifact type used by the referrers filter: the top-level
    /// `artifactType` if present, else the config media-type (the OCI rule).
    #[must_use]
    pub fn effective_artifact_type(&self) -> Option<String> {
        self.artifact_type
            .clone()
            .or_else(|| self.config.as_ref().and_then(|c| c.media_type.clone()))
    }
}

/// The referrers-API response: an OCI image-index whose `manifests` are the
/// descriptors of every manifest referring to the subject.
#[derive(Debug, Clone, Serialize)]
pub struct ReferrersIndex {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub manifests: Vec<Descriptor>,
}

impl ReferrersIndex {
    /// Build an image-index from a set of referrer descriptors.
    #[must_use]
    pub fn new(manifests: Vec<Descriptor>) -> Self {
        Self {
            schema_version: 2,
            media_type: MEDIA_TYPE_OCI_INDEX.to_string(),
            manifests,
        }
    }
}

/// The tags-list response body (`GET /v2/<name>/tags/list`).
#[derive(Debug, Clone, Serialize)]
pub struct TagsList {
    pub name: String,
    pub tags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subject_from_referrer_manifest() {
        let manifest = br#"{
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": "application/vnd.dev.cosign.artifact.sig.v1+json",
            "config": { "mediaType": "application/vnd.oci.empty.v1+json" },
            "subject": {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                "size": 42
            }
        }"#;
        let view = ManifestView::parse(manifest);
        assert!(view.subject.is_some());
        assert_eq!(
            view.effective_artifact_type().as_deref(),
            Some("application/vnd.dev.cosign.artifact.sig.v1+json")
        );
    }

    #[test]
    fn non_referrer_manifest_has_no_subject() {
        let manifest = br#"{ "schemaVersion": 2, "config": {} }"#;
        let view = ManifestView::parse(manifest);
        assert!(view.subject.is_none());
    }

    #[test]
    fn config_media_type_is_artifact_type_fallback() {
        let manifest = br#"{ "config": { "mediaType": "application/vnd.example.config.v1+json" } }"#;
        let view = ManifestView::parse(manifest);
        assert_eq!(
            view.effective_artifact_type().as_deref(),
            Some("application/vnd.example.config.v1+json")
        );
    }

    #[test]
    fn garbage_manifest_parses_as_no_subject() {
        let view = ManifestView::parse(b"not json at all");
        assert!(view.subject.is_none());
        assert!(view.effective_artifact_type().is_none());
    }

    #[test]
    fn referrers_index_serializes_to_image_index() {
        let idx = ReferrersIndex::new(vec![Descriptor {
            media_type: MEDIA_TYPE_OCI_MANIFEST.to_string(),
            digest: "sha256:2222222222222222222222222222222222222222222222222222222222222222".to_string(),
            size: 10,
            artifact_type: Some("application/vnd.example".to_string()),
        }]);
        let json = serde_json::to_value(&idx).unwrap();
        assert_eq!(json["schemaVersion"], 2);
        assert_eq!(json["mediaType"], MEDIA_TYPE_OCI_INDEX);
        assert_eq!(json["manifests"][0]["size"], 10);
    }
}
