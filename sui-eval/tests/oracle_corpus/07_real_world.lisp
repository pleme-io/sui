;; 07_real_world.lisp — programs shaped like what nixpkgs and real
;; flakes actually evaluate. These stress composition more than any
;; single builtin and are where drop-in-replacement status is won or
;; lost.

;; ── Fold-driven computation ──────────────────────────────────────

(defnix sum-via-foldl
  :source "builtins.foldl' (a: b: a + b) 0 (builtins.genList (x: x) 10)"
  :expected-json "45"
  :tags ("fold" "list" "compose"))

(defnix product-via-foldl
  :source "builtins.foldl' (a: b: a * b) 1 [ 1 2 3 4 5 ]"
  :expected-json "120"
  :tags ("fold" "list"))

(defnix max-via-foldl
  :source "builtins.foldl' (a: b: if a > b then a else b) 0 [ 3 1 4 1 5 9 2 6 ]"
  :expected-json "9"
  :tags ("fold" "list"))

;; ── Module-system shape ──────────────────────────────────────────

(defnix module-config-eval
  :source
    "let
       defaults = { enable = false; port = 8080; host = \"localhost\"; };
       user = { enable = true; port = 9090; };
       config = defaults // user;
     in { inherit (config) enable port host; }"
  :expected-json "{\"enable\":true,\"host\":\"localhost\",\"port\":9090}"
  :tags ("module-system" "overlay")
  :note
    "Minimal NixOS module-system shape — defaults, user override, //
     merge, inherit-from-attrs. Trips up any implementation where
     inherit-from doesn't force the config attrset correctly.")

(defnix module-with-submerge
  :source
    "let
       base = { networking = { firewall = { enable = true; ports = [ 22 ]; }; }; };
       extra = { networking = { firewall = { ports = [ 80 443 ]; }; }; };
     in base // extra"
  :expected-json "{\"networking\":{\"firewall\":{\"ports\":[80,443]}}}"
  :tags ("module-system" "overlay" "shallow")
  :note
    "// is SHALLOW — the extra `networking` wholly replaces base's
     networking. CppNix has the same behavior; `recursiveUpdate` is
     the deep variant that most module systems wrap.")

;; ── Let/where-style computation ──────────────────────────────────

(defnix sum-of-squares
  :source "let square = x: x * x; in builtins.foldl' (a: x: a + square x) 0 [ 1 2 3 4 5 ]"
  :expected-json "55"
  :tags ("fn" "fold" "compose"))

(defnix pipe-style-composition
  :source
    "let
       pipe = x: f: g: g (f x);
       double = x: x * 2;
       plus-one = x: x + 1;
     in pipe 5 double plus-one"
  :expected-json "11"
  :tags ("fn" "compose"))

;; ── Recursive attrset traversal ──────────────────────────────────

(defnix attr-tree-length
  :source
    "let
       tree = { a = { b = { c = { d = 1; }; }; }; };
     in builtins.length (builtins.attrNames tree.a.b.c)"
  :expected-json "1"
  :tags ("attrs" "nested"))

(defnix listToAttrs-roundtrip
  :source
    "let
       entries = [ { name = \"a\"; value = 1; } { name = \"b\"; value = 2; } ];
       attrs = builtins.listToAttrs entries;
     in attrs.a + attrs.b"
  :expected-json "3"
  :tags ("attrs" "builtin")
  :note
    "listToAttrs is how nixpkgs generates attrset-shaped data from
     programmatic sources (e.g. mapEachSystem).")

(defnix mapAttrs-then-values
  :source
    "let doubled = builtins.mapAttrs (n: v: v * 2) { a = 1; b = 2; c = 3; };
     in builtins.attrValues doubled"
  :expected-json "[2,4,6]"
  :tags ("attrs" "builtin" "compose"))

;; ── Filter + map pipelines ───────────────────────────────────────

(defnix filter-map-sum
  :source
    "builtins.foldl' (a: x: a + x) 0
       (builtins.map (x: x * x)
         (builtins.filter (x: x > 0) [ (0 - 2) (0 - 1) 0 1 2 3 ]))"
  :expected-json "14"
  :tags ("fold" "filter" "map" "compose")
  :note
    "A classic pipeline — filter out non-positives, square, sum.
     Exercises lazy list consumption + thunk chain unwrapping.
     Negative literals inside list syntax need parens; CppNix parses
     unary minus as a binary-minus ambiguity otherwise.")

;; ── Mixed-type coercion ──────────────────────────────────────────

(defnix concat-mixed-interp
  :source "let x = 3; y = \"apples\"; in \"I have ${toString x} ${y}\""
  :expected-json "\"I have 3 apples\""
  :tags ("string" "interp" "coerce"))

(defnix bool-arith-coercion
  :source "if 1 + 2 == 3 then \"math works\" else \"math broken\""
  :expected-json "\"math works\""
  :tags ("bool" "arith" "compose"))

;; ── Self-reference (the classic rec test) ────────────────────────

(defnix rec-fib-small
  :source
    "let fib = n:
       if n < 2 then n else fib (n - 1) + fib (n - 2);
     in fib 8"
  :expected-json "21"
  :tags ("fn" "recursion"))

(defnix rec-mutual-even-odd
  :source
    "let
       isEven = n: if n == 0 then true else isOdd (n - 1);
       isOdd = n: if n == 0 then false else isEven (n - 1);
     in [ (isEven 4) (isOdd 7) (isEven 13) ]"
  :expected-json "[true,true,false]"
  :tags ("fn" "recursion" "mutual"))
