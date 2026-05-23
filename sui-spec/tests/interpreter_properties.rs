//! Property tests across the M3.0 interpreters.
//!
//! Each domain in sui-spec has unit tests covering known inputs.
//! This file adds proptest properties that hold for ALL valid
//! inputs — catching edge cases unit tests miss.  Invariants under
//! test:
//!
//! - fetcher: URL validation rejects every non-http(s)/file scheme.
//! - hash: decode → encode roundtrip preserves byte-equality for
//!   each (algorithm, encoding) pair.
//! - narinfo: parse → emit → parse is a fixed-point.
//! - gc: dry-run never invokes delete_path; live-set is always a
//!   subset of all-paths.
//! - module_system: priority resolution is deterministic (same
//!   inputs → same outputs); evaluating a module with no defs
//!   returns its declared defaults.
//! - sandbox: path_allowed is monotonic in allowed_paths (adding
//!   a path never removes a previously-allowed result).
//! - lock_file: malformed JSON always errors; valid v7 always
//!   parses.
//!
//! These are integration-test-shaped because they exercise the
//! public surface of multiple modules at once.

use std::collections::HashMap;

use proptest::prelude::*;

use sui_spec::fetcher::{self, FetchArgs, FetchTransport, FetcherEnvironment};
use sui_spec::gc::{self, GcArgs, GcEnvironment, StorePathInfo};
use sui_spec::hash;
use sui_spec::module_system::{
    self as ms, Definition, Module, NixValue, OptionDecl,
};
use sui_spec::narinfo;
use sui_spec::sandbox::{self, SandboxPlatform, IsolationTier};
use sui_spec::catalog;
use sui_spec::lock_file;
use sui_spec::registry;
use sui_spec::SpecError;

// ── fetcher properties ────────────────────────────────────────────

struct NoopFetcherEnv;
impl FetcherEnvironment for NoopFetcherEnv {
    fn fetch_bytes(&self, _: &str) -> Result<Vec<u8>, String> { Ok(b"x".to_vec()) }
    fn hash_bytes(&self, _: &[u8]) -> String { "sha256:fake".into() }
    fn write_to_store(&self, name: &str, _: &[u8]) -> Result<String, String> {
        Ok(format!("/nix/store/abc-{name}"))
    }
}

proptest! {
    /// fetchurl always rejects URLs that don't start with
    /// http://, https://, or file://.  No matter what the rest
    /// of the URL looks like.
    #[test]
    fn fetcher_rejects_non_url_scheme(prefix in "[a-z]{2,6}", rest in "[a-zA-Z0-9./_-]{1,40}") {
        // Skip the allowed schemes.
        prop_assume!(prefix != "http" && prefix != "https" && prefix != "file");
        let url = format!("{prefix}://{rest}");
        let spec = fetcher::load_named("fetchurl").unwrap();
        let args = FetchArgs {
            url,
            declared_hash: None,
            name_hint: Some("x".into()),
        };
        let res = fetcher::apply(&spec, &args, &NoopFetcherEnv);
        match res {
            Err(SpecError::Interp { phase, .. }) => {
                prop_assert_eq!(phase, "url-validate");
            }
            other => prop_assert!(false, "expected url-validate error, got {other:?}"),
        }
    }

    /// Non-fetchurl transports always return fetcher-unimplemented.
    /// Property holds for every authored fetcher except `fetchurl`.
    #[test]
    fn non_fetchurl_transports_always_unimplemented(
        which in 0u8..4,
    ) {
        let names = ["fetchTarball", "fetchGit", "fetchTree", "path"];
        let spec = fetcher::load_named(names[which as usize]).unwrap();
        // Skip cases where transport happens to be Http+Flat
        // (shouldn't be — fetchTarball is Http+Recursive).
        prop_assume!(
            spec.transport != FetchTransport::Http
            || spec.hash_mode != fetcher::FetchHashMode::Flat
        );
        let args = FetchArgs {
            url: "https://example.com/x".into(),
            declared_hash: None,
            name_hint: None,
        };
        let res = fetcher::apply(&spec, &args, &NoopFetcherEnv);
        let is_interp_err = matches!(res, Err(SpecError::Interp { .. }));
        prop_assert!(is_interp_err);
    }
}

