//! Substrate self-description — the typed catalog of every authored
//! sui-spec domain.
//!
//! Sui-spec has grown to 18 typed domains.  Operators + tooling
//! benefit from a typed inventory: "what does this substrate
//! cover?" becomes a typed query rather than a doc-string search.
//! Per the CSE pattern, the catalog is itself a typed Lisp spec —
//! reflection-as-spec.
//!
//! Adding a new domain to sui-spec means landing one
//! `(defsubstrate-domain ...)` form alongside the domain's own
//! module + spec.  Consumers (the future `sui spec list` CLI,
//! generated docs, drift detectors) iterate this catalog
//! mechanically.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

/// One substrate domain — a typed border + Lisp spec inside
/// sui-spec.  Catalog entries name the domain itself, not the
/// types it owns.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defsubstrate-domain")]
pub struct SubstrateDomain {
    /// Module name (`"derivation"`, `"fetcher"`, ...).
    pub name: String,
    /// Lisp authoring keyword(s) the domain exposes.
    #[serde(rename = "authoringKeywords")]
    pub authoring_keywords: Vec<String>,
    /// Implementation maturity gate (M0..M5).
    #[serde(rename = "gate")]
    pub gate: MaturityGate,
    /// What this domain covers in one short phrase.
    pub purpose: String,
    /// Which cppnix subsystem the domain mirrors.
    #[serde(rename = "cppnixMirror")]
    pub cppnix_mirror: String,
    /// Other domains this one depends on (by name).  Forms the
    /// substrate dependency graph — `activation_script` depends
    /// on `module_system`; `fetcher` depends on `derivation` (FOD
    /// variant); etc.  Adding a new domain means declaring its
    /// dependencies here.
    #[serde(default, rename = "dependsOn")]
    pub depends_on: Vec<String>,
}

/// Implementation maturity level.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MaturityGate {
    /// Substrate primitive + interpreter — production-ready.
    Working,
    /// Typed border + canonical Lisp authored; interpreter is
    /// scoped to M2 (module system).
    M2TypedOnly,
    /// Typed border + canonical Lisp authored; interpreter is
    /// scoped to M3 (everything depending on the module system).
    M3TypedOnly,
    /// Typed border + canonical Lisp authored; interpreter is
    /// scoped to M4 (CA-derivations + dependent flow).
    M4TypedOnly,
    /// Informational only — no interpreter planned (e.g. format
    /// declarations, layout conventions).
    Informational,
}

pub const CANONICAL_CATALOG_LISP: &str = include_str!("../specs/catalog.lisp");

/// Compile the substrate catalog.
///
/// # Errors
///
/// Returns an error if the Lisp source fails to parse.
pub fn load_canonical() -> Result<Vec<SubstrateDomain>, SpecError> {
    crate::loader::load_all::<SubstrateDomain>(CANONICAL_CATALOG_LISP)
}

/// Find a domain entry by name.
///
/// # Errors
///
/// Returns an error if the catalog fails to parse or no entry matches.
pub fn lookup(name: &str) -> Result<SubstrateDomain, SpecError> {
    load_canonical()?
        .into_iter()
        .find(|d| d.name == name)
        .ok_or_else(|| SpecError::Load(format!(
            "no (defsubstrate-domain) with :name {name:?}",
        )))
}

/// Count of domains by maturity gate.
///
/// # Errors
///
/// Returns an error if the catalog fails to parse.
pub fn maturity_histogram() -> Result<std::collections::BTreeMap<&'static str, usize>, SpecError> {
    let cat = load_canonical()?;
    let mut h: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for d in &cat {
        let key = match d.gate {
            MaturityGate::Working => "Working",
            MaturityGate::M2TypedOnly => "M2TypedOnly",
            MaturityGate::M3TypedOnly => "M3TypedOnly",
            MaturityGate::M4TypedOnly => "M4TypedOnly",
            MaturityGate::Informational => "Informational",
        };
        *h.entry(key).or_default() += 1;
    }
    Ok(h)
}

