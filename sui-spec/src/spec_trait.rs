//! The [`Spec`] trait — the unifying abstraction across every
//! authored sui-spec domain.
//!
//! Each domain in sui-spec is a typed Lisp surface: one keyword,
//! one embedded `.lisp` source, one `load_canonical()` function.
//! Until now those load functions were free functions with the
//! same shape but no type-level relationship — every domain
//! reinvented the same three lines.  This trait names the contract
//! once.  Generic code over `T: Spec` can:
//!
//! - load every domain's canonical corpus uniformly,
//! - iterate the substrate by type-erased trait objects when
//!   needed (the `SpecHandle` enum below carries the type-erasure),
//! - enforce at compile time that new domains expose the API
//!   (omitting it breaks downstream `T: Spec` consumers).
//!
//! Per the prime directive: solve once, in one place, stand on a
//! solid abstraction.

use serde::de::DeserializeOwned;
use tatara_lisp::TataraDomain;

use crate::SpecError;

/// The contract every typed sui-spec domain satisfies.
///
/// `TataraDomain` is the upstream marker the derive macro emits;
/// `DeserializeOwned` lets the trait load self from JSON when the
/// tatara compile pipeline returns deserialised values.  The
/// const `CANONICAL_LISP` carries the embedded source so generic
/// code doesn't need to know the file path.
pub trait Spec: TataraDomain + DeserializeOwned + Sized {
    /// The embedded canonical Lisp source for this domain.
    /// Wrapped over `include_str!` in the domain's own module.
    const CANONICAL_LISP: &'static str;

    /// Compile the canonical source and return every authored
    /// instance.  Default impl threads through
    /// `crate::loader::load_all`; domains generally don't need to
    /// override.
    ///
    /// # Errors
    ///
    /// Returns an error if the Lisp source fails to parse under
    /// the domain's typed schema.
    fn load_canonical_all() -> Result<Vec<Self>, SpecError> {
        crate::loader::load_all::<Self>(Self::CANONICAL_LISP)
    }

    /// Look up the canonical instance with the given `name`.
    /// Default impl scans `load_canonical_all` linearly; domains
    /// that store many instances may override with a hashmap.
    ///
    /// # Errors
    ///
    /// Returns `SpecError::Load` if the source fails to parse or no
    /// instance matches `name`.
    fn load_named(name: &str) -> Result<Self, SpecError>
    where
        Self: HasName,
    {
        Self::load_canonical_all()?
            .into_iter()
            .find(|s| s.name() == name)
            .ok_or_else(|| {
                SpecError::Load(format!(
                    "no ({}) with :name {name:?}",
                    Self::KEYWORD,
                ))
            })
    }
}

/// Sub-trait for spec types that carry a `name: String` field.
/// Most do; a few (derivation Phase, fetcher phase) don't because
/// they're sub-structs inside a parent that has the name.
pub trait HasName {
    fn name(&self) -> &str;
}

// ── Blanket impls — every existing domain plugs in by adding one ──
//    line in its module.  See sui-spec/src/derivation.rs etc.

