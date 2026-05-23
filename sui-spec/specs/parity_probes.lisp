;; sui-spec/specs/parity_probes.lisp — cross-repo parity probe corpus.
;;
;; Each `(defprobe …)` is a typed question of the form "does sui
;; agree with CppNix when you ask this?".  The runner
;; (`src/bin/sui-sweep.rs`) walks every pleme-io flake, substitutes
;; `$FLAKE` with that flake's absolute path, runs both engines,
;; classifies the result, and reports.
;;
;; Adding a new probe = adding one (defprobe …) form.  Promoting a
;; probe to a permanent regression guard = adding "regression" to
;; its :tags.
;;
;; Starter corpus — the same seven probes the first session's bash
;; sweep ran, now authored declaratively:

(defprobe
  :name     "getflake-outPath"
  :expr     "(builtins.getFlake \"path:$FLAKE\").outPath"
  :classify JsonEqual
  :tags     ("smoke" "drop-in-replacement"))

(defprobe
  :name     "getflake-inputs-keys"
  :expr     "builtins.attrNames ((builtins.getFlake \"path:$FLAKE\").inputs or {})"
  :classify JsonEqual
  :tags     ("smoke" "flake-shape"))

(defprobe
  :name     "getflake-outputs-keys"
  :expr     "builtins.attrNames (builtins.getFlake \"path:$FLAKE\")"
  :classify JsonEqual
  :tags     ("smoke" "flake-shape" "regression"))

(defprobe
  :name     "getflake-packages-keys"
  :expr     "builtins.attrNames ((builtins.getFlake \"path:$FLAKE\").packages.aarch64-darwin or {})"
  :classify JsonEqual
  :tags     ("smoke" "flake-outputs"))

(defprobe
  :name     "getflake-devShells-keys"
  :expr     "builtins.attrNames ((builtins.getFlake \"path:$FLAKE\").devShells.aarch64-darwin or {})"
  :classify JsonEqual
  :tags     ("smoke" "flake-outputs"))

(defprobe
  :name     "getflake-overlays-keys"
  :expr     "builtins.attrNames ((builtins.getFlake \"path:$FLAKE\").overlays or {})"
  :classify JsonEqual
  :tags     ("smoke" "flake-outputs"))

(defprobe
  :name     "getflake-homeManagerModules-keys"
  :expr     "builtins.attrNames ((builtins.getFlake \"path:$FLAKE\").homeManagerModules or {})"
  :classify JsonEqual
  :tags     ("smoke" "flake-outputs"))

;; ── Lazy evaluation edge cases ────────────────────────────────────
;;
;; Each probe pins down a different load-bearing laziness property
;; of the evaluator.  Any divergence here would surface a serious
;; semantics bug.

(defprobe
  :name     "lazy-forward-reference-in-let"
  :expr     "let _ = \"$FLAKE\"; in let a = b; b = 7; in a"
  :classify JsonEqual
  :tags     ("lazy" "let-in"))

(defprobe
  :name     "lazy-recursive-attrset"
  :expr     "let _ = \"$FLAKE\"; in rec { a = 1; b = a + 1; c = b * 2; }.c"
  :classify JsonEqual
  :tags     ("lazy" "recursive-attrs"))

(defprobe
  :name     "lazy-fixpoint-fold"
  :expr     "let _ = \"$FLAKE\"; in builtins.foldl' (acc: x: acc + x) 0 [ 1 2 3 4 5 6 7 8 9 10 ]"
  :classify JsonEqual
  :tags     ("lazy" "foldl"))

(defprobe
  :name     "lazy-only-touches-demanded-keys"
  :expr     "let _ = \"$FLAKE\"; in (let s = { a = 1; b = throw \"poisoned\"; }; in s.a)"
  :classify JsonEqual
  :tags     ("lazy" "selection" "regression"))

(defprobe
  :name     "lazy-thunk-memoization-no-double-eval"
  :expr     "let _ = \"$FLAKE\"; t = 1 + 2; in [ t t t ]"
  :classify JsonEqual
  :tags     ("lazy" "memoization"))

