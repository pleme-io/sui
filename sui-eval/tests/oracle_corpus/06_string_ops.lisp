;; 06_string_ops.lisp — string manipulation beyond the basics covered
;; in 02 and 05. These programs exercise regex/match/split, context
;; introspection, and the coercion surface that nixpkgs lib/strings.nix
;; hits on nearly every module evaluation.

;; ── Pattern matching ─────────────────────────────────────────────

(defnix string-match-captures
  :source "builtins.match \"([a-z]+)([0-9]+)\" \"foo42\""
  :expected-json "[\"foo\",\"42\"]"
  :tags ("string" "regex")
  :note
    "builtins.match returns the capture groups as a list, or null when
     the regex doesn't match the full string (anchored semantics).")

(defnix string-match-no-match
  :source "builtins.match \"([0-9]+)\" \"no digits here\""
  :expected-json "null"
  :tags ("string" "regex"))

(defnix string-match-full-anchor
  :source "builtins.match \"[a-z]+\" \"foo42\""
  :expected-json "null"
  :tags ("string" "regex")
  :note
    "builtins.match is ANCHORED — the pattern must match the full
     input. `[a-z]+` matches `foo` but not the trailing `42`, so the
     overall match fails. Use `builtins.split` for partial matching.")

(defnix string-split-simple
  :source "builtins.split \",\" \"a,b,c\""
  :expected-json "[\"a\",[],\"b\",[],\"c\"]"
  :tags ("string" "regex" "split")
  :note
    "builtins.split returns an alternating [literal, captures, literal,
     captures, ...] — captures are lists even when there are none.
     This shape is what lib.splitString's implementation destructures.")

;; ── Predicates ───────────────────────────────────────────────────

(defnix string-hasPrefix
  :source "lib.hasPrefix \"foo\" \"foobar\""
  :expected-json "true"
  :tags ("string" "predicate")
  :skip #t
  :note
    "hasPrefix is in nixpkgs lib, not builtins. Skipped until we add a
     nixpkgs-lib-shaped helper or bind lib.hasPrefix through a test
     harness prelude.")

(defnix string-concatSep
  :source "builtins.concatStringsSep \", \" [ \"a\" \"b\" \"c\" ]"
  :expected-json "\"a, b, c\""
  :tags ("string" "builtin"))

(defnix string-concatMap
  :source "builtins.concatStringsSep \"-\" (builtins.map (x: builtins.toString (x * 2)) [ 1 2 3 ])"
  :expected-json "\"2-4-6\""
  :tags ("string" "builtin" "compose"))

;; ── Escapes + special characters ─────────────────────────────────

(defnix string-escape-tab
  :source "\"a\\tb\""
  :expected-json "\"a\\tb\""
  :tags ("string" "escape"))

(defnix string-escape-newline
  :source "\"a\\nb\""
  :expected-json "\"a\\nb\""
  :tags ("string" "escape"))

(defnix string-unicode
  :source "\"hello 世界\""
  :expected-json "\"hello 世界\""
  :tags ("string" "unicode"))

(defnix string-length-unicode
  :source "builtins.stringLength \"héllo\""
  :expected-json "6"
  :tags ("string" "unicode")
  :note
    "Nix stringLength counts BYTES, not characters. 'é' is 2 bytes in
     UTF-8, so 'héllo' is 6 bytes. CppNix semantics — if sui counts
     chars instead, this test catches it.")

;; ── Coercion paths ───────────────────────────────────────────────

(defnix string-coerce-path
  :source "toString ./foo"
  :expected-json "\"/foo\""
  :tags ("string" "coerce" "path")
  :skip #t
  :note
    "Path coercion result depends on evaluation CWD. Needs a fixture
     with a known relative base to assert deterministically.")

(defnix string-coerce-int-in-interp
  :source "\"${toString 42}\""
  :expected-json "\"42\""
  :tags ("string" "interp"))

(defnix string-coerce-drv-like
  :source "toString { outPath = \"/nix/store/abc-foo\"; }"
  :expected-json "\"/nix/store/abc-foo\""
  :tags ("string" "coerce" "outPath")
  :note
    "Derivation-like coercion: when toString sees an attrset with an
     outPath field, it returns the outPath string. This is how nixpkgs
     string-interpolates a package into a build command without an
     explicit .outPath access.")

;; ── Multi-line + indentation ─────────────────────────────────────

(defnix string-multiline-stripped
  :source "''\n    foo\n    bar\n  ''"
  :expected-json "\"foo\\nbar\\n\""
  :tags ("string" "multiline")
  :note
    "Indented strings strip the common leading whitespace from every
     line. The trailing '' also matters — it sets the indent reference.")

(defnix string-multiline-with-interp
  :source "let name = \"world\"; in ''\n  hello ${name}\n''"
  :expected-json "\"hello world\\n\""
  :tags ("string" "multiline" "interp"))
