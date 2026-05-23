;; sui-spec/specs/trust_model.lisp — typed trust postures.

;; ── Single-user (permissive) ─────────────────────────────────────

(deftrust-model
  :name                  "single-user-permissive"
  :trusted-public-keys   ("cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=")
  :trusted-substituters  ("https://cache.nixos.org")
  :trusted-users         ("*")
  :posture               Permissive)

;; ── Multi-user (cppnix default) ──────────────────────────────────

(deftrust-model
  :name                  "multi-user-default"
  :trusted-public-keys   ("cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=")
  :trusted-substituters  ("https://cache.nixos.org")
  :trusted-users         ("@wheel")
  :posture               MultiUser)

;; ── Sealed (compliance / air-gapped) ─────────────────────────────

(deftrust-model
  :name                  "sealed-compliance"
  :trusted-public-keys   ()
  :trusted-substituters  ()
  :trusted-users         ("root")
  :posture               Sealed)
