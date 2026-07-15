//! Seal: a LOCKED flake input's OWN `self` carries its `rev`/`shortRev`/
//! `lastModified`, exactly as CppNix populates a fetched input's sourceInfo.
//!
//! Offline, deterministic regression seal for GATE 1's marquee darwin root
//! (2026-07-15): nix-darwin's `flake.nix` derives the system label from its
//! OWN self —
//!
//!     system.darwinVersionSuffix = ".${self.shortRev or self.dirtyShortRev or "dirty"}";
//!     system.darwinRevision      = self.rev or self.dirtyRev or null;
//!
//! sui populated `rev`/`shortRev` on the PARENT's view of the input, but the
//! `self` passed to the input flake's OWN `outputs` (built by the recursive
//! `evaluate_flake_inner` from `nar_hash_source_tree`) carried only
//! outPath/narHash — never the locked node's rev.  So nix-darwin's
//! `self.shortRev` was unset and it fell to "dirty":
//!
//!     sui:  darwin-system-25.11.dirty     nix:  darwin-system-25.11.ebec37a
//!
//! The fixture reproduces the exact shape with a pure `type = "path"` input
//! (so it runs offline) whose ROOT-lock node carries a synthetic `rev` +
//! `lastModified`.  The rev-injection code path in `evaluate_flake_inner` is
//! identical regardless of how the input was fetched (path vs git), so this
//! faithfully seals the injection without a network fetch:
//!
//!   root
//!    └─ inputs.child → node `child` (locked.rev = REV, locked.lastModified = LM)
//!
//!   child/flake.nix reads its OWN `self.rev` / `self.shortRev` and re-exposes
//!   them; root surfaces them so the test can assert child's self carried the
//!   locked rev.

use std::path::Path;

const REV: &str = "ebec37af1821deadbeefcafe0123456789abcdef";
const SHORT_REV: &str = "ebec37a"; // first 7 chars of REV
const LAST_MODIFIED: i64 = 1_700_000_000;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn build_fixture(root_dir: &Path) {
    // The child input flake reads its OWN self-rev — the nix-darwin shape.
    let child = root_dir.join("child");
    write(
        &child.join("flake.nix"),
        r#"{
  description = "child reads its own self-rev";
  outputs = { self }: {
    selfRev = self.rev or "NONE";
    selfShortRev = self.shortRev or "NONE";
    selfLastModified = self.lastModified or (-1);
  };
}"#,
    );

    // Root consumes child, surfaces child's OWN self-rev values.
    write(
        &root_dir.join("flake.nix"),
        r#"{
  description = "root";
  inputs.child.url = "path:./child";
  outputs = { self, child }: {
    childRev = child.selfRev;
    childShortRev = child.selfShortRev;
    childLastModified = child.selfLastModified;
  };
}"#,
    );

    // Root lock: the `child` node's locked info carries a rev + lastModified.
    // (type = "path" keeps it offline; the rev-injection path is identical to
    // a git/github input.)
    write(&root_dir.join("flake.lock"), &root_lock(&child));
}

fn root_lock(child: &Path) -> String {
    serde_json::json!({
        "nodes": {
            "child": {
                "flake": true,
                "locked": {
                    "type": "path",
                    "path": child.to_string_lossy(),
                    "rev": REV,
                    "lastModified": LAST_MODIFIED,
                    "narHash": "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                },
                "original": { "type": "path", "path": "./child" }
            },
            "root": { "inputs": { "child": "child" } }
        },
        "root": "root",
        "version": 7
    })
    .to_string()
}

fn out_str(result: &sui_eval::value::Value, key: &str) -> String {
    let attrs = result.as_attrs().expect("flake result is an attrset");
    let outputs =
        sui_eval::eval::force_value(attrs.get("outputs").expect("outputs present")).unwrap();
    let out_attrs = outputs.as_attrs().expect("outputs is an attrset");
    let v = sui_eval::eval::force_value(out_attrs.get(key).unwrap_or_else(|| panic!("{key} present")))
        .unwrap();
    v.as_string().expect("string output").to_string()
}

#[test]
fn locked_input_self_carries_its_own_rev_and_short_rev() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    build_fixture(&root);

    let result =
        sui_eval::builtins::evaluate_flake(&root).expect("root flake should evaluate");

    let child_rev = out_str(&result, "childRev");
    let child_short_rev = out_str(&result, "childShortRev");

    assert_eq!(
        child_rev, REV,
        "a locked input flake's OWN self.rev must equal its locked node's rev. \
         Got \"{child_rev}\" — the self-rev injection has regressed (nix-darwin's \
         darwinRevision would fall back to null)."
    );
    assert_eq!(
        child_short_rev, SHORT_REV,
        "a locked input flake's OWN self.shortRev must equal the first 7 chars of \
         its locked rev. Got \"{child_short_rev}\" — regressed (nix-darwin's \
         darwinVersionSuffix would fall back to \"dirty\", diverging the toplevel \
         drvPath)."
    );
}
