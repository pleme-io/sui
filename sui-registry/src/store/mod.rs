//! The [`RegistryStore`] seam: the testability contract for porto.
//!
//! Every side effect the OCI handlers perform — blob get/put, manifest
//! get/put, tag resolution, referrer indexing — flows through this async
//! trait. Tests drive [`MemStore`] (pure in-memory) so the whole registry
//! proves green with zero real I/O; production drives [`SuiCacheStore`]
//! (blobs + manifests onto the content-addressed [`sui_castore::StorageBackend`]).
//! The two impls are interchangeable by construction — a handler names only the
//! trait.
//!
//! Content model:
//! - A **blob** is opaque bytes keyed by its [`Digest`]. Immutable at its
//!   address (writing the same digest twice is idempotent, never a mutation).
//! - A **manifest** is bytes + a stored media-type, keyed by its own
//!   [`Digest`]. A **tag** is a *mutable pointer* from `(name, tag)` to a
//!   manifest digest.
//! - The **referrers index** is a reverse map from a subject digest to the
//!   manifests that declare `subject: <that digest>` — this is the seam where
//!   a lacre signature / SBOM / attestation manifest attaches to the image it
//!   describes.

mod layout;
mod mem;
mod overlay;
mod sui;

pub use layout::{LayoutError, LayoutStore, TagConflict};
pub use mem::MemStore;
pub use overlay::OverlayStore;
pub use sui::SuiCacheStore;

use async_trait::async_trait;

use crate::digest::Digest;

/// A stored manifest: its raw bytes plus the media-type it was written under.
///
/// The media-type is stored (not re-sniffed) so a `GET`/`HEAD` returns the
/// exact `Content-Type` the client `PUT` — the OCI spec requires the registry
/// to preserve it, and content-negotiation on read depends on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredManifest {
    /// The raw manifest bytes (the content the digest addresses).
    pub bytes: Vec<u8>,
    /// The media-type the manifest was stored under.
    pub media_type: String,
}

/// A referrer descriptor: one entry in the referrers image-index.
///
/// Produced when a manifest carrying a `subject` field is `PUT`; returned by
/// the referrers API for a given subject digest. `artifact_type` drives the
/// `?artifactType=` filter (end-12b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Referrer {
    /// The digest of the referring manifest.
    pub digest: Digest,
    /// The referring manifest's media-type (the descriptor `mediaType`).
    pub media_type: String,
    /// The referring manifest's `artifactType` (or its config media-type),
    /// used by the `?artifactType=` filter.
    pub artifact_type: Option<String>,
    /// The byte size of the referring manifest.
    pub size: u64,
}

/// A page of tags plus the pagination cursor to continue from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagPage {
    /// The tag names in this page (lexically ordered).
    pub tags: Vec<String>,
    /// The last tag name in the page, if the store has more beyond it — the
    /// value a client passes as `?last=` to fetch the next page. `None` when
    /// this page reached the end.
    pub next_last: Option<String>,
}

/// Whether a repository accepts writes.
///
/// Answered by the store, consulted by the dispatcher BEFORE any write handler
/// runs: a write (`POST`/`PUT`/`PATCH`/`DELETE`) on a [`RepoAccess::ReadOnly`]
/// repository is a typed `DENIED` on the wire and never reaches the upload FSM
/// or the store. The variant is the decision, so a handler cannot "forget" to
/// check a flag — the check lives in one place, ahead of every write arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoAccess {
    /// Pushes, deletes and tag moves are accepted.
    ReadWrite,
    /// Served from immutable content (an OCI image layout in the Nix store);
    /// every write is refused.
    ReadOnly,
}

/// Apply OCI `?n=`/`?last=` pagination to an already lexically-ordered
/// sequence of names — the one implementation behind every store's tags list
/// and the catalog.
pub(crate) fn paginate<I>(ordered: I, n: Option<usize>, last: Option<&str>) -> TagPage
where
    I: IntoIterator<Item = String>,
{
    let mut all: Vec<String> = ordered
        .into_iter()
        .filter(|t| last.is_none_or(|last| t.as_str() > last))
        .collect();
    let more = n.is_some_and(|limit| all.len() > limit);
    if let Some(limit) = n {
        all.truncate(limit);
    }
    let next_last = if more { all.last().cloned() } else { None };
    TagPage { tags: all, next_last }
}