(defprobe
  :name     "lazy-with-scope-no-shadow-eager"
  :expr     "let _ = \"$FLAKE\"; in (with { a = 1; b = throw \"poison\"; }; a)"
  :classify JsonEqual
  :tags     ("lazy" "with-scope" "regression"))

;; ── Builtin coverage gaps (beyond the smoke corpus) ──────────────

(defprobe
  :name     "lib-length-of-empty"
  :expr     "let _ = \"$FLAKE\"; in builtins.length []"
  :classify JsonEqual
  :tags     ("builtins" "lists"))

(defprobe
  :name     "lib-elem-membership"
  :expr     "let _ = \"$FLAKE\"; in builtins.elem 3 [ 1 2 3 4 5 ]"
  :classify JsonEqual
  :tags     ("builtins" "lists"))

(defprobe
  :name     "lib-listToAttrs-roundtrip"
  :expr     "let _ = \"$FLAKE\"; in builtins.listToAttrs [ { name = \"a\"; value = 1; } { name = \"b\"; value = 2; } ]"
  :classify JsonEqual
  :tags     ("builtins" "attrs"))

(defprobe
  :name     "lib-mapAttrs-doubles-values"
  :expr     "let _ = \"$FLAKE\"; in builtins.mapAttrs (_: v: v * 2) { a = 1; b = 2; c = 3; }"
  :classify JsonEqual
  :tags     ("builtins" "attrs"))

(defprobe
  :name     "lib-attrValues-orders"
  :expr     "let _ = \"$FLAKE\"; in builtins.attrValues { c = 3; a = 1; b = 2; }"
  :classify JsonEqual
  :tags     ("builtins" "attrs" "ordering"))

(defprobe
  :name     "lib-genList-shape"
  :expr     "let _ = \"$FLAKE\"; in builtins.genList (i: i * i) 5"
  :classify JsonEqual
  :tags     ("builtins" "lists"))

(defprobe
  :name     "lib-concatMap-flattens"
  :expr     "let _ = \"$FLAKE\"; in builtins.concatMap (n: [ n n ]) [ 1 2 3 ]"
  :classify JsonEqual
  :tags     ("builtins" "lists"))

(defprobe
  :name     "lib-filter-empty"
  :expr     "let _ = \"$FLAKE\"; in builtins.filter (_: false) [ 1 2 3 ]"
  :classify JsonEqual
  :tags     ("builtins" "lists"))

(defprobe
  :name     "lib-replaceStrings-shape"
  :expr     "let _ = \"$FLAKE\"; in builtins.replaceStrings [ \"a\" \"b\" ] [ \"X\" \"Y\" ] \"abcab\""
  :classify JsonEqual
  :tags     ("builtins" "strings"))

(defprobe
  :name     "lib-isInt-true"
  :expr     "let _ = \"$FLAKE\"; in builtins.isInt 42"
  :classify JsonEqual
  :tags     ("builtins" "type-predicates"))

(defprobe
  :name     "lib-isAttrs-true"
  :expr     "let _ = \"$FLAKE\"; in builtins.isAttrs { x = 1; }"
  :classify JsonEqual
  :tags     ("builtins" "type-predicates"))

(defprobe
  :name     "lib-isList-true"
  :expr     "let _ = \"$FLAKE\"; in builtins.isList [ 1 2 ]"
  :classify JsonEqual
  :tags     ("builtins" "type-predicates"))

;; ── Numeric edge cases ────────────────────────────────────────────

(defprobe
  :name     "math-integer-overflow-bound"
  :expr     "let _ = \"$FLAKE\"; in 9223372036854775807 - 1"
  :classify JsonEqual
  :tags     ("math" "boundary"))

(defprobe
  :name     "math-float-precision"
  :expr     "let _ = \"$FLAKE\"; in builtins.div 10.0 3.0"
  :classify JsonEqual
  :tags     ("math" "float"))

