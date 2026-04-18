;; 99_executable_specs.lisp — tests for builtins that sui DOES NOT
;; implement yet. Each entry is `:skip #t` with a `:note` explaining
;; what's missing. When someone implements the builtin in sui-eval,
;; they flip `:skip #f` and the test becomes the acceptance check.
;;
;; This file IS the TODO list for Track B (ecosystem completeness)
;; from the audit. Adding new entries here is the preferred way to
;; document "we need this builtin" — the spec doubles as the
;; regression guard once it lands.

;; ── replaceStrings — string substitution ─────────────────────────
;; CppNix semantics: builtins.replaceStrings [from1 from2 …] [to1 to2 …] subject
;; Walks the subject left-to-right, replacing each from[i] with to[i]
;; using greedy matching. Empty from matches each position exactly once.

(defnix spec-replaceStrings-simple
  :source "builtins.replaceStrings [ \"a\" ] [ \"z\" ] \"banana\""
  :expected-json "\"bznznz\""
  :tags ("spec" "string")
  :skip #t
  :note
    "sui does not implement builtins.replaceStrings yet. Spec per CppNix:
     each occurrence of from[i] in subject becomes to[i]. Multiple froms
     matched left-to-right greedily.")

(defnix spec-replaceStrings-multi
  :source "builtins.replaceStrings [ \"foo\" \"bar\" ] [ \"baz\" \"qux\" ] \"foo and bar\""
  :expected-json "\"baz and qux\""
  :tags ("spec" "string")
  :skip #t
  :note
    "Length-2 from/to arrays. Each pair is an independent rewrite rule
     applied in parallel (no from[i] result feeds into the next match).")

;; ── catAttrs — concat a field across a list of attrsets ──────────
;; CppNix: builtins.catAttrs name list
;; Returns [ elem.name | elem ∈ list, elem ? name ]. Missing keys silently skipped.

(defnix spec-catAttrs-basic
  :source
    "builtins.catAttrs \"x\" [ { x = 1; y = 10; } { x = 2; } { y = 20; } { x = 3; } ]"
  :expected-json "[1,2,3]"
  :tags ("spec" "attrs")
  :skip #t
  :note
    "sui does not implement builtins.catAttrs yet. Spec: project name out of
     each list element, skipping elements that don't have the attribute.")

;; ── groupBy — partition a list by a key function ─────────────────
;; CppNix: builtins.groupBy f list → attrset of name → [elem].

(defnix spec-groupBy-parity
  :source
    "let parity = n: if (n / 2) * 2 == n then \"even\" else \"odd\";
     in builtins.groupBy parity [ 1 2 3 4 5 ]"
  :expected-json "{\"even\":[2,4],\"odd\":[1,3,5]}"
  :tags ("spec" "list" "attrs")
  :skip #t
  :note
    "sui does not implement builtins.groupBy yet. Each elem goes into the
     bucket whose name is `f elem`. Bucket order follows input order
     (stable). Buckets are sorted by name in the returned attrset.")

;; ── path — filtered path import with name + filter ───────────────
;; CppNix: builtins.path { path; name ? …; filter ? …; recursive ? … }
;; Imports a path into the store with user-controlled filtering. Very
;; common in flake source sets; missing it blocks most real flakes.

(defnix spec-path-import-basic
  :source "builtins.path { path = /tmp/nonexistent-oracle-path; name = \"x\"; }"
  :expected-json "\"\""
  :tags ("spec" "path" "flake-blocker")
  :skip #t
  :note
    "Placeholder expected value — the actual store path depends on filesystem
     content hashing, which won't match a literal string. When implemented,
     this spec will need a structural matcher (regex on /nix/store/HASH-NAME
     format). Documents the calling shape so the implementation target
     is clear.")

;; ── fetchClosure — retrieve a closure from a binary cache ────────
;; CppNix: builtins.fetchClosure { fromStore; fromPath; toPath; }
;; Blocks any flake that pins substituted store paths by hash.

(defnix spec-fetchClosure-basic
  :source
    "builtins.fetchClosure {
       fromStore = \"https://cache.nixos.org\";
       fromPath = \"/nix/store/abc-hello\";
     }"
  :expected-json "\"/nix/store/abc-hello\""
  :tags ("spec" "fetcher" "flake-blocker")
  :skip #t
  :note
    "Fetcher-class builtin; requires substituter client (Track C gap).
     Expected is a placeholder — when we implement this, the result is
     the toPath (or fromPath when content-preserving).")

;; ── unsafeDiscardOutputDependency — string context trim ──────────
;; CppNix: builtins.unsafeDiscardOutputDependency str
;; Strips derivation-output context from a string, leaving deriver context.
;; Used by stdenv to break build-time → runtime context chains.

(defnix spec-unsafeDiscardOutputDependency-passthrough
  :source "builtins.unsafeDiscardOutputDependency \"hello\""
  :expected-json "\"hello\""
  :tags ("spec" "string-context")
  :skip #t
  :note
    "Context-free input should round-trip verbatim. sui currently has this
     as a stub that doesn't modify context at all — true coverage needs
     a derivation in the input to observe the context change.")

;; ── nixVersion — sui identifies as a Nix-compatible evaluator ────

(defnix spec-nixVersion-string
  :source "builtins.nixVersion"
  :expected-json "\"2.24.0\""
  :tags ("spec" "meta")
  :skip #t
  :note
    "sui currently returns a hardcoded version string that matches CppNix
     2.24. Flip this to un-skipped once we confirm the format — test is
     here to catch drift if someone changes the hardcoded value.")
