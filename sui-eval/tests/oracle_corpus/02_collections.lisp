;; 02_collections.lisp — attrsets and lists.

;; ── Lists ────────────────────────────────────────────────────────

(defnix list-empty
  :source "[]"
  :expected-json "[]"
  :tags ("list"))

(defnix list-homogeneous-int
  :source "[ 1 2 3 ]"
  :expected-json "[1,2,3]"
  :tags ("list" "int"))

(defnix list-mixed
  :source "[ 1 \"two\" true null ]"
  :expected-json "[1,\"two\",true,null]"
  :tags ("list" "mixed"))

(defnix list-concat
  :source "[ 1 2 ] ++ [ 3 4 ]"
  :expected-json "[1,2,3,4]"
  :tags ("list" "concat"))

(defnix list-of-lists
  :source "[ [ 1 ] [ 2 3 ] [] ]"
  :expected-json "[[1],[2,3],[]]"
  :tags ("list" "nested"))

;; ── Attrsets ─────────────────────────────────────────────────────

(defnix attrset-empty
  :source "{}"
  :expected-json "{}"
  :tags ("attrs"))

(defnix attrset-flat
  :source "{ a = 1; b = 2; }"
  :expected-json "{\"a\":1,\"b\":2}"
  :tags ("attrs"))

(defnix attrset-access
  :source "{ a = 1; b = 2; }.a"
  :expected-json "1"
  :tags ("attrs" "select"))

(defnix attrset-access-nested
  :source "{ a = { b = { c = 42; }; }; }.a.b.c"
  :expected-json "42"
  :tags ("attrs" "select" "nested"))

(defnix attrset-has
  :source "{ a = 1; } ? a"
  :expected-json "true"
  :tags ("attrs" "has"))

(defnix attrset-has-missing
  :source "{ a = 1; } ? b"
  :expected-json "false"
  :tags ("attrs" "has"))

(defnix attrset-or-present
  :source "{ a = 1; }.a or 99"
  :expected-json "1"
  :tags ("attrs" "or"))

(defnix attrset-or-missing
  :source "{ a = 1; }.b or 99"
  :expected-json "99"
  :tags ("attrs" "or"))

(defnix attrset-rec
  :source "rec { a = 10; b = a * 2; }.b"
  :expected-json "20"
  :tags ("attrs" "rec")
  :note "`rec` enables self-reference within the attrset body")

(defnix attrset-nested-dotted
  :source "{ a.b.c = 42; }.a.b.c"
  :expected-json "42"
  :tags ("attrs" "dotted-key"))

;; ── Strings ──────────────────────────────────────────────────────

(defnix string-literal
  :source "\"hello\""
  :expected-json "\"hello\""
  :tags ("string"))

(defnix string-concat
  :source "\"foo\" + \"bar\""
  :expected-json "\"foobar\""
  :tags ("string" "concat"))

(defnix string-interp-int
  :source "let n = 42; in \"n is ${toString n}\""
  :expected-json "\"n is 42\""
  :tags ("string" "interp"))

(defnix string-interp-chain
  :source "let a = \"hi\"; b = \"there\"; in \"${a}, ${b}\""
  :expected-json "\"hi, there\""
  :tags ("string" "interp"))

(defnix string-multiline
  :source "''\n  hello\n  world\n''"
  :expected-json "\"hello\\nworld\\n\""
  :tags ("string" "multiline")
  :note "Indented-string strips common leading whitespace")
