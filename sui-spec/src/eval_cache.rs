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

// ── M3.0 cache-key derivation ──────────────────────────────────────

/// Inputs to a cache-key derivation.  When `key_input ==
/// ExprPlusInputs`, all fields contribute; for `CanonicalAst`
/// only `expr` does.
#[derive(Debug, Clone, Default)]
pub struct CacheKeyInputs {
    pub expr: String,
    pub flake_input_narhashes: Vec<(String, String)>,
    pub system: String,
    pub impure_env: Vec<(String, String)>,
    pub nix_version: String,
}

/// Derive a cache key for an evaluation.  M3.0 uses hex of sha256
/// (or blake3) of a canonical text serialisation of the inputs.
/// The resulting key is what the cache database indexes by.
///
/// Format-specific:
/// - `sha256` → 64-char lowercase hex.
/// - `blake3` → 64-char lowercase hex (BLAKE3 produces 32 bytes
///   like sha256, conveniently).
///
/// # Errors
///
/// Returns no error today; the signature is `Result` to keep room
/// for M3.1 (when the impure-env may need decryption etc.).
pub fn derive_cache_key(
    format: &EvalCacheFormat,
    inputs: &CacheKeyInputs,
) -> Result<String, SpecError> {
    let mut buf = String::new();
    buf.push_str("nix-version=");
    buf.push_str(&inputs.nix_version);
    buf.push('\n');
    if format.key_input == EvalCacheKeyInput::ExprPlusInputs {
        buf.push_str("system=");
        buf.push_str(&inputs.system);
        buf.push('\n');
        let mut narhashes = inputs.flake_input_narhashes.clone();
        narhashes.sort();
        for (k, v) in &narhashes {
            buf.push_str("input.");
            buf.push_str(k);
            buf.push('=');
            buf.push_str(v);
            buf.push('\n');
        }
        let mut impure = inputs.impure_env.clone();
        impure.sort();
        for (k, v) in &impure {
            buf.push_str("env.");
            buf.push_str(k);
            buf.push('=');
            buf.push_str(v);
            buf.push('\n');
        }
    }
    buf.push_str("expr=");
    buf.push_str(&inputs.expr);

    match format.hash_algo {
        EvalCacheHash::Sha256 => {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(buf.as_bytes());
            let bytes = h.finalize();
            Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
        }
        EvalCacheHash::Blake3 => {
            // Avoid pulling blake3 in to sui-spec; reuse sha2 with
            // a domain-separating prefix.  M3.1 will switch to real
            // BLAKE3 when sui-spec consumes the blake3 crate.
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(b"blake3-domain-separator:");
            h.update(buf.as_bytes());
            let bytes = h.finalize();
            Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
        }
    }
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

    // ── M3.0 cache-key tests ───────────────────────────────────

    fn cppnix() -> EvalCacheFormat {
        load_canonical().unwrap().into_iter()
            .find(|f| f.name == "cppnix-eval-cache-v5").unwrap()
    }

    #[test]
    fn derive_cache_key_is_deterministic() {
        let f = cppnix();
        let inputs = CacheKeyInputs {
            expr: "1 + 2".into(),
            flake_input_narhashes: vec![("nixpkgs".into(), "sha256:abc".into())],
            system: "aarch64-darwin".into(),
            impure_env: vec![],
            nix_version: "2.18".into(),
        };
        let k1 = derive_cache_key(&f, &inputs).unwrap();
        let k2 = derive_cache_key(&f, &inputs).unwrap();
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64);  // sha256 hex
    }

    #[test]
    fn input_order_doesnt_change_key() {
        let f = cppnix();
        let a = CacheKeyInputs {
            expr: "x".into(),
            flake_input_narhashes: vec![
                ("a".into(), "1".into()),
                ("b".into(), "2".into()),
            ],
            system: "x86_64-linux".into(),
            impure_env: vec![],
            nix_version: "2.18".into(),
        };
        let b = CacheKeyInputs {
            flake_input_narhashes: vec![
                ("b".into(), "2".into()),
                ("a".into(), "1".into()),
            ],
            ..a.clone()
        };
        assert_eq!(derive_cache_key(&f, &a).unwrap(), derive_cache_key(&f, &b).unwrap());
    }

    #[test]
    fn different_exprs_yield_different_keys() {
        let f = cppnix();
        let a = CacheKeyInputs { expr: "1".into(), ..Default::default() };
        let b = CacheKeyInputs { expr: "2".into(), ..Default::default() };
        assert_ne!(derive_cache_key(&f, &a).unwrap(), derive_cache_key(&f, &b).unwrap());
    }
}
