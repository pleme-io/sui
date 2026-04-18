;; 05_builtins.lisp — coverage of the builtins subset that sui already
;; implements. When a case here regresses, sui's nixpkgs compat
;; silently shifts — this corpus is the first line of defence.

;; ── List builtins ────────────────────────────────────────────────

(defnix builtins-length
  :source "builtins.length [ 1 2 3 4 5 ]"
  :expected-json "5"
  :tags ("builtin" "list"))

(defnix builtins-head
  :source "builtins.head [ 10 20 30 ]"
  :expected-json "10"
  :tags ("builtin" "list"))

(defnix builtins-tail
  :source "builtins.tail [ 10 20 30 ]"
  :expected-json "[20,30]"
  :tags ("builtin" "list"))

(defnix builtins-map
  :source "builtins.map (x: x * 2) [ 1 2 3 ]"
  :expected-json "[2,4,6]"
  :tags ("builtin" "list" "map"))

(defnix builtins-filter
  :source "builtins.filter (x: x > 2) [ 1 2 3 4 5 ]"
  :expected-json "[3,4,5]"
  :tags ("builtin" "list" "filter"))

(defnix builtins-foldl-sum
  :source "builtins.foldl' (acc: x: acc + x) 0 [ 1 2 3 4 ]"
  :expected-json "10"
  :tags ("builtin" "list" "fold"))

(defnix builtins-elemAt
  :source "builtins.elemAt [ \"a\" \"b\" \"c\" ] 1"
  :expected-json "\"b\""
  :tags ("builtin" "list"))

(defnix builtins-concatLists
  :source "builtins.concatLists [ [ 1 ] [ 2 3 ] [] [ 4 ] ]"
  :expected-json "[1,2,3,4]"
  :tags ("builtin" "list"))

(defnix builtins-genList
  :source "builtins.genList (x: x * x) 4"
  :expected-json "[0,1,4,9]"
  :tags ("builtin" "list"))

;; ── Attrset builtins ─────────────────────────────────────────────

(defnix builtins-attrNames
  :source "builtins.attrNames { b = 2; a = 1; c = 3; }"
  :expected-json "[\"a\",\"b\",\"c\"]"
  :tags ("builtin" "attrs")
  :note "Result is lex-sorted — specified by Nix semantics.")

(defnix builtins-attrValues
  :source "builtins.attrValues { b = 2; a = 1; c = 3; }"
  :expected-json "[1,2,3]"
  :tags ("builtin" "attrs")
  :note "Values are returned in attrNames order (lex-sorted keys).")

(defnix builtins-hasAttr-yes
  :source "builtins.hasAttr \"a\" { a = 1; }"
  :expected-json "true"
  :tags ("builtin" "attrs"))

(defnix builtins-hasAttr-no
  :source "builtins.hasAttr \"x\" { a = 1; }"
  :expected-json "false"
  :tags ("builtin" "attrs"))

(defnix builtins-getAttr
  :source "builtins.getAttr \"a\" { a = 42; b = 99; }"
  :expected-json "42"
  :tags ("builtin" "attrs"))

(defnix builtins-mapAttrs
  :source "builtins.mapAttrs (k: v: v + 1) { a = 1; b = 2; }"
  :expected-json "{\"a\":2,\"b\":3}"
  :tags ("builtin" "attrs"))

;; ── String builtins ──────────────────────────────────────────────

(defnix builtins-stringLength
  :source "builtins.stringLength \"hello\""
  :expected-json "5"
  :tags ("builtin" "string"))

(defnix builtins-substring
  :source "builtins.substring 1 3 \"hello\""
  :expected-json "\"ell\""
  :tags ("builtin" "string"))

(defnix builtins-toString-int
  :source "builtins.toString 42"
  :expected-json "\"42\""
  :tags ("builtin" "string" "coerce"))

(defnix builtins-toString-bool
  :source "builtins.toString true"
  :expected-json "\"1\""
  :tags ("builtin" "string" "coerce")
  :note "Nix toString coerces true→\"1\" and false→\"\".")

;; ── Type predicates ──────────────────────────────────────────────

(defnix builtins-isNull
  :source "builtins.isNull null"
  :expected-json "true"
  :tags ("builtin" "type"))

(defnix builtins-isList
  :source "builtins.isList [ 1 ]"
  :expected-json "true"
  :tags ("builtin" "type"))

(defnix builtins-isAttrs
  :source "builtins.isAttrs { a = 1; }"
  :expected-json "true"
  :tags ("builtin" "type"))

(defnix builtins-isInt
  :source "builtins.isInt 42"
  :expected-json "true"
  :tags ("builtin" "type"))

(defnix builtins-isString
  :source "builtins.isString \"x\""
  :expected-json "true"
  :tags ("builtin" "type"))

(defnix builtins-typeOf
  :source "builtins.typeOf 42"
  :expected-json "\"int\""
  :tags ("builtin" "type"))
