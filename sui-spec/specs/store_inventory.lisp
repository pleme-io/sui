;; Typed store-inventory profiles.
;;
;; Each (defstore-inventory-profile …) declares HOW to walk a
;; Nix store.  Operators reference a profile by name when invoking
;; `sui store inventory <profile>`.

;; ── default — full /nix/store walk, no NAR hashing ──────────────

(defstore-inventory-profile
  :name             "default"
  :source-root      "/nix/store"
  :max-entries      0
  :skip-pattern     ""
  :compute-nar-hash #f)

;; ── tiny — first 20 entries, for smoke tests + demos ────────────

(defstore-inventory-profile
  :name             "tiny"
  :source-root      "/nix/store"
  :max-entries      20
  :skip-pattern     ""
  :compute-nar-hash #f)

;; ── sources-only — `.*-source$` names only ──────────────────────

(defstore-inventory-profile
  :name             "sources-only"
  :source-root      "/nix/store"
  :max-entries      0
  :skip-pattern     "^(?!.*-source$).*"
  :compute-nar-hash #f)

;; ── deep — full walk + NAR hashing per entry (expensive) ────────

(defstore-inventory-profile
  :name             "deep"
  :source-root      "/nix/store"
  :max-entries      0
  :skip-pattern     ""
  :compute-nar-hash #t)
