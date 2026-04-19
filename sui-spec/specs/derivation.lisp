;; sui-spec/specs/derivation.lisp — CppNix derivation path algorithm,
;; authored as Lisp data instead of as imperative Rust code.
;;
;; This spec is THE source of truth for how `builtins.derivation`
;; computes `.drv` paths and output store paths.  Both the sui
;; tree-walker and the sui bytecode VM interpret this spec.  Four
;; spec bugs this session were fixes at this layer — each bug, if
;; we had been authoring here, would have been a one-line edit.
;;
;; ── Algorithm intuition ──────────────────────────────────────────
;;
;; CppNix constructs a derivation's identity in four phases:
;;
;;   1. Mask:       empty every output's path, plus any env entry
;;                  whose name matches an output name.  This is the
;;                  "unresolved form".
;;   2. Inner hash: sha256 of the unresolved ATerm serialization.
;;                  Call the hex digest `inner-hex`.
;;   3. Fill:       from `inner-hex` + output name, compute each
;;                  output's store path and write it back into BOTH
;;                  `outputs[name].path` AND `env[name]`.  The
;;                  derivation is now in "final form".
;;   4. Drv hash:   sha256 of the final ATerm → build the `.drv`
;;                  store path via `compute_drv_path`.
;;
;; Two bugs we fixed — #12 and #13 — were both in phase 1/2: sui
;; was hashing the unresolved form for BOTH the output paths AND
;; the .drv path, and was dropping env.out entirely instead of
;; masking it to "".  Here the two hashes are visibly distinct
;; slots (`inner-hex` vs `final-hex`) and the mask step is named.

(defderivation-algorithm
  :name "cppnix-input-addressed"
  :phases ((:kind MaskOutputsAndEnv)
           (:kind Serialize        :bind "unresolved")
           (:kind Sha256           :from "unresolved" :bind "unresolved-hex")
           (:kind ComputeOutputPaths                  :from-hash "unresolved-hex")
           (:kind FillPlaceholders)
           (:kind Serialize        :bind "final")
           (:kind Sha256           :from "final"      :bind "final-hex")
           (:kind ComputeDrvPath                      :from-hash "final-hex")))
