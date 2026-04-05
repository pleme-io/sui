//! Nix store abstraction with SeaORM metadata.

pub mod binary_cache;
pub mod entity;
pub mod local;
pub mod traits;

pub use binary_cache::BinaryCacheStore;
pub use local::LocalStore;
pub use traits::{PathInfo, Store, StoreError, StoreResult};
