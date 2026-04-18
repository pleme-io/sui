//! Criterion benchmarks for the sui evaluator.
//!
//! Covers parse-only, trivial primitives, moderate lets, foldl'
//! over a list, and a fixpoint through `rec`. Baselines should be
//! captured once on a clean machine and pinned in
//! `sui-eval/benches/BASELINES.md` for regression spotting.
//!
//! Run with: `cargo bench -p sui-eval`.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn bench_parse(c: &mut Criterion) {
    let inputs = [
        ("trivial", "1 + 1"),
        ("let_chain", "let a = 1; b = 2; c = 3; in a + b + c"),
        (
            "nested_attrs",
            "{ a = 1; b = { c = 2; d = { e = 3; f = 4; }; }; }",
        ),
        (
            "long_list",
            "[ 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 ]",
        ),
        (
            "complex_flake",
            r#"{
                description = "test flake";
                outputs = { self, nixpkgs }: {
                    packages.default = nixpkgs.hello;
                    devShells.default = nixpkgs.mkShell { buildInputs = [ nixpkgs.git ]; };
                };
            }"#,
        ),
    ];

    let mut group = c.benchmark_group("parse");
    for (name, input) in &inputs {
        group.bench_function(*name, |b| {
            b.iter(|| {
                let parsed = rnix::Root::parse(black_box(input));
                black_box(parsed);
            });
        });
    }
    group.finish();
}

fn bench_eval(c: &mut Criterion) {
    let inputs = [
        ("arith", "1 + 2 * 3 - 4"),
        ("let_5", "let a = 1; b = a + 1; c = b + 1; d = c + 1; e = d + 1; in e"),
        // Note: Nix `let`-bindings are implicitly recursive — no `rec`
        // keyword needed (that's attrset-only). This originally read
        // `let rec f = …` which is a parse error.
        ("rec_fib_small", "let f = n: if n < 2 then n else f (n - 1) + f (n - 2); in f 10"),
        ("list_map_20", "builtins.map (x: x * x) (builtins.genList (x: x) 20)"),
        (
            "list_foldl_100",
            "builtins.foldl' (acc: x: acc + x) 0 (builtins.genList (x: x) 100)",
        ),
        (
            "attrset_merge",
            "{ a = 1; b = 2; c = 3; } // { b = 20; d = 4; } // { e = 5; }",
        ),
    ];

    let mut group = c.benchmark_group("eval");
    for (name, input) in &inputs {
        group.bench_function(*name, |b| {
            b.iter(|| {
                let result = sui_eval::eval(black_box(input));
                black_box(result.expect("eval ok"));
            });
        });
    }
    group.finish();
}

fn bench_to_json(c: &mut Criterion) {
    // A medium-size evaluated value, then measure Value::to_json
    let value = sui_eval::eval(
        "{ a = 1; b = [ 1 2 3 4 5 ]; c = { d = \"s\"; e = [ { f = 1; } { f = 2; } ]; }; }",
    )
    .unwrap();
    c.bench_function("to_json_medium", |b| {
        b.iter(|| {
            let j = black_box(&value).to_json();
            black_box(j);
        });
    });
}

/// Realistic workloads that stress the interner, overlay cache, and
/// deep attribute access. The microbenches above exercise tiny inputs
/// where cold paths dominate; these are where foundational perf work
/// (interner hot-symbol prewarm, overlay cache fast-path, FxHashMap
/// key lookup) actually shows up.
fn bench_realistic(c: &mut Criterion) {
    // 50 keys + point access to each one. Stresses interner hashmap
    // + NixAttrs::get_sym under many unique identifiers.
    let big_attrset = {
        let mut s = String::from("let attrs = { ");
        for i in 0..50 {
            s.push_str(&format!("key_{i:02} = {i}; "));
        }
        s.push_str("}; in attrs.key_00 + attrs.key_25 + attrs.key_49");
        s
    };

    // Three overlay merges, then full iteration (warms cache), then
    // per-key access. Exercises the overlay `get_sym` fast-path after
    // `as_flat` populates.
    let overlay_then_access = {
        let mut base = String::from("let base = { ");
        for i in 0..20 {
            base.push_str(&format!("a{i:02} = {i}; "));
        }
        base.push_str("}; ");
        let mut overlay = String::from("overlay = { ");
        for i in 20..30 {
            overlay.push_str(&format!("a{i:02} = {i}; "));
        }
        overlay.push_str("}; ");
        let mut final_layer = String::from("final = { ");
        for i in 30..35 {
            final_layer.push_str(&format!("a{i:02} = {i}; "));
        }
        final_layer.push_str("}; ");
        format!(
            "{base}{overlay}{final_layer}merged = base // overlay // final; in \
             builtins.length (builtins.attrNames merged) + merged.a00 + \
             merged.a25 + merged.a32"
        )
    };

    // 100 let-bindings in a chain — simulates module-system style
    // where every binding references the previous. Stresses env
    // HAMT clones + thunk chain.
    let deep_let = {
        let mut s = String::from("let x_00 = 0; ");
        for i in 1..100 {
            s.push_str(&format!("x_{i:02} = x_{:02} + 1; ", i - 1));
        }
        s.push_str("in x_99");
        s
    };

    let mut group = c.benchmark_group("realistic");
    group.bench_function("attrset_50_point_access", |b| {
        b.iter(|| {
            let result = sui_eval::eval(black_box(&big_attrset));
            black_box(result.expect("eval ok"));
        });
    });
    group.bench_function("overlay_merge_iter_access", |b| {
        b.iter(|| {
            let result = sui_eval::eval(black_box(&overlay_then_access));
            black_box(result.expect("eval ok"));
        });
    });
    group.bench_function("deep_let_chain_100", |b| {
        b.iter(|| {
            let result = sui_eval::eval(black_box(&deep_let));
            black_box(result.expect("eval ok"));
        });
    });
    group.finish();
}

criterion_group!(benches, bench_parse, bench_eval, bench_to_json, bench_realistic);
criterion_main!(benches);
