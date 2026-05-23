;; sui-spec/specs/lock_file.lisp — typed border for flake.lock.

(deflock-file-format
  :name            "cppnix-flake-lock-v7"
  :version         7
  :encoding        JsonText
  :required-fields ("version" "root" "nodes")
  :node-fields     ("inputs" "locked" "original" "flake")
  :phases ((:kind ParseJson)
           (:kind ValidateVersion)
           (:kind ValidateNodeGraph)
           (:kind ResolveTransitiveInputs)
           (:kind EmitCanonicalJson)))
