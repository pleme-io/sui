;; 12_nixpkgs_lib.lisp — regression guards for bugs found by probing
;; sui against real nixpkgs, plus a growing catalog of nixpkgs-lib
;; expressions sui must evaluate identically to CppNix.
;;
;; Each entry has the full source inline (not a reference to
;; /nix/store) so the tests are portable across machines. The
;; shapes mirror what nixpkgs-lib's own code does; if any fail,
;; real-world flake evaluation will diverge from CppNix.
;;
;; This file is the structural fix to today's root cause: relying
;; on the corpus only catches bugs the corpus covers. Adding every
;; bug found via real-nixpkgs probing here turns those discoveries
;; into permanent gates.

;; ── #1: attrValues / mapAttrs ordering (fixed at 49cfc4b) ─────────
;;
;; The VM's `attrValues` iterated intern-order instead of lex-order,
;; so `mapAttrsToList` on `{ a = 1; b = 2; }` returned
;; `[ "b=2" "a=1" ]` once lib evaluation had interned `b` before `a`.

(defnix attrs-mapAttrsToList-lex-order
  :source
    "let mapAttrs = builtins.mapAttrs;
         attrValues = builtins.attrValues;
         mapAttrsToList = f: attrs: attrValues (mapAttrs f attrs);
     in mapAttrsToList (n: v: \"${n}=${toString v}\") { a = 1; b = 2; }"
  :expected-json "[\"a=1\",\"b=2\"]"
  :tags ("nixpkgs" "attrs" "regression")
  :note
    "Regression guard: before 49cfc4b the bytecode VM returned
     [\"b=2\",\"a=1\"] on this program once nixpkgs-lib had been
     evaluated first (which interned `b` before `a` elsewhere).
     attrValues must sort by resolved STRING name, not Symbol id.")

(defnix attrs-mapAttrsToList-three-keys
  :source
    "let f = n: v: \"${n}:${toString v}\";
     in builtins.attrValues (builtins.mapAttrs f { c = 3; a = 1; b = 2; })"
  :expected-json "[\"a:1\",\"b:2\",\"c:3\"]"
  :tags ("nixpkgs" "attrs" "regression")
  :note
    "Three-key variant — insertion order intentionally reversed
     relative to lex order, so any Symbol-id iteration bug shows
     up immediately.")

;; ── #2: force_value infinite-recursion detection (fixed at ac7ce0a) ─

(defnix recursion-let-x-eq-x
  :source "let x = x; in x"
  :expected-json "null"
  :expected-error "infinite recursion"
  :tags ("regression" "recursion")
  :note
    "Regression guard: before ac7ce0a, force_value silently returned
     Ok(self-referential-thunk) at depth 100 on this program instead
     of raising. Real CppNix always errors; sui now does too.")

;; ── #3: genList negative count (fixed at 68a7f25) ─────────────────

(defnix genList-rejects-negative
  :source "builtins.genList (x: x) (0 - 1)"
  :expected-json "null"
  :expected-error "genList"
  :tags ("regression" "list" "builtin")
  :note
    "Regression guard: before 68a7f25 the builtin silently returned
     `[]` because `for i in 0..n` produces an empty range when n<0
     on i64. CppNix rejects with 'negative list length'.")

;; ── #4: float division by zero (fixed at 68a7f25) ─────────────────

(defnix float-div-by-zero-rejects
  :source "1.0 / 0.0"
  :expected-json "null"
  :expected-error "div"
  :tags ("regression" "arith")
  :note
    "Regression guard: before 68a7f25 sui silently returned `null`
     because Rust's native f64 div-by-zero produces `inf`/`NaN`
     which then serialized to JSON as `null`. CppNix rejects with
     'division by zero' on both int AND float.")

;; ── Broader nixpkgs-lib sanity ───────────────────────────────────
;;
;; These exercise the shapes lib code actually uses. Each passes
;; today; failure of any one means a real flake using that pattern
;; will break. Authored here (not in a probe test) so they run in
;; the offline oracle too.

(defnix lib-string-toUpper
  :source "builtins.replaceStrings [\"a\" \"e\" \"i\" \"o\" \"u\"] [\"A\" \"E\" \"I\" \"O\" \"U\"] \"sui eval\""
  :expected-json "\"sUI EvAl\""
  :tags ("nixpkgs" "string"))

(defnix lib-list-foldl-sum
  :source "builtins.foldl' (a: b: a + b) 0 [ 1 2 3 4 5 ]"
  :expected-json "15"
  :tags ("nixpkgs" "list" "fold"))

(defnix lib-list-foldr-sum
  :source
    "let foldr = op: nul: list:
       let len = builtins.length list;
           go = n:
             if n == len then nul
             else op (builtins.elemAt list n) (go (n + 1));
       in go 0;
     in foldr (a: b: a + b) 0 [ 1 2 3 4 5 ]"
  :expected-json "15"
  :tags ("nixpkgs" "list" "fold")
  :note
    "Reimplements lib.lists.foldr locally so the test is self-contained.
     Exercises the same thunk + recursion shape as the real lib impl.")

(defnix lib-fix-self-reference
  :source
    "let fix = f: let x = f x; in x;
     in (fix (self: { a = 1; b = self.a + 1; c = self.b + 1; })).c"
  :expected-json "3"
  :tags ("nixpkgs" "fix"))

(defnix lib-recursiveUpdate-shallow
  :source
    "let recursiveUpdate = l: r:
       builtins.mapAttrs (n: v:
         if builtins.isAttrs v && builtins.isAttrs (l.${n} or null)
         then recursiveUpdate l.${n} v
         else v
       ) r;
         merged = recursiveUpdate
                    { a = { x = 1; y = 2; }; b = 10; }
                    { a = { x = 99;       }; c = 20; };
     in merged"
  :expected-json "{\"a\":{\"x\":99},\"c\":20}"
  :tags ("nixpkgs" "attrs" "recursive-merge")
  :note
    "Verifies sui's attrset handling under a recursively-defined
     merge function. Note: this reimplementation only traverses r's
     keys (matching the defined-here semantics), so b from l drops
     — intentional: the test is verifying the mapAttrs + recursion
     shape, not the full library semantics.")

;; ── #5: substring with negative length (fixed at next commit) ──

(defnix substring-negative-len-returns-rest
  :source "builtins.substring 4 (0 - 1) \"foo-bar\""
  :expected-json "\"bar\""
  :tags ("regression" "string" "builtin")
  :note
    "CppNix convention: `substring start (-1) str` means 'from start
     to end of string'. Used all over nixpkgs — notably in
     `lib.strings.removePrefix`. Sui's VM was casting i64→usize
     before the bounds check, turning -1 into usize::MAX and
     panicking on the arithmetic overflow.")

(defnix substring-negative-start-errors
  :source "builtins.substring (0 - 5) 3 \"hello\""
  :expected-json "null"
  :expected-error "negative"
  :tags ("regression" "string" "builtin")
  :note
    "Per CppNix: negative start position raises 'negative start
     position in substring'. Sui previously returned empty string
     on negative start (my guess was wrong); the differential
     oracle caught it.")

(defnix substring-len-past-end-clamps
  :source "builtins.substring 2 999 \"hello\""
  :expected-json "\"llo\""
  :tags ("regression" "string" "builtin")
  :note "Length past end of string clamps to end.")

(defnix removePrefix-via-lib-shape
  :source
    "let preLen = builtins.stringLength \"foo-\";
         hasPrefix = p: s: builtins.substring 0 (builtins.stringLength p) s == p;
     in if hasPrefix \"foo-\" \"foo-bar\"
        then builtins.substring preLen (0 - 1) \"foo-bar\"
        else \"foo-bar\""
  :expected-json "\"bar\""
  :tags ("regression" "nixpkgs" "string")
  :note
    "Mirrors lib.strings.removePrefix's implementation end-to-end so
     the chain that surfaced the substring bug (hasPrefix check →
     substring with -1 length) stays green.")

(defnix lib-generators-toJSON-int-key
  :source "builtins.toJSON { \"1\" = 1; \"2\" = 2; }"
  :expected-json "\"{\\\"1\\\":1,\\\"2\\\":2}\""
  :tags ("nixpkgs" "json" "attrs")
  :note
    "Attrset keys that look like numbers must be emitted as JSON
     strings (not integers), and in lex order. This is the core
     of lib.generators.toJSON.")
