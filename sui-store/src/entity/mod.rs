//! SeaORM entities for Nix store metadata.
//!
//! Maps 1:1 to the existing Nix SQLite schema at `/nix/var/nix/db/db.sqlite`.
//! Tables: ValidPaths, Refs, DerivationOutputs.

pub mod derivation_output;
pub mod reference;
pub mod valid_path;
