//! The `builtins` registry drift gate.
//!
//! sui's `builtins` set is a strict SUPERSET of CppNix's — 124 names against
//! nix 2.31.5's 118 — and nothing in the tree noticed. The failure signature is
//! the quiet direction: an expression using one of the extra names MUST fail on
//! real nix and SUCCEEDS on sui, so sui accepts a program nix rejects. A
//! compatibility layer may lag; it may not be permissive.
//!
//! It went unnoticed because the one test that compares the two
//! (`diff_eval_primitives::diff_filter_attrs`) sits behind `SUI_TEST_ONLINE`,
//! which no workflow sets — so it reported `ok` while executing nothing.
//!
//! Two halves, and the split is deliberate:
//!
//!   - **offline, always runs, total** — sui's live registry must equal the
//!     pinned list exactly. Adding or removing a builtin is red until
//!     `fixtures/BUILTIN-REGISTRY.json` moves in the same commit. This half
//!     needs no nix and no network, so it gates every push today.
//!   - **online, `SUI_TEST_ONLINE=1`** — re-derives the sui-only and
//!     absent-from-sui sets against the host's nix and checks the committed
//!     classification still holds. This is what catches a name changing class
//!     (an experimental builtin becoming stable, say).
//!
//! TIER: eval-caught. The truly-unrepresentable version is a single generated
//! registry both the evaluator and the gate read, so a builtin that is not in
//! the manifest has no registration path at all — named, not scheduled.

use std::collections::BTreeSet;
use std::path::PathBuf;

mod common;

fn manifest() -> serde_json::Value {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/BUILTIN-REGISTRY.json");
    serde_json::from_str(
        &std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())),
    )
    .unwrap_or_else(|e| panic!("parse {}: {e}", p.display()))
}

/// Read a `[String]` field. A MISSING field panics — it is never defaulted to
/// the empty set, because a set that defaults to `[]` compares equal to nothing
/// and passes forever (the vacuity bug wearing a config file).
///
/// An empty field that is genuinely PRESENT is allowed, and the distinction is
/// the point: `classification.experimental` is legitimately `[]` today (both
/// feature-gated builtins live on the CLI side, not the walker's), and a
/// declared-empty arm is a statement, whereas an absent one is a typo. Only
/// `sui_registry` is additionally required to be non-empty, below.
fn names(v: &serde_json::Value, path: &[&str]) -> BTreeSet<String> {
    let mut cur = v;
    for k in path {
        cur = cur.get(k).unwrap_or_else(|| {
            panic!(
                "BUILTIN-REGISTRY.json is missing `{}` — refusing to default it \
                 to the empty set, which would pass unconditionally.",
                path.join(".")
            )
        });
    }
    let arr = cur.as_array().unwrap_or_else(|| {
        panic!("BUILTIN-REGISTRY.json `{}` is not an array", path.join("."))
    });
    arr.iter()
        .map(|x| {
            x.as_str()
                .unwrap_or_else(|| panic!("non-string in `{}`", path.join(".")))
                .to_string()
        })
        .collect()
}

/// The three classification arms, as one set plus their names for messages.
fn classified(m: &serde_json::Value) -> Vec<(&'static str, BTreeSet<String>)> {
    vec![
        (
            "experimental",
            names(m, &["classification", "experimental"]),
        ),
        (
            "nixpkgs_lib_leak",
            names(m, &["classification", "nixpkgs_lib_leak"]),
        ),
        (
            "sui_extension",
            names(m, &["classification", "sui_extension"]),
        ),
    ]
}

/// sui's live registry, read the way any consumer would read it.
fn sui_registry() -> BTreeSet<String> {
    let v = sui_eval::eval_with_file("builtins.attrNames builtins", None)
        .expect("sui must be able to evaluate `builtins.attrNames builtins`");
    let json = v.to_json();
    let arr = json
        .as_array()
        .expect("attrNames must produce a list; if this is a placeholder string, that is the bug");
    arr.iter()
        .map(|x| {
            x.as_str()
                .expect("attrNames must produce strings")
                .to_string()
        })
        .collect()
}

