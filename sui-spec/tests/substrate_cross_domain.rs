//! Substrate-wide cross-domain invariants.
//!
//! These tests don't probe a single domain — they ride across
//! the boundary between domains to catch the failure modes
//! that show up only when the substrate's pieces are composed
//! together.  Each test fires the moment a new domain breaks
//! one of the invariants future code relies on.

use sui_spec::{catalog, cli_coverage};

#[test]
fn every_catalog_domain_loads_at_least_one_instance() {
    // For every typed domain in the substrate catalog, the
    // `load_canonical` function for its Lisp surface must return
    // at least one parsed instance.  Catches the failure mode
    // where a domain is declared but its `.lisp` file is empty.
    use sui_spec::Spec;

    let domains = catalog::load_canonical().expect("catalog must load");
    let mut failures = Vec::new();

    macro_rules! check_domain {
        ($name:literal, $ty:ty) => {{
            if domains.iter().any(|d| d.name == $name) {
                match <$ty>::load_canonical_all() {
                    Ok(v) if v.is_empty() => failures.push(format!("{}: zero canonical instances", $name)),
                    Ok(_)  => {} // ≥1 instance — good
                    Err(e) => failures.push(format!("{}: load failed: {e:?}", $name)),
                }
            }
        }};
    }

    check_domain!("derivation", sui_spec::derivation::DerivationAlgorithm);
    check_domain!("flake", sui_spec::flake::FlakeShape);
    check_domain!("module_system", sui_spec::module_system::ModuleEvalAlgorithm);
    check_domain!("activation_script", sui_spec::activation_script::ActivationScriptAlgorithm);
    check_domain!("fetcher", sui_spec::fetcher::FetcherSpec);
    check_domain!("substituter", sui_spec::substituter::SubstituterSpec);
    check_domain!("sandbox", sui_spec::sandbox::SandboxSpec);
    check_domain!("store_layout", sui_spec::store_layout::StoreLayout);
    check_domain!("gc", sui_spec::gc::GcAlgorithm);
    check_domain!("hash", sui_spec::hash::HashAlgorithm);
    check_domain!("nar", sui_spec::nar::NarFormat);
    check_domain!("narinfo", sui_spec::narinfo::NarinfoFormat);
    check_domain!("eval_cache", sui_spec::eval_cache::EvalCacheFormat);
    check_domain!("profile", sui_spec::profile::ProfileFormat);
    check_domain!("realisation", sui_spec::realisation::RealisationFormat);
    check_domain!("lock_file", sui_spec::lock_file::LockFileFormat);
    check_domain!("registry", sui_spec::registry::RegistryFormat);
    check_domain!("trust_model", sui_spec::trust_model::TrustModel);
    check_domain!("worker_protocol", sui_spec::worker_protocol::WorkerProtocol);

    assert!(failures.is_empty(),
        "domains without canonical instances:\n  - {}",
        failures.join("\n  - "));
}

#[test]
fn every_catalog_entry_with_substrate_ref_has_real_target() {
    // The cli_coverage catalog references substrate domains by
    // name.  Every such name must exist in the catalog.  Catches
    // typos and dropped domains.
    let coverage = cli_coverage::load_canonical().unwrap();
    let domains = catalog::load_canonical().unwrap();
    let domain_names: std::collections::HashSet<String> =
        domains.iter().map(|d| d.name.clone()).collect();
    let mut failures = Vec::new();
    for c in &coverage {
        for s in &c.substrate {
            if !domain_names.contains(s) {
                failures.push(format!("{}: references non-existent substrate `{s}`", c.name));
            }
        }
    }
    assert!(failures.is_empty(),
        "broken substrate references:\n  - {}", failures.join("\n  - "));
}

#[test]
fn topological_order_includes_every_catalog_domain() {
    let topo = catalog::topological_order().expect("topo must compute");
    let cat = catalog::load_canonical().unwrap();
    assert_eq!(topo.len(), cat.len(),
        "topo dropped domains: {} vs {}", topo.len(), cat.len());
    let topo_names: std::collections::HashSet<String> =
        topo.iter().map(|d| d.name.clone()).collect();
    for d in &cat {
        assert!(topo_names.contains(&d.name),
            "topo missing domain `{}`", d.name);
    }
}

#[test]
fn working_commands_cover_at_least_70_percent_of_substrate() {
    // Every substrate domain should have at least one Working
    // CLI command consuming it — otherwise the substrate work
    // isn't reaching operators.  Floor: 70%.
    let coverage = cli_coverage::load_canonical().unwrap();
    let domains = catalog::load_canonical().unwrap();

    let working_substrate_refs: std::collections::HashSet<String> = coverage.iter()
        .filter(|c| c.maturity == cli_coverage::SuiCommandMaturity::Working)
        .flat_map(|c| c.substrate.iter().cloned())
        .collect();

    let total = domains.len();
    let covered = domains.iter()
        .filter(|d| working_substrate_refs.contains(&d.name))
        .count();
    let pct = covered as f64 / total as f64;
    assert!(pct >= 0.70,
        "substrate utilization regressed: {covered} of {total} domains ({:.1}%) have a Working consumer",
        pct * 100.0);
}

