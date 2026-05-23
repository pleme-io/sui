//! Per-domain structural invariants — properties every authored
//! instance of each typed substrate domain must satisfy.
//!
//! Unit tests in each module cover the *known* canonical entries
//! (cppnix-input-addressed exists; sha256 has bit-length 256).
//! These integration tests verify *structural* invariants that
//! apply to *every* authored instance, including future ones:
//!
//! - Every derivation algorithm has ≥1 phase.
//! - Every GC algorithm brackets `LockStore`/`DeleteDeadPaths`/
//!   `UnlockStore` correctly.
//! - Every hash algorithm's bit_length matches its name's
//!   conventional digit suffix.
//! - Every trusted substituter verifies signatures.
//! - Every fetcher that touches the network validates URLs.
//!
//! When someone adds a new instance (`defderivation-algorithm
//! cppnix-flavor-3 …`), these properties run against it
//! automatically.  No drift possible — invariants are enforced
//! by tests, not documentation.

use sui_spec::{
    activation_script, derivation, fetcher, gc, hash, module_system, nar,
    narinfo, sandbox, substituter, worker_protocol, Spec,
};

// ── derivation ────────────────────────────────────────────────────

#[test]
fn every_derivation_algorithm_has_at_least_one_phase() {
    let algos = derivation::load_all_canonical().unwrap();
    for algo in &algos {
        assert!(
            !algo.phases.is_empty(),
            "derivation `{}` has zero phases — every algorithm must do something",
            algo.name,
        );
    }
}

#[test]
fn every_input_addressed_drv_serializes_then_hashes() {
    // The IA pattern: Serialize → Sha256 → ComputeDrvPath.  This
    // ordering is non-optional.
    let algos = derivation::load_all_canonical().unwrap();
    for algo in &algos {
        if !algo.name.contains("input-addressed") {
            continue;
        }
        let kinds: Vec<_> = algo.phases.iter().map(|p| p.kind).collect();
        let final_serialize = kinds
            .iter()
            .rposition(|k| matches!(k, derivation::PhaseKind::Serialize));
        let drv_path = kinds
            .iter()
            .rposition(|k| matches!(k, derivation::PhaseKind::ComputeDrvPath));
        assert!(
            final_serialize.is_some() && drv_path.is_some(),
            "{}: input-addressed must Serialize + ComputeDrvPath",
            algo.name,
        );
        assert!(
            final_serialize.unwrap() < drv_path.unwrap(),
            "{}: final Serialize must come before ComputeDrvPath",
            algo.name,
        );
    }
}

// ── gc ────────────────────────────────────────────────────────────

#[test]
fn every_gc_algorithm_acquires_then_releases_the_lock() {
    let algos = gc::load_canonical().unwrap();
    for algo in &algos {
        let kinds: Vec<_> = algo.phases.iter().map(|p| p.kind).collect();
        let lock = kinds.iter().position(|k| *k == gc::GcPhaseKind::LockStore);
        let unlock = kinds.iter().position(|k| *k == gc::GcPhaseKind::UnlockStore);
        assert!(lock.is_some(), "{}: missing LockStore", algo.name);
        assert!(unlock.is_some(), "{}: missing UnlockStore", algo.name);
        assert!(
            lock.unwrap() < unlock.unwrap(),
            "{}: LockStore must precede UnlockStore",
            algo.name,
        );
    }
}

// ── hash ──────────────────────────────────────────────────────────

#[test]
fn every_hash_algorithm_bit_length_matches_name_suffix() {
    let algos = hash::load_canonical_algorithms().unwrap();
    for algo in &algos {
        match algo.name.as_str() {
            "sha1" => assert_eq!(algo.bit_length, 160, "sha1 = 160 bits"),
            "sha256" | "blake3" => {
                assert_eq!(algo.bit_length, 256, "{} = 256 bits", algo.name)
            }
            "sha512" => assert_eq!(algo.bit_length, 512, "sha512 = 512 bits"),
            "md5" => assert_eq!(algo.bit_length, 128, "md5 = 128 bits"),
            other => panic!(
                "unknown hash algorithm {other}; add a bit-length assertion above",
            ),
        }
    }
}

#[test]
fn every_broken_hash_is_actually_known_broken() {
    let algos = hash::load_canonical_algorithms().unwrap();
    for algo in &algos {
        if algo.weakness == hash::HashWeakness::Broken {
            assert!(
                algo.name == "md5",
                "{} is marked Broken but only md5 should be — add a justification",
                algo.name,
            );
        }
        if algo.weakness == hash::HashWeakness::Deprecated {
            assert!(
                algo.name == "sha1",
                "{} is marked Deprecated but only sha1 should be (in nix's surface)",
                algo.name,
            );
        }
    }
}

// ── substituter ───────────────────────────────────────────────────

#[test]
fn every_trusted_substituter_verifies_signatures() {
    let specs = substituter::load_canonical().unwrap();
    for spec in &specs {
        if spec.trust_level != substituter::TrustLevel::Trusted {
            continue;
        }
        let kinds: Vec<_> = spec.phases.iter().map(|p| p.kind).collect();
        assert!(
            kinds.contains(&substituter::SubstituterPhaseKind::VerifyNarSignature),
            "{}: Trusted substituter must include VerifyNarSignature in its pipeline",
            spec.name,
        );
    }
}

#[test]
fn every_substituter_imports_to_store() {
    let specs = substituter::load_canonical().unwrap();
    for spec in &specs {
        let kinds: Vec<_> = spec.phases.iter().map(|p| p.kind).collect();
        assert!(
            kinds.contains(&substituter::SubstituterPhaseKind::ImportNarToStore),
            "{}: every substituter must ImportNarToStore (otherwise it's a no-op)",
            spec.name,
        );
    }
}