impl Spec for crate::derivation::DerivationAlgorithm {
    const CANONICAL_LISP: &'static str = crate::derivation::CPPNIX_INPUT_ADDRESSED_LISP;
}
impl HasName for crate::derivation::DerivationAlgorithm {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::flake::FlakeShape {
    const CANONICAL_LISP: &'static str = crate::flake::CPPNIX_FLAKE_SHAPE_LISP;
}
impl HasName for crate::flake::FlakeShape {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::probe::Probe {
    const CANONICAL_LISP: &'static str = crate::probe::CANONICAL_PROBES_LISP;
}
impl HasName for crate::probe::Probe {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::rebuild::RebuildProbe {
    const CANONICAL_LISP: &'static str = crate::rebuild::CANONICAL_REBUILD_PROBES_LISP;
}
impl HasName for crate::rebuild::RebuildProbe {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::module_system::ModuleEvalAlgorithm {
    const CANONICAL_LISP: &'static str = crate::module_system::CANONICAL_MODULE_SYSTEM_LISP;
}
impl HasName for crate::module_system::ModuleEvalAlgorithm {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::module_system::OptionTypeSpec {
    const CANONICAL_LISP: &'static str = crate::module_system::CANONICAL_MODULE_SYSTEM_LISP;
}
impl HasName for crate::module_system::OptionTypeSpec {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::module_system::PriorityRank {
    const CANONICAL_LISP: &'static str = crate::module_system::CANONICAL_MODULE_SYSTEM_LISP;
}
impl HasName for crate::module_system::PriorityRank {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::activation_script::ActivationScriptAlgorithm {
    const CANONICAL_LISP: &'static str = crate::activation_script::CANONICAL_ACTIVATION_LISP;
}
impl HasName for crate::activation_script::ActivationScriptAlgorithm {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::fetcher::FetcherSpec {
    const CANONICAL_LISP: &'static str = crate::fetcher::CANONICAL_FETCHERS_LISP;
}
impl HasName for crate::fetcher::FetcherSpec {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::substituter::SubstituterSpec {
    const CANONICAL_LISP: &'static str = crate::substituter::CANONICAL_SUBSTITUTERS_LISP;
}
impl HasName for crate::substituter::SubstituterSpec {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::sandbox::SandboxSpec {
    const CANONICAL_LISP: &'static str = crate::sandbox::CANONICAL_SANDBOX_LISP;
}
impl HasName for crate::sandbox::SandboxSpec {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::store_layout::StoreLayout {
    const CANONICAL_LISP: &'static str = crate::store_layout::CANONICAL_STORE_LAYOUT_LISP;
}
impl HasName for crate::store_layout::StoreLayout {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::gc::GcAlgorithm {
    const CANONICAL_LISP: &'static str = crate::gc::CANONICAL_GC_LISP;
}
impl HasName for crate::gc::GcAlgorithm {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::hash::HashAlgorithm {
    const CANONICAL_LISP: &'static str = crate::hash::CANONICAL_HASH_LISP;
}
impl HasName for crate::hash::HashAlgorithm {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::hash::HashEncoding {
    const CANONICAL_LISP: &'static str = crate::hash::CANONICAL_HASH_LISP;
}
impl HasName for crate::hash::HashEncoding {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::nar::NarFormat {
    const CANONICAL_LISP: &'static str = crate::nar::CANONICAL_NAR_LISP;
}
impl HasName for crate::nar::NarFormat {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::narinfo::NarinfoFormat {
    const CANONICAL_LISP: &'static str = crate::narinfo::CANONICAL_NARINFO_LISP;
}
impl HasName for crate::narinfo::NarinfoFormat {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::eval_cache::EvalCacheFormat {
    const CANONICAL_LISP: &'static str = crate::eval_cache::CANONICAL_EVAL_CACHE_LISP;
}
impl HasName for crate::eval_cache::EvalCacheFormat {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::profile::ProfileFormat {
    const CANONICAL_LISP: &'static str = crate::profile::CANONICAL_PROFILE_LISP;
}
impl HasName for crate::profile::ProfileFormat {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::realisation::RealisationFormat {
    const CANONICAL_LISP: &'static str = crate::realisation::CANONICAL_REALISATION_LISP;
}
impl HasName for crate::realisation::RealisationFormat {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::lock_file::LockFileFormat {
    const CANONICAL_LISP: &'static str = crate::lock_file::CANONICAL_LOCK_FILE_LISP;
}
impl HasName for crate::lock_file::LockFileFormat {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::registry::RegistryFormat {
    const CANONICAL_LISP: &'static str = crate::registry::CANONICAL_REGISTRY_LISP;
}
impl HasName for crate::registry::RegistryFormat {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::trust_model::TrustModel {
    const CANONICAL_LISP: &'static str = crate::trust_model::CANONICAL_TRUST_LISP;
}
impl HasName for crate::trust_model::TrustModel {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::worker_protocol::WorkerProtocol {
    const CANONICAL_LISP: &'static str = crate::worker_protocol::CANONICAL_WORKER_PROTOCOL_LISP;
}
impl HasName for crate::worker_protocol::WorkerProtocol {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::worker_protocol::WorkerOpcode {
    const CANONICAL_LISP: &'static str = crate::worker_protocol::CANONICAL_WORKER_PROTOCOL_LISP;
}
impl HasName for crate::worker_protocol::WorkerOpcode {
    fn name(&self) -> &str { &self.name }
}

impl Spec for crate::catalog::SubstrateDomain {
    const CANONICAL_LISP: &'static str = crate::catalog::CANONICAL_CATALOG_LISP;
}
impl HasName for crate::catalog::SubstrateDomain {
    fn name(&self) -> &str { &self.name }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::Probe;

    #[test]
    fn spec_load_canonical_all_works_through_trait() {
        let probes = Probe::load_canonical_all().expect("load via trait");
        assert!(!probes.is_empty());
    }

    #[test]
    fn spec_load_named_finds_known_probe() {
        let p = Probe::load_named("getflake-outPath").expect("load via trait");
        assert_eq!(p.name(), "getflake-outPath");
    }

    #[test]
    fn spec_load_named_errors_on_missing() {
        let err = Probe::load_named("no-such-probe").expect_err("must error");
        match err {
            SpecError::Load(msg) => {
                assert!(msg.contains("no-such-probe"));
                assert!(msg.contains("defprobe"),
                    "error must mention the keyword");
            }
            _ => panic!("expected SpecError::Load"),
        }
    }

    #[test]
    fn spec_keyword_matches_attribute() {
        // The trait's KEYWORD comes from #[tatara(keyword=...)].
        assert_eq!(Probe::KEYWORD, "defprobe");
    }

    /// Generic function over Spec — proves the trait composes.
    /// Counts the canonical instances of any Spec type.
    fn count_canonical<S: Spec>() -> usize {
        S::load_canonical_all().map(|v| v.len()).unwrap_or(0)
    }

    #[test]
    fn generic_count_over_spec_trait() {
        let derivation_count = count_canonical::<crate::derivation::DerivationAlgorithm>();
        let fetcher_count = count_canonical::<crate::fetcher::FetcherSpec>();
        let catalog_count = count_canonical::<crate::catalog::SubstrateDomain>();
        // Substrate has at least one derivation algo, five fetchers,
        // and the full catalog.
        assert!(derivation_count >= 1);
        assert!(fetcher_count >= 5);
        assert!(catalog_count >= 15);
    }
}