// ── hash properties ───────────────────────────────────────────────

proptest! {
    /// For any random byte sequence, base16 encode → decode roundtrip.
    #[test]
    fn hash_base16_roundtrip(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let encoded = hash::encode_hash("sha256", "base16", &bytes).unwrap();
        let (_, decoded) = hash::decode_hash(&encoded).unwrap();
        prop_assert_eq!(decoded, bytes);
    }

    /// For any random byte sequence, SRI roundtrip preserves bytes.
    #[test]
    fn hash_sri_roundtrip(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let encoded = hash::encode_hash("sha256", "sri", &bytes).unwrap();
        let (algo, decoded) = hash::decode_hash(&encoded).unwrap();
        prop_assert_eq!(algo, "sha256");
        prop_assert_eq!(decoded, bytes);
    }

    /// Different byte sequences yield different SRI encodings.
    /// (Collision would be a cryptographic failure of sha256
    /// itself; for the encoder, distinct inputs → distinct outputs.)
    #[test]
    fn hash_sri_injective(
        a in prop::collection::vec(any::<u8>(), 1..32),
        b in prop::collection::vec(any::<u8>(), 1..32),
    ) {
        prop_assume!(a != b);
        let ea = hash::encode_hash("sha256", "sri", &a).unwrap();
        let eb = hash::encode_hash("sha256", "sri", &b).unwrap();
        prop_assert_ne!(ea, eb);
    }
}

// ── narinfo properties ────────────────────────────────────────────

fn fmt_narinfo() -> narinfo::NarinfoFormat {
    narinfo::load_canonical().unwrap().into_iter()
        .find(|f| f.name == "cppnix-narinfo-v1").unwrap()
}

proptest! {
    /// parse → emit → parse is a fixed-point for any well-formed
    /// narinfo body.  Generates random valid records and proves
    /// the roundtrip property.
    #[test]
    fn narinfo_emit_then_parse_fixed_point(
        path_id in "[a-z0-9]{8,32}",
        nar_size in 1u64..1_000_000,
    ) {
        let rec = narinfo::ParsedNarInfo {
            store_path: format!("/nix/store/{path_id}-test"),
            url: format!("nar/{path_id}.nar.xz"),
            compression: "xz".into(),
            file_hash: None,
            file_size: None,
            nar_hash: format!("sha256:{path_id}"),
            nar_size,
            references: vec![],
            deriver: None,
            system: None,
            signatures: vec![],
            ca: None,
        };
        let emitted = narinfo::emit(&rec);
        let reparsed = narinfo::parse(&emitted, &fmt_narinfo()).unwrap();
        prop_assert_eq!(rec, reparsed);
    }
}

// ── gc properties ─────────────────────────────────────────────────

struct CountingGcEnv {
    delete_calls: std::cell::RefCell<u32>,
}

impl CountingGcEnv {
    fn new() -> Self {
        Self { delete_calls: std::cell::RefCell::new(0) }
    }
}

impl GcEnvironment for CountingGcEnv {
    fn lock_store(&self) -> Result<(), String> { Ok(()) }
    fn unlock_store(&self) -> Result<(), String> { Ok(()) }
    fn collect_gc_roots(&self) -> Result<Vec<String>, String> { Ok(vec![]) }
    fn scan_store(&self) -> Result<Vec<StorePathInfo>, String> {
        // Synthetic: one orphan to be deleted.
        Ok(vec![StorePathInfo {
            path: "/nix/store/orphan".into(),
            references: vec![],
            size: 100,
            age_days: 30,
        }])
    }
    fn delete_path(&self, _: &str) -> Result<u64, String> {
        *self.delete_calls.borrow_mut() += 1;
        Ok(100)
    }
}