/// Compute the transitive dependency closure of one domain (the
/// set of all domains reachable via `depends_on` edges, including
/// the domain itself).
///
/// # Errors
///
/// Returns `SpecError::Load` if the catalog fails to parse or
/// `name` is missing.  Returns `SpecError::Interp` with phase
/// `dependency-cycle` if the graph contains a cycle.
pub fn transitive_dependencies(name: &str) -> Result<std::collections::BTreeSet<String>, SpecError> {
    let cat = load_canonical()?;
    let by_name: std::collections::HashMap<&str, &SubstrateDomain> =
        cat.iter().map(|d| (d.name.as_str(), d)).collect();
    if !by_name.contains_key(name) {
        return Err(SpecError::Load(format!("domain `{name}` not in catalog")));
    }
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut stack: Vec<String> = vec![name.to_string()];
    let mut in_path: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    while let Some(current) = stack.pop() {
        if !seen.insert(current.clone()) {
            continue;
        }
        if !in_path.insert(current.clone()) {
            return Err(SpecError::Interp {
                phase: "dependency-cycle".into(),
                message: format!("cycle detected involving `{current}`"),
            });
        }
        let Some(domain) = by_name.get(current.as_str()) else {
            return Err(SpecError::Load(format!(
                "domain `{current}` referenced but not in catalog",
            )));
        };
        for dep in &domain.depends_on {
            stack.push(dep.clone());
        }
    }
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn catalog_parses() {
        let cat = load_canonical().expect("catalog must compile");
        assert!(
            cat.len() >= 15,
            "catalog must enumerate at least 15 domains, got {}",
            cat.len(),
        );
    }

    #[test]
    fn every_authored_domain_is_in_catalog() {
        let cat = load_canonical().unwrap();
        let names: HashSet<&str> = cat.iter().map(|d| d.name.as_str()).collect();
        // The 18 domains today.  If a new domain lands without a
        // catalog entry, this test fires.
        for required in [
            "derivation",
            "realisation",
            "module_system",
            "activation_script",
            "flake",
            "lock_file",
            "registry",
            "fetcher",
            "substituter",
            "sandbox",
            "store_layout",
            "gc",
            "hash",
            "nar",
            "narinfo",
            "eval_cache",
            "profile",
            "trust_model",
            "worker_protocol",
        ] {
            assert!(
                names.contains(required),
                "catalog missing domain `{required}` — sui-spec/src/{required}.rs \
                 exists but its catalog entry doesn't",
            );
        }
    }

    #[test]
    fn maturity_histogram_sums_to_catalog_size() {
        let cat = load_canonical().unwrap();
        let h = maturity_histogram().unwrap();
        let total: usize = h.values().sum();
        assert_eq!(total, cat.len());
    }

    #[test]
    fn working_domains_include_the_known_three() {
        let cat = load_canonical().unwrap();
        let working: HashSet<&str> = cat
            .iter()
            .filter(|d| d.gate == MaturityGate::Working)
            .map(|d| d.name.as_str())
            .collect();
        // These three have full implementations on the substrate.
        for required in ["derivation", "flake"] {
            assert!(
                working.contains(required),
                "catalog: {required} should be Working — substrate has the impl today",
            );
        }
    }

    #[test]
    fn lookup_finds_known_domain() {
        let d = lookup("derivation").expect("derivation must be in catalog");
        assert_eq!(d.name, "derivation");
        assert!(d.purpose.len() > 10);
    }

    #[test]
    fn lookup_errors_on_missing() {
        let err = lookup("nonexistent-domain").expect_err("must error on unknown");
        match err {
            SpecError::Load(msg) => assert!(msg.contains("nonexistent-domain")),
            _ => panic!("expected SpecError::Load"),
        }
    }

    #[test]
    fn transitive_dependencies_of_activation_includes_module_system() {
        let deps = transitive_dependencies("activation_script").unwrap();
        // activation_script depends on module_system (M2 gate) +
        // derivation; derivation depends on hash + store_layout.
        // Closure must contain all five.
        for required in [
            "activation_script", // self
            "module_system",
            "derivation",
            "hash",
            "store_layout",
        ] {
            assert!(
                deps.contains(required),
                "transitive deps missing `{required}`: {deps:?}",
            );
        }
    }

    #[test]
    fn transitive_dependencies_of_hash_is_just_itself() {
        let deps = transitive_dependencies("hash").unwrap();
        assert_eq!(deps.len(), 1);
        assert!(deps.contains("hash"));
    }

    #[test]
    fn every_declared_dependency_exists_in_catalog() {
        let cat = load_canonical().unwrap();
        let names: std::collections::HashSet<&str> =
            cat.iter().map(|d| d.name.as_str()).collect();
        for d in &cat {
            for dep in &d.depends_on {
                assert!(
                    names.contains(dep.as_str()),
                    "domain `{}` declares dependency on `{dep}`, \
                     which is not in the catalog",
                    d.name,
                );
            }
        }
    }

    #[test]
    fn substrate_graph_has_no_cycles() {
        let cat = load_canonical().unwrap();
        for d in &cat {
            let _ = transitive_dependencies(&d.name)
                .unwrap_or_else(|e| panic!("cycle starting from `{}`: {e:?}", d.name));
        }
    }
}
