//! Does carrying a `sui-normalize` plan cost anything at eval time?
//!
//! Answer, measured 2026-08-18: **no** — +0.65% / +0.98% / -0.00% against a
//! ±1% A-vs-A noise floor, on a deliberately attrset-saturated workload
//! (~120k binder evaluations in ~93ms and almost nothing else). Real nix does
//! far more work per attrset, so that is an upper bound.
//!
//! ★ THE CONTROL IS THE WHOLE INSTRUMENT — read this before editing either
//! source. A first version of this benchmark reported **+8.1%**, and it was an
//! artifact: the planned source carried one extra `let` binding, `IrEnv`
//! lookup scans its scope, and every variable reference in a 40k-iteration hot
//! loop paid for it. The +8% survived deleting the plan map ENTIRELY, which is
//! what exposed the mistake — a wrong conclusion that would have driven a
//! wrong optimisation. So A and B must differ in exactly ONE thing: whether
//! `seed` needs a plan. Same binding count, same env depth, same everything
//! else.
//!
//! Pass the same file twice to read the noise floor; a `delta` below it means
//! the run resolved nothing, not that the effect is zero.
//!
//! Reps are INTERLEAVED inside one process and the statistic is the MIN.
//! Running all of A then all of B maps machine drift straight onto the
//! variable — a mistake already made once in this pass — and a first attempt
//! across two processes was swamped outright: A's max exceeded B's min, so the
//! ordering was unresolvable. Min-of-N is the robust estimator for "this run
//! met the least interference".
//!
//!   cargo run --release -p sui-ir --example plan_lookup_cost -- \
//!     sui-ir/examples/plan-cost/a-unplanned.nix \
//!     sui-ir/examples/plan-cost/b-planned.nix 30
use std::rc::Rc;
use std::time::Instant;

use sui_ir::eval_ir::{eval_ir, IrEnv};
use sui_ir::lower_file;

fn load(path: &str) -> Rc<sui_ir::ir::Program> {
    let src = std::fs::read_to_string(path).expect("read");
    Rc::new(lower_file(&src).expect("lower"))
}

fn one(prog: &Rc<sui_ir::ir::Program>) -> (f64, String) {
    let env = IrEnv::with_pure_builtins();
    let t = Instant::now();
    let v = eval_ir(prog, prog.root, &env).expect("eval");
    (t.elapsed().as_secs_f64(), format!("{v:?}"))
}

fn main() {
    let mut a = std::env::args().skip(1);
    let pa = a.next().expect("usage: plan_lookup_cost <A.nix> <B.nix> [reps]");
    let pb = a.next().expect("need B");
    let reps: u32 = a.next().map_or(25, |s| s.parse().expect("reps"));
    let (ga, gb) = (load(&pa), load(&pb));
    let (mut ba, mut bb) = (f64::MAX, f64::MAX);
    let (mut va, mut vb) = (String::new(), String::new());
    for _ in 0..reps {
        let (t, v) = one(&ga);
        ba = ba.min(t);
        va = v;
        let (t, v) = one(&gb);
        bb = bb.min(t);
        vb = v;
    }
    assert_eq!(va, vb, "the two sources must compute the same answer");
    println!("A unplanned  min {ba:.4}s  plans={}", ga.plans.len());
    println!("B planned    min {bb:.4}s  plans={}", gb.plans.len());
    println!("delta        {:+.2}%   (answer {va})", (bb / ba - 1.0) * 100.0);
}
