//! Layer 5: differential eval — nixpkgs `lib.*` snippets.
//!
//! **Currently ignored** — sui-eval is blocked on several gaps that
//! prevent `import <nixpkgs>` / `import /path/to/nixpkgs {}` from
//! evaluating:
//!
//!   Gap A — `<name>` search-path syntax is not plumbed to
//!           `findFile` / `NIX_PATH` in sui.
//!   Gap B — `import` on a directory does not fall back to that
//!           directory's `default.nix`; sui errors with "Is a
//!           directory".
//!   Gap C — Bare identifiers like `map`, `filter`, `null` that
//!           nixpkgs' `lib/` relies on (they come from the implicit
//!           `with builtins;` scope at the top of `default.nix`)
//!           are undefined in sui. Every nixpkgs lib function
//!           cascades through this.
//!
//! Once any one of these is fixed, the corresponding test below
//! should start passing with real nix on the current machine — just
//! remove the `#[ignore]` annotation to re-enable it.
//!
//! Each test uses the exact same nixpkgs path on both sides (sui
//! and real nix) so the comparison is deterministic across machines
//! that share a store.
//!
//! **To run this layer anyway:** `SUI_TEST_ONLINE=1 cargo test -p
//! sui-eval --test diff_eval_nixpkgs_lib -- --ignored`.

mod common;

use std::path::PathBuf;

/// Resolve `<nixpkgs>` via real nix so both sides use the same path.
/// Returns `None` if real nix can't resolve it.
fn resolve_nixpkgs_path() -> Option<PathBuf> {
    let out = std::process::Command::new("nix-instantiate")
        .args(["--eval", "--expr", "<nixpkgs>"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // Output is `/nix/store/...-source\n` with surrounding quotes
    // stripped from `--eval` output — strip any trailing whitespace.
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    Some(PathBuf::from(raw))
}

/// Build a `let lib = import <nixpkgs>/lib; in <body>` expression
/// pinned to the machine's nixpkgs path so real nix and sui see the
/// same source tree. We import `<nixpkgs/lib>` directly rather than
/// `(import <nixpkgs> {}).lib` to avoid the heavyweight pkgs eval.
fn lib_expr(body: &str) -> Option<String> {
    let p = resolve_nixpkgs_path()?;
    Some(format!(
        "let lib = import {}/lib; in {body}",
        p.display()
    ))
}

fn diff(body: &str) {
    if common::skip_if_offline("diff_nixpkgs_lib") {
        return;
    }
    let Some(expr) = lib_expr(body) else {
        eprintln!("skip: cannot resolve <nixpkgs> via real nix");
        return;
    };
    common::assert_eq_nix(&expr);
}

// ── lib.lists ────────────────────────────────────────────────────────

#[test]
fn lib_lists_length() {
    diff("lib.lists.length [ 1 2 3 ]");
}

#[test]
fn lib_lists_fold() {
    diff("lib.lists.fold (a: b: a + b) 0 [ 1 2 3 4 ]");
}

#[test]
fn lib_lists_foldl_prime() {
    diff("lib.lists.foldl' (acc: x: acc + x) 0 [ 1 2 3 4 ]");
}

#[test]
fn lib_lists_unique() {
    diff("lib.lists.unique [ 1 2 3 2 1 ]");
}

#[test]
fn lib_lists_flatten() {
    diff("lib.lists.flatten [ 1 [ 2 [ 3 4 ] ] 5 ]");
}

// ── lib.strings ──────────────────────────────────────────────────────

#[test]
fn lib_strings_concat_strings() {
    diff(r#"lib.strings.concatStrings [ "a" "b" "c" ]"#);
}

#[test]
fn lib_strings_split_string() {
    diff(r#"lib.strings.splitString "," "a,b,c,d""#);
}

#[test]
fn lib_strings_has_prefix() {
    diff(r#"lib.strings.hasPrefix "abc" "abcdef""#);
}

#[test]
fn lib_strings_has_suffix() {
    diff(r#"lib.strings.hasSuffix "def" "abcdef""#);
}

// ── lib.attrsets ─────────────────────────────────────────────────────

#[test]
fn lib_attrsets_filter_attrs() {
    diff("lib.attrsets.filterAttrs (n: v: v > 1) { a = 1; b = 2; c = 3; }");
}

#[test]
fn lib_attrsets_map_attrs_prime() {
    diff(r#"lib.attrsets.mapAttrs' (n: v: { name = n + "!"; value = v + 1; }) { a = 1; b = 2; }"#);
}

#[test]
fn lib_attrsets_recursive_update() {
    diff(
        r#"lib.attrsets.recursiveUpdate
            { a = { b = 1; c = 2; }; d = 3; }
            { a = { b = 10; e = 20; }; f = 30; }"#,
    );
}

// ── lib.trivial ──────────────────────────────────────────────────────

#[test]
fn lib_trivial_pipe() {
    diff("lib.trivial.pipe 3 [ (x: x + 1) (x: x * 2) (x: x - 5) ]");
}

#[test]
fn lib_trivial_id() {
    diff("lib.trivial.id 42");
}

#[test]
fn lib_trivial_flip() {
    diff("(lib.trivial.flip (a: b: [ a b ])) 1 2");
}

// ── lib.versions ─────────────────────────────────────────────────────

#[test]
fn lib_versions_major() {
    diff(r#"lib.versions.major "1.2.3""#);
}

#[test]
fn lib_versions_split_version() {
    diff(r#"lib.versions.splitVersion "1.2.3""#);
}
