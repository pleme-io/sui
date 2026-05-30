//! Typed errors for the graph store.
//!
//! Every fallible operation returns [`Result<T>`]. The variants are
//! deliberately specific so callers can react (`HashMismatch` should
//! drop the cache entry and re-fetch; `NotFound` is benign on
//! lookup-or-compute paths).

use std::io;
use std::path::PathBuf;

use crate::content_address::GraphHash;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("graph store I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("redb database error: {0}")]
    Db(#[from] redb::DatabaseError),

    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),

    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),

    /// Asked for a hash the index has no record of.
    #[error("graph not found: {hash}")]
    NotFound { hash: GraphHash },

    /// The blob file's BLAKE3 didn't match the expected hash. Index entry
    /// is stale or the blob was tampered with — caller should drop the
    /// entry and re-fetch from upstream.
    #[error("hash mismatch on blob {expected}: bytes hash to {actual}")]
    HashMismatch {
        expected: GraphHash,
        actual: GraphHash,
    },

    /// rkyv validation rejected the archive shape. The blob is structurally
    /// invalid for the requested type — never write what the typed border
    /// can't read.
    #[error("rkyv validation failed for blob {hash}: {message}")]
    Validation { hash: GraphHash, message: String },

    /// Caller passed a malformed [`GraphHash`] (wrong length / wrong
    /// base32 alphabet).
    #[error("invalid graph hash {input:?}: {reason}")]
    BadHash { input: String, reason: &'static str },
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
