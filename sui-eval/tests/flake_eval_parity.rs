//! Layer 8: flake evaluation parity.
//!
//! Narrow scope (per plan): **path-only** flakes with no network
//! inputs. For each fixture, evaluate specific output selectors
//! through both sui (`builtins.getFlake "<path>".x.y.z`) and real
//! `nix eval --json --no-write-lock-file "path:<path>#x.y.z"` and
//! assert the JSON matches.
//!
//! The `common::assert_eq_nix` helper isn't quite right here —
//! flakes need the `nix eval` command (not `nix-instantiate`) and a
//! `path:` reference — so this file builds its own thin oracle
//! wrapper. Everything still gates on `SUI_TEST_ONLINE=1`.
//!
//! Current corpus:
//!   - tests/fixtures/flakes/minimal/flake.nix (no inputs,
//!     hand-crafted to have deterministic scalar/list/attr outputs)
//!
//! Real pleme-io flakes deliberately stay out of this layer for
//! now — they all pull nixpkgs/substrate/blackmatter via network
//! inputs, which needs fetchTree + network. Add them once that
//! infrastructure lands.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/flakes/minimal");
    p
}

/// Evaluate `<fixture>#<selector>` via real nix, returning JSON.
/// Returns an `__error` JSON wrapper on failure.
fn nix_flake_eval(fixture: &Path, selector: &str) -> serde_json::Value {
    let ref_str = format!("path:{}#{selector}", fixture.display());
    let out = Command::new("nix")
        .args([
            "eval",
            "--json",
            "--no-write-lock-file",
            "--extra-experimental-features",
            "nix-command flakes",
            &ref_str,
        ])
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => return wrap_error(format!("spawn nix: {e}")),
    };
    if !out.status.success() {
        return wrap_error(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(e) => wrap_error(format!("nix stdout not json: {e}: {stdout}")),
    }
}

/// Evaluate `(builtins.getFlake "<path>").<selector>` through sui
/// and return JSON. `selector` uses dot syntax (`a.b.c`); we emit
/// the expression as `(getFlake ...).a.b.c`.
fn sui_flake_eval(fixture: &Path, selector: &str) -> serde_json::Value {
    let expr = format!(
        r#"(builtins.getFlake "{}").{selector}"#,
        fixture.display()
    );
    common::sui_eval_json(&expr)
}

fn wrap_error(msg: String) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    m.insert("__error".to_string(), serde_json::Value::String(msg));
    serde_json::Value::Object(m)
}

/// Differential assertion on a single flake selector.
fn assert_flake_parity(fixture: &Path, selector: &str) {
    if common::skip_if_offline("flake_parity") {
        return;
    }
    let oracle = nix_flake_eval(fixture, selector);
    let ours = sui_flake_eval(fixture, selector);
    if oracle != ours {
        panic!(
            "flake parity mismatch on {} # {selector}\n  nix: {}\n  sui: {}",
            fixture.display(),
            serde_json::to_string(&oracle).unwrap_or_default(),
            serde_json::to_string(&ours).unwrap_or_default(),
        );
    }
}

// ── Minimal fixture: scalar outputs ─────────────────────────────────

#[test]
fn minimal_flake_answer() {
    assert_flake_parity(&fixture_dir(), "answer");
}

#[test]
fn minimal_flake_greeting() {
    assert_flake_parity(&fixture_dir(), "greeting");
}

#[test]
fn minimal_flake_numbers() {
    assert_flake_parity(&fixture_dir(), "numbers");
}

#[test]
fn minimal_flake_nested() {
    assert_flake_parity(&fixture_dir(), "nested");
}

#[test]
fn minimal_flake_nested_deeper() {
    assert_flake_parity(&fixture_dir(), "nested.b.c");
}

// ── Flake metadata checks ────────────────────────────────────────────

/// `nix flake metadata --json --no-write-lock-file` reports the
/// description. sui exposes `(getFlake ...).description` — assert
/// both agree.
#[test]
fn minimal_flake_description_matches() {
    if common::skip_if_offline("minimal_flake_description_matches") {
        return;
    }
    let dir = fixture_dir();

    // Real nix: `nix flake metadata --json`
    let ref_str = format!("path:{}", dir.display());
    let out = Command::new("nix")
        .args([
            "flake",
            "metadata",
            "--json",
            "--no-write-lock-file",
            "--extra-experimental-features",
            "nix-command flakes",
            &ref_str,
        ])
        .output()
        .expect("spawn nix flake metadata");
    assert!(
        out.status.success(),
        "nix flake metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let nix_desc = meta.get("description").and_then(|v| v.as_str()).unwrap();

    // sui: (getFlake path).description
    let sui_desc = sui_flake_eval(&dir, "description");

    assert_eq!(
        sui_desc,
        serde_json::Value::String(nix_desc.to_string()),
        "description mismatch: nix={:?}, sui={sui_desc}",
        nix_desc
    );
}
