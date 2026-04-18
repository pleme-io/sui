;; 99_executable_specs.lisp — tests for builtins marked as "missing"
;; in the initial audit. Turns out most of them are already implemented
;; in sui — the audit's grep was incomplete. Each entry documents the
;; expected CppNix behavior; `:skip #t` means we haven't verified yet
;; OR the builtin is genuinely missing.
;;
;; This file doubles as the TODO list for Track-B ecosystem gaps (one
;; per `:skip #t`). Flipping `:skip #f` after the builtin lands (or
;; after verifying existing impl matches CppNix) turns the spec into
;; the acceptance test.

;; ── replaceStrings — string substitution ─────────────────────────
;; sui implements this in sui-eval/src/builtins/strings.rs. Flipping
;; :skip #f to assert CppNix-compat.

(defnix spec-replaceStrings-simple
  :source "builtins.replaceStrings [ \"a\" ] [ \"z\" ] \"banana\""
  :expected-json "\"bznznz\""
  :tags ("spec" "string" "verified")
  :note
    "CppNix semantics: each occurrence of from[i] in subject becomes
     to[i]. Multiple froms matched left-to-right greedily.")

(defnix spec-replaceStrings-multi
  :source "builtins.replaceStrings [ \"foo\" \"bar\" ] [ \"baz\" \"qux\" ] \"foo and bar\""
  :expected-json "\"baz and qux\""
  :tags ("spec" "string" "verified")
  :note
    "Length-2 from/to arrays. Each pair is an independent rewrite rule
     applied in parallel (no from[i] result feeds into the next match).")

;; ── catAttrs — concat a field across a list of attrsets ──────────
;; sui implements this in sui-eval/src/builtins/attrs.rs.

(defnix spec-catAttrs-basic
  :source
    "builtins.catAttrs \"x\" [ { x = 1; y = 10; } { x = 2; } { y = 20; } { x = 3; } ]"
  :expected-json "[1,2,3]"
  :tags ("spec" "attrs" "verified")
  :note
    "Spec: project name out of each list element, skipping elements
     that don't have the attribute.")

;; ── groupBy — partition a list by a key function ─────────────────
;; sui implements this in sui-eval/src/builtins/lists.rs.

(defnix spec-groupBy-parity
  :source
    "let parity = n: if (n / 2) * 2 == n then \"even\" else \"odd\";
     in builtins.groupBy parity [ 1 2 3 4 5 ]"
  :expected-json "{\"even\":[2,4],\"odd\":[1,3,5]}"
  :tags ("spec" "list" "attrs" "verified")
  :note
    "Each elem goes into the bucket whose name is `f elem`. Bucket
     order follows input order (stable). Attrset keys are lex-sorted
     in to_json output.")

;; ── unsafeDiscardOutputDependency — string context trim ──────────
;; sui implements this in sui-eval/src/builtins/context.rs (stub for
;; non-derivation input, which is all we test here).

(defnix spec-unsafeDiscardOutputDependency-passthrough
  :source "builtins.unsafeDiscardOutputDependency \"hello\""
  :expected-json "\"hello\""
  :tags ("spec" "string-context" "verified")
  :note
    "Context-free input round-trips verbatim. Full coverage needs a
     derivation in the input to observe the actual context change —
     deferred to the derivation-store integration corpus.")

;; ── nixVersion — hardcoded in sui-eval/src/builtins/mod.rs:199 ───

(defnix spec-nixVersion-string
  :source "builtins.nixVersion"
  :expected-json "\"2.24.0\""
  :tags ("spec" "meta" "verified")
  :note
    "sui identifies as Nix 2.24.0 for compat. This test catches drift
     if someone changes the hardcoded value without a semantic reason.")

;; ── path — filtered path import ──────────────────────────────────
;; sui implements this in sui-eval/src/builtins/paths.rs but it
;; requires a real filesystem path that exists. The test below uses
;; a placeholder; we keep it skipped until we add a tmpdir fixture.

(defnix spec-path-import-basic
  :source "builtins.path { path = /tmp/nonexistent-oracle-path; name = \"x\"; }"
  :expected-json "\"\""
  :tags ("spec" "path" "needs-fixture")
  :skip #t
  :note
    "sui's builtins.path requires a real path. Promote to a proper
     test using tempfile fixtures so the store path is deterministic.
     Needs a structural matcher for /nix/store/HASH-NAME format too.")

;; ── fetchClosure — retrieve a closure from a binary cache ────────
;; NOT yet implemented in sui. Track-C gap (substituter client).

(defnix spec-fetchClosure-basic
  :source
    "builtins.fetchClosure {
       fromStore = \"https://cache.nixos.org\";
       fromPath = \"/nix/store/abc-hello\";
     }"
  :expected-json "\"/nix/store/abc-hello\""
  :tags ("spec" "fetcher" "flake-blocker" "missing")
  :skip #t
  :note
    "Not implemented. Fetcher-class builtin; requires substituter
     client (Track C gap). Expected is a placeholder — when we
     implement this, the result is the toPath (or fromPath when
     content-preserving).")
