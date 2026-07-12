//! Seal: transitive-input resolution honors the ROOT lock's node graph
//! (with `follows` redirection), never a sub-flake's own `flake.lock`.
//!
//! This is the offline, deterministic regression seal for the marquee
//! darwin root cause (2026-07-12): sui recursed into a sub-flake
//! (`ishou`) and re-read its OWN `flake.lock` (`substrate = fcd35143…`),
//! ignoring the root lock's `ishou.inputs.substrate = ["substrate"]`
//! follows edge that redirects to root's `substrate_5 = b2802c62…`.
//! Every downstream drvPath diverged from nix.
//!
//! CppNix pins a flake's ENTIRE transitive input closure in the root
//! lock and passes each sub-flake its inputs as resolved by THAT graph.
//! The fixture below reproduces the exact shape with pure `type = "path"`
//! inputs — no network — so it runs in the sealed offline gate:
//!
//!   root
//!    ├─ inputs.sub  → node `sub`
//!    ├─ inputs.depA → node `depA` (marker = "A")
//!    └─ inputs.depB → node `depB` (marker = "B")
//!
//!   root lock: sub.inputs.dep = ["depA"]   (FOLLOWS → root's depA)
//!   sub's OWN lock (present, and a TRAP): sub.inputs.dep → depB
//!
//! nix semantics (and the fix): `sub.inputs.dep` resolves to depA
//! (marker "A"), because the ROOT lock's follows edge wins.  The
//! pre-fix bug read sub's own lock and resolved depB (marker "B").

use std::path::Path;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// A minimal leaf flake exposing a single string `marker`.
fn leaf_flake(marker: &str) -> String {
    format!(
        r#"{{
  description = "leaf {marker}";
  outputs = {{ self }}: {{ marker = "{marker}"; }};
}}"#
    )
}

/// Build the fixture tree under `root_dir`, return the root path.
fn build_fixture(root_dir: &Path) {
    // Leaf flakes depA / depB — distinguishable markers.
    let dep_a = root_dir.join("depA");
    let dep_b = root_dir.join("depB");
    write(&dep_a.join("flake.nix"), &leaf_flake("A"));
    write(&dep_b.join("flake.nix"), &leaf_flake("B"));

    // Sub-flake: declares `dep`, re-exposes `dep.marker` as `subDepMarker`.
    let sub = root_dir.join("sub");
    write(
        &sub.join("flake.nix"),
        r#"{
  description = "sub";
  inputs.dep.url = "path:../depB";
  outputs = { self, dep }: { subDepMarker = dep.marker; };
}"#,
    );
    // Sub's OWN flake.lock is a TRAP: it pins `dep` → depB (marker "B").
    // The root lock's follows edge must override this.
    write(
        &sub.join("flake.lock"),
        &sub_own_lock(&dep_b),
    );

    // Root flake: consumes sub + depA + depB, surfaces sub's resolved marker.
    write(
        &root_dir.join("flake.nix"),
        r#"{
  description = "root";
  inputs.sub.url = "path:./sub";
  inputs.depA.url = "path:./depA";
  inputs.depB.url = "path:./depB";
  outputs = { self, sub, depA, depB }: {
    # The value under test: which dep did `sub` resolve?
    resolved = sub.subDepMarker;
  };
}"#,
    );
    // Root lock: sub.inputs.dep = ["depA"]  (FOLLOWS root's depA).
    write(
        &root_dir.join("flake.lock"),
        &root_lock(&dep_a, &dep_b, &sub),
    );
}

/// The sub-flake's OWN lock — pins `dep` directly to depB.  This is the
/// trap the buggy recursion fell into.
fn sub_own_lock(dep_b: &Path) -> String {
    serde_json::json!({
        "nodes": {
            "dep": {
                "flake": true,
                "locked": { "type": "path", "path": dep_b.to_string_lossy() },
                "original": { "type": "path", "path": "../depB" }
            },
            "root": { "inputs": { "dep": "dep" } }
        },
        "root": "root",
        "version": 7
    })
    .to_string()
}

/// The ROOT lock — the authoritative closure.  Critically,
/// `sub.inputs.dep` is a FOLLOWS array `["depA"]`, redirecting to the
/// root's depA node, NOT sub's own depB pin.
fn root_lock(dep_a: &Path, dep_b: &Path, sub: &Path) -> String {
    serde_json::json!({
        "nodes": {
            "depA": {
                "flake": true,
                "locked": { "type": "path", "path": dep_a.to_string_lossy() },
                "original": { "type": "path", "path": "./depA" }
            },
            "depB": {
                "flake": true,
                "locked": { "type": "path", "path": dep_b.to_string_lossy() },
                "original": { "type": "path", "path": "./depB" }
            },
            "sub": {
                "flake": true,
                // FOLLOWS: sub's `dep` is redirected to the ROOT depA node.
                "inputs": { "dep": ["depA"] },
                "locked": { "type": "path", "path": sub.to_string_lossy() },
                "original": { "type": "path", "path": "./sub" }
            },
            "root": {
                "inputs": {
                    "sub": "sub",
                    "depA": "depA",
                    "depB": "depB"
                }
            }
        },
        "root": "root",
        "version": 7
    })
    .to_string()
}

#[test]
fn transitive_input_honors_root_lock_follows_not_subflake_own_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    build_fixture(&root);

    let result = sui_eval::builtins::evaluate_flake(&root)
        .expect("root flake should evaluate");

    // Navigate outputs.resolved — the marker `sub` resolved for its `dep`.
    let attrs = result.as_attrs().expect("flake result is an attrset");
    let outputs = sui_eval::eval::force_value(
        attrs.get("outputs").expect("outputs present"),
    )
    .unwrap();
    let out_attrs = outputs.as_attrs().expect("outputs is an attrset");
    let resolved = sui_eval::eval::force_value(
        out_attrs.get("resolved").expect("resolved present"),
    )
    .unwrap();
    let marker = resolved.as_string().expect("resolved is a string");

    assert_eq!(
        marker, "A",
        "sub.inputs.dep must resolve via the ROOT lock's follows edge (→ depA, \
         marker \"A\"), NOT sub's own flake.lock (→ depB, marker \"B\"). \
         Got \"{marker}\" — the transitive-input divergence has regressed."
    );
}
