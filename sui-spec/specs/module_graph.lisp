;; Typed border for the L4 ModuleGraph — the compiled, slice-aware,
;; content-addressed representation of a NixOS / nix-darwin /
;; home-manager module system.
;;
;; ## Why this exists
;;
;; CppNix re-evaluates the entire module fixed point on every rebuild,
;; even when zero module files changed. For rio's nixosConfigurations
;; (~500-2000 modules, ~5-20 import levels deep) this is the dominant
;; cost of `nixos-rebuild` — ~24 seconds on the rio sweep we ran in
;; the Phase-1 analysis.
;;
;; The L4 substrate compiles each module's config setter into a typed
;; assignment closure once per source-tree state. Two pivotal wins:
;;
;;   1. Worker/wrapper split (GHC). Each setter declares the **slice**
;;      of `config` it actually reads ("input slice"). Worker computes
;;      the contribution; wrapper projects the slice. The fixed-point
;;      solver re-fires a module only when its declared slice changes.
;;   2. Defunctionalization (MLton). Higher-order setters
;;      ({config, lib, pkgs, ...}: { ... }) become first-order closures
;;      indexed by a small integer tag. The fixed point becomes a
;;      bounded graph problem.
;;
;; Cache key = (graph-hash of typed-AST hashes of every module file ⊕
;; resolved import set). Same cache key → identical compiled closure
;; → execute, not resolve.
;;
;; ## Wire shape
;;
;; This file holds *fixture* instances used by tests. Production
;; ModuleGraphs are built in memory from a slice of AstGraphs and
;; persisted as rkyv blobs in sui-graph-store::GraphKind::Module.

;; ── Fixture 1: minimal one-module graph ──────────────────────────
;; { config, lib, pkgs, ... }: { networking.hostName = "rio"; }

(defmodule-graph-fixture
  :name        "single-module-hostname"
  :module-count 1
  :setter-count 1
  :option-count 1
  :slice-count  0
  :notes       "Smallest module graph. One setter writes one option, reads no slice.")

;; ── Fixture 2: two modules with slice dependency ─────────────────
;; Module A: { config, ... }: { networking.hostName = "rio"; }
;; Module B: { config, ... }: { boot.kernelParams =
;;   if config.networking.hostName == "rio" then ["amd_pstate=active"]
;;   else []; }
;; → Module B has slice ["networking.hostName"]; rebuilds only when
;;   that slice changes (not when any other option changes).

(defmodule-graph-fixture
  :name        "two-modules-with-slice"
  :module-count 2
  :setter-count 2
  :option-count 2
  :slice-count  1
  :notes       "Exercises the slice-keyed re-firing primitive.")

;; ── Fixture 3: imports chain ─────────────────────────────────────
;; Root imports profile A which imports component B which imports
;; component C. Topological closure size = 4.

(defmodule-graph-fixture
  :name        "imports-chain-depth-4"
  :module-count 4
  :setter-count 4
  :option-count 4
  :slice-count  0
  :notes       "Exercises topological discovery + SCC detection on import edges.")
