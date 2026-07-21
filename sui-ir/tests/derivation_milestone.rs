//! L3 slice 6 MILESTONE: a real `derivation` / `derivationStrict` drvPath
//! evaluated through `eval_ir`, byte-identical to BOTH the tree-walker
//! (`sui_eval`, the semantic oracle) AND `nix eval …drvPath`.
//!
//! Every `NIX_*` constant below was captured live from **nix 2.34.7**
//! (aarch64-darwin) — a drvPath is a content-address, so it is fixed given the
//! pinned `system` attr (these fixtures pin `system` explicitly, so the bytes
//! are stable regardless of the host). Each case asserts the THREE-WAY match:
//! `eval_ir == tree-walker == nix`. The tree-walker's own suite already proves
//! `tree-walker == nix`; this test adds `eval_ir == nix` and, by transitivity,
//! locks the new engine to the oracle. A byte-match against nix's own drvPath
//! is the only proof — so these are the real bytes, not a re-derived scheme.

use std::rc::Rc;

use sui_ir::eval_ir::{eval_ir, IrEnv, IrValue};
use sui_ir::lower_file;

// Shared two-engine value renderers (the same ones the eval differential uses):
// `render_ir_value` renders an `IrValue`, `render_tree` a walker `Value`, into
// one normalized textual form — context-blind, so a fair value compare.
#[path = "common/render.rs"]
mod render;
use render::{render_ir_value, render_tree};

// ── live-verified nix 2.34.7 byte targets ─────────────────────────────────

// CASE A — the minimal leaf: `derivation { name; system; builder; }`.
const NIX_A_DRV: &str = "/nix/store/qy5dy7076236qbbn67pv5wwh6b2r31xa-hello.drv";
// CASE B — leaf + `args` (the walker's own hard-coded parity assertion).
const NIX_B_DRV: &str = "/nix/store/mypmkciickjnhjjimhzjn6w7qj7g8n2k-hello.drv";
const NIX_B_OUT: &str = "/nix/store/k6lq59b6dilrfy0blhkr10m27ga7ncwr-hello";
// 2-LEVEL — outer references `${inner}` in an env attr: the context edge must
// flow inner.drv into outer's inputDrvs, or outer.drv diverges.
const NIX_INNER_DRV: &str = "/nix/store/ak26j49vaz0cs2ri36g5crz8hwvjvxyf-inner.drv";
const NIX_OUTER_DRV: &str = "/nix/store/79wizwb7lm6ihi0phacdfkxsqwi83wz6-outer.drv";
const NIX_INNER_OUT: &str = "/nix/store/qz0s6n9bsmj0camkaz1pvkgpbw7dhyl2-inner";
// MULTI-OUTPUT producer + a consumer selecting its `.dev`.
const NIX_MO_DRV: &str = "/nix/store/l55zyqbyb1zd1iavh3g7ff64mfl398v2-mo.drv";
const NIX_MO_DEV_OUT: &str = "/nix/store/7qyv76lid0dfnwv4zb45qhfgg5p4sjgk-mo-dev";
const NIX_CONS_DRV: &str = "/nix/store/mp63h3k4k69w2i3cmg10888x238jnbnz-cons.drv";

// ── engine string extractors ──────────────────────────────────────────────

/// Evaluate `src` on `eval_ir`, force to WHNF, require a string, return its
/// chars (context ignored — nix `==`/render are context-blind).
fn ir_str(src: &str) -> String {
    let prog = Rc::new(lower_file(src).unwrap_or_else(|e| panic!("lower failed: {e}\n{src}")));
    let env = IrEnv::with_pure_builtins();
    let v = eval_ir(&prog, prog.root, &env)
        .unwrap_or_else(|e| panic!("eval_ir failed: {e}\n{src}"))
        .force()
        .unwrap_or_else(|e| panic!("force failed: {e}\n{src}"));
    match v {
        IrValue::Str(s, _) => (*s).clone(),
        other => panic!("expected string, got {}\n{src}", other.type_name()),
    }
}

/// The same on the tree-walker (the oracle).
fn walker_str(src: &str) -> String {
    let v = sui_eval::eval(src).unwrap_or_else(|e| panic!("walker eval failed: {e}\n{src}"));
    let f = sui_eval::eval::force_value(&v)
        .unwrap_or_else(|e| panic!("walker force failed: {e}\n{src}"));
    f.to_str()
        .unwrap_or_else(|e| panic!("walker to_str failed: {e}\n{src}"))
}

/// The three-way gate: `eval_ir(src) == tree-walker(src) == nix_bytes`.
#[track_caller]
fn three_way(src: &str, nix_bytes: &str) {
    let ir = ir_str(src);
    let walker = walker_str(src);
    assert_eq!(
        walker, nix_bytes,
        "tree-walker DIVERGES from nix (oracle regression!)\n  src: {src}"
    );
    assert_eq!(
        ir, nix_bytes,
        "eval_ir DIVERGES from nix (and the walker)\n  src: {src}\n  eval_ir: {ir}\n  nix:     {nix_bytes}"
    );
    assert_eq!(ir, walker, "eval_ir and walker disagree\n  src: {src}");
}

