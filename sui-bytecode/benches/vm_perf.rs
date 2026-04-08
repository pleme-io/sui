//! Bytecode VM performance benchmarks.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn bench_simple_eval(c: &mut Criterion) {
    c.bench_function("vm_eval_1+2", |b| {
        b.iter(|| {
            let _ = black_box(sui_bytecode::eval("1 + 2"));
        });
    });
}

criterion_group!(benches, bench_simple_eval);
criterion_main!(benches);
