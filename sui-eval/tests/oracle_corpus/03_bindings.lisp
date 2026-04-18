;; 03_bindings.lisp — let, with, inherit, functions.

;; ── Let ──────────────────────────────────────────────────────────

(defnix let-single
  :source "let x = 1; in x"
  :expected-json "1"
  :tags ("let"))

(defnix let-chain
  :source "let a = 1; b = a + 1; c = b + 1; in c"
  :expected-json "3"
  :tags ("let" "chain")
  :note "Nix let is implicitly recursive — bindings see each other.")

(defnix let-forward-ref
  :source "let a = b + 1; b = 10; in a"
  :expected-json "11"
  :tags ("let" "forward-ref"))

(defnix let-shadow
  :source "let x = 1; in let x = 2; in x"
  :expected-json "2"
  :tags ("let" "shadow"))

;; ── Functions ────────────────────────────────────────────────────

(defnix fn-identity
  :source "let id = x: x; in id 42"
  :expected-json "42"
  :tags ("fn" "identity"))

(defnix fn-two-args
  :source "let add = a: b: a + b; in add 3 4"
  :expected-json "7"
  :tags ("fn" "curry"))

(defnix fn-recursion
  :source "let fac = n: if n <= 1 then 1 else n * fac (n - 1); in fac 5"
  :expected-json "120"
  :tags ("fn" "recursion"))

(defnix fn-mutual-recursion
  :source
    "let even = n: if n == 0 then true else odd (n - 1);
         odd  = n: if n == 0 then false else even (n - 1);
     in even 4"
  :expected-json "true"
  :tags ("fn" "mutual-rec"))

(defnix fn-destructure
  :source "let f = { a, b }: a + b; in f { a = 3; b = 4; }"
  :expected-json "7"
  :tags ("fn" "destructure"))

(defnix fn-destructure-default
  :source "let f = { a, b ? 10 }: a + b; in f { a = 3; }"
  :expected-json "13"
  :tags ("fn" "destructure" "default"))

(defnix fn-destructure-rest
  :source "let f = { a, ... }@args: args.a + args.b; in f { a = 1; b = 2; }"
  :expected-json "3"
  :tags ("fn" "destructure" "rest"))

;; ── With ─────────────────────────────────────────────────────────

(defnix with-simple
  :source "with { a = 1; b = 2; }; a + b"
  :expected-json "3"
  :tags ("with"))

(defnix with-let-precedence
  :source "let x = 1; in with { x = 99; }; x"
  :expected-json "1"
  :tags ("with" "precedence")
  :note "let binds tighter than with — the explicit let x = 1 wins.")

;; ── Inherit ──────────────────────────────────────────────────────

(defnix inherit-simple
  :source "let x = 1; y = 2; in { inherit x y; }.y"
  :expected-json "2"
  :tags ("inherit"))

(defnix inherit-from
  :source "let src = { a = 10; b = 20; }; in { inherit (src) a b; }.a"
  :expected-json "10"
  :tags ("inherit" "from"))
