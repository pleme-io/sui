;; 10_errors.lisp — programs CppNix rejects with specific error
;; categories. sui must also fail on these, and the error message
;; must contain the declared substring (case-insensitive).
;;
;; This file is the durable guard against "silent-Ok" regressions
;; like the one fixed at ac7ce0a where sui's force_value returned
;; an undefined-shape thunk value on `let x = x; in x` instead of
;; the infinite-recursion error CppNix raises. Every entry here
;; exercises that entire class: any program where success/failure
;; itself is the observation.
;;
;; Schema note: :expected-json is ignored when :expected-error is
;; set. Using "null" as a placeholder keeps the field present for
;; tools that still scan it.

;; ── Infinite recursion ──────────────────────────────────────────

(defnix error-direct-self-reference
  :source "let x = x; in x"
  :expected-json "null"
  :expected-error "infinite recursion"
  :tags ("error" "recursion")
  :note "The canonical blackhole test. Fixed at ac7ce0a.")

(defnix error-mutual-self-reference
  :source "let a = b; b = a; in a"
  :expected-json "null"
  :expected-error "infinite recursion"
  :tags ("error" "recursion" "mutual"))

(defnix error-self-plus-one
  :source "let x = x + 1; in x"
  :expected-json "null"
  :expected-error "infinite recursion"
  :tags ("error" "recursion")
  :note
    "Slightly different shape — x depends on itself through an
     arithmetic op. Same infinite-recursion diagnosis applies.")

;; ── Undefined variable ──────────────────────────────────────────

(defnix error-undefined-free-var
  :source "undefined_free_var"
  :expected-json "null"
  :expected-error "undefined variable"
  :tags ("error" "env"))

(defnix error-undefined-in-expression
  :source "1 + this_does_not_exist"
  :expected-json "null"
  :expected-error "undefined variable"
  :tags ("error" "env"))

;; ── Type errors ─────────────────────────────────────────────────

(defnix error-add-int-string
  :source "1 + \"hello\""
  :expected-json "null"
  :expected-error "cannot"
  :tags ("error" "type")
  :note
    "sui: 'cannot add int and string'. CppNix: 'cannot add a string
     to an integer'. The 'cannot' prefix is the reliable substring
     across both engines; needle stays intentionally broad so wording
     polish on either side doesn't break the test.")

(defnix error-call-non-function
  :source "42 1"
  :expected-json "null"
  :expected-error "call"
  :tags ("error" "type")
  :note
    "sui: 'cannot call int'. CppNix: 'attempt to call something which
     is not a function'. Both contain 'call'.")

(defnix error-select-non-attrset
  :source "(42).foo"
  :expected-json "null"
  :expected-error "select"
  :tags ("error" "type")
  :note
    "sui: 'cannot select from int'. CppNix: 'selecting an attribute'.
     Both contain 'select'.")

;; ── Missing attribute ───────────────────────────────────────────

(defnix error-missing-attr
  :source "({ a = 1; }).nonexistent"
  :expected-json "null"
  :expected-error "attribute"
  :tags ("error" "attrs"))

;; ── Assertion / throw / abort ──────────────────────────────────

(defnix error-assert-fails
  :source "assert 1 == 2; 42"
  :expected-json "null"
  :expected-error "assert"
  :tags ("error" "assert"))

(defnix error-throw
  :source "throw \"something went wrong\""
  :expected-json "null"
  :expected-error "something went wrong"
  :tags ("error" "throw")
  :note
    "The error message from `throw` contains the thrown string
     verbatim in both engines.")

(defnix error-abort
  :source "abort \"stopping now\""
  :expected-json "null"
  :expected-error "stopping now"
  :tags ("error" "abort"))
