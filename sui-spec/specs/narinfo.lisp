;; sui-spec/specs/narinfo.lisp — typed border for the cppnix narinfo
;; file format.  Today's only variant is the v1 baseline.

(defnarinfo-format
  :name "cppnix-narinfo-v1"
  :field-names ("StorePath" "URL" "Compression" "FileHash"
                "FileSize" "NarHash" "NarSize" "References"
                "Deriver" "System" "Sig" "CA")
  :fields (Required Required Required Optional
           Optional Required Required Optional
           Optional Optional Repeatable Optional)
  :phases ((:kind ParseTextFields)
           (:kind ValidateRequiredFields)
           (:kind ValidateNarHashShape)
           (:kind ParseSignatures)
           (:kind ParseReferences)
           (:kind EmitTextOutput)))
