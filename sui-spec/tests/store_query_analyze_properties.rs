//! Property tests for the typed query + analyze primitives.
//!
//! Locks the substrate's behavioural invariants under random
//! inputs:
//! - StorePredicate compose laws (And/Or/Not De Morgan-style)
//! - Findings stability under unchanged inventory
//! - mine_upgrade_paths is well-formed (from < to in version order)
//! - histogram total matches finding count

use proptest::prelude::*;
use sui_spec::store_analyze::{self, AnalyzeConfig, Finding};
use sui_spec::store_inventory::{RefIndex, StoreEntry, StoreInventory};
use sui_spec::store_layout::ParsedStorePath;
use sui_spec::store_query::{matches, StorePredicate};

fn make_entry(name: &str, size: u64, file_count: usize) -> StoreEntry {
    StoreEntry {
        path: std::path::PathBuf::from(format!(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-{name}",
        )),
        parsed: ParsedStorePath {
            algorithm: None,
            hash: "a".repeat(32),
            name: name.to_string(),
            sub_path: None,
        },
        is_directory: false,
        file_count,
        size,
    }
}

// ── StorePredicate compose laws ─────────────────────────────────

proptest! {
    /// `Not(Not(p))` == `p` for every predicate over every entry.
    #[test]
    fn double_negation_is_identity(
        name in "[a-z]{1,8}",
        size in 0u64..1_000_000,
    ) {
        let e = make_entry(&name, size, 1);
        for p in &[
            StorePredicate::SizeAtLeast(100),
            StorePredicate::SizeAtMost(100),
            StorePredicate::NameMatches("[a-z]".into()),
            StorePredicate::FileCountAtLeast(0),
        ] {
            let pp = StorePredicate::Not(Box::new(
                StorePredicate::Not(Box::new(p.clone()))
            ));
            prop_assert_eq!(matches(&e, p, None), matches(&e, &pp, None));
        }
    }

    /// `All([p])` == `p`.
    #[test]
    fn all_singleton_equals_inner(
        size in 0u64..1_000_000,
    ) {
        let e = make_entry("x", size, 1);
        let inner = StorePredicate::SizeAtLeast(500);
        let wrapped = StorePredicate::All(vec![inner.clone()]);
        prop_assert_eq!(matches(&e, &inner, None), matches(&e, &wrapped, None));
    }

    /// `Any([p])` == `p`.
    #[test]
    fn any_singleton_equals_inner(
        size in 0u64..1_000_000,
    ) {
        let e = make_entry("x", size, 1);
        let inner = StorePredicate::SizeAtMost(500);
        let wrapped = StorePredicate::Any(vec![inner.clone()]);
        prop_assert_eq!(matches(&e, &inner, None), matches(&e, &wrapped, None));
    }

    /// `All([])` is always true; `Any([])` is always false.
    #[test]
    fn empty_compose_is_well_defined(
        size in 0u64..1_000_000,
    ) {
        let e = make_entry("x", size, 1);
        prop_assert!(matches(&e, &StorePredicate::All(vec![]), None));
        prop_assert!(!matches(&e, &StorePredicate::Any(vec![]), None));
    }

    /// De Morgan: `Not(All([p, q]))` == `Any([Not(p), Not(q)])`.
    #[test]
    fn de_morgan_all_to_any(
        size in 0u64..2_000_000,
    ) {
        let e = make_entry("x", size, 1);
        let p = StorePredicate::SizeAtLeast(500);
        let q = StorePredicate::SizeAtMost(1500);
        let lhs = StorePredicate::Not(Box::new(StorePredicate::All(vec![p.clone(), q.clone()])));
        let rhs = StorePredicate::Any(vec![
            StorePredicate::Not(Box::new(p)),
            StorePredicate::Not(Box::new(q)),
        ]);
        prop_assert_eq!(matches(&e, &lhs, None), matches(&e, &rhs, None));
    }

    /// De Morgan: `Not(Any([p, q]))` == `All([Not(p), Not(q)])`.
    #[test]
    fn de_morgan_any_to_all(
        size in 0u64..2_000_000,
    ) {
        let e = make_entry("x", size, 1);
        let p = StorePredicate::SizeAtLeast(500);
        let q = StorePredicate::SizeAtMost(1500);
        let lhs = StorePredicate::Not(Box::new(StorePredicate::Any(vec![p.clone(), q.clone()])));
        let rhs = StorePredicate::All(vec![
            StorePredicate::Not(Box::new(p)),
            StorePredicate::Not(Box::new(q)),
        ]);
        prop_assert_eq!(matches(&e, &lhs, None), matches(&e, &rhs, None));
    }

    /// Boundary: SizeAtLeast(N) ∧ SizeAtMost(N) holds exactly when
    /// size == N.
    #[test]
    fn size_boundary_intersection(
        target in 0u64..10_000,
        actual in 0u64..10_000,
    ) {
        let e = make_entry("x", actual, 1);
        let p = StorePredicate::All(vec![
            StorePredicate::SizeAtLeast(target),
            StorePredicate::SizeAtMost(target),
        ]);
        prop_assert_eq!(matches(&e, &p, None), actual == target);
    }
}

