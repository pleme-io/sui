;; 04_overlays.lisp — the // operator, the workhorse of NixOS module
;; merges, flake overlays, and nixpkgs extension. This is the hot path
;; my recent perf work targeted (NixAttrs::get_sym cached fast-path).

(defnix overlay-flat
  :source "{ a = 1; b = 2; } // { c = 3; }"
  :expected-json "{\"a\":1,\"b\":2,\"c\":3}"
  :tags ("overlay"))

(defnix overlay-override
  :source "{ a = 1; b = 2; } // { b = 99; }"
  :expected-json "{\"a\":1,\"b\":99}"
  :tags ("overlay" "override"))

(defnix overlay-three-way
  :source "{ a = 1; } // { b = 2; } // { c = 3; }"
  :expected-json "{\"a\":1,\"b\":2,\"c\":3}"
  :tags ("overlay" "chain"))

(defnix overlay-right-wins
  :source "{ x = 1; } // { x = 2; } // { x = 3; }"
  :expected-json "{\"x\":3}"
  :tags ("overlay" "precedence"))

(defnix overlay-then-select
  :source "({ a = 1; b = 2; } // { a = 99; }).a"
  :expected-json "99"
  :tags ("overlay" "select"))

(defnix overlay-iter-then-access
  :source
    "let merged = { a = 1; b = 2; } // { c = 3; } // { d = 4; };
     in (builtins.length (builtins.attrNames merged)) + merged.a + merged.d"
  :expected-json "9"
  :tags ("overlay" "iter-then-access")
  :note
    "This is the hot pattern my overlay get_sym cache fast-path targets:
     iterate once (which warms the flat cache), then do repeated point
     accesses. All point accesses after the iter should be O(1).")

(defnix overlay-empty-lhs
  :source "{} // { a = 1; }"
  :expected-json "{\"a\":1}"
  :tags ("overlay" "empty"))

(defnix overlay-empty-rhs
  :source "{ a = 1; } // {}"
  :expected-json "{\"a\":1}"
  :tags ("overlay" "empty"))

(defnix overlay-deep-nested-shallow-merge
  :source "{ a = { x = 1; y = 2; }; } // { a = { x = 99; }; }"
  :expected-json "{\"a\":{\"x\":99}}"
  :tags ("overlay" "shallow")
  :note
    "// is a SHALLOW merge. The right .a wholly replaces the left .a;
     the left's { y = 2; } is gone. Deep merge is what `lib.recursiveUpdate`
     does and isn't tested here.")
