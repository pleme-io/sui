//! Decisive empirical check: does a REAL cache.nixos.org signature verify
//! against sui's fingerprint construction?
//!
//! Fixture is a real, currently-served narinfo for hello-2.12.3 fetched from
//! cache.nixos.org (2026-07). The signature is a real Ed25519 signature by
//! the `cache.nixos.org-1` key over nix's canonical fingerprint. This test
//! is the ground truth for whether sui's `compute_fingerprint` (which is fed
//! the narinfo `References:` field verbatim = BARE BASENAMES) matches what
//! nix actually signed (nix prints references as FULL /nix/store paths).

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sui_compat::hash::base64_decode;
use sui_compat::signature::compute_fingerprint;

// Real cache.nixos.org narinfo for hello-2.12.3 (fetched 2026-07).
const STORE_PATH: &str = "/nix/store/pg2zfrrbm58ynbjshhzkgg4q466spinf-hello-2.12.3";
const NAR_HASH: &str = "sha256:14qxzyn4mjn5gqyfwdq0rvr83q1hfy7z0gzbqhyds62kh7q2m46c";
const NAR_SIZE: u64 = 279_624;
// References as they appear in the narinfo wire form: bare basenames.
const REF_BASENAMES: &[&str] = &[
    "ias8xacs1h3jy7xgwi2awvim61k2ji6c-glibc-2.42-67",
    "pg2zfrrbm58ynbjshhzkgg4q466spinf-hello-2.12.3",
];
const SIG: &str = "cache.nixos.org-1:ngMSyeL2+RJMgNKgd84M+rJegrC4w9kWOJLMr916YxYmwAfDKdozkLe4QgIP0T9+FtEaCf/PhBJbfE/KOzLxAQ==";
// The default trusted key.
const CACHE_KEY: &str = "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=";

fn pubkey() -> VerifyingKey {
    let (_name, b64) = CACHE_KEY.split_once(':').unwrap();
    let bytes = base64_decode(b64).unwrap();
    let arr: [u8; 32] = bytes.as_slice().try_into().unwrap();
    VerifyingKey::from_bytes(&arr).unwrap()
}

fn sig() -> Signature {
    let (_name, b64) = SIG.split_once(':').unwrap();
    let bytes = base64_decode(b64).unwrap();
    let arr: [u8; 64] = bytes.as_slice().try_into().unwrap();
    Signature::from_bytes(&arr)
}

fn verifies(fingerprint: &str) -> bool {
    pubkey().verify(fingerprint.as_bytes(), &sig()).is_ok()
}

#[test]
fn basename_refs_verify_or_not() {
    // What sui does TODAY: pass the narinfo References verbatim (basenames).
    let refs: Vec<String> = REF_BASENAMES.iter().map(|s| (*s).to_string()).collect();
    let fp = compute_fingerprint(STORE_PATH, NAR_HASH, NAR_SIZE, &refs);
    eprintln!("BASENAME fingerprint = {fp}");
    eprintln!("BASENAME verifies    = {}", verifies(&fp));

    // What nix ACTUALLY signs: references printed as full store paths.
    let full: Vec<String> = REF_BASENAMES
        .iter()
        .map(|s| format!("/nix/store/{s}"))
        .collect();
    let fp_full = compute_fingerprint(STORE_PATH, NAR_HASH, NAR_SIZE, &full);
    eprintln!("FULLPATH fingerprint = {fp_full}");
    eprintln!("FULLPATH verifies    = {}", verifies(&fp_full));

    // Assert which one is the real signed form (this is the ground truth).
    assert!(
        verifies(&fp_full),
        "full-store-path references MUST verify — this is nix's canonical fingerprint"
    );
}
/// The BASENAME form (what sui fed the fingerprint before the fix) must NOT
/// verify — this is the exact bug the substituter fix closes. Pinning this
/// makes the divergence a permanent, falsifiable fact of the fixture.
#[test]
fn basename_refs_do_not_verify() {
    let refs: Vec<String> = REF_BASENAMES.iter().map(|s| (*s).to_string()).collect();
    let fp = compute_fingerprint(STORE_PATH, NAR_HASH, NAR_SIZE, &refs);
    assert!(
        !verifies(&fp),
        "bare-basename references must NOT verify a real cache.nixos.org signature"
    );
}