// ── Findings stability + histogram ─────────────────────────────

#[test]
fn findings_histogram_total_matches_count() {
    // Build a synthetic mini-inventory with known shapes.
    let mut inv = StoreInventory {
        root: std::path::PathBuf::from("/nix/store"),
        entries: Default::default(),
    };
    inv.entries.insert("aa-x-1.0".to_string(), make_entry("x-1.0", 100, 1));
    inv.entries.insert("aa-x-1.1".to_string(), make_entry("x-1.1", 100, 1));
    inv.entries.insert("aa-y-2.0".to_string(), make_entry("y-2.0", 200, 2));

    let idx = RefIndex::default();
    let findings = store_analyze::analyze(&inv, Some(&idx), &AnalyzeConfig {
        detect_duplicates:     false,
        detect_orphans:        true,
        high_fanout_threshold: 0,
        detect_version_shadows: true,
    });
    let h = store_analyze::histogram(&findings);
    let total = h.duplicates + h.orphans + h.high_fanout + h.version_shadows;
    assert_eq!(total, findings.len(),
        "histogram total {} != findings count {}", total, findings.len());
}

#[test]
fn version_shadow_pairs_are_age_ordered() {
    // For every VersionShadow finding, the parsed versions must
    // be in increasing order under the version_cmp heuristic.
    let mut inv = StoreInventory {
        root: std::path::PathBuf::from("/nix/store"),
        entries: Default::default(),
    };
    inv.entries.insert("aa-tool-1.0".into(), make_entry("tool-1.0", 0, 1));
    inv.entries.insert("aa-tool-1.1".into(), make_entry("tool-1.1", 0, 1));
    inv.entries.insert("aa-tool-2.0".into(), make_entry("tool-2.0", 0, 1));

    let findings = store_analyze::analyze(&inv, None, &AnalyzeConfig {
        detect_duplicates:     false,
        detect_orphans:        false,
        high_fanout_threshold: 0,
        detect_version_shadows: true,
    });

    for f in &findings {
        if let Finding::VersionShadow { older_version, newer_version, .. } = f {
            assert!(older_version < newer_version,
                "expected {older_version} < {newer_version}");
        }
    }
}

#[test]
fn orphan_detection_skips_paths_with_referrers() {
    let mut inv = StoreInventory {
        root: std::path::PathBuf::from("/nix/store"),
        entries: Default::default(),
    };
    inv.entries.insert("aa-x".into(), make_entry("x", 100, 1));
    inv.entries.insert("aa-y".into(), make_entry("y", 100, 1));

    // y references x.
    let mut idx = RefIndex::default();
    let x_path = inv.entries["aa-x"].path.clone();
    let y_path = inv.entries["aa-y"].path.clone();
    idx.referrers.entry(x_path.clone()).or_default().insert(y_path.clone());
    idx.references.entry(y_path).or_default().insert(x_path);

    let findings = store_analyze::analyze(&inv, Some(&idx), &AnalyzeConfig {
        detect_duplicates:     false,
        detect_orphans:        true,
        high_fanout_threshold: 0,
        detect_version_shadows: false,
    });
    // y has no referrers → orphan.  x has 1 referrer → not orphan.
    let orphan_names: Vec<String> = findings.iter().filter_map(|f| match f {
        Finding::Orphan { path, .. } => path.file_name().and_then(|n| n.to_str()).map(|s| s.to_string()),
        _ => None,
    }).collect();
    assert!(orphan_names.iter().any(|n| n.contains("-y")),
        "y should be flagged as orphan; got {orphan_names:?}");
    assert!(!orphan_names.iter().any(|n| n.contains("-x")),
        "x has referrers, should not be orphan; got {orphan_names:?}");
}

#[test]
fn mine_upgrade_paths_produces_ordered_pairs() {
    let mut inv = StoreInventory {
        root: std::path::PathBuf::from("/nix/store"),
        entries: Default::default(),
    };
    inv.entries.insert("aa-pkg-1.0".into(), make_entry("pkg-1.0", 0, 1));
    inv.entries.insert("aa-pkg-1.1".into(), make_entry("pkg-1.1", 0, 1));
    let idx = RefIndex::default();
    let findings = store_analyze::analyze(&inv, None, &AnalyzeConfig {
        detect_duplicates:     false,
        detect_orphans:        false,
        high_fanout_threshold: 0,
        detect_version_shadows: true,
    });
    let upgrades = store_analyze::mine_upgrade_paths(&findings, &idx);
    for up in &upgrades {
        assert!(up.from_version < up.to_version);
        assert_eq!(up.name_root, "pkg");
    }
}
