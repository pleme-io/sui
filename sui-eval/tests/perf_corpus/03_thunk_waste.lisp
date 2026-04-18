;; 03_thunk_waste.lisp — the oracle perf profile reports 25%
;; corpus-wide thunk waste (14 of 56 created thunks never forced).
;; These experiments probe where the waste originates so a future
;; pass can decide whether to not-thunk certain literal-like
;; expressions.

(defperfexp thunk-waste-never-used
  :hypothesis
    "A let-binding whose value is never referenced in the body
     creates a thunk that's never forced — classic dead-binding
     waste. `thunks_created - thunks_forced` should be large
     here relative to the used-all baseline."
  :variants (
    (:name "used-all-3"
      :source "let a = 1; b = 2; c = 3; in a + b + c")
    (:name "used-one-of-3"
      :source "let a = 1; b = 2; c = 3; in a")
    (:name "used-one-of-10"
      :source
        "let a = 1; b = 2; c = 3; d = 4; e = 5;
             f = 6; g = 7; h = 8; i = 9; j = 10;
         in a"))
  :iterations 1000
  :tags ("thunk" "waste"))

(defperfexp literal-vs-complex-rhs
  :hypothesis
    "Thunks around trivial literal RHS (`let x = 42`) could
     arguably be skipped — a literal has no side effects, no
     computation to defer. Today sui thunks uniformly. This
     experiment measures the overhead of that uniform policy
     by comparing literal-RHS vs complex-RHS bindings of the
     same shape."
  :variants (
    (:name "literal-rhs-5"
      :source "let a = 1; b = 2; c = 3; d = 4; e = 5; in a + b + c + d + e")
    (:name "simple-expr-rhs-5"
      :source
        "let a = 1 + 0; b = 2 + 0; c = 3 + 0; d = 4 + 0; e = 5 + 0;
         in a + b + c + d + e")
    (:name "arith-chain-rhs-5"
      :source
        "let a = 1 * 2 + 3; b = 2 * 2 + 3; c = 3 * 2 + 3;
             d = 4 * 2 + 3; e = 5 * 2 + 3;
         in a + b + c + d + e"))
  :iterations 1000
  :tags ("thunk" "binding-kind"))
