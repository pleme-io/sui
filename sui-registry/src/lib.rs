//! porto — the pleme-io-native OCI Distribution Spec v1.1 registry SERVER.
//!
//! This is the "naturalize(zot)" build: zot's registry-server *capability*
//! rebuilt as native pleme-io substrate over sui-cache's content-addressed
//! store — not a wrapper around zot, not a vendored fork. porto is an axum
//! `/v2/` OCI registry whose durable bytes live in
//! [`sui_cache::storage::StorageBackend`] (the same store that holds Nix NARs),
//! whose every failure is a typed [`error::OciError`] wire code, and whose blob
//! uploads pass through a typestate FSM that makes an out-of-order chunk or a
//! wrong finalize-digest *unrepresentable-as-a-silent-success*.
//!
//! ## The pieces
//!
//! - [`digest::Digest`] / [`digest::Reference`] — the typed content-address
//!   with parse-at-boundary (a malformed digest is `DIGEST_INVALID`, never a
//!   constructed-wrong value).
//! - [`store::RegistryStore`] — the mockable storage seam (the testability
//!   contract). Two impls: [`store::MemStore`] (in-memory, tests) and
//!   [`store::SuiCacheStore`] (production, over sui-cache).
//! - [`upload::UploadSessions`] — the blob-upload typestate FSM.
//! - [`error::OciError`] — the typed OCI error surface (code + status + JSON).
//! - [`oci`] — the typed OCI wire structs (never hand-built JSON).
//! - [`server`] — the axum router + the end-1 … end-14 handlers.
//! - [`config::RegistryConfig`] — the typed config surface (shikumi-shaped).
//!
//! ## Where lacre and doca sit
//!
//! porto is the *store server*. **lacre** (the cartorio admission-gate proxy)
//! and **doca** (the client) sit AROUND it and are NOT implemented here. The
//! seam where a lacre signature / SBOM / attestation manifest attaches is the
//! **referrers reverse-index** ([`store::RegistryStore::add_referrer`] /
//! `list_referrers`): a signing manifest declares `subject: <image-digest>`
//! and porto records the reverse edge, so `GET /v2/<name>/referrers/<digest>`
//! returns the signing manifest. lacre reads that edge to enforce admission;
//! porto only stores and serves it.

pub mod config;
pub mod digest;
pub mod error;
pub mod oci;
pub mod route;
pub mod server;
pub mod store;
pub mod upload;

pub use config::RegistryConfig;
pub use digest::{Digest, DigestError, Reference};
pub use error::OciError;
pub use server::{build_router, serve, AppState};
pub use store::{MemStore, Referrer, RegistryStore, StoreError, StoredManifest, SuiCacheStore, TagPage};
pub use upload::{UploadError, UploadId, UploadSessions};
