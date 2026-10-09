//! [`LayoutStore`] — a READ-ONLY [`RegistryStore`] over OCI image layouts.
//!
//! An OCI image layout (`oci-layout` + `index.json` + `blobs/<alg>/<hex>`) is
//! what a Nix derivation emits for a chart or image repo; dropped in
//! `/nix/store` it is immutable and content-addressed twice over (by its store
//! path and by every blob's digest). This store serves such layouts as
//! registry repositories:
//!
//! - **Tags are derived, never written.** Every `index.json` descriptor's
//!   `org.opencontainers.image.ref.name` becomes a tag (see
//!   [`crate::config::LayoutMount`] for the `<tag>` / `<path>:<tag>` grammar).
//!   Since the layout is immutable, the tags survive a restart by construction
//!   — no pointer table, nothing to lose.
//! - **Conflicts are a startup error.** Two mounts (or two entries of one
//!   `index.json`) binding the same `repository:tag` to different digests is a
//!   [`LayoutError::ConflictingTag`]; there is no last-wins order to depend on.
//! - **Corruption is refused, not served.** Every manifest is hashed at load.
//!   Blobs are hashed at load ([`BlobVerification::Eager`]) or on first read
//!   with the verdict cached ([`BlobVerification::Lazy`]); either way a blob
//!   whose bytes do not hash to its digest is never returned. Declared sizes
//!   are checked against the file at load in both modes.
//! - **Every write is refused.** [`RegistryStore::access`] answers
//!   [`RepoAccess::ReadOnly`] for every repository, and every mutating method
//!   returns [`StoreError::ReadOnly`].
//!
//! Only blobs a loaded manifest references are served; a stray file under
//! `blobs/` is not reachable.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use async_trait::async_trait;

use crate::config::{BlobVerification, LayoutMount};
use crate::digest::{is_valid_name, Digest, DigestError, Reference};
use crate::oci::{
    is_index_media_type, AnnotatedDescriptor, ImageLayoutMarker, IndexView, ManifestBlobs,
    ManifestView, ANNOTATION_REF_NAME, IMAGE_LAYOUT_VERSION,
};

use super::{paginate, Referrer, RegistryStore, RepoAccess, StoreError, StoredManifest, TagPage};

/// How deep an index may nest (index → index → manifest). Real layouts use
/// one or two levels; the bound turns a cyclic or adversarial layout into a
/// typed error instead of a stack overflow.
const MAX_INDEX_DEPTH: usize = 8;

/// A layout that cannot be served. Every variant names the file or layout at
/// fault, so a failed boot says exactly which store path to look at.
#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    /// A file the layout must contain could not be read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The I/O failure.
        source: std::io::Error,
    },
    /// A JSON document (`oci-layout`, `index.json`, a manifest) is malformed.
    #[error("malformed JSON in {path}: {source}")]
    Json {
        /// The file.
        path: PathBuf,
        /// The parse failure.
        source: serde_json::Error,
    },
    /// The `oci-layout` marker names a version this server does not speak.
    #[error("{layout} is not an OCI image layout v{IMAGE_LAYOUT_VERSION} (imageLayoutVersion {found:?})")]
    UnsupportedLayoutVersion {
        /// The layout root.
        layout: PathBuf,
        /// The version it declares.
        found: String,
    },
    /// A mount's `repository` (or a repository derived from a ref name) is not
    /// a valid OCI repository name.
    #[error("{layout}: invalid repository name {repository:?}")]
    InvalidRepository {
        /// The layout root.
        layout: PathBuf,
        /// The offending name.
        repository: String,
    },
    /// A `ref.name` annotation does not split into a valid repository + tag.
    #[error("{layout}: ref.name {ref_name:?} is not '<tag>' or '<path>:<tag>' with a valid tag")]
    InvalidRefName {
        /// The layout root.
        layout: PathBuf,
        /// The annotation value.
        ref_name: String,
    },
    /// A descriptor's digest string is malformed.
    #[error("{layout}: bad digest {raw:?}: {source}")]
    BadDigest {
        /// The layout root.
        layout: PathBuf,
        /// The digest as written.
        raw: String,
        /// The parse failure.
        source: DigestError,
    },
    /// A file's size differs from its descriptor's `size`.
    #[error("{path}: descriptor says {declared} bytes, file has {actual}")]
    SizeMismatch {
        /// The blob file.
        path: PathBuf,
        /// The descriptor's size.
        declared: u64,
        /// The file's size.
        actual: u64,
    },
    /// A file's bytes do not hash to the digest it is stored under — the
    /// store path is corrupt and is refused.
    #[error("{path}: content hashes to {actual}, not {expected} — refusing a corrupt layout")]
    DigestMismatch {
        /// The blob file.
        path: PathBuf,
        /// The digest it is addressed by.
        expected: String,
        /// The digest of its bytes.
        actual: String,
    },
    /// A manifest's own `mediaType` disagrees with the descriptor pointing at
    /// it (serving either would lie to some client).
    #[error("{path}: descriptor mediaType {descriptor:?} but the manifest says {manifest:?}")]
    MediaTypeMismatch {
        /// The manifest file.
        path: PathBuf,
        /// The descriptor's media type.
        descriptor: String,
        /// The manifest's own `mediaType` field.
        manifest: String,
    },
    /// Index nesting exceeded [`MAX_INDEX_DEPTH`].
    #[error("{layout}: image indexes nest deeper than {MAX_INDEX_DEPTH}")]
    TooDeep {
        /// The layout root.
        layout: PathBuf,
    },
    /// Two sources bind one `repository:tag` to different manifests (boxed:
    /// the largest variant, kept off every `Result`'s hot size).
    #[error("{0}")]
    ConflictingTag(Box<TagConflict>),
}

