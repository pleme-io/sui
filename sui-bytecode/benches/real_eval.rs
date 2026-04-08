//! Head-to-head benchmarks: tree-walker (sui-eval) vs bytecode VM (sui-bytecode).
//!
//! Each benchmark runs the same Nix expression through both backends so we can
//! directly compare throughput. Expressions are chosen to exercise the most
//! common patterns in real nixpkgs evaluation.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

// ── Tree-walker helpers ──────────────────────────────────────────

fn tw_eval(input: &str) -> sui_eval::Value {
    sui_eval::eval(input).unwrap()
}

// ── Bytecode VM helpers ──────────────────────────────────────────

fn bc_eval(input: &str) -> sui_bytecode::VMValue {
    sui_bytecode::eval(input).unwrap()
}

// We also want to measure compile-only and execute-only costs.
fn bc_compile(input: &str) -> (sui_bytecode::Chunk, sui_bytecode::Interner) {
    sui_bytecode::Compiler::compile(input).unwrap()
}

fn bc_execute(chunk: sui_bytecode::Chunk, interner: &mut sui_bytecode::Interner) -> sui_bytecode::VMValue {
    sui_bytecode::VM::execute(chunk, interner).unwrap()
}

// ── Benchmark expressions ────────────────────────────────────────

const ARITHMETIC: &str = "1 + 2 * 3";

const LET_CHAIN_10: &str =
    "let a = 1; b = a + 1; c = b + 1; d = c + 1; e = d + 1; f = e + 1; g = f + 1; h = g + 1; i = h + 1; j = i + 1; in j";

const APPLY_CHAIN: &str = "let f = x: x + 1; g = x: f (f (f x)); in g 0";

const REC_ATTRSET: &str = "rec { a = 1; b = a + 1; c = b + a; }";

const WITH_LOOKUP: &str = "with { x = 1; y = 2; z = 3; }; x + y + z";

const FIXPOINT: &str =
    "let fix = f: let x = f x; in x; in (fix (self: { a = 1; b = self.a + 1; })).b";

const PATTERN_DESTR: &str = "({ a, b, c ? 10 }: a + b + c) { a = 1; b = 2; }";

const NESTED_SELECT: &str = "{ a = { b = { c = 42; }; }; }.a.b.c";

const LIST_CONSTRUCT_20: &str = "[1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20]";

const LIST_CONCAT: &str = "[1 2 3 4 5] ++ [6 7 8 9 10]";

const UPDATE_ATTRS: &str = "{ a = 1; b = 2; c = 3; } // { d = 4; e = 5; f = 6; }";

const IF_THEN_ELSE: &str = "let x = 5; in if x > 3 then x * 2 else x + 1";

const STRING_INTERP: &str = r#"let name = "world"; in "hello ${name}""#;

// ── Dynamically-generated large expressions ──────────────────────

fn attrset_n(n: usize) -> String {
    let fields: Vec<String> = (0..n).map(|i| format!("a{i} = {i};")).collect();
    format!(
        "let s = {{ {} }}; in s.a{}",
        fields.join(" "),
        n / 2
    )
}

fn let_chain_n(n: usize) -> String {
    let mut parts = vec!["let a0 = 1".to_string()];
    for i in 1..n {
        parts.push(format!("a{i} = a{} + 1", i - 1));
    }
    parts.push(format!("in a{}", n - 1));
    parts.join("; ")
}

fn nested_calls_n(n: usize) -> String {
    // let f = x: x + 1; in f (f (f ... (f 0)))
    let mut expr = "0".to_string();
    for _ in 0..n {
        expr = format!("f ({expr})");
    }
    format!("let f = x: x + 1; in {expr}")
}

// ── Benchmark groups ─────────────────────────────────────────────

fn bench_tree_walker(c: &mut Criterion) {
    let mut group = c.benchmark_group("tree_walker");

    group.bench_function("arithmetic", |b| {
        b.iter(|| tw_eval(black_box(ARITHMETIC)))
    });

    group.bench_function("let_chain_10", |b| {
        b.iter(|| tw_eval(black_box(LET_CHAIN_10)))
    });

    let attrset_100 = attrset_n(100);
    group.bench_function("attrset_100", |b| {
        b.iter(|| tw_eval(black_box(&attrset_100)))
    });

    group.bench_function("apply_chain", |b| {
        b.iter(|| tw_eval(black_box(APPLY_CHAIN)))
    });

    group.bench_function("rec_attrset", |b| {
        b.iter(|| tw_eval(black_box(REC_ATTRSET)))
    });

    group.bench_function("with_lookup", |b| {
        b.iter(|| tw_eval(black_box(WITH_LOOKUP)))
    });

    group.bench_function("fixpoint", |b| {
        b.iter(|| tw_eval(black_box(FIXPOINT)))
    });

    group.bench_function("pattern_destr", |b| {
        b.iter(|| tw_eval(black_box(PATTERN_DESTR)))
    });

    group.bench_function("nested_select", |b| {
        b.iter(|| tw_eval(black_box(NESTED_SELECT)))
    });

    group.bench_function("list_construct_20", |b| {
        b.iter(|| tw_eval(black_box(LIST_CONSTRUCT_20)))
    });

    group.bench_function("list_concat", |b| {
        b.iter(|| tw_eval(black_box(LIST_CONCAT)))
    });

    group.bench_function("update_attrs", |b| {
        b.iter(|| tw_eval(black_box(UPDATE_ATTRS)))
    });

    group.bench_function("if_then_else", |b| {
        b.iter(|| tw_eval(black_box(IF_THEN_ELSE)))
    });

    group.bench_function("string_interp", |b| {
        b.iter(|| tw_eval(black_box(STRING_INTERP)))
    });

    let let_chain_50 = let_chain_n(50);
    group.bench_function("let_chain_50", |b| {
        b.iter(|| tw_eval(black_box(&let_chain_50)))
    });

    let nested_calls_10 = nested_calls_n(10);
    group.bench_function("nested_calls_10", |b| {
        b.iter(|| tw_eval(black_box(&nested_calls_10)))
    });

    group.finish();
}

