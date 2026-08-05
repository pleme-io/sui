//! Content-addressed store backends for sui.
//!
//! This crate owns the [`StorageBackend`] trait and every concrete
//! implementation (Local / S3 / Redis / Postgres / Tiered), plus the
//! [`BackendConfig`] config-select enum and the [`build_backend`] factory.
//! Both `sui-cache` (the Nix binary-cache server) and `sui-registry` (the OCI
//! registry `porto`) depend on this crate; neither depends on the other.
//!
//! # Feature flags
//!
//! - **`redis-client`** — enables the production [`RedisConnectionManager`]
//!   transport. Without it the Redis arm of [`build_backend`] returns a typed
//!   [`StoreError::NotImplemented`].
//! - **`postgres`** — enables the production [`SqlxPgCacheConn`] transport.
//!   Without it the Postgres arm returns [`StoreError::NotImplemented`].

pub mod config;
pub mod env_expand;
pub mod storage;

pub use config::BackendConfig;
pub use env_expand::{expand_env_vars, ExpandEnvError};
pub use storage::{
    advertised_nar_url, advertised_url_line, build_backend, bytes_stream, collect_nar,
    empty_stream, file_stream, is_addressable_nar_path, is_servable_narinfo, referrer_of,
    spool_or_buffer,
    whole_value_stream, BytesNarSource,
    FileNarSource, LocalStorage, MemNarRefIndex, NarRefIndex, NarRefKey, NarRefScan, NarResidency,
    NarSource, NarStream, PgCacheConn, PgStorageBackend, PgTable, RedisBackend, RedisConn,
    S3Storage, SpooledNarSource, StorageBackend, StorageIndex, TieredBackend, TieredTier,
    WritePolicy, DEFAULT_INGEST_MEMORY_CAP, NAR_CHUNK_BYTES, NAR_REF_PREFIX, TIERED_BACKEND_TIER,
};

#[cfg(feature = "redis-client")]
pub use storage::RedisConnectionManager;

#[cfg(feature = "postgres")]
pub use storage::SqlxPgCacheConn;

/// Error type for content-addressed store operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// An I/O operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A store path was not found on the local filesystem.
    #[error("path not found: {0}")]
    PathNotFound(String),

    /// A signing or verification operation failed.
    #[error("signing error: {0}")]
    Signing(String),

    /// A feature is not yet implemented (missing Cargo feature flag).
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    /// A narinfo could not be parsed or was invalid UTF-8.
    #[error("narinfo error: {0}")]
    NarInfo(String),

    /// A value exceeded a tier's configured byte cap and was **refused**
    /// rather than buffered.
    ///
    /// This is a *bound*, not a failure of the cache: the refusing tier is
    /// always a best-effort hot tier, and the durable tiers below it stream the
    /// same content without a cap. Refusing is the whole point — a tier that
    /// buffers whatever it is handed is exactly how a 6 GiB pod is killed by one
    /// large NAR.
    ///
    /// `at_least` is a lower bound, not the true size: collection stops the
    /// moment the cap is crossed, so the rest of the value is never read.
    #[error("value too large: refused at {at_least}+ bytes against a {limit}-byte cap")]
    TooLarge {
        /// The tier's configured cap, in bytes.
        limit: u64,
        /// A lower bound on the value's size, in bytes.
        at_least: u64,
    },

    /// The backend's schema is **absent** — its tables do not exist (e.g. a
    /// durable tier came back up on a fresh volume, or its database was
    /// dropped/recreated under a live connection pool).
    ///
    /// Kept as its own variant, distinct from [`Io`](StoreError::Io), precisely
    /// because it is **self-healable**: the owning backend's DDL is idempotent
    /// (`CREATE TABLE IF NOT EXISTS`), so a consumer that sees this can re-run
    /// it and retry rather than failing the request. A backend that cannot
    /// re-create its own schema must return `Io` instead — never round a
    /// permanent failure up into a healable one.
    #[error("schema missing: {0}")]
    SchemaMissing(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_error_display_path_not_found() {
        let e = StoreError::PathNotFound("/nix/store/abc".to_string());
        assert!(format!("{e}").contains("/nix/store/abc"));
    }

    #[test]
    fn store_error_display_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let e = StoreError::Io(io_err);
        assert!(format!("{e}").contains("missing"));
    }

    #[test]
    fn store_error_display_signing() {
        let e = StoreError::Signing("bad key".to_string());
        assert!(format!("{e}").contains("bad key"));
    }

    #[test]
    fn store_error_display_not_implemented() {
        let e = StoreError::NotImplemented("redis L1");
        assert!(format!("{e}").contains("redis L1"));
    }

    #[test]
    fn store_error_display_narinfo() {
        let e = StoreError::NarInfo("parse failed".to_string());
        assert!(format!("{e}").contains("parse failed"));
    }
}