/// The detail of a [`LayoutError::ConflictingTag`].
#[derive(Debug, thiserror::Error)]
#[error(
    "tag {repository}:{tag} is {first} in {first_layout} but {second} in {second_layout} — refusing to pick one"
)]
pub struct TagConflict {
    /// The repository.
    pub repository: String,
    /// The tag.
    pub tag: String,
    /// The digest bound first.
    pub first: String,
    /// The layout that bound it.
    pub first_layout: PathBuf,
    /// The conflicting digest.
    pub second: String,
    /// The layout that tried to bind it.
    pub second_layout: PathBuf,
}

/// Where a tag came from (for the conflict error).
#[derive(Debug, Clone)]
struct TagBinding {
    digest: Digest,
    layout: PathBuf,
}

/// One served repository.
#[derive(Debug, Default)]
struct LayoutRepo {
    tags: BTreeMap<String, TagBinding>,
    manifests: HashMap<Digest, StoredManifest>,
    referrers: HashMap<Digest, Vec<Referrer>>,
}

/// One servable blob: where its bytes live and whether they were proven.
#[derive(Debug)]
struct LayoutBlob {
    path: PathBuf,
    size: u64,
    /// The cached verification verdict. Set at load for eager blobs; set on
    /// first read for lazy ones. `Err` carries the mismatch for the log.
    verdict: OnceLock<Result<(), String>>,
}

/// The read-only layout-backed registry store. Build with
/// [`LayoutStore::load`].
#[derive(Debug, Default)]
pub struct LayoutStore {
    repos: BTreeMap<String, LayoutRepo>,
    blobs: HashMap<Digest, LayoutBlob>,
}

impl LayoutStore {
    /// Load every mount, verifying as described in the module docs.
    ///
    /// # Errors
    ///
    /// The first [`LayoutError`] found; a store is never half-loaded.
    pub fn load(mounts: &[LayoutMount]) -> Result<Self, LayoutError> {
        let mut store = Self::default();
        for mount in mounts {
            store.mount(mount)?;
        }
        Ok(store)
    }

    /// Whether `name` is a repository some layout serves.
    #[must_use]
    pub fn serves(&self, name: &str) -> bool {
        self.repos.contains_key(name)
    }

    /// Whether `digest` is a blob some layout serves.
    #[must_use]
    pub fn holds_blob(&self, digest: &Digest) -> bool {
        self.blobs.contains_key(digest)
    }

