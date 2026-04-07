//! Nix store abstraction with SeaORM metadata.
//!
//! Provides the [`Store`] trait that all store backends implement (local filesystem,
//! binary cache over HTTP). Includes SeaORM entity models that map 1:1 to the Nix
//! SQLite schema.

pub mod binary_cache;
pub mod entity;
pub mod http;
pub mod local;
pub mod traits;

pub use binary_cache::BinaryCacheStore;
pub use http::{HttpClient, HttpError, HttpResponse, ReqwestHttpClient};
pub use local::LocalStore;
pub use traits::{PathInfo, Store, StoreError, StoreResult};
