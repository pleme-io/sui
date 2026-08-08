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
/// Basename references MUST verify, because `compute_fingerprint` absolutizes
/// them itself.
///
/// ── THIS ASSERTION USED TO SAY THE OPPOSITE, AND WAS RED ─────────────────
/// It was written when the caller owned absolutization, and pinned the
/// divergence ("bare basenames must NOT verify") as the falsifiable fact. The
/// later fix moved absolutization INSIDE `compute_fingerprint` — deliberately,
/// so signer and verifier canonicalize at one point — which made the old
/// assertion describe a state the code can no longer reach. Measured
/// 2026-08-08 on a clean tree: `basename_refs_do_not_verify` FAILED, and had
/// been failing since that fix landed.
///
/// The invariant worth pinning now is the stronger one: the function is
/// INDIFFERENT to the reference form it is handed. Both forms must produce the
/// identical fingerprint and both must verify a real cache.nixos.org
/// signature — which is what makes the wrong-form fingerprint unconstructible
/// rather than merely avoided.
#[test]
fn reference_form_is_irrelevant_both_verify() {
    let basenames: Vec<String> = REF_BASENAMES.iter().map(|s| (*s).to_string()).collect();
    let absolute: Vec<String> = REF_BASENAMES
        .iter()
        .map(|s| format!("/nix/store/{s}"))
        .collect();

    let fp_base = compute_fingerprint(STORE_PATH, NAR_HASH, NAR_SIZE, &basenames);
    let fp_abs = compute_fingerprint(STORE_PATH, NAR_HASH, NAR_SIZE, &absolute);

    assert_eq!(
        fp_base, fp_abs,
        "basename and absolute reference forms must canonicalize to ONE fingerprint"
    );
    assert!(
        verifies(&fp_base),
        "the canonical fingerprint must verify a real cache.nixos.org signature"
    );
}

/// A narinfo carrying a HEX `NarHash` must verify against a real
/// cache.nixos.org signature exactly as the base32 form does.
///
/// This is the regression for the fleet-origin outage measured 2026-08-08:
/// rio's `sui cache serve` emitted `NarHash: sha256:<hex>` and signed that
/// string verbatim, while Nix fingerprints with Nix-base32 — so every path it
/// served was discarded as unsigned despite carrying a real `Sig:` by a real
/// trusted key whose public half genuinely derived from the signing secret.
///
/// The fixture is deliberately the SAME authoritative cache.nixos.org
/// signature used above, with only the hash ENCODING changed. That keeps the
/// oracle real: this cannot pass by agreeing with sui's own signer, only by
/// agreeing with Nix. Before the `canonical_nar_hash` fix this assertion
/// fails; after it, both encodings collapse to one fingerprint.
#[test]
fn hex_nar_hash_verifies_like_base32() {
    use sui_compat::hash::{decode_hash_any, HashAlgorithm, NixHash};

    let (algo_str, raw) = NAR_HASH.split_once(':').unwrap();
    let algo = HashAlgorithm::from_nix_str(algo_str).unwrap();
    let digest = decode_hash_any(algo, raw).unwrap();
    // `to_hex` is sui's own renderer — the same one whose output ends up in a
    // narinfo, so this fixture is the real emitted shape, not a hand-rolled one.
    let hex_form = format!("{algo_str}:{}", NixHash::new(algo, digest).to_hex());

    assert_ne!(hex_form, NAR_HASH, "fixture must actually differ in encoding");

    let refs: Vec<String> = REF_BASENAMES.iter().map(|s| (*s).to_string()).collect();
    let fp_b32 = compute_fingerprint(STORE_PATH, NAR_HASH, NAR_SIZE, &refs);
    let fp_hex = compute_fingerprint(STORE_PATH, &hex_form, NAR_SIZE, &refs);

    assert_eq!(
        fp_b32, fp_hex,
        "hex and base32 NarHash must canonicalize to ONE fingerprint"
    );
    assert!(
        verifies(&fp_hex),
        "a hex-encoded NarHash must verify a real cache.nixos.org signature"
    );
}
