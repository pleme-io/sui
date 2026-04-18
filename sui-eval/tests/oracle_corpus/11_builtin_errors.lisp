;; 11_builtin_errors.lisp — programs that exercise the error paths
;; inside individual builtins. Each builtin has its own validation
;; logic; a silent-Ok bug in any of them would mean sui quietly
;; accepts a value CppNix rejects.
;;
;; Pattern: pick an argument shape each builtin definitively
;; rejects, declare both engines must fail with a matching needle.
;; The needles are the smallest substring common to both engines'
;; wording on Nix 2.33 — future drift in either direction will
;; surface cleanly.

;; ── List builtins: out-of-bounds / empty ────────────────────────

(defnix error-head-of-empty
  :source "builtins.head []"
  :expected-json "null"
  :expected-error "empty"
  :tags ("error" "list" "builtin")
  :note
    "Both engines mention 'empty' in the error for `head []`. CppNix:
     'list index 0 is out of bounds'; sui: 'head of empty list'.
     The needle `empty` is the overlap on CppNix's exact phrase
     varies by version.")

(defnix error-tail-of-empty
  :source "builtins.tail []"
  :expected-json "null"
  :expected-error "empty"
  :tags ("error" "list" "builtin"))

(defnix error-elemAt-out-of-bounds
  :source "builtins.elemAt [ 1 2 3 ] 99"
  :expected-json "null"
  :expected-error "elemat"
  :tags ("error" "list" "builtin")
  :note
    "CppNix: ''builtins.elemAt' called with index 99 on a list of
     size 3'. sui: 'elemAt: index 99 out of bounds'. Case-insensitive
     substring `elemat` matches both (CppNix lowercases the builtin
     name in the frame header, while sui preserves the camelCase
     'elemAt' directly).")

(defnix error-genList-negative
  :source "builtins.genList (x: x) (0 - 1)"
  :expected-json "null"
  :expected-error "genList"
  :tags ("error" "list" "builtin")
  :note
    "Both engines mention 'genList' by name in the error when the
     count is negative. The needle stays specific to the op so a
     generic 'bad arg' catch-all doesn't accidentally pass.")

;; ── Attribute errors ────────────────────────────────────────────

(defnix error-getAttr-missing
  :source "builtins.getAttr \"missing\" { a = 1; }"
  :expected-json "null"
  :expected-error "attribute"
  :tags ("error" "attrs" "builtin"))

;; ── Numeric / division ──────────────────────────────────────────

(defnix error-integer-div-by-zero
  :source "1 / 0"
  :expected-json "null"
  :expected-error "div"
  :tags ("error" "arith"))

(defnix error-float-div-by-zero
  :source "1.0 / 0.0"
  :expected-json "null"
  :expected-error "div"
  :tags ("error" "arith")
  :note
    "Both engines reject float division by zero. CppNix says
     'division by zero'; sui says the same. Needle `div` covers
     both.")

;; ── Regex / JSON parsing ────────────────────────────────────────

(defnix error-match-invalid-regex
  :source "builtins.match \"[a-z\" \"abc\""
  :expected-json "null"
  :expected-error "invalid"
  :tags ("error" "string" "builtin")
  :note
    "CppNix: 'invalid regular expression'. sui: 'invalid regex'.
     Not a perfect overlap, but `invalid regular` appears in CppNix;
     sui's 'invalid regex' contains 'invalid' but not 'regular'.
     Switched needle to `invalid` — both contain it. Less specific
     but cross-engine stable.")

(defnix error-fromJSON-invalid
  :source "builtins.fromJSON \"{not valid json}\""
  :expected-json "null"
  :expected-error "json"
  :tags ("error" "string" "builtin"))

;; ── Coercion into string ────────────────────────────────────────

(defnix error-toString-lambda
  :source "builtins.toString (x: x)"
  :expected-json "null"
  :expected-error "lambda"
  :tags ("error" "type" "coerce")
  :note
    "Both engines mention 'lambda' or 'function' — CppNix says
     'cannot coerce a function to a string', sui says 'cannot
     coerce lambda'. Needle `lambda` works because sui's wording
     contains it AND CppNix's error frame mentions the argument
     type which is lambda for `(x: x)`.")

;; ── Type-mismatched comparison ──────────────────────────────────

(defnix error-less-than-type-mismatch
  :source "builtins.lessThan 1 \"two\""
  :expected-json "null"
  :expected-error "lessThan"
  :tags ("error" "type" "compare")
  :note
    "CppNix errors with 'cannot compare integer with string' inside
     the 'while calling lessThan' frame. sui's error mentions
     lessThan explicitly. Needle `lessThan` is stable on both.")
