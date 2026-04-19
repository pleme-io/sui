;; sui-spec/specs/flake.lisp — getFlake/callFlake result shape policy.
;;
;; The top-level attrset CppNix builds for `(builtins.getFlake …)`
;; has a specific inventory:
;;
;;   _type       = "flake";         -- the marker
;;   outPath     = <store path>;    -- source root as copied to store
;;   sourceInfo  = { … };           -- { outPath, narHash, lastModified… }
;;   inputs      = { … };           -- resolved input attrsets
;;   outputs     = <raw fn result>; -- accessible via .outputs
;;   …           = <raw fn result>; -- ALSO spread at the top level
;;
;; And specifically NOT:
;;
;;   description  — lives only in sourceInfo (if anywhere)
;;   nixConfig    — not surfaced
;;
;; Historic bug: sui's flake assembler iterated the parsed flake
;; body and copied every unclaimed key onto the top level — which
;; leaked `description` and any other metadata the flake author
;; had declared.  The first cross-repo parity sweep caught this
;; on every flake that sets a description.  Fix = this spec.

(defflake-shape
  :name                       "cppnix"
  :type-marker                "flake"
  :required-keys              ("_type" "outPath" "sourceInfo"
                               "inputs" "outputs" "narHash")
  :spread-from-output-fn       #t
  :never-leak-from-flake-body ("description" "nixConfig" "formatter"))
