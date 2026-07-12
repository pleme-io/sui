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

// ── M2.6 ROOT #2: the module-system OVER-FORCE regression ─────────────
// A config binding whose dynamic key depends on ANOTHER module's option
// (`config.homes.${config.pleme.userName}`) must resolve when
// `config.homes` is demanded, and only then. sui used to over-force the
// `homes` definition's dynamic key while merely reading the enclosing
// `config` attrset's WHNF (via `pushDownProperties m.config` during
// definition collection) — the sibling key cppnix never touches when
// `config.pleme.userName` alone is selected. Fixed by making
// `build_tail_attrs_now` resolve one tail level and re-defer the rest.
// Byte-verified 2026-07-11; `diff` asserts sui == nix.
#[test]
fn module_dynamic_key_from_sibling_option_resolves() {
    diff(
        "(lib.evalModules { modules = [ \
         ({ config, lib, ... }: { \
           options.pleme.userName = lib.mkOption { type = lib.types.str; default = \"luis\"; }; \
           options.homes = lib.mkOption { type = lib.types.attrsOf lib.types.int; default = {}; }; \
           config.homes.${config.pleme.userName} = 7; }) \
         ({ ... }: { config.pleme.userName = \"drzzln\"; }) \
         ]; }).config.homes",
    );
}

// Selecting ONLY `config.pleme.userName` must NOT force the sibling
// `homes` definition's dynamic key (KEYFORCE discriminator): a throwing
// key proves the over-force is gone — both engines return "drzzln".
#[test]
fn module_unrelated_select_does_not_force_sibling_dynamic_key() {
    diff(
        "(lib.evalModules { modules = [ \
         ({ config, lib, ... }: { \
           options.pleme.userName = lib.mkOption { type = lib.types.str; default = \"luis\"; }; \
           options.homes = lib.mkOption { type = lib.types.attrsOf lib.types.int; default = {}; }; \
           config.homes.${throw \"KEYFORCE\"} = 7; }) \
         ({ ... }: { config.pleme.userName = \"drzzln\"; }) \
         ]; }).config.pleme.userName",
    );
}

// ── M2.6 ROOT #3: the INTERPOLATED-STRING dynamic-key OVER-FORCE ──────
// ROOT #2 deferred only bare `${e}` tail keys; an INTERPOLATED-STRING
// tail key (`config.environment.etc."iwd/${nm}"`) still fell to the
// eager path and forced `nm` at construction. In the module system that
// forces a `config.<x>` read while `config` is mid-fixpoint (iwd's
// `environment.etc."iwd/${configFile.name}"`, where `configFile` reads
// `with config.networking.networkmanager`), yielding the empty-Promise
// partial → the `set/null` softening. Fixed by teaching
// `attrs_have_dynamic` that an interpolated `Str` attr key is dynamic,
// routing it through the same per-level deferral as `${e}`.
// Byte-verified 2026-07-11; `diff` asserts sui == nix (returns 42).
#[test]
fn module_interpolated_string_dynamic_key_stays_lazy() {
    diff(
        "(lib.evalModules { modules = [ \
         ({ lib, ... }: { \
           options.other.enable = lib.mkOption { type = lib.types.bool; default = true; }; \
           options.probe = lib.mkOption { type = lib.types.int; default = 0; }; \
           config.probe = 42; }) \
         ({ config, lib, ... }: \
           let d = with config.other; lib.optionalAttrs enable { x = 1; }; \
               nm = if d ? x then \"yes\" else \"no\"; \
           in { options.environment.etc = lib.mkOption { type = lib.types.attrs; default = {}; }; \
                config.environment.etc.\"iwd/${nm}\" = { source = 1; }; }) \
         ]; }).config.probe",
    );
}

