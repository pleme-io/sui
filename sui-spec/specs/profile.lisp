;; sui-spec/specs/profile.lisp — typed profile formats.

(defprofile-format
  :name                     "cppnix-system-profile"
  :kind                     System
  :generation-link-pattern  "<profile>-<N>-link"
  :manifest-path            "manifest.nix")

(defprofile-format
  :name                     "cppnix-user-profile-modern"
  :kind                     User
  :generation-link-pattern  "<profile>-<N>-link"
  :manifest-path            "manifest.json")

(defprofile-format
  :name                     "cppnix-ephemeral-shell"
  :kind                     Ephemeral
  :generation-link-pattern  "<tempdir>/shell-<N>-link"
  :manifest-path            "")