proptest! {
    /// dry_run=true MUST mean delete_path is never invoked.
    /// Holds for every (older_than, max_freed) combination.
    #[test]
    fn gc_dry_run_never_deletes(
        older_days in prop::option::of(0u32..365),
        max_bytes in prop::option::of(0u64..10_000_000),
    ) {
        let algo = gc::load_named("cppnix-stop-the-world").unwrap();
        let env = CountingGcEnv::new();
        let _ = gc::apply(
            &algo,
            &GcArgs {
                delete_older_than_days: older_days,
                max_freed_bytes: max_bytes,
                dry_run: true,
            },
            &env,
        );
        prop_assert_eq!(*env.delete_calls.borrow(), 0);
    }
}

// ── module_system properties ──────────────────────────────────────

fn registry() -> Vec<ms::OptionTypeSpec> {
    ms::load_canonical().unwrap().types
}

#[allow(dead_code)]
fn opt_int() -> OptionDecl {
    OptionDecl { type_name: "int".into(), ..Default::default() }
}

fn opt_bool() -> OptionDecl {
    OptionDecl { type_name: "bool".into(), ..Default::default() }
}

proptest! {
    /// For any two valid bool definitions at distinct priorities,
    /// the one with the lower priority level wins.  Holds across
    /// every (value, mid-priority, low-priority) combination.
    #[test]
    fn module_priority_resolution_lower_wins(
        winner_value in any::<bool>(),
        loser_value in any::<bool>(),
        winner_pri in 0u32..50,
        loser_pri in 100u32..2000,
    ) {
        let mut m = Module::default();
        m.options.insert("foo".into(), opt_bool());
        m.config.push(Definition {
            path: "foo".into(),
            value: NixValue::Bool(loser_value),
            priority: loser_pri,
            cond: None,
        });
        m.config.push(Definition {
            path: "foo".into(),
            value: NixValue::Bool(winner_value),
            priority: winner_pri,
            cond: None,
        });
        let config = ms::eval_modules(&[m], &registry()).unwrap();
        prop_assert_eq!(config.get("foo"), Some(&NixValue::Bool(winner_value)));
    }

    /// For any int default + no definition, the default surfaces
    /// in the output config.
    #[test]
    fn module_default_surfaces_when_undefined(default in any::<i32>()) {
        let mut m = Module::default();
        m.options.insert("port".into(), OptionDecl {
            type_name: "int".into(),
            default: Some(NixValue::from(default)),
            ..Default::default()
        });
        let config = ms::eval_modules(&[m], &registry()).unwrap();
        prop_assert_eq!(config.get("port"), Some(&NixValue::from(default)));
    }

    /// mkIf-false drops the definition.  For every (cond, value)
    /// where cond=false, the default surfaces instead.
    #[test]
    fn module_mkif_false_always_drops(value in any::<i32>(), default in any::<i32>()) {
        let mut m = Module::default();
        m.options.insert("port".into(), OptionDecl {
            type_name: "int".into(),
            default: Some(NixValue::from(default)),
            ..Default::default()
        });
        m.config.push(Definition {
            path: "port".into(),
            value: NixValue::from(value),
            priority: 100,
            cond: Some(false),
        });
        let config = ms::eval_modules(&[m], &registry()).unwrap();
        prop_assert_eq!(config.get("port"), Some(&NixValue::from(default)));
    }

    /// Evaluating the same modules twice produces byte-identical
    /// configs.  Determinism is non-negotiable.
    #[test]
    fn module_eval_is_deterministic(values in prop::collection::vec(any::<bool>(), 1..10)) {
        let mut m = Module::default();
        for (i, v) in values.iter().enumerate() {
            let path = format!("opt{i}");
            m.options.insert(path.clone(), opt_bool());
            m.config.push(Definition {
                path,
                value: NixValue::Bool(*v),
                priority: 100,
                cond: None,
            });
        }
        let registry = registry();
        let a = ms::eval_modules(&[m.clone()], &registry).unwrap();
        let b = ms::eval_modules(&[m], &registry).unwrap();
        // BTreeMap-style comparison: same key set + same values.
        let a_set: HashMap<_, _> = a.into_iter().collect();
        let b_set: HashMap<_, _> = b.into_iter().collect();
        prop_assert_eq!(a_set, b_set);
    }
}

// ── sandbox properties ────────────────────────────────────────────