// ── M2.6 ROOT #3 (collision case): dynamic tail key under a COLLIDING
// head. When a sibling binding already wrote the head (osquery's
// `systemd.services.… = …` then
// `systemd.tmpfiles.settings."10-osquery".${dirname …}.d`), the ROOT
// #1/#2 deferral bailed (head present) and the eager path forced the
// dynamic key at construction → the same empty-Promise partial. Fixed by
// `merge_deferred_dynamic_tail`: descend the existing head along the
// tail's static prefix and splice a deferred thunk at the first dynamic
// level, preserving both laziness AND the static deep-merge.
// Byte-verified 2026-07-11; `diff` asserts sui == nix (returns 42).
#[test]
fn module_dynamic_tail_key_under_colliding_head_stays_lazy() {
    diff(
        "(lib.evalModules { modules = [ \
         ({ lib, ... }: { \
           options.svc.path = lib.mkOption { type = lib.types.str; default = \"/a/b\"; }; \
           options.probe = lib.mkOption { type = lib.types.int; default = 0; }; \
           config.probe = 42; }) \
         ({ config, lib, ... }: { \
           options.sd.services = lib.mkOption { type = lib.types.attrs; default = {}; }; \
           options.sd.tmpfiles = lib.mkOption { type = lib.types.attrs; default = {}; }; \
           config.sd.services.x = { a = 1; }; \
           config.sd.tmpfiles.${builtins.dirOf config.svc.path}.d = { m = 1; }; }) \
         ]; }).config.probe",
    );
}

// The static deep-merge under the colliding head MUST still work: the
// deferred dynamic-tail binding must not clobber the sibling
// `sd.services.x`. Demanding the dynamic branch resolves the key AND the
// sibling static branch remains intact.
#[test]
fn module_dynamic_tail_collision_preserves_static_sibling() {
    diff(
        "(lib.evalModules { modules = [ \
         ({ lib, ... }: { \
           options.svc.path = lib.mkOption { type = lib.types.str; default = \"/a/b\"; }; }) \
         ({ config, lib, ... }: { \
           options.sd.services = lib.mkOption { type = lib.types.attrs; default = {}; }; \
           options.sd.tmpfiles = lib.mkOption { type = lib.types.attrs; default = {}; }; \
           config.sd.services.x = { a = 1; }; \
           config.sd.tmpfiles.${builtins.dirOf config.svc.path}.d = { m = 1; }; }) \
         ]; }).config.sd",
    );
}

// ── M2.6 ROOT #4 (OPEN): the field-lazy `concatLists: got null` frontier ──
// The full `lib.nixosSystem { modules = []; }` diverges at
// `concatLists: expected list, got null` (checkUnmatched → seq →
// merged.unmatchedDefns → mergeModules' recursion → a mkIf's content that
// reads `config.services.<x>` mid-fixpoint → the `in_promise_eval`
// select-miss softening returns `null` where cppnix resolves the field to
// its option default). This is the field-independence gap, NOT a dynamic-
// key over-force — see `docs/repros/root4-concatlists-null.md`. It has NO
// minimal pure-lib reducer (the empty-Promise partial only arises from the
// full base-module `_module.args` fixpoint re-entrance). The tests below
// pin the BOUNDARY: the doRename / mkIf-reading-config shapes that CARRY
// the bug in-situ resolve CORRECTLY in isolation (sui == nix), proving the
// divergence is emergent from the full-set re-entrance and guarding the
// boundary against regression when the field-lazy fix lands.

// A `doRename`/`mkRenamedOptionModule` alias forwards the old option's
// definition to the new option — resolves correctly in isolation.
#[test]
fn module_rename_forwards_definition() {
    diff(
        "(lib.evalModules { modules = [ \
         (lib.mkRenamedOptionModule [ \"old\" \"opt\" ] [ \"new\" \"opt\" ]) \
         ({ lib, ... }: { options.new.opt = lib.mkOption { type = lib.types.str; default = \"def\"; }; }) \
         ({ ... }: { old.opt = \"fromold\"; }) \
         ]; }).config.new.opt",
    );
}

// A `doRename` whose `to` path descends into an `attrsOf submodule` (the
// nixos base-module shape) + selecting an UNRELATED option: the alias's
// `setAttrByPath`/`atDepth` dynamic-head-key is on the force chain but its
// key force is correct — both engines return the unrelated option's value.
#[test]
fn module_rename_into_submodule_unrelated_select() {
    diff(
        "(lib.evalModules { modules = [ \
         (lib.doRename { from = [ \"old\" \"sub\" \"field\" ]; to = [ \"new\" \"sub\" \"field\" ]; \
            visible = false; warn = false; use = x: x; }) \
         ({ lib, ... }: { \
           options.new.sub = lib.mkOption { \
             type = lib.types.submodule { options.field = lib.mkOption { type = lib.types.str; default = \"d\"; }; }; \
             default = {}; }; \
           options.probe = lib.mkOption { type = lib.types.str; default = \"PROBE\"; }; }) \
         ({ ... }: { old.sub.field = \"fromold\"; }) \
         ]; }).config.probe",
    );
}

