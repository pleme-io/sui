;; sui-spec/specs/registry.lisp — typed border for nix flake
;; registries.  Four scopes with precedence (lowest = checked first).

(defregistry-format
  :name         "cppnix-registry-flake-local"
  :version      2
  :scope        FlakeLocal
  :precedence   0
  :default-path "<flake>/flake.nix#nixConfig.flake-registry")

(defregistry-format
  :name         "cppnix-registry-user"
  :version      2
  :scope        User
  :precedence   1
  :default-path "~/.config/nix/registry.json")

(defregistry-format
  :name         "cppnix-registry-system"
  :version      2
  :scope        System
  :precedence   2
  :default-path "/etc/nix/registry.json")

(defregistry-format
  :name         "cppnix-registry-global"
  :version      2
  :scope        Global
  :precedence   3
  :default-path "https://channels.nixos.org/flake-registry.json")
