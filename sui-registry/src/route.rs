//! Typed parsing of the OCI `/v2/` path tail into an [`OciRoute`].
//!
//! **Why not axum path params?** OCI repository names contain `/`
//! (`library/hello`, `my-org/team/app`), so `{name}` — which captures a single
//! path segment — cannot match them. The spec-faithful shape is a single
//! catch-all `/v2/{*path}` whose tail we parse here into a typed route. The
//! resource keywords (`blobs`, `manifests`, `tags`, `referrers`,
//! `blobs/uploads`) are fixed *suffixes*, so we split the name from the
//! resource unambiguously by scanning from the right.
//!
//! The parser is total and pure (no I/O), so it is exhaustively unit-tested; a
//! path that matches no route is [`OciRoute::Unknown`], which the dispatcher
//! renders as a typed `NAME_UNKNOWN` — never a bodiless 404.

/// A parsed OCI route: the repository name plus the resource being addressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OciRoute {
    /// `/v2/<name>/blobs/<digest>`
    Blob { name: String, digest: String },
    /// `/v2/<name>/blobs/uploads/` (trailing slash) — start an upload.
    UploadStart { name: String },
    /// `/v2/<name>/blobs/uploads/<uuid>` — chunk/finalize/status/cancel.
    Upload { name: String, uuid: String },
    /// `/v2/<name>/manifests/<reference>`
    Manifest { name: String, reference: String },
    /// `/v2/<name>/tags/list`
    Tags { name: String },
    /// `/v2/<name>/referrers/<digest>`
    Referrers { name: String, digest: String },
    /// A `/v2/...` path matching no known resource shape.
    Unknown,
}

impl OciRoute {
    /// Parse the tail of a `/v2/{*path}` route (i.e. `path` WITHOUT the leading
    /// `v2/`) into a typed route.
    ///
    /// The scan is right-anchored on the fixed resource keywords, so a
    /// slash-containing name is captured whole.
    #[must_use]
    pub fn parse(tail: &str) -> Self {
        // `blobs/uploads/` and `blobs/uploads/<uuid>` — check before the plain
        // `blobs/<digest>` shape, since `uploads` would otherwise look like a
        // digest.
        if let Some(name) = tail.strip_suffix("/blobs/uploads/") {
            if !name.is_empty() {
                return OciRoute::UploadStart { name: name.to_string() };
            }
        }
        // `.../blobs/uploads/<uuid>`
        if let Some((left, uuid)) = tail.rsplit_once('/') {
            if let Some(name) = left.strip_suffix("/blobs/uploads") {
                if !name.is_empty() && !uuid.is_empty() {
                    return OciRoute::Upload {
                        name: name.to_string(),
                        uuid: uuid.to_string(),
                    };
                }
            }
        }
        // `.../tags/list`
        if let Some(name) = tail.strip_suffix("/tags/list") {
            if !name.is_empty() {
                return OciRoute::Tags { name: name.to_string() };
            }
        }
        // `.../blobs/<digest>`
        if let Some((left, digest)) = tail.rsplit_once("/blobs/") {
            if !left.is_empty() && !digest.is_empty() && !digest.contains('/') {
                return OciRoute::Blob {
                    name: left.to_string(),
                    digest: digest.to_string(),
                };
            }
        }
        // `.../manifests/<reference>`
        if let Some((left, reference)) = tail.rsplit_once("/manifests/") {
            if !left.is_empty() && !reference.is_empty() && !reference.contains('/') {
                return OciRoute::Manifest {
                    name: left.to_string(),
                    reference: reference.to_string(),
                };
            }
        }
        // `.../referrers/<digest>`
        if let Some((left, digest)) = tail.rsplit_once("/referrers/") {
            if !left.is_empty() && !digest.is_empty() && !digest.contains('/') {
                return OciRoute::Referrers {
                    name: left.to_string(),
                    digest: digest.to_string(),
                };
            }
        }
        OciRoute::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_segment_name() {
        assert_eq!(
            OciRoute::parse("hello/blobs/sha256:abc"),
            OciRoute::Blob {
                name: "hello".into(),
                digest: "sha256:abc".into()
            }
        );
    }

    #[test]
    fn parses_multi_segment_name() {
        assert_eq!(
            OciRoute::parse("my-org/team/app/blobs/sha256:abc"),
            OciRoute::Blob {
                name: "my-org/team/app".into(),
                digest: "sha256:abc".into()
            }
        );
    }

    #[test]
    fn parses_upload_start_and_session() {
        assert_eq!(
            OciRoute::parse("lib/x/blobs/uploads/"),
            OciRoute::UploadStart { name: "lib/x".into() }
        );
        assert_eq!(
            OciRoute::parse("lib/x/blobs/uploads/abc-123"),
            OciRoute::Upload {
                name: "lib/x".into(),
                uuid: "abc-123".into()
            }
        );
    }

    #[test]
    fn parses_manifest_by_tag_and_digest() {
        assert_eq!(
            OciRoute::parse("lib/x/manifests/latest"),
            OciRoute::Manifest {
                name: "lib/x".into(),
                reference: "latest".into()
            }
        );
        assert_eq!(
            OciRoute::parse("lib/x/manifests/sha256:dead"),
            OciRoute::Manifest {
                name: "lib/x".into(),
                reference: "sha256:dead".into()
            }
        );
    }

    #[test]
    fn parses_tags_and_referrers() {
        assert_eq!(
            OciRoute::parse("lib/x/tags/list"),
            OciRoute::Tags { name: "lib/x".into() }
        );
        assert_eq!(
            OciRoute::parse("lib/x/referrers/sha256:beef"),
            OciRoute::Referrers {
                name: "lib/x".into(),
                digest: "sha256:beef".into()
            }
        );
    }

    #[test]
    fn upload_start_beats_blob_shape() {
        // `blobs/uploads/` must not be read as a blob named `uploads`.
        match OciRoute::parse("lib/x/blobs/uploads/") {
            OciRoute::UploadStart { .. } => {}
            other => panic!("expected UploadStart, got {other:?}"),
        }
    }

    #[test]
    fn unknown_path_is_unknown() {
        assert_eq!(OciRoute::parse("lib/x/bogus/thing"), OciRoute::Unknown);
        assert_eq!(OciRoute::parse(""), OciRoute::Unknown);
    }
}
