;; Typed store-operation primitives — declarative slice
;; selection for `sui store materialize` + future ops.
;;
;; Each (defstore-slice …) names a subset of /nix/store that
;; sui knows how to operate on safely.  Operators reference
;; slices by name when invoking sui store-ops commands.

;; ── small file sources slice ─────────────────────────────────
;;
;; Tiny sample (≤5 entries, ≤1 MB each) of paths whose name ends
;; in "-source".  Used by the materialize-verify pipeline:
;; round-trips NAR through sui's encoder + decoder, hashes both
;; sides, proves byte-equivalence.  Default destination is a
;; throwaway directory under /tmp.

(defstore-slice
  :name        "tiny-sources"
  :source-root  "/nix/store"
  :name-pattern ".*-source$"
  :max-entries  5
  :max-size-bytes 1048576)

;; ── tiny patches slice ───────────────────────────────────────
;;
;; Single-file patches under /nix/store.  Selecting these
;; exercises the file-only encoder path; total disk usage is
;; negligible.

(defstore-slice
  :name        "tiny-patches"
  :source-root  "/nix/store"
  :name-pattern ".*\\.patch$"
  :max-entries  3
  :max-size-bytes 524288)

;; ── tiny derivations slice ───────────────────────────────────
;;
;; .drv ATerm files.  Round-tripping these proves both NAR
;; encode/decode + the ATerm parser/serializer compose
;; correctly end-to-end.

(defstore-slice
  :name        "tiny-drvs"
  :source-root  "/nix/store"
  :name-pattern ".*\\.drv$"
  :max-entries  3
  :max-size-bytes 65536)