    fn mount(&mut self, mount: &LayoutMount) -> Result<(), LayoutError> {
        let layout = mount.layout.as_path();
        if !is_valid_name(&mount.repository) {
            return Err(LayoutError::InvalidRepository {
                layout: layout.to_path_buf(),
                repository: mount.repository.clone(),
            });
        }
        let marker: ImageLayoutMarker = read_json(&layout.join("oci-layout"))?;
        if marker.image_layout_version != IMAGE_LAYOUT_VERSION {
            return Err(LayoutError::UnsupportedLayoutVersion {
                layout: layout.to_path_buf(),
                found: marker.image_layout_version,
            });
        }
        let index: IndexView = read_json(&layout.join("index.json"))?;
        for desc in &index.manifests {
            let (repo, tag) = placement(
                &mount.repository,
                desc.annotations.get(ANNOTATION_REF_NAME).map(String::as_str),
                layout,
            )?;
            let digest = self.load_manifest(layout, mount.verify, &repo, desc, 0)?;
            if let Some(tag) = tag {
                self.bind_tag(&repo, tag, digest, layout)?;
            }
        }
        Ok(())
    }

    /// Load (and verify) one manifest or index and everything under it into
    /// `repo`. Returns its digest.
    fn load_manifest(
        &mut self,
        layout: &Path,
        verify: BlobVerification,
        repo: &str,
        desc: &AnnotatedDescriptor,
        depth: usize,
    ) -> Result<Digest, LayoutError> {
        if depth > MAX_INDEX_DEPTH {
            return Err(LayoutError::TooDeep { layout: layout.to_path_buf() });
        }
        let digest = parse_digest(layout, &desc.digest)?;
        if self
            .repos
            .get(repo)
            .is_some_and(|r| r.manifests.contains_key(&digest))
        {
            return Ok(digest);
        }
        let path = blob_path(layout, &digest);
        let bytes = std::fs::read(&path).map_err(|source| LayoutError::Io {
            path: path.clone(),
            source,
        })?;
        check_size(&path, desc.size, bytes.len() as u64)?;
        // Manifests are small and every one is needed to serve — always eager.
        let actual = Digest::of(digest.algorithm(), &bytes);
        if actual != digest {
            return Err(LayoutError::DigestMismatch {
                path,
                expected: digest.to_wire(),
                actual: actual.to_wire(),
            });
        }

        let blobs: ManifestBlobs = parse_json(&path, &bytes)?;
        if let Some(own) = &blobs.media_type
            && own != &desc.media_type
        {
            return Err(LayoutError::MediaTypeMismatch {
                path,
                descriptor: desc.media_type.clone(),
                manifest: own.clone(),
            });
        }
        if is_index_media_type(&desc.media_type) {
            let index: IndexView = parse_json(&path, &bytes)?;
            for child in &index.manifests {
                self.load_manifest(layout, verify, repo, child, depth + 1)?;
            }
        } else {
            for blob in blobs.config.iter().chain(blobs.layers.iter()) {
                self.register_blob(layout, verify, blob)?;
            }
        }

        // The manifest is itself a blob of the layout (already verified).
        self.blobs.entry(digest.clone()).or_insert_with(|| LayoutBlob {
            path: path.clone(),
            size: desc.size,
            verdict: OnceLock::from(Ok(())),
        });

        let size = desc.size;
        let view = ManifestView::parse(&bytes);
        let entry = self.repos.entry(repo.to_string()).or_default();
        entry.manifests.insert(
            digest.clone(),
            StoredManifest {
                bytes,
                media_type: desc.media_type.clone(),
            },
        );
        if let Some(subject) = &view.subject {
            let subject = parse_digest(layout, &subject.digest)?;
            let referrers = entry.referrers.entry(subject).or_default();
            if !referrers.iter().any(|r| r.digest == digest) {
                referrers.push(Referrer {
                    digest: digest.clone(),
                    media_type: desc.media_type.clone(),
                    artifact_type: view.effective_artifact_type(),
                    size,
                });
            }
        }
        Ok(digest)
    }

