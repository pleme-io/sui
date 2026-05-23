;; sui-spec/specs/nar.lisp — typed border for the Nix Archive (.nar)
;; format.  Today's only variant is the cppnix baseline; future
;; format extensions (xattrs, sparse files) land as additional
;; (defnar-format ...) forms.

(defnar-format
  :name        "cppnix-nar"
  :magic       "nix-archive-1"
  :encoding    LengthPrefixedString
  :entry-types (Regular Executable Directory Symlink)
  :phases ((:kind ReadMagic)
           (:kind ParseRootNode)
           (:kind StreamEntries  :bind "entries")
           (:kind BuildTree      :from "entries" :bind "tree")
           (:kind ValidateChecksum)))