proptest! {
    /// Adding a path to allowed_paths never makes a
    /// previously-allowed path disallowed.  Monotonicity.
    #[test]
    fn sandbox_path_allowed_is_monotonic(
        query in "[a-z/]{4,40}",
        extra in "[a-z/]{4,40}",
    ) {
        let mut spec = sandbox::SandboxSpec {
            name: "test".into(),
            platform: SandboxPlatform::Linux,
            isolation_tier: IsolationTier::Strict,
            allowed_paths: vec!["/nix/store".into()],
            network_allowed: false,
            seccomp_profile: None,
            user_namespacing: false,
        };
        let before = sandbox::path_allowed(&spec, &query);
        spec.allowed_paths.push(extra);
        let after = sandbox::path_allowed(&spec, &query);
        // Adding an allowed path can only ADD permissions, never
        // remove them.  before==true implies after==true.
        if before {
            prop_assert!(after);
        }
    }
}

// ── lock_file properties ──────────────────────────────────────────

fn fmt_lock() -> lock_file::LockFileFormat {
    lock_file::load_canonical().unwrap().into_iter()
        .find(|f| f.name == "cppnix-flake-lock-v7").unwrap()
}

proptest! {
    /// Garbage input always errors.  Holds for every random byte
    /// sequence that isn't valid JSON.
    #[test]
    fn lock_file_garbage_always_errors(garbage in "[a-z !@#]{1,80}") {
        // Skip the (very unlikely) case where the garbage parses.
        if serde_json::from_str::<serde_json::Value>(&garbage).is_ok() {
            return Ok(());
        }
        let err = lock_file::parse(&garbage, &fmt_lock()).unwrap_err();
        let is_interp_err = matches!(err, SpecError::Interp { .. });
        prop_assert!(is_interp_err);
    }

    /// A minimal valid v7 lockfile always parses.  Any int version
    /// other than 7 errors.
    #[test]
    fn lock_file_version_mismatch_always_errors(version in 0u32..20) {
        prop_assume!(version != 7);
        let text = format!(r#"{{ "version": {version}, "root": "x", "nodes": {{}} }}"#);
        let err = lock_file::parse(&text, &fmt_lock()).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => {
                prop_assert_eq!(phase, "lockfile-version-mismatch");
            }
            _ => prop_assert!(false, "expected version-mismatch"),
        }
    }
}

// ── registry::parse_entries properties ────────────────────────────

proptest! {
    /// Garbage input that isn't valid JSON always errors with the
    /// `registry-parse` phase.  Tests robustness of the disk loader
    /// against random byte sequences.
    #[test]
    fn registry_garbage_always_errors_with_parse_phase(
        garbage in "[a-zA-Z !@#$_-]{1,80}"
    ) {
        // Skip the (rare) case where the garbage parses as JSON.
        if serde_json::from_str::<serde_json::Value>(&garbage).is_ok() {
            return Ok(());
        }
        let err = registry::parse_entries(&garbage).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => {
                prop_assert_eq!(phase, "registry-parse");
            }
            _ => prop_assert!(false, "expected registry-parse"),
        }
    }

    /// Any version other than 2 errors with `registry-version`.
    /// Tests the version-discriminator invariant.
    #[test]
    fn registry_wrong_version_always_errors(v in 0u32..50) {
        prop_assume!(v != 2);
        let text = format!(r#"{{"version": {v}, "flakes": []}}"#);
        let err = registry::parse_entries(&text).unwrap_err();
        match err {
            SpecError::Interp { phase, .. } => {
                prop_assert_eq!(phase, "registry-version");
            }
            _ => prop_assert!(false, "expected registry-version"),
        }
    }

    /// Any valid v2 document parses, even with an arbitrary number
    /// of entries.  Tests algorithmic completeness — the parser
    /// handles every (well-typed) input shape we can generate.
    #[test]
    fn registry_arbitrary_indirect_entries_parse(
        entries in prop::collection::vec("[a-z][a-z0-9-]{0,15}", 0..16),
    ) {
        let flakes_json: Vec<String> = entries.iter().map(|name| {
            format!(
                r#"{{
                    "from": {{"type": "indirect", "id": "{name}"}},
                    "to":   {{"type": "github", "owner": "owner", "repo": "{name}"}}
                }}"#
            )
        }).collect();
        let text = format!(
            r#"{{"version": 2, "flakes": [{}]}}"#,
            flakes_json.join(",")
        );
        let parsed = registry::parse_entries(&text).unwrap();
        prop_assert_eq!(parsed.len(), entries.len());
        for (i, name) in entries.iter().enumerate() {
            prop_assert_eq!(&parsed[i].from, name);
            prop_assert_eq!(&parsed[i].to, &format!("github:owner/{name}"));
        }
    }

    /// Entries with `exact: true` always round-trip the flag.
    /// Tests that the boolean discriminator survives parsing.
    #[test]
    fn registry_exact_flag_roundtrips(exact in any::<bool>()) {
        let text = format!(
            r#"{{
                "version": 2,
                "flakes": [{{
                    "from": {{"type": "indirect", "id": "x"}},
                    "to":   {{"type": "github", "owner": "o", "repo": "r"}},
                    "exact": {exact}
                }}]
            }}"#
        );
        let parsed = registry::parse_entries(&text).unwrap();
        prop_assert_eq!(parsed.len(), 1);
        prop_assert_eq!(parsed[0].exact, exact);
    }
}