    /// Register a config/layer blob: it must exist at its declared size; an
    /// eager mount hashes it now.
    fn register_blob(
        &mut self,
        layout: &Path,
        verify: BlobVerification,
        desc: &AnnotatedDescriptor,
    ) -> Result<(), LayoutError> {
        let digest = parse_digest(layout, &desc.digest)?;
        let path = blob_path(layout, &digest);
        let meta = std::fs::metadata(&path).map_err(|source| LayoutError::Io {
            path: path.clone(),
            source,
        })?;
        check_size(&path, desc.size, meta.len())?;
        let verdict = OnceLock::new();
        if verify == BlobVerification::Eager {
            let file = std::fs::File::open(&path).map_err(|source| LayoutError::Io {
                path: path.clone(),
                source,
            })?;
            let actual = Digest::of_reader(digest.algorithm(), std::io::BufReader::new(file))
                .map_err(|source| LayoutError::Io {
                    path: path.clone(),
                    source,
                })?;
            if actual != digest {
                return Err(LayoutError::DigestMismatch {
                    path,
                    expected: digest.to_wire(),
                    actual: actual.to_wire(),
                });
            }
            let _ = verdict.set(Ok(()));
        }
        // First registration of a digest stands — content-addressed, so a
        // second copy is the same bytes (and was just proven so if eager).
        self.blobs.entry(digest).or_insert(LayoutBlob {
            path,
            size: desc.size,
            verdict,
        });
        Ok(())
    }

    fn bind_tag(
        &mut self,
        repo: &str,
        tag: String,
        digest: Digest,
        layout: &Path,
    ) -> Result<(), LayoutError> {
        let entry = self.repos.entry(repo.to_string()).or_default();
        match entry.tags.get(&tag) {
            Some(existing) if existing.digest != digest => {
                Err(LayoutError::ConflictingTag(Box::new(TagConflict {
                    repository: repo.to_string(),
                    tag,
                    first: existing.digest.to_wire(),
                    first_layout: existing.layout.clone(),
                    second: digest.to_wire(),
                    second_layout: layout.to_path_buf(),
                })))
            }
            Some(_) => Ok(()),
            None => {
                entry.tags.insert(
                    tag,
                    TagBinding {
                        digest,
                        layout: layout.to_path_buf(),
                    },
                );
                Ok(())
            }
        }
    }
}

/// Split a `ref.name` into the served repository and tag. See
/// [`crate::config::LayoutMount`].
fn placement(
    base: &str,
    ref_name: Option<&str>,
    layout: &Path,
) -> Result<(String, Option<String>), LayoutError> {
    let Some(raw) = ref_name else {
        return Ok((base.to_string(), None));
    };
    let (repo, tag) = match raw.rsplit_once(':') {
        Some((path, tag)) => (format!("{base}/{path}"), tag),
        None => (base.to_string(), raw),
    };
    if !is_valid_name(&repo) || !Reference::is_valid_tag(tag) {
        return Err(LayoutError::InvalidRefName {
            layout: layout.to_path_buf(),
            ref_name: raw.to_string(),
        });
    }
    Ok((repo, Some(tag.to_string())))
}

fn blob_path(layout: &Path, digest: &Digest) -> PathBuf {
    layout
        .join("blobs")
        .join(digest.algorithm().to_string())
        .join(digest.hex())
}

fn parse_digest(layout: &Path, raw: &str) -> Result<Digest, LayoutError> {
    Digest::parse(raw).map_err(|source| LayoutError::BadDigest {
        layout: layout.to_path_buf(),
        raw: raw.to_string(),
        source,
    })
}

