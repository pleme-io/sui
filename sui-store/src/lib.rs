//! Nix store abstraction with SeaORM metadata.
//!
//! Provides the [`Store`] trait that all store backends implement (local filesystem,
//! binary cache over HTTP). Includes SeaORM entity models that map 1:1 to the Nix
//! SQLite schema.
//!
//! # Architecture
//!
//! - [`Store`] — async trait defining the store contract (query, add, GC)
//! - [`LocalStore`] — reads `/nix/store` + SQLite via SeaORM
//! - [`BinaryCacheStore`] — read-only HTTP client for cache.nixos.org / Cachix / Attic
//! - [`HttpClient`] — pluggable HTTP backend for testability
//!
//! # Error handling
//!
//! All store operations return [`StoreResult<T>`], which wraps [`StoreError`].
//! Binary cache operations produce [`BinaryCacheError`] internally, which converts
//! into [`StoreError`] via `From` impls.

pub mod binary_cache;
pub mod convergence;
pub mod entity;
pub mod http;
pub mod local;
pub mod nar;
pub mod profile;
pub mod substitute;
pub mod traits;

pub use binary_cache::{BinaryCacheError, BinaryCacheStore, BinaryCacheStoreBuilder};
pub use http::{HttpClient, HttpError, HttpResponse, ReqwestHttpClient};
pub use local::{LocalStore, LocalStoreMode};
pub use nar::decompress_nar;
pub use profile::{Generation, ProfileError, ProfileManager};
pub use substitute::{SubstituteResult, Substitutor};
pub use local::find_gc_roots;
pub use convergence::{ConvergenceStore, DefaultConvergenceStore, GenerationalPath, ImpactReport};
pub use traits::{CorruptPath, GcOptions, GcResult, OptimiseResult, PathInfo, Store, StoreError, StoreResult, VerifyResult};
