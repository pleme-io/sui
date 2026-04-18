;; 01_primitives.lisp — primitive types, arithmetic, boolean logic.
;;
;; Each `(defnix NAME :source "..." :expected-json "..." ...)` form is
;; one oracle test. The harness runs `:source` through `sui_eval::eval`,
;; parses `:expected-json` as JSON, and diffs the two.

;; ── Integers ─────────────────────────────────────────────────────

(defnix int-literal
  :source "42"
  :expected-json "42"
  :tags ("primitive" "int"))

(defnix int-addition
  :source "1 + 2"
  :expected-json "3"
  :tags ("arith" "int"))

(defnix int-precedence
  :source "1 + 2 * 3"
  :expected-json "7"
  :tags ("arith" "precedence")
  :note "* binds tighter than +, as in C / Nix")

(defnix int-parens-override
  :source "(1 + 2) * 3"
  :expected-json "9"
  :tags ("arith" "parens"))

(defnix int-subtraction
  :source "10 - 3 - 2"
  :expected-json "5"
  :tags ("arith" "left-assoc")
  :note "- is left-associative: (10 - 3) - 2, not 10 - (3 - 2)")

(defnix int-negative
  :source "0 - 5"
  :expected-json "-5"
  :tags ("arith"))

;; ── Floats ───────────────────────────────────────────────────────

(defnix float-literal
  :source "3.14"
  :expected-json "3.14"
  :tags ("primitive" "float"))

(defnix float-addition
  :source "1.5 + 2.25"
  :expected-json "3.75"
  :tags ("arith" "float"))

(defnix int-plus-float-promotes
  :source "2 + 0.5"
  :expected-json "2.5"
  :tags ("arith" "coercion")
  :note "Nix promotes int+float to float, matching CppNix.")

;; ── Booleans ─────────────────────────────────────────────────────

(defnix bool-true
  :source "true"
  :expected-json "true"
  :tags ("primitive" "bool"))

(defnix bool-false
  :source "false"
  :expected-json "false"
  :tags ("primitive" "bool"))

(defnix bool-and
  :source "true && false"
  :expected-json "false"
  :tags ("bool" "logic"))

(defnix bool-or
  :source "true || false"
  :expected-json "true"
  :tags ("bool" "logic"))

(defnix bool-not
  :source "!true"
  :expected-json "false"
  :tags ("bool" "logic"))

(defnix bool-implication
  :source "false -> true"
  :expected-json "true"
  :tags ("bool" "logic")
  :note "-> is boolean implication in Nix (p -> q equivalent to !p || q)")

;; ── Null ─────────────────────────────────────────────────────────

(defnix null-literal
  :source "null"
  :expected-json "null"
  :tags ("primitive" "null"))

(defnix null-eq-null
  :source "null == null"
  :expected-json "true"
  :tags ("primitive" "eq"))

(defnix null-ne-zero
  :source "null == 0"
  :expected-json "false"
  :tags ("primitive" "eq"))

;; ── Comparisons ──────────────────────────────────────────────────

(defnix compare-lt
  :source "1 < 2"
  :expected-json "true"
  :tags ("compare"))

(defnix compare-gt
  :source "3 > 5"
  :expected-json "false"
  :tags ("compare"))

(defnix compare-le-equal
  :source "4 <= 4"
  :expected-json "true"
  :tags ("compare"))

(defnix compare-ge-equal
  :source "4 >= 4"
  :expected-json "true"
  :tags ("compare"))

;; ── If/Then/Else ─────────────────────────────────────────────────

(defnix if-true-branch
  :source "if true then 1 else 2"
  :expected-json "1"
  :tags ("control" "if"))

(defnix if-false-branch
  :source "if false then 1 else 2"
  :expected-json "2"
  :tags ("control" "if"))

(defnix if-nested
  :source "if 1 < 2 then (if 3 > 2 then \"ok\" else \"no\") else \"never\""
  :expected-json "\"ok\""
  :tags ("control" "if" "nested"))