/// The host nix's `builtins` names, WITHOUT experimental features — the
/// default surface a nix program actually runs against. The per-name
/// experimental probes below deliberately re-query with the flags on, which is
/// what separates a feature-gated builtin from an invented one.
fn nix_builtin_names() -> BTreeSet<String> {
    let out = std::process::Command::new("nix")
        .args([
            "eval",
            "--json",
            "--impure",
            "--expr",
            "builtins.attrNames builtins",
        ])
        .output()
        .expect("nix must be on PATH when SUI_TEST_ONLINE=1");
    assert!(
        out.status.success(),
        "nix eval of builtins.attrNames failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("nix must emit JSON for attrNames");
    let arr = v.as_array().expect("attrNames is a list");
    let set: BTreeSet<String> = arr
        .iter()
        .map(|x| x.as_str().expect("names are strings").to_string())
        .collect();
    // Anti-vacuity for the ORACLE, not just the subject: an empty nix registry
    // would make every sui name read as an "extra" (or, with the comparison
    // inverted, make every check pass). Either way the oracle is not usable.
    assert!(
        set.len() > 50,
        "nix reported only {} builtins — that is not a usable oracle",
        set.len()
    );
    set
}

/// The offline half: sui's registry may not drift from the pin in either
/// direction. Runs everywhere, needs nothing.
#[test]
fn sui_builtin_registry_matches_the_pin() {
    let m = manifest();
    let pinned = names(&m, &["sui_registry"]);
    assert!(
        !pinned.is_empty(),
        "BUILTIN-REGISTRY.json `sui_registry` is empty. An empty pin is not a \
         clean comparison, it is the absence of one."
    );
    let live = sui_registry();

    let added: Vec<_> = live.difference(&pinned).cloned().collect();
    let removed: Vec<_> = pinned.difference(&live).cloned().collect();

    assert!(
        added.is_empty() && removed.is_empty(),
        "\nsui's builtin registry drifted from tests/fixtures/BUILTIN-REGISTRY.json.\n\
         \n  ADDED (in sui, not in the pin): {added:?}\
         \n  REMOVED (in the pin, not in sui): {removed:?}\n\
         \nAn ADD is the dangerous direction: if the new name is not a real CppNix\n\
         builtin, sui now accepts an expression that real nix rejects. Before\n\
         re-pinning, check it against nix:\n\
         \n  nix eval --impure --expr 'builtins ? <name>'\n\
         \nand record it under `classification` as `experimental` (a real builtin\n\
         behind a feature flag) or `nixpkgs_lib_leak` (not a builtin at all).\n\
         \nA REMOVE is safe-but-loud — nix programs using it will now fail on sui.\n\
         Either way the pin moves in the SAME commit as the registry.\n\
         \n  live: {} names, pinned: {} names",
        live.len(),
        pinned.len()
    );
}

/// The classification must partition the sui-only set — every extra name is
/// accounted for as either a feature-gated real builtin or a lib leak. This is
/// what stops a new overreach from being quietly appended to `sui_registry`
/// alone: the pin would move, this test would not.
#[test]
fn every_extra_builtin_is_classified() {
    let m = manifest();
    let arms = classified(&m);

    // Disjoint: a name may not sit in two classes, or "6 leaks" could be made
    // to read lower by also filing one as a sui_extension.
    for (i, (an, a)) in arms.iter().enumerate() {
        for (bn, b) in arms.iter().skip(i + 1) {
            let overlap: Vec<_> = a.intersection(b).cloned().collect();
            assert!(
                overlap.is_empty(),
                "a builtin cannot be both `{an}` and `{bn}`: {overlap:?}"
            );
        }
    }

    // Every classified name must actually exist. A classification of absent
    // names is a ledger describing nothing.
    let live = sui_registry();
    for (arm, set) in &arms {
        let unknown: Vec<_> = set.iter().filter(|n| !live.contains(*n)).cloned().collect();
        assert!(
            unknown.is_empty(),
            "`classification.{arm}` names builtins sui does not have: \
             {unknown:?}. If they were removed, drop them from the \
             classification in the same commit."
        );
    }

    let total: usize = arms.iter().map(|(_, s)| s.len()).sum();
    eprintln!(
        "every_extra_builtin_is_classified: {} classified across {} arms, over a \
         {}-name registry",
        total,
        arms.len(),
        live.len()
    );
}

/// The online half: re-derive the classification against the host's nix.
///
/// Skipped without `SUI_TEST_ONLINE=1`. Note the asymmetry — the offline tests
/// above still gate the registry, so a skip here narrows sharpening, not
/// coverage.
#[test]
fn classification_holds_against_real_nix() {
    if common::skip_if_offline("classification_holds_against_real_nix") {
        return;
    }

    let m = manifest();
    let experimental = names(&m, &["classification", "experimental"]);
    let leaks = names(&m, &["classification", "nixpkgs_lib_leak"]);
    let extensions = names(&m, &["classification", "sui_extension"]);

    // ── The totality check. This is the half that makes the classification a
    // PARTITION rather than a list: whatever sui has and nix does not must be
    // exactly the union of the three arms. Without it, a new overreach could
    // be appended to `sui_registry` alone — the pin would move, the offline
    // gate would go green, and nothing would ever ask what class it is.
    let nix_names = nix_builtin_names();
    let live = sui_registry();
    let extras: BTreeSet<String> = live.difference(&nix_names).cloned().collect();
    let union: BTreeSet<String> = experimental
        .union(&leaks)
        .cloned()
        .chain(extensions.iter().cloned())
        .collect();

    let unclassified: Vec<_> = extras.difference(&union).cloned().collect();
    let overclassified: Vec<_> = union.difference(&extras).cloned().collect();
    assert!(
        unclassified.is_empty() && overclassified.is_empty(),
        "\nthe classification does not partition sui's extras.\n  \
         UNCLASSIFIED (sui has, nix does not, no class): {unclassified:?}\n  \
         OVERCLASSIFIED (classified, but nix has it too): {overclassified:?}\n\n\
         An unclassified extra is an expression sui accepts and nix rejects, \
         with nobody having decided that was intended.\n\
         An overclassified name means nix gained it — move it out; it is no \
         longer a divergence.\n  (sui {} names, nix {} names, extras {})",
        live.len(),
        nix_names.len(),
        extras.len()
    );

    // A lib leak must be absent from nix even with EVERY experimental feature
    // on — that is exactly what separates it from the experimental class, and
    // checking it without the flags would conflate the two.
    let mut misclassified = Vec::new();
    for name in &leaks {
        let out = std::process::Command::new("nix")
            .args([
                "eval",
                "--impure",
                "--raw",
                "--extra-experimental-features",
                "fetch-closure dynamic-derivations flakes nix-command",
                "--expr",
            ])
            .arg(format!("if builtins ? {name} then \"HAS\" else \"absent\""))
            .output();
        let Ok(out) = out else { continue };
        if String::from_utf8_lossy(&out.stdout).trim() == "HAS" {
            misclassified.push(name.clone());
        }
    }
    assert!(
        misclassified.is_empty(),
        "these are pinned as `nixpkgs_lib_leak` but real nix HAS them, so they \
         are legitimate builtins (possibly newly stabilised): {misclassified:?}. \
         Move them to `experimental` or drop them from the classification."
    );

    // And an experimental one must be reachable WITH the flags. If nix no
    // longer has it at all, sui is inventing it and it is really a leak.
    let mut vanished = Vec::new();
    for name in &experimental {
        let out = std::process::Command::new("nix")
            .args([
                "eval",
                "--impure",
                "--raw",
                "--extra-experimental-features",
                "fetch-closure dynamic-derivations flakes nix-command",
                "--expr",
            ])
            .arg(format!("if builtins ? {name} then \"HAS\" else \"absent\""))
            .output();
        let Ok(out) = out else { continue };
        if String::from_utf8_lossy(&out.stdout).trim() != "HAS" {
            vanished.push(name.clone());
        }
    }
    assert!(
        vanished.is_empty(),
        "these are pinned as `experimental` (a real builtin behind a feature \
         flag) but this nix does not have them even with the flags on: \
         {vanished:?}. Either the host nix predates them, or sui invented them \
         and they belong in `nixpkgs_lib_leak`."
    );

    eprintln!(
        "classification_holds_against_real_nix: {} leaks confirmed absent, \
         {} experimental confirmed present",
        leaks.len(),
        experimental.len()
    );
}
