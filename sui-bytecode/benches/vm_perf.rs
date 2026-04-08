//! Performance benchmarks for the bytecode VM.
//!
//! Measures compile + execute time for representative Nix expressions.
//! These benchmarks verify that optimizations (string interning, constant
//! folding, superinstructions) actually improve real workloads.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use sui_bytecode::{Compiler, VM};

/// Compile and execute a Nix expression, returning the result.
fn eval(input: &str) -> sui_bytecode::VMValue {
    let (chunk, mut interner) = Compiler::compile(input).unwrap();
    VM::execute(chunk, &mut interner).unwrap()
}

fn bench_arithmetic(c: &mut Criterion) {
    let mut group = c.benchmark_group("arithmetic");

    group.bench_function("constant_folded", |b| {
        b.iter(|| eval(black_box("1 + 2 * 3 - 4")));
    });

    group.bench_function("variable_arithmetic", |b| {
        b.iter(|| eval(black_box("let x = 10; y = 20; in x * y + x - y")));
    });

    group.bench_function("let_chain_5", |b| {
        b.iter(|| {
            eval(black_box(
                "let a = 1; b = 1; c = a + b; d = b + c; e = c + d; in e",
            ))
        });
    });

    group.finish();
}

fn bench_attrset(c: &mut Criterion) {
    let mut group = c.benchmark_group("attrset");

    group.bench_function("construct_3", |b| {
        b.iter(|| eval(black_box("{ a = 1; b = 2; c = 3; }")));
    });

    group.bench_function("select_simple", |b| {
        b.iter(|| eval(black_box("{ a = 1; b = 2; c = 3; }.b")));
    });

    group.bench_function("select_nested", |b| {
        b.iter(|| eval(black_box("{ a = { b = { c = 42; }; }; }.a.b.c")));
    });

    // Tests the GetLocalAttr superinstruction.
    group.bench_function("let_select", |b| {
        b.iter(|| {
            eval(black_box(
                "let set = { x = 10; y = 20; z = 30; }; in set.x + set.y + set.z",
            ))
        });
    });

    group.bench_function("update_merge", |b| {
        b.iter(|| {
            eval(black_box(
                "{ a = 1; b = 2; } // { c = 3; d = 4; }",
            ))
        });
    });

    group.bench_function("has_attr", |b| {
        b.iter(|| eval(black_box("{ a = 1; b = 2; } ? a")));
    });

    group.bench_function("select_or_default", |b| {
        b.iter(|| eval(black_box("{ a = 1; }.missing or 0")));
    });

    group.finish();
}

fn bench_function(c: &mut Criterion) {
    let mut group = c.benchmark_group("function");

    group.bench_function("identity", |b| {
        b.iter(|| eval(black_box("(x: x) 42")));
    });

    // Tests the GetLocalCall superinstruction.
    group.bench_function("let_apply", |b| {
        b.iter(|| eval(black_box("let f = x: x * 2; in f 5")));
    });

    group.bench_function("composition", |b| {
        b.iter(|| {
            eval(black_box(
                "let inc = x: x + 1; double = x: x * 2; in double (inc 3)",
            ))
        });
    });

    group.bench_function("pattern_destructure", |b| {
        b.iter(|| eval(black_box("({ a, b, c ? 10 }: a + b + c) { a = 1; b = 2; }")));
    });

    group.finish();
}

fn bench_compile(c: &mut Criterion) {
    let mut group = c.benchmark_group("compile");

    group.bench_function("trivial", |b| {
        b.iter(|| Compiler::compile(black_box("42")));
    });

    group.bench_function("let_chain", |b| {
        b.iter(|| {
            Compiler::compile(black_box(
                "let a = 1; b = 1; c = a + b; d = b + c; e = c + d; in e",
            ))
        });
    });

    group.bench_function("attrset_select", |b| {
        b.iter(|| {
            Compiler::compile(black_box(
                "let set = { x = 10; y = 20; z = 30; }; in set.x + set.y + set.z",
            ))
        });
    });

    group.bench_function("lambda_apply", |b| {
        b.iter(|| {
            Compiler::compile(black_box(
                "let f = x: x * 2; g = x: x + 1; in f (g 5)",
            ))
        });
    });

    group.finish();
}

fn bench_list(c: &mut Criterion) {
    let mut group = c.benchmark_group("list");

    group.bench_function("construct_5", |b| {
        b.iter(|| eval(black_box("[1 2 3 4 5]")));
    });

    group.bench_function("concat", |b| {
        b.iter(|| eval(black_box("[1 2 3] ++ [4 5 6]")));
    });

    group.finish();
}

fn bench_logic(c: &mut Criterion) {
    let mut group = c.benchmark_group("logic");

    group.bench_function("short_circuit_and", |b| {
        b.iter(|| eval(black_box("let a = true; b = false; in a && b")));
    });

    group.bench_function("short_circuit_or", |b| {
        b.iter(|| eval(black_box("let a = false; b = true; in a || b")));
    });

    group.bench_function("conditional", |b| {
        b.iter(|| eval(black_box("let x = 5; in if x > 3 then x * 2 else x + 1")));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_arithmetic,
    bench_attrset,
    bench_function,
    bench_compile,
    bench_list,
    bench_logic,
);
criterion_main!(benches);