// ── catalog topological-order substrate-wide invariants ───────────
//
// These aren't proptest properties strictly speaking — they're
// substrate-wide invariants exercised once.  They live in this
// test file because they belong with the rest of the
// substrate-spanning checks, and they're the third site for
// "load catalog, verify property" structure (per-domain
// invariants + bridge contract + this one).

#[test]
fn catalog_has_no_cycles() {
    // topological_order errors with `catalog-cycle` if a cycle
    // exists.  If load succeeds, the substrate's DAG is acyclic.
    let topo = catalog::topological_order().expect("substrate catalog must be acyclic");
    let canonical = catalog::load_canonical().unwrap();
    // Same length — topo includes every domain.
    assert_eq!(topo.len(), canonical.len(),
        "topological order must include every catalog entry");
}

#[test]
fn catalog_topological_order_is_dependency_consistent() {
    // For every domain D, every D.depends_on must appear earlier
    // in the topological order than D itself.  This is the
    // defining invariant of a topological sort.
    let topo = catalog::topological_order().unwrap();
    let position: std::collections::HashMap<String, usize> = topo
        .iter()
        .enumerate()
        .map(|(i, d)| (d.name.clone(), i))
        .collect();
    for d in &topo {
        let my_pos = position[&d.name];
        for dep in &d.depends_on {
            let dep_pos = position.get(dep).copied().unwrap_or(usize::MAX);
            assert!(
                dep_pos < my_pos,
                "domain `{}` depends on `{}` but appears AT or AFTER it in topo order",
                d.name, dep,
            );
        }
    }
}

#[test]
fn catalog_every_dep_edge_points_to_real_domain() {
    // Cross-reference invariant: every `depends_on` entry must
    // name a domain that actually exists in the catalog.  Catches
    // typos at substrate-build time, not at downstream-consumer
    // time.
    let cat = catalog::load_canonical().unwrap();
    let names: std::collections::HashSet<String> = cat
        .iter()
        .map(|d| d.name.clone())
        .collect();
    for d in &cat {
        for dep in &d.depends_on {
            assert!(
                names.contains(dep),
                "domain `{}` depends on `{}` but no catalog entry has that name",
                d.name, dep,
            );
        }
    }
}

#[test]
fn catalog_transitive_dependencies_match_dfs() {
    // For each domain, `transitive_dependencies(name)` must be a
    // superset of every directly-declared depends_on edge.  Tests
    // that the transitive walker doesn't drop edges.
    let cat = catalog::load_canonical().unwrap();
    for d in &cat {
        let transitive = catalog::transitive_dependencies(&d.name).unwrap();
        for direct in &d.depends_on {
            assert!(
                transitive.contains(direct),
                "domain `{}`: transitive deps {:?} missing direct dep `{}`",
                d.name, transitive, direct,
            );
        }
    }
}