#[test]
fn every_lisp_spec_file_is_nonempty() {
    // Smoke-check that every CANONICAL_*_LISP constant is non-trivial.
    // If a domain's spec file got emptied, we want to know IMMEDIATELY.
    let specs: Vec<(&str, &str)> = vec![
        ("derivation",        sui_spec::derivation::CPPNIX_INPUT_ADDRESSED_LISP),
        ("flake",             sui_spec::flake::CPPNIX_FLAKE_SHAPE_LISP),
        ("module_system",     sui_spec::module_system::CANONICAL_MODULE_SYSTEM_LISP),
        ("activation_script", sui_spec::activation_script::CANONICAL_ACTIVATION_LISP),
        ("fetcher",           sui_spec::fetcher::CANONICAL_FETCHERS_LISP),
        ("substituter",       sui_spec::substituter::CANONICAL_SUBSTITUTERS_LISP),
        ("sandbox",           sui_spec::sandbox::CANONICAL_SANDBOX_LISP),
        ("store_layout",      sui_spec::store_layout::CANONICAL_STORE_LAYOUT_LISP),
        ("gc",                sui_spec::gc::CANONICAL_GC_LISP),
        ("hash",              sui_spec::hash::CANONICAL_HASH_LISP),
        ("nar",               sui_spec::nar::CANONICAL_NAR_LISP),
        ("narinfo",           sui_spec::narinfo::CANONICAL_NARINFO_LISP),
        ("eval_cache",        sui_spec::eval_cache::CANONICAL_EVAL_CACHE_LISP),
        ("profile",           sui_spec::profile::CANONICAL_PROFILE_LISP),
        ("realisation",       sui_spec::realisation::CANONICAL_REALISATION_LISP),
        ("lock_file",         sui_spec::lock_file::CANONICAL_LOCK_FILE_LISP),
        ("registry",          sui_spec::registry::CANONICAL_REGISTRY_LISP),
        ("trust_model",       sui_spec::trust_model::CANONICAL_TRUST_LISP),
        ("worker_protocol",   sui_spec::worker_protocol::CANONICAL_WORKER_PROTOCOL_LISP),
        ("catalog",           sui_spec::catalog::CANONICAL_CATALOG_LISP),
        ("cli_coverage",      sui_spec::cli_coverage::CANONICAL_CLI_COVERAGE_LISP),
    ];

    for (name, source) in &specs {
        assert!(source.len() > 50,
            "{name}: Lisp source is suspiciously short ({} bytes)", source.len());
        assert!(source.contains("(def"),
            "{name}: Lisp source has no `(def...)` form");
    }
}

#[test]
fn cli_coverage_reports_one_hundred_percent() {
    // Substrate-wide invariant — the gauge MUST be 100%.
    // Pair with coverage_at_100.rs but expressed as a
    // cross-domain check.
    let pct = cli_coverage::replacement_percentage().unwrap();
    assert!((pct - 1.0).abs() < f64::EPSILON,
        "nix-replacement coverage regressed: {:.1}%", pct * 100.0);
}

#[test]
fn no_two_substrate_domains_share_a_lisp_keyword() {
    // Every TataraDomain's #[tatara(keyword = "...")] must be
    // unique across the substrate — otherwise the Lisp reader
    // would try to dispatch ambiguously.
    let mut keywords: std::collections::HashMap<&'static str, &'static str> = Default::default();

    macro_rules! register_kw {
        ($name:literal, $ty:ty) => {{
            let kw = <$ty as tatara_lisp::TataraDomain>::KEYWORD;
            if let Some(existing) = keywords.insert(kw, $name) {
                panic!("keyword `{kw}` claimed by both `{existing}` and `{}`", $name);
            }
        }};
    }
    register_kw!("derivation",        sui_spec::derivation::DerivationAlgorithm);
    register_kw!("flake",             sui_spec::flake::FlakeShape);
    register_kw!("module_system",     sui_spec::module_system::ModuleEvalAlgorithm);
    register_kw!("activation_script", sui_spec::activation_script::ActivationScriptAlgorithm);
    register_kw!("fetcher",           sui_spec::fetcher::FetcherSpec);
    register_kw!("substituter",       sui_spec::substituter::SubstituterSpec);
    register_kw!("sandbox",           sui_spec::sandbox::SandboxSpec);
    register_kw!("store_layout",      sui_spec::store_layout::StoreLayout);
    register_kw!("gc",                sui_spec::gc::GcAlgorithm);
    register_kw!("hash_alg",          sui_spec::hash::HashAlgorithm);
    register_kw!("hash_enc",          sui_spec::hash::HashEncoding);
    register_kw!("nar",               sui_spec::nar::NarFormat);
    register_kw!("narinfo",           sui_spec::narinfo::NarinfoFormat);
    register_kw!("eval_cache",        sui_spec::eval_cache::EvalCacheFormat);
    register_kw!("profile",           sui_spec::profile::ProfileFormat);
    register_kw!("realisation",       sui_spec::realisation::RealisationFormat);
    register_kw!("lock_file",         sui_spec::lock_file::LockFileFormat);
    register_kw!("registry",          sui_spec::registry::RegistryFormat);
    register_kw!("trust_model",       sui_spec::trust_model::TrustModel);
    register_kw!("worker_protocol",   sui_spec::worker_protocol::WorkerProtocol);
    register_kw!("catalog",           sui_spec::catalog::SubstrateDomain);
    register_kw!("cli_coverage",      sui_spec::cli_coverage::SuiCommand);
    // 22 unique keywords minimum.
    assert!(keywords.len() >= 22);
}