// ── the milestone ─────────────────────────────────────────────────────────

#[test]
fn case_a_leaf_drv_path_three_way() {
    three_way(
        "(derivation { name = \"hello\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; }).drvPath",
        NIX_A_DRV,
    );
}

#[test]
fn case_b_leaf_with_args_three_way() {
    let src = "(derivation { name = \"hello\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo hi > $out\" ]; })";
    three_way(&format!("{src}.drvPath"), NIX_B_DRV);
    three_way(&format!("{src}.outPath"), NIX_B_OUT);
}

#[test]
fn derivation_strict_is_derivation() {
    // `derivationStrict` is the alias — same drvPath through both.
    three_way(
        "(derivationStrict { name = \"hello\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; }).drvPath",
        NIX_A_DRV,
    );
}

#[test]
fn two_level_context_flows_into_input_drvs() {
    // The load-bearing context test: `${inner}` in outer's `dep` attr coerces
    // inner's outPath (carrying `Output{inner.drv, out}`), which must land in
    // outer's inputDrvs — or outer.drv diverges from nix. The byte-match IS the
    // proof that string context propagated end-to-end.
    let prelude = "let inner = derivation { name = \"inner\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; }; outer = derivation { name = \"outer\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; dep = \"${inner}/lib\"; }; in ";
    three_way(&format!("{prelude} inner.drvPath"), NIX_INNER_DRV);
    three_way(&format!("{prelude} inner.outPath"), NIX_INNER_OUT);
    three_way(&format!("{prelude} outer.drvPath"), NIX_OUTER_DRV);
}

#[test]
fn multi_output_producer_and_dev_consumer() {
    three_way(
        "(derivation { name = \"mo\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; outputs = [ \"out\" \"dev\" ]; }).drvPath",
        NIX_MO_DRV,
    );
    three_way(
        "(derivation { name = \"mo\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; outputs = [ \"out\" \"dev\" ]; }).dev.outPath",
        NIX_MO_DEV_OUT,
    );
    // Consumer selecting `.dev` of a multi-output producer — the `Output{drv,
    // "dev"}` edge must reach cons's inputDrvs (with output name "dev", not
    // "out").
    let prelude = "let mo = derivation { name = \"mo\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; outputs = [ \"out\" \"dev\" ]; }; cons = derivation { name = \"cons\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; BI = \"${mo.dev}/include\"; }; in ";
    three_way(&format!("{prelude} cons.drvPath"), NIX_CONS_DRV);
}

#[test]
fn string_context_builtins_wired_to_real_context() {
    // hasContext on a plain literal → false; on a derivation output → true.
    assert_eq!(ir_str("if builtins.hasContext \"plain\" then \"T\" else \"F\""), "F");
    assert_eq!(walker_str("if builtins.hasContext \"plain\" then \"T\" else \"F\""), "F");

    let has_ctx = "let d = derivation { name = \"x\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; }; in if builtins.hasContext \"${d}\" then \"T\" else \"F\"";
    assert_eq!(ir_str(has_ctx), "T");
    assert_eq!(walker_str(has_ctx), "T");

    // getContext of `${inner}/lib` is `{ "<inner.drv>" = { outputs = ["out"]; }; }`
    // — render it on both engines and byte-compare (drop-in oracle check).
    let get_ctx = "let inner = derivation { name = \"inner\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; }; in builtins.attrNames (builtins.getContext \"${inner}/lib\")";
    // The single context key is inner's .drv path.
    assert_eq!(
        ir_str(&format!("builtins.head ({get_ctx})")),
        NIX_INNER_DRV
    );
    assert_eq!(
        walker_str(&format!("builtins.head ({get_ctx})")),
        NIX_INNER_DRV
    );

    // unsafeDiscardStringContext strips context but keeps chars.
    let discard = "let d = derivation { name = \"x\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; }; in builtins.hasContext (builtins.unsafeDiscardStringContext \"${d}\")";
    assert_eq!(ir_str(&format!("if {discard} then \"T\" else \"F\"")), "F");
    assert_eq!(walker_str(&format!("if {discard} then \"T\" else \"F\"")), "F");
}

