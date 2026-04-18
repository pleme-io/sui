;; 02_function_costs.lisp — the `fn` category was the highest-avg
;; tag in the oracle perf report (318 µs avg). These experiments
;; break down WHERE that time goes: argument thunking, env
;; cloning, recursion depth, closure capture.

(defperfexp apply-depth-scaling
  :hypothesis
    "A flat chain of N function applications costs ~N units of env
     clone + thunk forcing. Time should grow linearly with depth."
  :variants (
    (:name "apply-1"
      :source "let f = x: x + 1; in f 1")
    (:name "apply-5"
      :source "let f = x: x + 1; in f (f (f (f (f 1))))")
    (:name "apply-20"
      :source
        "let f = x: x + 1;
         in f (f (f (f (f (f (f (f (f (f
            (f (f (f (f (f (f (f (f (f (f 1)))))))))))))))))))"))
  :iterations 1000
  :tags ("fn" "apply" "scaling"))

(defperfexp recursion-cost
  :hypothesis
    "Direct recursion (fac n) incurs one stack frame + one env
     clone per call. This experiment times fac at three depths to
     see the per-frame amortized cost. The delta from fac-5 → fac-10
     should be ~5× the per-frame cost."
  :variants (
    (:name "fac-5"
      :source "let fac = n: if n <= 1 then 1 else n * fac (n - 1); in fac 5")
    (:name "fac-10"
      :source "let fac = n: if n <= 1 then 1 else n * fac (n - 1); in fac 10")
    (:name "fac-15"
      :source "let fac = n: if n <= 1 then 1 else n * fac (n - 1); in fac 15"))
  :iterations 500
  :tags ("fn" "recursion"))

(defperfexp closure-capture-cost
  :hypothesis
    "Closures that capture more of the enclosing env should cost
     more to construct. This experiment holds the body constant (x +
     y) but varies how many additional bindings are in scope when
     the closure is defined. A smart implementation only captures
     referenced free vars; a naive one captures everything.

     If sui tracks used-vars precisely, small-env and big-env should
     be within noise. If not, big-env will be measurably slower."
  :variants (
    (:name "closure-capture-2"
      :source "let x = 1; y = 2; f = a: a + x + y; in f 10")
    (:name "closure-capture-10"
      :source
        "let a = 1; b = 2; c = 3; d = 4; e = 5; f = 6; g = 7; h = 8;
             x = 9; y = 10;
             clo = z: z + x + y;
         in clo 0")
    (:name "closure-capture-20"
      :source
        "let p0=0; p1=1; p2=2; p3=3; p4=4; p5=5; p6=6; p7=7; p8=8; p9=9;
             q0=0; q1=1; q2=2; q3=3; q4=4; q5=5; q6=6; q7=7;
             x = 100; y = 200;
             clo = z: z + x + y;
         in clo 0"))
  :iterations 500
  :tags ("fn" "closure" "env"))
