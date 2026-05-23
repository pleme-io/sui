//! Substrate-wide integration invariants.
//!
//! These are the contracts every typed domain in sui-spec
//! collectively obeys.  Adding a new domain to sui-spec means
//! the new domain's canonical Lisp must parse, its catalog
//! entry must exist, its `apply()` (if exposed) must return a
//! typed result (no panics, no `unimplemented!()`).
//!
//! This test file is the cross-domain backstop — unit tests in
//! each module cover that domain in isolation; this file proves
//! the substrate composes.

use sui_spec::{
    activation_script, catalog, derivation, eval_cache, fetcher, flake, gc,
    hash, lock_file, module_system, nar, narinfo, profile, realisation,
    registry, sandbox, store_layout, substituter, trust_model,
    worker_protocol, SpecError,
};

/// Every typed domain's canonical Lisp must compile without panic.
/// This is the "substrate boots" invariant — a syntax error in any
/// spec breaks every consumer.
#[test]
fn every_canonical_spec_compiles() {
    derivation::load_all_canonical().expect("derivation canonical specs must compile");
    flake::load_canonical().expect("flake canonical spec must compile");
    module_system::load_canonical().expect("module_system canonical specs must compile");
    activation_script::load_canonical()
        .expect("activation_script canonical specs must compile");
    fetcher::load_canonical().expect("fetcher canonical specs must compile");
    substituter::load_canonical().expect("substituter canonical specs must compile");
    sandbox::load_canonical().expect("sandbox canonical specs must compile");
    store_layout::load_canonical().expect("store_layout canonical specs must compile");
    gc::load_canonical().expect("gc canonical specs must compile");
    hash::load_canonical_algorithms().expect("hash algorithms canonical spec must compile");
    hash::load_canonical_encodings().expect("hash encodings canonical spec must compile");
    nar::load_canonical().expect("nar canonical specs must compile");
    narinfo::load_canonical().expect("narinfo canonical specs must compile");
    eval_cache::load_canonical().expect("eval_cache canonical specs must compile");
    profile::load_canonical().expect("profile canonical specs must compile");
    realisation::load_canonical().expect("realisation canonical specs must compile");
    lock_file::load_canonical().expect("lock_file canonical specs must compile");
    registry::load_canonical().expect("registry canonical specs must compile");
    trust_model::load_canonical().expect("trust_model canonical specs must compile");
    worker_protocol::load_canonical_protocols()
        .expect("worker_protocol canonical protocols must compile");
    worker_protocol::load_canonical_opcodes()
        .expect("worker_protocol canonical opcodes must compile");
    catalog::load_canonical().expect("catalog must compile");
}

/// M2/M3/M4 stub interpreters MUST return a typed
/// `SpecError::Interp` rather than panic or silently succeed.
/// This is the "no silent wrong answer" invariant — every gap is
/// surfaced as a typed error, not as a misleading Ok.
#[test]
fn every_stub_apply_returns_typed_error() {
    // module_system::apply
    let algo = module_system::ModuleEvalAlgorithm {
        name: "test".into(),
        phases: vec![],
    };
    let err = module_system::apply(
        &algo,
        module_system::EvalModulesArgs::new(vec![]),
    )
    .expect_err("module_system::apply must return typed error");
    assert!(matches!(err, SpecError::Interp { .. }));

    // activation_script::apply — M3.0 has a working interpreter.
    // The substrate-wide check used to assert it returned a typed
    // not-yet error; now we verify it successfully runs on an
    // empty config and returns a well-formed outcome.
    let a_algo = activation_script::load_canonical().unwrap()
        .into_iter()
        .find(|a| a.target == activation_script::ActivationTarget::Darwin)
        .unwrap();
    let _outcome = activation_script::apply(
        &a_algo,
        &activation_script::ActivationArgs {
            config: sui_spec::module_system::Config::new(),
            toplevel_path: "/nix/store/x".into(),
            host: "h".into(),
            user: "u".into(),
        },
    )
    .expect("activation_script::apply must succeed on empty config");

    // fetcher::apply — M3.0 has a working interpreter for
    // fetchurl, so this test verifies that a non-fetchurl
    // transport (fetchGit) still surfaces a typed not-yet error.
    let f = fetcher::load_named("fetchGit").unwrap();
    struct NoEnv;
    impl fetcher::FetcherEnvironment for NoEnv {
        fn fetch_bytes(&self, _: &str) -> Result<Vec<u8>, String> {
            Err("unused".into())
        }
        fn hash_bytes(&self, _: &[u8]) -> String { "unused".into() }
        fn write_to_store(&self, _: &str, _: &[u8]) -> Result<String, String> {
            Err("unused".into())
        }
    }
    let err = fetcher::apply(
        &f,
        &fetcher::FetchArgs {
            url: "https://example.com/repo".into(),
            declared_hash: None,
            name_hint: None,
        },
        &NoEnv,
    )
    .expect_err("fetchGit must return typed error until M3.1");
    assert!(matches!(err, SpecError::Interp { .. }));

    // substituter::apply — M3.0 has a working interpreter.
    // Verify it surfaces a typed error when the requested path
    // isn't in the substituter (`narinfo-not-found`).
    struct NoPathsEnv;
    impl substituter::SubstituterEnvironment for NoPathsEnv {
        fn query_narinfo(&self, _: &str, _: &str)
            -> Result<Option<substituter::NarInfoRecord>, String>
        { Ok(None) }
        fn fetch_nar(&self, _: &str, _: &str) -> Result<Vec<u8>, String> { unreachable!() }
        fn decompress(&self, _: &str, _: &[u8]) -> Result<Vec<u8>, String> { unreachable!() }
        fn verify_nar_hash(&self, _: &str, _: &[u8]) -> Result<bool, String> { unreachable!() }
        fn import_nar(&self, _: &str, _: &[u8]) -> Result<String, String> { unreachable!() }
    }
    let s = substituter::load_named("cache.nixos.org").unwrap();
    let err = substituter::apply(
        &s,
        &substituter::SubstituteArgs {
            store_path_hash: "abc".into(),
            name_hint: None,
        },
        &NoPathsEnv,
    )
    .expect_err("substituter::apply must error when no path found");
    assert!(matches!(err, SpecError::Interp { .. }));

    // gc::apply — M3.0 has a working interpreter.  Verify it
    // succeeds on an empty store + no roots (vacuous GC = no-op).
    let gc_algo = gc::load_named("cppnix-stop-the-world").unwrap();
    struct EmptyGc;
    impl gc::GcEnvironment for EmptyGc {
        fn lock_store(&self) -> Result<(), String> { Ok(()) }
        fn unlock_store(&self) -> Result<(), String> { Ok(()) }
        fn collect_gc_roots(&self) -> Result<Vec<String>, String> { Ok(vec![]) }
        fn scan_store(&self) -> Result<Vec<gc::StorePathInfo>, String> { Ok(vec![]) }
        fn delete_path(&self, _: &str) -> Result<u64, String> { unreachable!() }
    }
    let _report = gc::apply(
        &gc_algo,
        &gc::GcArgs {
            delete_older_than_days: None,
            max_freed_bytes: None,
            dry_run: false,
        },
        &EmptyGc,
    )
    .expect("vacuous GC on empty store must succeed");
}

