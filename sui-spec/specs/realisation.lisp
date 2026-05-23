;; sui-spec/specs/realisation.lisp — typed border for CA-drv
;; realisation records.

(defrealisation-format
  :name            "cppnix-realisation-v1"
  :version         1
  :encoding        JsonText
  :required-fields ("id" "outPath" "signatures" "dependentRealisations"))
