//! ★ PARITY WITH THE BRIDGES INSTALLED — the blind spot `tests/parity.rs` has.
//!
//! `tests/parity.rs` runs the VM with **no bridge installed**. That is the
//! right shape for the ~171 pure-language cases it covers, and it is exactly
//! why three bridge-dependent divergences lived in the VM undetected: a bug
//! that only appears once sui-eval has wired the VM up is invisible to a
//! bridgeless harness.
//!
//! Every test here installs the REAL production wiring —
//! [`sui_eval::install_vm_bridges`], the single install site
//! `BytecodeEvaluator` itself calls — and then runs the VM through
//! `sui_bytecode::eval_full` **directly**, NOT through `BytecodeEvaluator`.
//! That distinction is load-bearing: `BytecodeEvaluator` falls back to the
//! tree-walker on any VM error, so driving the VM through it would launder a
//! wrong answer into a right one and the test would pass while the VM stayed
//! broken.
//!
//! The three divergences these tests pin, all measured 2026-08-17 against
//! nix 2.31.5 and all SILENT (none raised an error, so the VM's per-file
//! fallback structurally could not catch them):
//!
//! 1. The VM had **no filesystem redirect**: a flake input's
//!    `/nix/store/<narhash>-source` path — which sui never writes to disk —
//!    read as ABSENT. `pathExists` answered `false` on the VM and `true` on
//!    the walker.
//! 2. `is_global_builtin` listed 49 names against nix's real 23, and those
//!    extras resolve ABOVE the `with`-scope lookup, so `with lib; isFunction`
//!    reached the builtin instead of `lib`'s functor-aware redefinition.
//! 3. `builtins ? path` answered `false` while `builtins.path` worked, so
//!    nixpkgs `lib`'s feature detection silently took the other branch.

use sui_bytecode::StringKeyedValue as Skv;

/// Force a tree-walker value and render it as a `StringKeyedValue`.
fn walker_skv(expr: &str) -> Result<Skv, String> {
    let raw = sui_eval::eval(expr).map_err(|e| e.to_string())?;
    let forced = sui_eval::eval::force_value(&raw).map_err(|e| e.to_string())?;
    Ok(sui_eval::eval_to_string_keyed(&forced))
}

/// Run the VM **directly** (no `BytecodeEvaluator` fallback) and render.
fn vm_skv(expr: &str) -> Result<Skv, String> {
    sui_bytecode::eval_full(expr)
        .map(|r| r.to_string_keyed())
        .map_err(|e| e.to_string())
}

/// Assert the VM and the tree-walker agree, with the bridges installed.
///
/// Both sides must SUCCEED — an expression where the VM errors and falls back
/// is a different (louder, self-healing) failure mode than the silent wrong
/// answers this file exists to pin, so it is not accepted as agreement.
fn assert_bridged_parity(expr: &str, expected: &Skv) {
    let walker = walker_skv(expr)
        .unwrap_or_else(|e| panic!("tree-walker failed for `{expr}`: {e}"));
    let vm = vm_skv(expr).unwrap_or_else(|e| panic!("bytecode VM failed for `{expr}`: {e}"));
    assert_eq!(
        &walker, expected,
        "the ORACLE moved for `{expr}` — fix the expectation, not the VM"
    );
    assert_eq!(
        vm, walker,
        "bridged parity mismatch for `{expr}`\n  tree-walker => {walker:?}\n  bytecode VM => {vm:?}"
    );
}

// ── FIX 1: the filesystem redirect ─────────────────────────────────────
//
// Mirror of the tree-walker's own oracle
// (`sui-eval/src/builtins/tests.rs::fs_builtins_agree_on_a_registered_input_source_path`),
// lifted to the VM side. The walker's version proved the walker materializes;
// this one proves the VM does too, and that both land on the same bytes.