/// The catalog is the source of truth for "what does this substrate
/// cover?".  Every catalog entry must correspond to a real module
/// that can load its canonical Lisp.  No phantom entries.
#[test]
fn every_catalog_entry_has_a_loadable_module() {
    let cat = catalog::load_canonical().unwrap();
    // For each entry, attempt to load that domain's spec.  The set
    // is exhaustive — if a new catalog entry lands without a
    // corresponding load function, this test fires (compile-time
    // for new domains because the match is `_ =>` ban).
    for entry in &cat {
        let outcome: Result<(), SpecError> = match entry.name.as_str() {
            "derivation" => derivation::load_all_canonical().map(drop),
            "flake" => flake::load_canonical().map(drop),
            "module_system" => module_system::load_canonical().map(drop),
            "activation_script" => activation_script::load_canonical().map(drop),
            "fetcher" => fetcher::load_canonical().map(drop),
            "substituter" => substituter::load_canonical().map(drop),
            "sandbox" => sandbox::load_canonical().map(drop),
            "store_layout" => store_layout::load_canonical().map(drop),
            "gc" => gc::load_canonical().map(drop),
            "hash" => hash::load_canonical_algorithms().map(drop),
            "nar" => nar::load_canonical().map(drop),
            "narinfo" => narinfo::load_canonical().map(drop),
            "eval_cache" => eval_cache::load_canonical().map(drop),
            "profile" => profile::load_canonical().map(drop),
            "realisation" => realisation::load_canonical().map(drop),
            "lock_file" => lock_file::load_canonical().map(drop),
            "registry" => registry::load_canonical().map(drop),
            "trust_model" => trust_model::load_canonical().map(drop),
            "worker_protocol" => worker_protocol::load_canonical_protocols().map(drop),
            other => panic!(
                "catalog references unknown domain `{other}` — \
                 add a match arm in tests/substrate_invariants.rs::\
                 every_catalog_entry_has_a_loadable_module"
            ),
        };
        outcome.unwrap_or_else(|e| panic!("catalog `{}` failed to load: {e:?}", entry.name));
    }
}

/// The maturity histogram must add up to the full catalog size —
/// every entry is in exactly one gate.
#[test]
fn maturity_histogram_partitions_catalog() {
    let cat = catalog::load_canonical().unwrap();
    let hist = catalog::maturity_histogram().unwrap();
    let total: usize = hist.values().sum();
    assert_eq!(
        total,
        cat.len(),
        "histogram sum {total} != catalog size {}; missing variant in maturity_histogram",
        cat.len(),
    );
}

/// Property-style: every authoring keyword in the catalog must be
/// unique across the whole substrate.  Two domains can't share a
/// `(defwhatever)` keyword without ambiguity.
#[test]
fn authoring_keywords_are_globally_unique() {
    let cat = catalog::load_canonical().unwrap();
    let mut seen: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for entry in &cat {
        for kw in &entry.authoring_keywords {
            if let Some(prev) = seen.get(kw) {
                panic!(
                    "authoring keyword `{kw}` is claimed by both `{prev}` and `{}`",
                    entry.name,
                );
            }
            seen.insert(kw.clone(), entry.name.clone());
        }
    }
}
