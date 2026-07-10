;; sui-spec/specs/coercion.lisp — value→string coercion at the eval→drv
;; boundary.  The complement of laziness.lisp: how a forced value
;; renders + what store-context it carries.  Each rule below captures a
;; sui root byte-verified against the live nix oracle.  Doctrine:
;; theory/BUILD.md §II.

;; ── copy-to-store path coercion (root ec238d7) ──────────────────────
;; A path in interpolation / a derivation attribute copies to the store
;; and tracks context.
(defcoercion-rule
  :name           "path-in-interpolation"
  :input-kind     "path"
  :context        Interpolation
  :copy-to-store  #t
  :tracks-context #t
  :yields         StorePathWithContext)

;; The SAME path under `builtins.toString` stays plain — no copy, no
;; context.  This distinction is what makes ec238d7 byte-correct.
(defcoercion-rule
  :name           "path-in-tostring"
  :input-kind     "path"
  :context        ToString
  :copy-to-store  #f
  :tracks-context #f
  :yields         PlainString)

;; ── string-context propagation (root a67c244) ───────────────────────
;; A string in interpolation carries its prior context forward (a
;; multi-output producer's `out` survives into the consumer's drv).
(defcoercion-rule
  :name           "string-in-interpolation"
  :input-kind     "string"
  :context        Interpolation
  :copy-to-store  #f
  :tracks-context #t
  :yields         PlainString)

;; ── fixed-output `out` (root 31f52e0) ───────────────────────────────
(defcoercion-rule
  :name           "fod-output"
  :input-kind     "derivation"
  :context        FodOutput
  :copy-to-store  #f
  :tracks-context #t
  :yields         StorePathWithContext)