fn bench_bytecode_vm(c: &mut Criterion) {
    let mut group = c.benchmark_group("bytecode_vm");

    group.bench_function("arithmetic", |b| {
        b.iter(|| bc_eval(black_box(ARITHMETIC)))
    });

    group.bench_function("let_chain_10", |b| {
        b.iter(|| bc_eval(black_box(LET_CHAIN_10)))
    });

    let attrset_100 = attrset_n(100);
    group.bench_function("attrset_100", |b| {
        b.iter(|| bc_eval(black_box(&attrset_100)))
    });

    group.bench_function("apply_chain", |b| {
        b.iter(|| bc_eval(black_box(APPLY_CHAIN)))
    });

    group.bench_function("rec_attrset", |b| {
        b.iter(|| bc_eval(black_box(REC_ATTRSET)))
    });

    group.bench_function("with_lookup", |b| {
        b.iter(|| bc_eval(black_box(WITH_LOOKUP)))
    });

    group.bench_function("fixpoint", |b| {
        b.iter(|| bc_eval(black_box(FIXPOINT)))
    });

    group.bench_function("pattern_destr", |b| {
        b.iter(|| bc_eval(black_box(PATTERN_DESTR)))
    });

    group.bench_function("nested_select", |b| {
        b.iter(|| bc_eval(black_box(NESTED_SELECT)))
    });

    group.bench_function("list_construct_20", |b| {
        b.iter(|| bc_eval(black_box(LIST_CONSTRUCT_20)))
    });

    group.bench_function("list_concat", |b| {
        b.iter(|| bc_eval(black_box(LIST_CONCAT)))
    });

    group.bench_function("update_attrs", |b| {
        b.iter(|| bc_eval(black_box(UPDATE_ATTRS)))
    });

    group.bench_function("if_then_else", |b| {
        b.iter(|| bc_eval(black_box(IF_THEN_ELSE)))
    });

    group.bench_function("string_interp", |b| {
        b.iter(|| bc_eval(black_box(STRING_INTERP)))
    });

    let let_chain_50 = let_chain_n(50);
    group.bench_function("let_chain_50", |b| {
        b.iter(|| bc_eval(black_box(&let_chain_50)))
    });

    let nested_calls_10 = nested_calls_n(10);
    group.bench_function("nested_calls_10", |b| {
        b.iter(|| bc_eval(black_box(&nested_calls_10)))
    });

    group.finish();
}

/// Measure compile-only vs execute-only to identify where time is spent.
fn bench_compile_vs_execute(c: &mut Criterion) {
    let mut group = c.benchmark_group("compile_vs_execute");

    // Simple expression — shows per-invocation overhead.
    group.bench_function("compile_arithmetic", |b| {
        b.iter(|| bc_compile(black_box(ARITHMETIC)))
    });
    group.bench_function("execute_arithmetic", |b| {
        let (chunk, mut interner) = bc_compile(ARITHMETIC);
        b.iter(|| bc_execute(chunk.clone(), &mut interner))
    });

    // Medium expression — let chain.
    group.bench_function("compile_let_chain_10", |b| {
        b.iter(|| bc_compile(black_box(LET_CHAIN_10)))
    });
    group.bench_function("execute_let_chain_10", |b| {
        let (chunk, mut interner) = bc_compile(LET_CHAIN_10);
        b.iter(|| bc_execute(chunk.clone(), &mut interner))
    });

    // Complex expression — attrset 100.
    let attrset_100 = attrset_n(100);
    group.bench_function("compile_attrset_100", |b| {
        b.iter(|| bc_compile(black_box(&attrset_100)))
    });
    group.bench_function("execute_attrset_100", |b| {
        let (chunk, mut interner) = bc_compile(&attrset_100);
        b.iter(|| bc_execute(chunk.clone(), &mut interner))
    });

    // Function application chain.
    group.bench_function("compile_apply_chain", |b| {
        b.iter(|| bc_compile(black_box(APPLY_CHAIN)))
    });
    group.bench_function("execute_apply_chain", |b| {
        let (chunk, mut interner) = bc_compile(APPLY_CHAIN);
        b.iter(|| bc_execute(chunk.clone(), &mut interner))
    });

    // Fixpoint: compile + execute (lazy let bindings).
    group.bench_function("compile_fixpoint", |b| {
        b.iter(|| bc_compile(black_box(FIXPOINT)))
    });
    group.bench_function("execute_fixpoint", |b| {
        let (chunk, mut interner) = bc_compile(FIXPOINT);
        b.iter(|| bc_execute(chunk.clone(), &mut interner))
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_tree_walker,
    bench_bytecode_vm,
    bench_compile_vs_execute,
);
criterion_main!(benches);
