//! On-disk layout of the graph store.
//!
//! ```text
//! <root>/
//! ├── index.redb            ← redb DB (one file). ZFS dataset tuning:
//! │                            recordsize=16K, logbias=latency.
//! └── blobs/
//!     └── <kind>/           ← per-graph-kind subtree (cheap directory
//!         │                   walk for kind-scoped GC, no key prefix
//!         │                   shenanigans in the index).
//!         └── <aa>/<bb>/    ← two-byte/two-byte CAS fan-out. Git-shape.
//!             └── <full-hash>.rkyv
//! ```
//!
//! The fan-out gives 256 × 256 = 65 536 leaf directories per kind,
//! which keeps any single directory under ~16 entries until the cache
//! exceeds ~1 M blobs per kind. That sidesteps every well-known
//! filesystem "too many entries in one dir" cliff (ext4 dir_index, ZFS
//! large_dnode, you name it).
//!
//! ZFS expectations:
//!
//! * `<root>` is a dedicated dataset, `recordsize=1M, compression=zstd-3,
//!   atime=off, xattr=sa`. The 1 M record size is the right knob for sui's
//!   distribution (small blobs fit in one allocation; the 25 MB lockfile
//!   reads in 25 sequential records, where zstd's dictionary works best).
//! * The redb file lives on a sibling dataset
//!   (`<root>/../index`) tuned `recordsize=16K, logbias=latency`. Putting
//!   them on one dataset is fine for small fleets; split if write-amp
//!   from the redb WAL starts dominating zstd compression CPU on blobs.
//! * `zfs snapshot <pool>/sui/blobs@<rev>` + `zfs send -R | zfs recv`
//!   replicates the cache atomically — the substituter primitive falls
//!   out for free.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::content_address::GraphHash;

/// The kind of graph held under a blob. Drives the kind-subtree in the
/// on-disk layout. Adding a kind = appending a variant + one new
/// subdirectory; never breaks the existing layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GraphKind {
    /// A `flake.lock` parsed + follows-resolved into a typed graph.
    /// Canonical L1 substrate for every flake operation.
    Lockfile,
    /// A parsed `.nix` file: rnix CST → typed AST → drop the green tree.
    /// Identifies a single Nix expression by its source-hash + import set.
    Ast,
    /// A compiled NixOS / nix-darwin / home-manager module graph: typed
    /// IR with worker/wrapper-split setters and topo order. Keyed by
    /// (module-ast-hashes ⊕ resolved-import-set).
    Module,
    /// A serialized eval-cache entry: (ast-hash, env-hash) → value.
    /// Replicates across the fleet via the substituter protocol.
    EvalCacheEntry,
    /// A derivation's typed graph form (pre-realisation), keyed by
    /// drv-hash. Substituters speak this directly.
    Derivation,
}

impl GraphKind {
    /// Subdirectory name beneath `<root>/blobs/`. Stable identifier;
    /// renaming would break the on-disk layout.
    #[must_use]
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Lockfile => "lockfile",
            Self::Ast => "ast",
            Self::Module => "module",
            Self::EvalCacheEntry => "eval-cache",
            Self::Derivation => "derivation",
        }
    }

    /// Encode as a single byte for the redb key prefix. Stable; never
    /// shift these values without a migration.
    #[must_use]
    pub fn tag(self) -> u8 {
        match self {
            Self::Lockfile => 1,
            Self::Ast => 2,
            Self::Module => 3,
            Self::EvalCacheEntry => 4,
            Self::Derivation => 5,
        }
    }

    /// Inverse of [`Self::tag`]. Returns `None` for unknown tags,
    /// which acts as a forward-compat shield for older binaries
    /// reading newer indexes.
    #[must_use]
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Lockfile),
            2 => Some(Self::Ast),
            3 => Some(Self::Module),
            4 => Some(Self::EvalCacheEntry),
            5 => Some(Self::Derivation),
            _ => None,
        }
    }
}

impl fmt::Display for GraphKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.dir_name())
    }
}

/// Path resolver for the on-disk layout. Cheap to construct; clone-able.
#[derive(Debug, Clone)]
pub struct StoreLayout {
    root: PathBuf,
}

impl StoreLayout {
    /// Pin a store layout at a filesystem root. The root and all
    /// subdirectories are created on first write — read paths fail
    /// fast with [`crate::Error::NotFound`] when a blob is missing.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Root of the store. Caller created or will create it.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to the redb index database file.
    #[must_use]
    pub fn index_db_path(&self) -> PathBuf {
        self.root.join("index.redb")
    }

    /// Directory holding blobs for a single graph kind.
    #[must_use]
    pub fn blobs_dir(&self, kind: GraphKind) -> PathBuf {
        self.root.join("blobs").join(kind.dir_name())
    }

    /// Full path of one blob (file may or may not exist).
    #[must_use]
    pub fn blob_path(&self, kind: GraphKind, hash: GraphHash) -> PathBuf {
        let (aa, bb) = hash.shard_prefix();
        self.blobs_dir(kind).join(aa).join(bb).join(format!(
            "{}.rkyv",
            hash.display_short(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn layout_paths_compose_as_documented() {
        let layout = StoreLayout::at("/var/lib/sui/graph-store");
        let hash = GraphHash::of(b"layout test");
        let (aa, bb) = hash.shard_prefix();
        let expected = format!(
            "/var/lib/sui/graph-store/blobs/lockfile/{}/{}/{}.rkyv",
            aa,
            bb,
            hash.display_short()
        );
        assert_eq!(
            layout.blob_path(GraphKind::Lockfile, hash).to_string_lossy(),
            expected
        );
    }

    #[test]
    fn every_kind_tag_round_trips() {
        for kind in [
            GraphKind::Lockfile,
            GraphKind::Ast,
            GraphKind::Module,
            GraphKind::EvalCacheEntry,
            GraphKind::Derivation,
        ] {
            assert_eq!(GraphKind::from_tag(kind.tag()), Some(kind));
        }
    }

    #[test]
    fn unknown_tag_returns_none() {
        assert!(GraphKind::from_tag(0).is_none());
        assert!(GraphKind::from_tag(255).is_none());
    }
}