/// The porto storage seam. Errors are typed as [`StoreError`]; the handler
/// layer maps them to [`crate::error::OciError`] wire codes.
#[async_trait]
pub trait RegistryStore: Send + Sync {
    /// Whether `name` accepts writes. See [`RepoAccess`].
    fn access(&self, name: &str) -> RepoAccess;

    /// Every repository this store can serve, lexically ordered — the
    /// `GET /v2/_catalog` source.
    async fn list_repositories(&self) -> Result<Vec<String>, StoreError>;

    /// Fetch a blob's bytes by digest, or `None` if absent.
    async fn get_blob(&self, digest: &Digest) -> Result<Option<Vec<u8>>, StoreError>;

    /// Whether a blob exists at `digest` (a cheap `HEAD`-path check).
    async fn has_blob(&self, digest: &Digest) -> Result<bool, StoreError>;

    /// Store `bytes` at `digest`. The caller has already verified
    /// `Digest::of(bytes) == digest`; the store persists immutably-at-address.
    async fn put_blob(&self, digest: &Digest, bytes: &[u8]) -> Result<(), StoreError>;

    /// Delete a blob by digest. Idempotent (deleting an absent blob is `Ok`).
    async fn delete_blob(&self, digest: &Digest) -> Result<(), StoreError>;

    /// Fetch a manifest by its digest within a repository, or `None`.
    async fn get_manifest(
        &self,
        name: &str,
        digest: &Digest,
    ) -> Result<Option<StoredManifest>, StoreError>;

    /// Store a manifest under its digest within a repository.
    async fn put_manifest(
        &self,
        name: &str,
        digest: &Digest,
        manifest: &StoredManifest,
    ) -> Result<(), StoreError>;

    /// Delete a manifest by digest (also removes any tags pointing at it).
    async fn delete_manifest(&self, name: &str, digest: &Digest) -> Result<(), StoreError>;

    /// Point `tag` at `digest` within `name` (mutable pointer write).
    async fn put_tag(&self, name: &str, tag: &str, digest: &Digest) -> Result<(), StoreError>;

    /// Resolve `tag` to the manifest digest it points at, or `None`.
    async fn resolve_tag(&self, name: &str, tag: &str) -> Result<Option<Digest>, StoreError>;

    /// List a page of tags for `name`, applying `?n=`/`?last=` pagination.
    ///
    /// `n = None` means "all remaining"; `last = None` means "from the start".
    async fn list_tags(
        &self,
        name: &str,
        n: Option<usize>,
        last: Option<&str>,
    ) -> Result<TagPage, StoreError>;

    /// Record that `referrer` (a manifest carrying `subject: subject`) refers
    /// to `subject` within `name`. Idempotent per referrer digest.
    async fn add_referrer(
        &self,
        name: &str,
        subject: &Digest,
        referrer: &Referrer,
    ) -> Result<(), StoreError>;

    /// List the manifests referring to `subject`, optionally filtered to a
    /// single `artifact_type`.
    async fn list_referrers(
        &self,
        name: &str,
        subject: &Digest,
        artifact_type: Option<&str>,
    ) -> Result<Vec<Referrer>, StoreError>;
}

/// A storage-layer failure. Kept small and typed; the handler maps it to an
/// OCI wire code (a durable-store failure is not a client error, so it becomes
/// a bare 500 at the router edge, never a fake OCI success).
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The underlying durable backend failed.
    #[error("backend error: {0}")]
    Backend(String),
    /// A write reached a read-only store (the dispatcher's [`RepoAccess`]
    /// guard normally refuses it first; this is the store's own refusal, so a
    /// content-addressed op that names no repository — a blob delete — is
    /// refused too). Rendered as `DENIED`.
    #[error("read-only: {0}")]
    ReadOnly(String),
    /// Bytes on disk no longer hash to the digest they are served under. The
    /// blob is REFUSED, never served; rendered as a 500 (the server's content
    /// is broken, the client's request is not).
    #[error("corrupt content: {0}")]
    Corrupt(String),
}

impl From<sui_castore::StoreError> for StoreError {
    fn from(e: sui_castore::StoreError) -> Self {
        StoreError::Backend(e.to_string())
    }
}
