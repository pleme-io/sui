//! Typed border for the evaluation cache.
//!
//! cppnix has `eval-cache-v5.sqlite`; sui has `sui-cache-eval`
//! (BLAKE3-keyed memoization).  Both serve the same purpose:
//! avoid re-evaluating expressions whose dependencies haven't
//! changed.  This module names the contract.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defeval-cache-format")]
pub struct EvalCacheFormat {
    pub name: String,
    pub version: u32,
    pub backend: EvalCacheBackend,
    #[serde(rename = "hashAlgo")]
    pub hash_algo: EvalCacheHash,
    /// What the cache key is computed over.
    #[serde(rename = "keyInput")]
    pub key_input: EvalCacheKeyInput,
    /// Path the cache lives at (relative to state-root).
    #[serde(rename = "defaultPath")]
    pub default_path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalCacheBackend {
    /// SQLite database (cppnix).
    SQLite,
    /// `redb` content-addressed key-value store (sui).
    Redb,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalCacheHash {
    /// sha256 of canonicalised expression + dep narHashes (cppnix).
    Sha256,
    /// BLAKE3 (sui — same conceptual key, faster algorithm).
    Blake3,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalCacheKeyInput {
    /// Cache key = hash(expr-text + flake-input-narHashes +
    /// system + impure-env-vars + nix-version).
    ExprPlusInputs,
    /// Cache key = hash of the canonical AST encoding only
    /// (impure surface excluded — for pure expressions).
    CanonicalAst,
}

pub const CANONICAL_EVAL_CACHE_LISP: &str =
    include_str!("../specs/eval_cache.lisp");

/// Compile every authored eval-cache format.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<EvalCacheFormat>, SpecError> {
    crate::loader::load_all::<EvalCacheFormat>(CANONICAL_EVAL_CACHE_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_eval_cache_parses() {
        let formats = load_canonical().unwrap();
        assert!(!formats.is_empty());
    }

    #[test]
    fn cppnix_uses_sqlite_sha256() {
        let formats = load_canonical().unwrap();
        let cppnix = formats.iter().find(|f| f.name == "cppnix-eval-cache-v5").unwrap();
        assert_eq!(cppnix.backend, EvalCacheBackend::SQLite);
        assert_eq!(cppnix.hash_algo, EvalCacheHash::Sha256);
    }

    #[test]
    fn sui_uses_redb_blake3() {
        let formats = load_canonical().unwrap();
        let sui = formats.iter().find(|f| f.name == "sui-eval-cache-v1").unwrap();
        assert_eq!(sui.backend, EvalCacheBackend::Redb);
        assert_eq!(sui.hash_algo, EvalCacheHash::Blake3);
    }
}
