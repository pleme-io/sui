;; 08_flake_refs.lisp — parseFlakeRef + flakeRefToString coverage.
;; These test the core flake-ref grammar that nix flake's input
;; resolution builds on. Every input type supported by `inputs.foo.url`
;; should round-trip through parse → stringify cleanly.

;; ── github shorthand ─────────────────────────────────────────────

(defnix flake-github-simple
  :source "builtins.parseFlakeRef \"github:NixOS/nixpkgs\""
  :expected-json "{\"owner\":\"NixOS\",\"repo\":\"nixpkgs\",\"type\":\"github\"}"
  :tags ("flake" "parse" "github"))

(defnix flake-github-with-ref
  :source "builtins.parseFlakeRef \"github:NixOS/nixpkgs/nixos-unstable\""
  :expected-json "{\"owner\":\"NixOS\",\"ref\":\"nixos-unstable\",\"repo\":\"nixpkgs\",\"type\":\"github\"}"
  :tags ("flake" "parse" "github" "ref"))

(defnix flake-gitlab-simple
  :source "builtins.parseFlakeRef \"gitlab:owner/project\""
  :expected-json "{\"owner\":\"owner\",\"repo\":\"project\",\"type\":\"gitlab\"}"
  :tags ("flake" "parse" "gitlab"))

;; ── git+<scheme> ─────────────────────────────────────────────────

(defnix flake-git-https
  :source "builtins.parseFlakeRef \"git+https://example.com/foo.git\""
  :expected-json "{\"type\":\"git\",\"url\":\"https://example.com/foo.git\"}"
  :tags ("flake" "parse" "git"))

(defnix flake-git-ssh
  :source "builtins.parseFlakeRef \"git+ssh://git@example.com/foo.git\""
  :expected-json "{\"type\":\"git\",\"url\":\"ssh://git@example.com/foo.git\"}"
  :tags ("flake" "parse" "git"))

;; ── tarball+ ─────────────────────────────────────────────────────

(defnix flake-tarball
  :source "builtins.parseFlakeRef \"tarball+https://example.com/foo.tar.gz\""
  :expected-json "{\"type\":\"tarball\",\"url\":\"https://example.com/foo.tar.gz\"}"
  :tags ("flake" "parse" "tarball"))

;; ── path ─────────────────────────────────────────────────────────

(defnix flake-path-prefixed
  :source "builtins.parseFlakeRef \"path:/tmp/demo\""
  :expected-json "{\"path\":\"/tmp/demo\",\"type\":\"path\"}"
  :tags ("flake" "parse" "path"))

(defnix flake-path-bare
  :source "builtins.parseFlakeRef \"/tmp/demo\""
  :expected-json "{\"path\":\"/tmp/demo\",\"type\":\"path\"}"
  :tags ("flake" "parse" "path"))

;; ── indirect (NEW in this commit) ────────────────────────────────
;; `flake:nixpkgs` (and the bare-ID form commonly found in
;; `inputs.nixpkgs.url = "nixpkgs"`) now parse to a
;; type="indirect" attrset. Registry resolution that turns
;; indirect refs into concrete ones is still TODO — parsing is
;; the prerequisite.

(defnix flake-indirect-simple
  :source "builtins.parseFlakeRef \"flake:nixpkgs\""
  :expected-json "{\"id\":\"nixpkgs\",\"type\":\"indirect\"}"
  :tags ("flake" "parse" "indirect"))

(defnix flake-indirect-bare-id
  :source "builtins.parseFlakeRef \"nixpkgs\""
  :expected-json "{\"id\":\"nixpkgs\",\"type\":\"indirect\"}"
  :tags ("flake" "parse" "indirect")
  :note
    "Bare identifiers (no scheme prefix) resolve as indirect refs —
     CppNix does the same for inputs like `inputs.nixpkgs.url = \"nixpkgs\"`.")

(defnix flake-indirect-with-ref
  :source "builtins.parseFlakeRef \"flake:nixpkgs/nixos-unstable\""
  :expected-json "{\"id\":\"nixpkgs\",\"ref\":\"nixos-unstable\",\"type\":\"indirect\"}"
  :tags ("flake" "parse" "indirect" "ref"))

;; ── Round-trip — parse then stringify back ───────────────────────

(defnix flake-roundtrip-github
  :source
    "builtins.flakeRefToString (builtins.parseFlakeRef \"github:NixOS/nixpkgs\")"
  :expected-json "\"github:NixOS/nixpkgs\""
  :tags ("flake" "roundtrip"))

(defnix flake-roundtrip-indirect
  :source
    "builtins.flakeRefToString (builtins.parseFlakeRef \"flake:nixpkgs\")"
  :expected-json "\"flake:nixpkgs\""
  :tags ("flake" "roundtrip" "indirect"))

(defnix flake-roundtrip-indirect-ref
  :source
    "builtins.flakeRefToString (builtins.parseFlakeRef \"flake:nixpkgs/master\")"
  :expected-json "\"flake:nixpkgs/master\""
  :tags ("flake" "roundtrip" "indirect" "ref"))