#[test]
fn fs_builtins_agree_with_the_walker_on_a_registered_input_source() {
    let _bridges = sui_eval::install_vm_bridges();

    let real = std::env::temp_dir().join("sui_vm_bridge_parity_fs");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("Cargo.lock"), "hello").unwrap();
    std::fs::write(real.join("mod.nix"), "{ answer = 42; }").unwrap();

    // The store name is deliberately absent from disk — that is the point.
    let store = "/nix/store/1111111111111111111111111111111-source";
    assert!(
        !std::path::Path::new(store).exists(),
        "the fixture store path must NOT exist on disk"
    );
    sui_eval::path::register_input_source(std::path::Path::new(store), &real);

    // pathExists is the SILENT one: it answered `false` on the VM and `true`
    // on the walker, and `false` is a legal answer, so nothing errored.
    assert_bridged_parity(
        &format!(r#"builtins.pathExists "{store}/Cargo.lock""#),
        &Skv::Bool(true),
    );
    assert_bridged_parity(
        &format!(r#"builtins.pathExists "{store}/does-not-exist""#),
        &Skv::Bool(false),
    );
    assert_bridged_parity(
        &format!(r#"builtins.readFile "{store}/Cargo.lock""#),
        &Skv::String("hello".to_string()),
    );
    assert_bridged_parity(
        &format!(r#"builtins.readFileType "{store}/Cargo.lock""#),
        &Skv::String("regular".to_string()),
    );
    assert_bridged_parity(
        &format!(r#"builtins.readFileType "{store}""#),
        &Skv::String("directory".to_string()),
    );

    // `import` matters as much as the guards: redirecting `pathExists` while
    // leaving `import` bare reproduces the guard-passes-then-read-ENOENTs
    // shape that broke every fleet rebuild through `hashFile`.
    assert_bridged_parity(
        &format!(r#"(import {store}/mod.nix).answer"#),
        &Skv::Int(42),
    );

    // hashFile is bridge-dispatched, so it was already correct — it is the
    // calibration that says a green row here means agreement, not absence.
    assert_bridged_parity(
        &format!(r#"builtins.hashFile "sha256" "{store}/Cargo.lock""#),
        &Skv::String(
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string(),
        ),
    );

    std::fs::remove_dir_all(&real).ok();
}

/// Anti-vacuity: prove the fixture is not accidentally passing because the
/// store path happens to exist. With NO input source registered, the same
/// store path must read as absent on BOTH engines.
#[test]
fn an_unregistered_store_path_reads_as_absent_on_both_engines() {
    let _bridges = sui_eval::install_vm_bridges();
    let store = "/nix/store/2222222222222222222222222222222-source";
    assert!(!std::path::Path::new(store).exists());
    assert_bridged_parity(
        &format!(r#"builtins.pathExists "{store}/Cargo.lock""#),
        &Skv::Bool(false),
    );
}

// ── FIX 2: the global-builtin list vs `with` ───────────────────────────

#[test]
fn with_scope_shadows_every_name_that_is_not_a_real_nix_global() {
    let _bridges = sui_eval::install_vm_bridges();

    // nixpkgs-shaped: `with lib;` is everywhere and `lib.isFunction` /
    // `lib.functionArgs` are functor-aware REDEFINITIONS of the same-named
    // builtins. Each of these resolved to the BUILTIN on the VM before the
    // list was trimmed to nix's measured global scope.
    for name in [
        "isFunction",
        "typeOf",
        "functionArgs",
        "pathExists",
        "readFile",
        "readDir",
        "isAttrs",
        "isList",
        "isString",
        "isInt",
        "isBool",
        "isFloat",
        "isPath",
        "seq",
        "deepSeq",
        "tryEval",
        "trace",
        "toFile",
        "toPath",
        "fromJSON",
        "toJSON",
        "genericClosure",
        "addErrorContext",
        "unsafeGetAttrPos",
        "storeDir",
        "nixVersion",
        "nixPath",
        "currentSystem",
        "currentTime",
        "langVersion",
    ] {
        assert_bridged_parity(
            &format!(r#"with {{ {name} = x: "LIB"; }}; {name} 1"#),
            &Skv::String("LIB".to_string()),
        );
    }
}

/// The other direction, and the reason the list cannot simply be emptied: in
/// CppNix the base environment is the outermost LEXICAL scope, so a genuine
/// global SHADOWS a `with`. All 19 names that survived the trim must keep
/// doing so.
///
/// ★ THE ORACLE HERE IS NIX, NOT THE TREE-WALKER. Measured 2026-08-17 with
/// `nix eval --impure --expr 'with { <n> = "LIB"; }; <n> == "LIB"'` against
/// nix 2.31.5: all 19 answer `false`. The tree-walker answers `false` for 18
/// of them and `true` for **`break`** — a tree-walker divergence from nix that
/// lives in `sui-eval`'s ident resolution, NOT in the VM (the VM answers
/// `false`, i.e. correctly). Comparing the VM to the walker here would
/// therefore pin the wrong thing, so this test asserts the VM against nix's
/// measured answer and records the walker's `break` gap rather than encoding
/// it.
#[test]
fn a_real_global_still_wins_over_a_with_scope() {
    let _bridges = sui_eval::install_vm_bridges();

    for name in [
        "abort",
        "baseNameOf",
        "break",
        "derivation",
        "derivationStrict",
        "dirOf",
        "fetchGit",
        "fetchMercurial",
        "fetchTarball",
        "fetchTree",
        "fromTOML",
        "import",
        "isNull",
        "map",
        "placeholder",
        "removeAttrs",
        "scopedImport",
        "throw",
        "toString",
    ] {
        // Comparing the resolved value against the string the `with` binds is
        // what tells us WHICH one was reached: `false` means the global won.
        let expr = format!(r#"with {{ {name} = "LIB"; }}; {name} == "LIB""#);
        let vm = vm_skv(&expr).unwrap_or_else(|e| panic!("bytecode VM failed for `{expr}`: {e}"));
        assert_eq!(
            vm,
            Skv::Bool(false),
            "`{name}` is a real nix global — a `with` must not shadow it (nix: false)"
        );
    }
}

#[test]
fn a_bare_non_global_builtin_is_rejected_the_way_nix_rejects_it() {
    let _bridges = sui_eval::install_vm_bridges();

    // Second-order consequence of the over-long list: nix ERRORS on a bare
    // `typeOf` (`undefined variable`), and the VM silently succeeded —
    // swallowing a genuine undefined-variable bug in whatever it evaluated.
    for expr in ["typeOf 1", "isFunction 1", "pathExists \"/tmp\"", "storeDir"] {
        assert!(
            walker_skv(expr).is_err(),
            "the ORACLE moved: `{expr}` must be an undefined variable"
        );
        assert!(
            vm_skv(expr).is_err(),
            "`{expr}` must not resolve on the VM — nix has no such global"
        );
    }
}

// ── FIX 3: the builtins attrset advertises what it dispatches ──────────

#[test]
fn builtins_attrset_advertises_every_name_the_vm_can_dispatch() {
    let _bridges = sui_eval::install_vm_bridges();

    // `builtins.path` / `builtins.builtins` / `builtins.parseFlakeRef` /
    // `builtins.flakeRefToString` are dispatched by name in
    // `VM::try_vm_builtin`, but were absent from the attrset
    // `make_builtins_attrset` builds — which is what `?` answers from. So
    // feature detection LIED, and nixpkgs `lib` gates on exactly this shape.
    for name in [
        "builtins",
        "path",
        "parseFlakeRef",
        "flakeRefToString",
        // already correct — the calibration half of the row
        "readDir",
        "hashFile",
        "toFile",
        "convertHash",
        "filterSource",
        "toXML",
        "getEnv",
        "genericClosure",
        "currentSystem",
        "nixVersion",
        "storeDir",
    ] {
        assert_bridged_parity(&format!("builtins ? {name}"), &Skv::Bool(true));
    }

    // Anti-vacuity: `?` must still answer false for a name nix does not have,
    // or the row above would pass on a `builtins` that says yes to everything.
    for name in ["definitelyNotABuiltin", "isFunctionButNamedWrong"] {
        assert_bridged_parity(&format!("builtins ? {name}"), &Skv::Bool(false));
    }
}

#[test]
fn builtins_builtins_is_the_builtins_attrset() {
    let _bridges = sui_eval::install_vm_bridges();
    // One level deep is what the VM provides (nix's is infinitely
    // self-referential; the VM rebuilds `builtins` eagerly per reference, so a
    // cyclic value would not terminate). One level is what `lib` probes.
    //
    // Probed with `?` and with a value that is not version-dependent:
    // `builtins.nixVersion` is a PRE-EXISTING VM/walker divergence unrelated
    // to this change (walker `"2.34.7"`, VM `"2.24.0"`), so asserting on it
    // here would couple this test to a defect it does not own.
    assert_bridged_parity("builtins.builtins ? getEnv", &Skv::Bool(true));
    assert_bridged_parity("builtins.builtins ? path", &Skv::Bool(true));
    assert_bridged_parity("builtins.builtins.storeDir", &Skv::String("/nix/store".into()));
    assert_bridged_parity("builtins.builtins.langVersion", &Skv::Int(6));
}
