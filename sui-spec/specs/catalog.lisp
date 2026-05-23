;; sui-spec/specs/catalog.lisp — the substrate catalog.  One
;; `(defsubstrate-domain ...)` per authored sui-spec module.
;; When a new domain lands, its entry goes here in the same commit.

;; ── Working (full implementation on the substrate) ───────────────

(defsubstrate-domain
  :name               "derivation"
  :authoring-keywords ("defderivation-algorithm")
  :gate               Working
  :purpose            "Input-addressed + fixed-output + content-addressed derivation hashing → store path"
  :cppnix-mirror      "libstore/derivations.cc")

(defsubstrate-domain
  :name               "flake"
  :authoring-keywords ("defflake-shape")
  :gate               Working
  :purpose            "Top-level flake result shape — required keys + body-leak policy"
  :cppnix-mirror      "libflake/flake.cc")

;; ── M2: gated on module system implementation ────────────────────

(defsubstrate-domain
  :name               "module_system"
  :authoring-keywords ("defoption-type" "defpriority" "defmodule-eval-algorithm")
  :gate               M2TypedOnly
  :purpose            "lib.evalModules option-merge lattice — types, priorities, fixpoint pipeline"
  :cppnix-mirror      "lib/modules.nix")

;; ── M3: gated on module system + activation interpreters ─────────

(defsubstrate-domain
  :name               "activation_script"
  :authoring-keywords ("defactivation-script-algorithm")
  :gate               M3TypedOnly
  :purpose            "NixOS / Darwin / HM activation pipeline (toplevel → systemd/launchd → switch script)"
  :cppnix-mirror      "nixos/modules/system/activation, nix-darwin, home-manager")

(defsubstrate-domain
  :name               "fetcher"
  :authoring-keywords ("deffetcher")
  :gate               M3TypedOnly
  :purpose            "builtins.fetchurl / fetchTarball / fetchGit / fetchTree / path ingest layer"
  :cppnix-mirror      "libstore/builtins/fetchurl.cc, libexpr/primops/fetchTree.cc")

(defsubstrate-domain
  :name               "substituter"
  :authoring-keywords ("defsubstituter")
  :gate               M3TypedOnly
  :purpose            "Binary-cache substitution protocol — narinfo → NAR → import to store"
  :cppnix-mirror      "libstore/substitution-goal.cc, http-binary-cache-store.cc")

(defsubstrate-domain
  :name               "sandbox"
  :authoring-keywords ("defsandbox-spec")
  :gate               M3TypedOnly
  :purpose            "Build sandbox isolation contract — Linux ns/seccomp, Darwin sandbox-exec, off"
  :cppnix-mirror      "libstore/build/local-derivation-goal.cc")

(defsubstrate-domain
  :name               "gc"
  :authoring-keywords ("defgc-algorithm")
  :gate               M3TypedOnly
  :purpose            "Store garbage collector — roots → live set → dead set → delete"
  :cppnix-mirror      "libstore/gc.cc")

(defsubstrate-domain
  :name               "hash"
  :authoring-keywords ("defhash-algorithm" "defhash-encoding")
  :gate               M3TypedOnly
  :purpose            "Hash algorithm registry (sha1/256/512/md5/blake3) + encoding (base16/32/64/SRI)"
  :cppnix-mirror      "libutil/hash.cc")

(defsubstrate-domain
  :name               "nar"
  :authoring-keywords ("defnar-format")
  :gate               M3TypedOnly
  :purpose            "Nix Archive binary format — magic, framing, entry types"
  :cppnix-mirror      "libstore/nar-accessor.cc, libutil/serialise.cc")

(defsubstrate-domain
  :name               "narinfo"
  :authoring-keywords ("defnarinfo-format")
  :gate               M3TypedOnly
  :purpose            "Binary-cache narinfo text format — Required/Optional/Repeatable fields"
  :cppnix-mirror      "libstore/binary-cache-store.cc")

(defsubstrate-domain
  :name               "worker_protocol"
  :authoring-keywords ("defworker-protocol" "defworker-opcode")
  :gate               M3TypedOnly
  :purpose            "nix-daemon wire protocol — 32+ opcodes, handshake, WireType primitives"
  :cppnix-mirror      "libstore/worker-protocol.cc")

;; ── M4: gated on CA-derivation interpreter ───────────────────────

(defsubstrate-domain
  :name               "realisation"
  :authoring-keywords ("defrealisation-format")
  :gate               M4TypedOnly
  :purpose            "CA-drv post-build realisation records — drv-output id → store path mapping"
  :cppnix-mirror      "libstore/realisation.cc")

;; ── Informational (format declarations, no interpreter planned) ──

(defsubstrate-domain
  :name               "lock_file"
  :authoring-keywords ("deflock-file-format")
  :gate               Informational
  :purpose            "flake.lock structure — version, root, nodes graph, transitive resolution"
  :cppnix-mirror      "libflake/lockfile.cc")

(defsubstrate-domain
  :name               "registry"
  :authoring-keywords ("defregistry-format")
  :gate               Informational
  :purpose            "Flake registry precedence — FlakeLocal < User < System < Global"
  :cppnix-mirror      "libflake/flake.cc#registry handling")

(defsubstrate-domain
  :name               "store_layout"
  :authoring-keywords ("defstore-layout")
  :gate               Informational
  :purpose            "/nix/store + /nix/var/nix on-disk convention"
  :cppnix-mirror      "store directory convention")

(defsubstrate-domain
  :name               "eval_cache"
  :authoring-keywords ("defeval-cache-format")
  :gate               Informational
  :purpose            "Evaluation memoization — cppnix sqlite + sha256 vs sui redb + BLAKE3"
  :cppnix-mirror      "libcmd/installables.cc#eval-cache-v5.sqlite")

(defsubstrate-domain
  :name               "profile"
  :authoring-keywords ("defprofile-format")
  :gate               Informational
  :purpose            "nix-env / nix profile generation-link convention"
  :cppnix-mirror      "libstore/profiles.cc")

(defsubstrate-domain
  :name               "trust_model"
  :authoring-keywords ("deftrust-model")
  :gate               Informational
  :purpose            "Signature + substituter + user trust matrix (Permissive/MultiUser/Sealed)"
  :cppnix-mirror      "libstore/local-store.cc#trust handling")
