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
  :phases (;; ── Phase 1 — output placeholder paths ────────────────
           ;; Seed hash comes from the MODULO form with outputs+env
           ;; masked.  CppNix computes output paths from
           ;; `hashDerivationModulo(drv, maskOutputs=true)` — i.e.
           ;; each input-drv path in the ATerm is replaced by that
           ;; input's own recursive modulo hash before hashing.
           ;; Without the substitution, dependents see their output
           ;; paths shift whenever an input's .drv name bytes change
           ;; for non-semantic reasons (a property of Nix's
           ;; content-addressed design).
           (:kind MaskOutputsAndEnv)
           (:kind SerializeModulo  :bind "unresolved")
           (:kind Sha256           :from "unresolved" :bind "unresolved-hex")
           (:kind ComputeOutputPaths                  :from-hash "unresolved-hex")
           (:kind FillPlaceholders)
           ;; ── Phase 2 — the `.drv` path itself ─────────────────
           ;; The drvPath uses sha256 of the FINAL form (outputs
           ;; filled, input_derivations preserved AS THE REAL .drv
           ;; PATHS, not modulo-substituted).  These are the exact
           ;; bytes written to disk as the `.drv` file.  Refs in
           ;; the fingerprint come from input_derivations +
           ;; input_sources so `makeTextPath` can express the
           ;; dependency closure.
           (:kind Serialize        :bind "final")
           (:kind Sha256           :from "final"      :bind "final-hex")
           (:kind ComputeDrvPath                      :from-hash "final-hex")
           ;; ── Phase 3 — cache modulo for dependents ────────────
           ;; Someone downstream who depends on US will need our
           ;; `hashDerivationModulo` when they compute their own
           ;; drvPath.  That's sha256 of OUR modulo form (final
           ;; outputs, input_derivations substituted recursively).
           ;; We compute + cache it here; lookups go through the
           ;; thread-local store.
           (:kind SerializeModulo  :bind "modulo")
           (:kind Sha256           :from "modulo"     :bind "modulo-hex")
           (:kind CacheSelfModulo                     :from-hash "modulo-hex")))

;; ── Fixed-output derivation algorithm ─────────────────────────────
;;
;; Used by builtins.fetchurl, fetchTarball, fetchGit when an output
;; hash is supplied up front.  The output's store path derives from
;; the *content hash*, not the recipe.  This means an FOD's store
;; path is stable across recipe permutations as long as the resulting
;; bytes are identical — the property that makes binary caches work.
;;
;; The M3 implementation wires SeedFixedOutputHash to
;; sui_compat::store_path::compute_fixed_output_path; the rest of
;; the pipeline shares phases with input-addressed.

(defderivation-algorithm
  :name "cppnix-fixed-output"
  :phases ((:kind SeedFixedOutputHash)
           (:kind FillPlaceholders)
           (:kind Serialize        :bind "final")
           (:kind Sha256           :from "final"      :bind "final-hex")
           (:kind ComputeDrvPath                      :from-hash "final-hex")
           (:kind SerializeModulo  :bind "modulo")
           (:kind Sha256           :from "modulo"     :bind "modulo-hex")
           (:kind CacheSelfModulo                     :from-hash "modulo-hex")))

;; ── Content-addressed derivation algorithm ────────────────────────
;;
;; CA derivations defer output-path computation to build-time — the
;; output's store path is derived from the actually-realised content
;; hash, not the recipe.  At recipe time we emit *placeholders* the
;; builder rewrites once it knows the real hash.  Enabled by the
;; `ca-derivations` experimental feature on cppnix.
;;
;; M4 implementation wires MarkContentAddressed +
;; EmitCaPlaceholders to sui_compat::store_path.

(defderivation-algorithm
  :name "cppnix-content-addressed"
  :phases ((:kind MarkContentAddressed)
           (:kind MaskOutputsAndEnv)
           (:kind EmitCaPlaceholders)
           (:kind Serialize        :bind "final")
           (:kind Sha256           :from "final"      :bind "final-hex")
           (:kind ComputeDrvPath                      :from-hash "final-hex")
           (:kind SerializeModulo  :bind "modulo")
           (:kind Sha256           :from "modulo"     :bind "modulo-hex")
           (:kind CacheSelfModulo                     :from-hash "modulo-hex")))
