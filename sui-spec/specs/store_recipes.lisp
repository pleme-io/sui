;; Typed declarative store-operation pipelines.
;;
;; Each (defstore-recipe …) composes slice + transforms +
;; destination into one operator-callable artifact.  Run via
;; `sui store recipe <name>`.

;; ── redacted-sources ───────────────────────────────────────────
;;
;; Take the tiny-sources slice, redact secrets + strip shell
;; comments.  Useful for safe-to-ship reproductions of source
;; trees.

(defstore-recipe
  :name        "redacted-sources"
  :description "Tiny-sources slice with base64 secrets redacted + shell comments stripped"
  :slice       "tiny-sources"
  :transforms  ("redact-base64-secrets" "strip-shell-comments")
  :dest-suffix "redacted-sources")

;; ── clean-drvs ────────────────────────────────────────────────
;;
;; Materialize the tiny-drvs slice with shell-comment stripping.
;; (drvs are ATerm so comment-stripping is mostly a no-op; still
;; useful as a substrate determinism probe.)

(defstore-recipe
  :name        "clean-drvs"
  :description "Tiny-drvs slice with shell-style comment stripping (mostly no-op for ATerm)"
  :slice       "tiny-drvs"
  :transforms  ("strip-shell-comments")
  :dest-suffix "clean-drvs")

;; ── audit-sources ────────────────────────────────────────────
;;
;; Materialize the tiny-sources slice unchanged (no transforms)
;; — pure round-trip rematerialization that proves NAR byte-equiv
;; through the full pipeline.

(defstore-recipe
  :name        "audit-sources"
  :description "Tiny-sources rematerialized through the full pipeline with no transforms — round-trip determinism probe"
  :slice       "tiny-sources"
  :transforms  ()
  :dest-suffix "audit-sources")