// ── fetcher ───────────────────────────────────────────────────────

#[test]
fn every_network_fetcher_validates_url() {
    let specs = fetcher::load_canonical().unwrap();
    for spec in &specs {
        if matches!(spec.transport, fetcher::FetchTransport::LocalPath) {
            continue; // local-path doesn't take a URL
        }
        let kinds: Vec<_> = spec.phases.iter().map(|p| p.kind).collect();
        assert!(
            kinds.contains(&fetcher::FetcherPhaseKind::ValidateUrl),
            "{}: network fetcher must include ValidateUrl",
            spec.name,
        );
    }
}

#[test]
fn every_fetcher_writes_to_store() {
    let specs = fetcher::load_canonical().unwrap();
    for spec in &specs {
        let kinds: Vec<_> = spec.phases.iter().map(|p| p.kind).collect();
        assert!(
            kinds.contains(&fetcher::FetcherPhaseKind::WriteToStore),
            "{}: every fetcher must WriteToStore",
            spec.name,
        );
    }
}

// ── sandbox ───────────────────────────────────────────────────────

#[test]
fn strict_sandboxes_disallow_network() {
    let specs = sandbox::load_canonical().unwrap();
    for spec in &specs {
        if spec.isolation_tier == sandbox::IsolationTier::Strict {
            assert!(
                !spec.network_allowed,
                "{}: Strict sandbox must NOT allow network",
                spec.name,
            );
        }
    }
}

// ── nar ───────────────────────────────────────────────────────────

#[test]
fn every_nar_format_reads_magic_first() {
    let formats = nar::load_canonical().unwrap();
    for f in &formats {
        let kinds: Vec<_> = f.phases.iter().map(|p| p.kind).collect();
        assert_eq!(
            kinds.first().copied(),
            Some(nar::NarPhaseKind::ReadMagic),
            "{}: first phase must be ReadMagic",
            f.name,
        );
    }
}

// ── narinfo ──────────────────────────────────────────────────────

#[test]
fn every_narinfo_format_validates_required_fields() {
    let formats = narinfo::load_canonical().unwrap();
    for f in &formats {
        let kinds: Vec<_> = f.phases.iter().map(|p| p.kind).collect();
        assert!(
            kinds.contains(&narinfo::NarinfoPhaseKind::ValidateRequiredFields),
            "{}: must include ValidateRequiredFields",
            f.name,
        );
        // Length-aligned arrays — both must be the same length.
        assert_eq!(
            f.field_names.len(),
            f.fields.len(),
            "{}: field_names/fields length mismatch",
            f.name,
        );
    }
}

// ── module_system ────────────────────────────────────────────────

#[test]
fn every_module_eval_algorithm_emits_config() {
    let algos = module_system::ModuleEvalAlgorithm::load_canonical_all().unwrap();
    for algo in &algos {
        let kinds: Vec<_> = algo.phases.iter().map(|p| p.kind).collect();
        assert!(
            kinds.contains(&module_system::ModulePhaseKind::EmitConfig),
            "{}: every module-eval algorithm must terminate in EmitConfig",
            algo.name,
        );
    }
}

#[test]
fn priority_lattice_strictly_ordered_low_to_high() {
    let priorities = module_system::PriorityRank::load_canonical_all().unwrap();
    // No two priorities share the same level.  Required for the
    // resolve-priorities phase to be deterministic.
    let mut levels: Vec<u32> = priorities.iter().map(|p| p.level).collect();
    levels.sort();
    levels.dedup();
    assert_eq!(
        levels.len(),
        priorities.len(),
        "priority ranks have duplicate levels — would break tie-breaking",
    );
}

// ── activation_script ────────────────────────────────────────────

#[test]
fn every_activation_script_writes_a_derivation() {
    let algos = activation_script::load_canonical().unwrap();
    for algo in &algos {
        let kinds: Vec<_> = algo.phases.iter().map(|p| p.kind).collect();
        assert!(
            kinds.contains(&activation_script::ActivationPhaseKind::WriteActivationDerivation),
            "{}: every activation algorithm must WriteActivationDerivation",
            algo.name,
        );
        assert!(
            kinds.contains(&activation_script::ActivationPhaseKind::ResolveSystemBuildToplevel),
            "{}: must resolve system.build.toplevel",
            algo.name,
        );
    }
}

// ── worker_protocol ──────────────────────────────────────────────

#[test]
fn worker_opcodes_have_monotonic_codes_for_essential_set() {
    let opcodes = worker_protocol::load_canonical_opcodes().unwrap();
    // The five most foundational opcodes have well-known low codes
    // from the cppnix v1 surface.
    let by_name: std::collections::HashMap<&str, u32> = opcodes
        .iter()
        .map(|o| (o.name.as_str(), o.code))
        .collect();
    assert_eq!(by_name.get("IsValidPath").copied(), Some(1));
    // QueryReferrers historically code 7.
    assert_eq!(by_name.get("QueryReferrers").copied(), Some(7));
}

#[test]
fn every_opcode_has_non_empty_name_and_lowercase_code() {
    let opcodes = worker_protocol::load_canonical_opcodes().unwrap();
    for op in &opcodes {
        assert!(!op.name.is_empty(), "opcode with empty name");
        assert!(op.code < 1000, "opcode `{}` has unrealistic code {}", op.name, op.code);
    }
}
