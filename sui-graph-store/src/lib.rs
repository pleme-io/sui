//! sui-graph-store — content-addressed graph store for sui.
//!
//! Replaces the SeaORM/SQLite path with a typed two-tier store:
//!
//! * **L0 index** — `redb` 4.x copy-on-write B+ tree, pure Rust, ACID,
//!   MVCC snapshots, zero-copy `&[u8]` reads. Maps `GraphHash` →
//!   on-disk blob path + bookkeeping (size, kind, created-at).
//! * **L1 blobs** — `rkyv` 0.8 archives on a sharded content-addressed
//!   filesystem layout. Read path is `mmap` + cast → typed access with
//!   zero allocations. Wire format is frozen on the 0.8 series.
//!
//! ## Design contract
//!
//! Every byte written to a blob path is the canonical rkyv archive of a
//! typed graph (lockfile graph, AST graph, module graph). The blob's
//! filename **is** its BLAKE3 hash; the index entry asserts the binding.
//! Verification is implicit on every read: callers either trust the
//! local hash they just computed (`get_unchecked`) or pay the bytecheck
//! pass when pulling from an untrusted source (`get_validated`).
//!
//! The on-disk layout is ZFS-aware: blobs live on a dedicated dataset
//! tuned `recordsize=1M, compression=zstd-3, atime=off`; the redb index
//! lives on a sibling dataset tuned `recordsize=16K, logbias=latency`.
//! Snapshots and `zfs send`-based fleet replication fall out for free —
//! the whole cache is one `zfs send -R | zfs recv` away from every peer.
//!
//! ## Why this exists
//!
//! The previous SQLite path paid a SQL round-trip per access, didn't
//! support `mmap`, and treated the store as a row collection instead of
//! a blob collection. Content-addressed graphs want exactly the opposite
//! shape: write once, read many, verify by hash, no parse cost on read.
//! See `docs/architecture/l1-graph-store.md` (forthcoming) for the
//! full design brief.
//!
//! ## Usage
//!
//! ```no_run
//! use sui_graph_store::{GraphHash, GraphKind, GraphStore};
//! # use std::path::Path;
//!
//! let store = GraphStore::open(Path::new("/var/lib/sui/graph-store"))?;
//!
//! // Archive any rkyv-serializable graph.
//! let bytes = b"...rkyv-encoded payload...";
//! let hash = GraphHash::of(bytes);
//! store.put(GraphKind::Lockfile, hash, bytes)?;
//!
//! // Read it back — zero-copy mmap + cast.
//! let mmap = store.get(GraphKind::Lockfile, hash)?;
//! assert_eq!(&mmap[..], bytes);
//! # Ok::<(), sui_graph_store::Error>(())
//! ```

#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod content_address;
pub mod error;
pub mod layout;
pub mod store;

pub use content_address::GraphHash;
pub use error::{Error, Result};
pub use layout::{GraphKind, StoreLayout};
pub use store::{GraphStore, IndexEntry};