fn check_size(path: &Path, declared: u64, actual: u64) -> Result<(), LayoutError> {
    if declared == actual {
        Ok(())
    } else {
        Err(LayoutError::SizeMismatch {
            path: path.to_path_buf(),
            declared,
            actual,
        })
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, LayoutError> {
    let bytes = std::fs::read(path).map_err(|source| LayoutError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_json(path, &bytes)
}

fn parse_json<T: serde::de::DeserializeOwned>(path: &Path, bytes: &[u8]) -> Result<T, LayoutError> {
    serde_json::from_slice(bytes).map_err(|source| LayoutError::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn read_only(what: &str) -> StoreError {
    StoreError::ReadOnly(format!("{what}: repository is served from a read-only OCI image layout"))
}

#[async_trait]
impl RegistryStore for LayoutStore {
    fn access(&self, _name: &str) -> RepoAccess {
        RepoAccess::ReadOnly
    }

    async fn list_repositories(&self) -> Result<Vec<String>, StoreError> {
        Ok(self.repos.keys().cloned().collect())
    }

    async fn get_blob(&self, digest: &Digest) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(blob) = self.blobs.get(digest) else {
            return Ok(None);
        };
        let bytes = tokio::fs::read(&blob.path)
            .await
            .map_err(|e| StoreError::Backend(format!("{}: {e}", blob.path.display())))?;
        let verdict = blob.verdict.get_or_init(|| {
            let actual = Digest::of(digest.algorithm(), &bytes);
            if actual == *digest && bytes.len() as u64 == blob.size {
                Ok(())
            } else {
                Err(format!(
                    "{}: {} bytes hashing to {actual}, served as {digest}",
                    blob.path.display(),
                    bytes.len()
                ))
            }
        });
        match verdict {
            Ok(()) => Ok(Some(bytes)),
            Err(why) => Err(StoreError::Corrupt(why.clone())),
        }
    }

    async fn has_blob(&self, digest: &Digest) -> Result<bool, StoreError> {
        Ok(self
            .blobs
            .get(digest)
            .is_some_and(|b| !matches!(b.verdict.get(), Some(Err(_)))))
    }

    async fn put_blob(&self, digest: &Digest, _bytes: &[u8]) -> Result<(), StoreError> {
        Err(read_only(&digest.to_wire()))
    }

    async fn delete_blob(&self, digest: &Digest) -> Result<(), StoreError> {
        Err(read_only(&digest.to_wire()))
    }

    async fn get_manifest(
        &self,
        name: &str,
        digest: &Digest,
    ) -> Result<Option<StoredManifest>, StoreError> {
        Ok(self
            .repos
            .get(name)
            .and_then(|r| r.manifests.get(digest).cloned()))
    }

    async fn put_manifest(
        &self,
        name: &str,
        _digest: &Digest,
        _manifest: &StoredManifest,
    ) -> Result<(), StoreError> {
        Err(read_only(name))
    }

    async fn delete_manifest(&self, name: &str, _digest: &Digest) -> Result<(), StoreError> {
        Err(read_only(name))
    }

    async fn put_tag(&self, name: &str, _tag: &str, _digest: &Digest) -> Result<(), StoreError> {
        Err(read_only(name))
    }

    async fn resolve_tag(&self, name: &str, tag: &str) -> Result<Option<Digest>, StoreError> {
        Ok(self
            .repos
            .get(name)
            .and_then(|r| r.tags.get(tag))
            .map(|b| b.digest.clone()))
    }

    async fn list_tags(
        &self,
        name: &str,
        n: Option<usize>,
        last: Option<&str>,
    ) -> Result<TagPage, StoreError> {
        Ok(match self.repos.get(name) {
            Some(repo) => paginate(repo.tags.keys().cloned(), n, last),
            None => TagPage { tags: Vec::new(), next_last: None },
        })
    }

    async fn add_referrer(
        &self,
        name: &str,
        _subject: &Digest,
        _referrer: &Referrer,
    ) -> Result<(), StoreError> {
        Err(read_only(name))
    }

    async fn list_referrers(
        &self,
        name: &str,
        subject: &Digest,
        artifact_type: Option<&str>,
    ) -> Result<Vec<Referrer>, StoreError> {
        let list = self
            .repos
            .get(name)
            .and_then(|r| r.referrers.get(subject))
            .cloned()
            .unwrap_or_default();
        Ok(match artifact_type {
            Some(want) => list
                .into_iter()
                .filter(|r| r.artifact_type.as_deref() == Some(want))
                .collect(),
            None => list,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_splits_path_and_tag() {
        let l = Path::new("/l");
        assert_eq!(
            placement("pleme-io/charts", Some("pleme-porto:0.1.0"), l).unwrap(),
            ("pleme-io/charts/pleme-porto".to_string(), Some("0.1.0".to_string()))
        );
        assert_eq!(
            placement("pleme-io/charts/x", Some("1.2.3"), l).unwrap(),
            ("pleme-io/charts/x".to_string(), Some("1.2.3".to_string()))
        );
        assert_eq!(placement("r", None, l).unwrap(), ("r".to_string(), None));
    }

    #[test]
    fn placement_rejects_what_cannot_be_served() {
        let l = Path::new("/l");
        // A registry host (with a port) is not a repository path.
        assert!(placement("r", Some("host:5000/x:1"), l).is_err());
        // `+` is not legal in an OCI tag (helm maps semver build metadata to `_`).
        assert!(placement("r", Some("x:1.0.0+build"), l).is_err());
        assert!(placement("r", Some("Upper:1"), l).is_err());
    }
}