// A `config = mkIf (config.svc != {}) (with config.svc; { … })` over an
// `attrsOf submodule` option defaulting to `{}` — the borgbackup shape
// whose `with config.services.borgbackup` softens to null in-situ. In
// isolation `config.svc` resolves to its `{}` default; both engines agree.
#[test]
fn module_mkif_config_read_over_attrs_of_submodule() {
    diff(
        "(lib.evalModules { modules = [ \
         ({ config, lib, ... }: { \
           options.repos = lib.mkOption { \
             type = lib.types.attrsOf (lib.types.submodule { options.user = lib.mkOption { type = lib.types.str; default = \"u\"; }; }); \
             default = {}; }; \
           options.users = lib.mkOption { type = lib.types.attrsOf lib.types.anything; default = {}; }; \
           options.probe = lib.mkOption { type = lib.types.str; default = \"PROBE\"; }; \
           config = lib.mkIf (config.repos != {}) (with config.repos; { \
             users = lib.mkMerge (lib.mapAttrsToList (n: c: { \"${c.user}\" = {}; }) config.repos); }); }) \
         ]; }).config.probe",
    );
}

// ── M2.6 ROOT #4a — `with` namespace laziness (the OVER-FORCE) ────────
// nixpkgs' `config = mkIf … (with config.services.X; { … })` module shape:
// demanding the module's `config` WHNF/keys during collection must NOT
// force `config.services.X` (the `with` namespace).  sui used to EVALUATE
// the namespace at `with`-entry, forcing `config.services.X` mid-fixpoint
// → the empty-Promise partial → `concatLists null`.  Fixed by storing the
// namespace as a lazy thunk (`maybe_thunk`), forced only on fallthrough.
// Reduced pure repro (no modules): `attrNames (with (throw "X"); {a=1;})`
// → nix ["a"]; sui (before) → throws.  Here in the module shape both
// engines return the unrelated probe.
// Byte-verified 2026-07-11; `diff` asserts sui == nix.
#[test]
fn module_with_namespace_config_read_stays_lazy() {
    diff(
        "(lib.evalModules { modules = [ \
         ({ config, lib, ... }: { \
           options.svc.enable = lib.mkOption { type = lib.types.bool; default = false; }; \
           options.out = lib.mkOption { type = lib.types.attrsOf lib.types.int; default = {}; }; \
           options.probe = lib.mkOption { type = lib.types.str; default = \"PROBE\"; }; \
           config.out = lib.mkIf config.svc.enable (with config.svc; { x = 1; }); }) \
         ]; }).config.probe",
    );
}

// ── M2.6 ROOT #4b — depth-≥2 option-decl full-set + dotted sibling ────
// nixpkgs' alsa module declares `options.hardware.alsa = { enable = …;
// cardAliases = …; … }` AND `options.hardware.alsa.enablePersistence = …`
// in ONE module; these must DEEP-MERGE.  sui's `merge_nested_insert`
// dropped the full-set leaf (a lazy Thunk) on a Thunk-vs-Attrs collision,
// yielding only {enablePersistence} → `cardAliases` "does not exist".
// Fixed by forcing the existing thunk to WHNF (fields stay lazy) before
// the deep merge.  Here both engines see BOTH option keys.
// Byte-verified 2026-07-11; `diff` asserts sui == nix.
#[test]
fn module_options_fullset_and_dotted_sibling_deep_merge() {
    diff(
        "builtins.filter (n: n == \"enable\" || n == \"extra\") \
           (builtins.attrNames \
             (lib.evalModules { modules = [ \
              ({ lib, ... }: { \
                options.a = { enable = lib.mkOption { type = lib.types.bool; default = false; }; }; \
                options.a.extra = lib.mkOption { type = lib.types.int; default = 0; }; }) \
              ]; }).options.a)",
    );
}
