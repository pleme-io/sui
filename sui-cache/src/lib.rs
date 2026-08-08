//! Built-in binary cache server and push pipeline for sui.
//!
//! Replaces Attic, Cachix, and nix-serve with a single integrated component.
//! Implements the standard Nix binary cache HTTP protocol (narinfo + NAR).
//!
//! # Architecture
//!
//! - [`sui_castore`] — pluggable storage backends (shared with sui-registry);
//!   re-exported from here for backward compatibility.
//! - [`server`] — axum HTTP server implementing the cache protocol
//! - [`signing`] — ed25519 key management and narinfo signing
//! - [`push`] — pipeline to push store paths to the cache
//! - [`gc`] — garbage collection of unreferenced cache entries
//! - [`config`] — cache configuration types (CacheConfig; BackendConfig is in sui-castore)

pub mod config;
pub mod gc;
pub mod push;
pub mod resign;
pub mod server;
pub mod signing;
pub mod watch;

// ---------------------------------------------------------------------------
// Backward-compatibility re-exports from sui-castore.
//
// Every `sui_cache::X` path that existed before the extract-and-dominate
// refactor continues to resolve — callers need zero changes. The canonical
// home is now `sui_castore::X`.
// ---------------------------------------------------------------------------

/// `CacheError` is now `sui_castore::StoreError` (same variants, same derives).
/// This type alias preserves every existing `sui_cache::CacheError` use site.
pub use sui_castore::StoreError as CacheError;

pub use config::{CACHE_TIER_ENV, CacheConfig};

// BackendConfig lives in sui-castore; re-export at the sui-cache surface so
// `sui_cache::BackendConfig` still resolves.
pub use sui_castore::BackendConfig;

// `${VAR}` config-text expansion also lives in sui-castore; re-export here so
// `sui cache serve`'s config loader (`sui_cache::expand_env_vars`) can inject a
// secret-sourced DSN password without the value ever entering the ConfigMap.
pub use sui_castore::{ExpandEnvError, expand_env_vars};

pub use gc::GcResult;
pub use push::{LevelOutOfRange, NarCodec, PushResult, XzLevel, ZstdLevel};
pub use server::{AppState, build_router, serve};
pub use signing::{CacheSigner, verify_narinfo_signature};

// Storage primitives — all moved to sui-castore, re-exported here.
pub use sui_castore::{
    BytesNarSource, DEFAULT_INGEST_MEMORY_CAP, FileNarSource, LocalStorage, MemNarRefIndex,
    NAR_CHUNK_BYTES, NAR_REF_PREFIX, NarRefIndex, NarRefKey, NarRefScan, NarResidency, NarSource,
    NarStream, PgCacheConn, PgStorageBackend, PgTable, RedisBackend, RedisConn, S3Storage,
    SpooledNarSource, StorageBackend, StorageIndex, TIERED_BACKEND_TIER, TieredBackend, TieredTier,
    WritePolicy, advertised_nar_url, advertised_url_line, build_backend, bytes_stream, collect_nar,
    empty_stream, file_stream, is_addressable_nar_path, is_servable_narinfo, referrer_of,
    spool_or_buffer, whole_value_stream,
};

#[cfg(feature = "redis-client")]
pub use sui_castore::RedisConnectionManager;

#[cfg(feature = "postgres")]
pub use sui_castore::SqlxPgCacheConn;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_error_is_store_error_display() {
        let e = CacheError::PathNotFound("/nix/store/abc".to_string());
        assert!(format!("{e}").contains("/nix/store/abc"));
    }

    #[test]
    fn cache_error_io_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let e = CacheError::Io(io_err);
        assert!(format!("{e}").contains("missing"));
    }

    #[test]
    fn cache_error_signing_display() {
        let e = CacheError::Signing("bad key".to_string());
        assert!(format!("{e}").contains("bad key"));
    }

    #[test]
    fn cache_error_not_implemented_display() {
        let e = CacheError::NotImplemented("S3");
        assert!(format!("{e}").contains("S3"));
    }

    #[test]
    fn cache_error_narinfo_display() {
        let e = CacheError::NarInfo("parse failed".to_string());
        assert!(format!("{e}").contains("parse failed"));
    }
}