(defprobe
  :name     "math-bitwise-and"
  :expr     "let _ = \"$FLAKE\"; in builtins.bitAnd 12 10"
  :classify JsonEqual
  :tags     ("math" "bitwise"))

(defprobe
  :name     "math-bitwise-or"
  :expr     "let _ = \"$FLAKE\"; in builtins.bitOr 5 3"
  :classify JsonEqual
  :tags     ("math" "bitwise"))

;; ── String coercion + interpolation ──────────────────────────────

(defprobe
  :name     "str-interpolation-mixed"
  :expr     "let _ = \"$FLAKE\"; x = 7; y = \"world\"; in \"hello ${y} ${toString x}\""
  :classify JsonEqual
  :tags     ("strings" "interpolation"))

(defprobe
  :name     "str-concatStringsSep"
  :expr     "let _ = \"$FLAKE\"; in builtins.concatStringsSep \"/\" [ \"a\" \"b\" \"c\" ]"
  :classify JsonEqual
  :tags     ("strings" "compose"))

(defprobe
  :name     "str-substring-out-of-range"
  :expr     "let _ = \"$FLAKE\"; in builtins.substring 0 100 \"short\""
  :classify JsonEqual
  :tags     ("strings" "boundary"))

;; ── Path operations ──────────────────────────────────────────────

(defprobe
  :name     "path-baseNameOf-trailing-slash"
  :expr     "let _ = \"$FLAKE\"; in builtins.baseNameOf \"/nix/store/abc/\""
  :classify JsonEqual
  :tags     ("paths" "boundary"))

(defprobe
  :name     "path-dirOf-relative"
  :expr     "let _ = \"$FLAKE\"; in builtins.dirOf \"a/b/c\""
  :classify JsonEqual
  :tags     ("paths" "boundary"))

;; ── Function semantics ───────────────────────────────────────────

(defprobe
  :name     "fn-partial-application"
  :expr     "let _ = \"$FLAKE\"; add = a: b: a + b; in (add 3) 4"
  :classify JsonEqual
  :tags     ("functions" "currying"))

(defprobe
  :name     "fn-args-destructure-with-default"
  :expr     "let _ = \"$FLAKE\"; f = { a, b ? 7 }: a + b; in f { a = 3; }"
  :classify JsonEqual
  :tags     ("functions" "args"))

(defprobe
  :name     "fn-args-ellipsis"
  :expr     "let _ = \"$FLAKE\"; f = { a, ... }@args: args.b or 0; in f { a = 1; b = 2; }"
  :classify JsonEqual
  :tags     ("functions" "args"))

;; ── Conditional + boolean logic ──────────────────────────────────

(defprobe
  :name     "if-then-else-strict-condition"
  :expr     "let _ = \"$FLAKE\"; in if 1 < 2 then \"yes\" else \"no\""
  :classify JsonEqual
  :tags     ("control" "if"))

(defprobe
  :name     "bool-short-circuit-and"
  :expr     "let _ = \"$FLAKE\"; in false && (throw \"poison\")"
  :classify JsonEqual
  :tags     ("control" "short-circuit" "regression"))

(defprobe
  :name     "bool-short-circuit-or"
  :expr     "let _ = \"$FLAKE\"; in true || (throw \"poison\")"
  :classify JsonEqual
  :tags     ("control" "short-circuit" "regression"))

;; ── Flake-input shape probes ─────────────────────────────────────

(defprobe
  :name     "getflake-inputs-self-is-derivation-like"
  :expr     "((builtins.getFlake \"path:$FLAKE\").inputs.self.outPath or \"missing\") != \"\""
  :classify JsonEqual
  :tags     ("flake-input" "self"))

(defprobe
  :name     "getflake-result-has-required-type"
  :expr     "((builtins.getFlake \"path:$FLAKE\")._type or \"missing\")"
  :classify JsonEqual
  :tags     ("flake-shape" "regression"))