#[test]
fn hash_string_three_way() {
    for (algo, input, nix_hex) in [
        ("sha256", "abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        ("md5", "hello", "5d41402abc4b2a76b9719d911017c592"),
    ] {
        let src = format!("builtins.hashString \"{algo}\" \"{input}\"");
        assert_eq!(ir_str(&src), nix_hex, "eval_ir hashString {algo} diverges");
        assert_eq!(walker_str(&src), nix_hex, "walker hashString {algo} diverges");
    }
}

#[test]
fn convert_hash_three_way() {
    let src = "builtins.convertHash { hash = builtins.hashString \"sha256\" \"abc\"; hashAlgo = \"sha256\"; toHashFormat = \"sri\"; }";
    let nix = "sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=";
    assert_eq!(ir_str(src), nix, "eval_ir convertHash diverges");
    assert_eq!(walker_str(src), nix, "walker convertHash diverges");
}

/// Per-builtin differential (`eval_ir == tree-walker`, the standing ≥2-rows-
/// per-builtin coverage rule) for every builtin slice 6 added/changed. The
/// walker is the oracle; where a case also has a hardcoded nix constant the
/// three-way tests above pin it to nix, so walker == nix is already assured and
/// this locks eval_ir to the walker across a broader surface.
#[test]
fn new_builtins_differential_against_walker() {
    const D: &str =
        "let d = derivation { name = \"d\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; outputs = [ \"out\" \"dev\" ]; }; in ";
    let rows: &[&str] = &[
        // ── derivation / derivationStrict (structure + fields) ──
        "(derivation { name = \"z\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; }).type",
        "(derivation { name = \"z\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; }).outputName",
        "(derivation { name = \"z\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; }).name",
        "(derivationStrict { name = \"z\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; }).drvPath",
        "builtins.length (derivation { name = \"z\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; outputs = [ \"out\" \"dev\" \"lib\" ]; }).all",
        // ── hasContext (plain vs derivation-derived) ──
        "builtins.hasContext \"nope\"",
        &format!("{D} builtins.hasContext \"${{d}}\""),
        &format!("{D} builtins.hasContext \"${{d.dev}}\""),
        // ── getContext (empty vs one output vs concat of two) ──
        "builtins.getContext \"nope\"",
        &format!("{D} builtins.attrNames (builtins.getContext \"${{d.out}}${{d.dev}}\")"),
        &format!("{D} builtins.attrValues (builtins.getContext \"${{d}}\")"),
        // ── unsafeDiscardStringContext (chars kept, context stripped) ──
        "builtins.unsafeDiscardStringContext \"keepme\"",
        &format!("{D} builtins.hasContext (builtins.unsafeDiscardStringContext \"${{d}}\")"),
        // ── hashString (all four algos) ──
        "builtins.hashString \"sha1\" \"the quick brown fox\"",
        "builtins.hashString \"sha512\" \"pleme\"",
        "builtins.hashString \"sha256\" \"\"",
        "builtins.hashString \"md5\" \"\"",
        // ── convertHash (every output format) ──
        "builtins.convertHash { hash = builtins.hashString \"sha256\" \"abc\"; hashAlgo = \"sha256\"; toHashFormat = \"base16\"; }",
        "builtins.convertHash { hash = builtins.hashString \"sha256\" \"abc\"; hashAlgo = \"sha256\"; toHashFormat = \"base64\"; }",
        "builtins.convertHash { hash = builtins.hashString \"sha1\" \"abc\"; hashAlgo = \"sha1\"; toHashFormat = \"nix32\"; }",
        // ── string-context propagation through concat/interp/+ ──
        &format!("{D} builtins.hasContext (\"${{d.out}}/lib\" + \"/x\")"),
        &format!("{D} builtins.hasContext (builtins.concatStringsSep \":\" [ \"${{d.out}}/a\" \"${{d.dev}}/b\" ])"),
    ];
    for src in rows {
        let ir = {
            let prog = Rc::new(lower_file(src).unwrap_or_else(|e| panic!("lower: {e}\n{src}")));
            let env = IrEnv::with_pure_builtins();
            let v = eval_ir(&prog, prog.root, &env)
                .unwrap_or_else(|e| panic!("eval_ir: {e}\n{src}"));
            render_ir_value(&v).unwrap_or_else(|e| panic!("render_ir_value: {e}\n{src}"))
        };
        let walker = {
            let v = sui_eval::eval(src).unwrap_or_else(|e| panic!("walker eval: {e}\n{src}"));
            render_tree(&v).unwrap_or_else(|e| panic!("render_tree: {e}\n{src}"))
        };
        assert_eq!(ir, walker, "eval_ir DIVERGES from the tree-walker\n  src: {src}");
    }
}

/// Regression guard for the string-context representation change: adding the
/// boxed `Option<Rc<IrStringContext>>` to `IrValue::Str` must NOT enlarge
/// `IrValue` (the `Builtin`/`Str` payload already dominated), so the pure
/// force path — which never touches context — cannot regress from the field.
/// 24 bytes = tag + a 16-byte two-word payload on 64-bit.
#[test]
fn ir_value_size_unchanged() {
    assert!(
        std::mem::size_of::<IrValue>() <= 24,
        "IrValue grew to {} bytes — the boxed string context must stay a single \
         word so the force path is unaffected",
        std::mem::size_of::<IrValue>()
    );
}
