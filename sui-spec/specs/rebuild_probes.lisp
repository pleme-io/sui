;; sui-spec/specs/rebuild_probes.lisp — host-aware rebuild parity corpus.
;;
;; Each (defrebuild-probe …) tests ONE stage of the real fleet rebuild
;; against the cppnix oracle, on the operator's machine, without ever
;; mutating the system.  Stages map directly onto the phases in
;; pleme-io/fleet/src/commands/rebuild.rs (flake show / check / eval
;; toplevel / dry-run closure / closure shape), one probe per stage
;; plus per-input lock-hash parity probes that catch the registry
;; resolver before the cascade.
;;
;; Tag conventions:
;;   "smoke"            — fast probes that gate the rest of the sweep
;;   "rebuild-phase-1"  — flake metadata
;;   "rebuild-phase-2"  — flake input resolution + lock-hash parity
;;   "rebuild-phase-3"  — module-system eval (toplevel + home activation)
;;   "rebuild-phase-4"  — build closure shape
;;   "expensive"        — probes that build or walk the full closure
;;   "darwin" / "nixos" — OS-specific (auto-skipped on other OS)
;;
;; The sweep auto-skips probes whose stage `:target` doesn't match the
;; operator's OS (Darwin probes self-skip on Linux and vice versa) —
;; nothing manual is required at probe-author time.

;; ── Phase 1: flake metadata ───────────────────────────────────────

(defrebuild-probe
  :name      "flake-show-keys"
  :host-mode Current
  :stage     (:kind FlakeShowKeys)
  :compare   AttrNamesEqual
  :tags      ("smoke" "rebuild-phase-1"))

(defrebuild-probe
  :name      "flake-check-exit"
  :host-mode Current
  :stage     (:kind FlakeCheckExit)
  :compare   ExitCode
  :tags      ("rebuild-phase-1"))

;; ── Phase 2: per-input lock-hash parity ───────────────────────────
;;
;; The registry resolver in sui must agree with cppnix on every input's
;; narHash.  Catches early divergences before they cascade into the
;; module-system + closure layers.  Each probe is independent — a flake
;; that doesn't declare a given input will surface as `SuiFailOnly` or
;; `BothFail` and is filtered later by the report consumer.

(defrebuild-probe
  :name      "input-lock-hash-nixpkgs"
  :host-mode Current
  :stage     (:kind InputLockHash :input "nixpkgs")
  :compare   JsonEqual
  :tags      ("rebuild-phase-2" "flake-input"))

(defrebuild-probe
  :name      "input-lock-hash-home-manager"
  :host-mode Current
  :stage     (:kind InputLockHash :input "home-manager")
  :compare   JsonEqual
  :tags      ("rebuild-phase-2" "flake-input"))

(defrebuild-probe
  :name      "input-lock-hash-nix-darwin"
  :host-mode Current
  :stage     (:kind InputLockHash :input "nix-darwin")
  :compare   JsonEqual
  :tags      ("rebuild-phase-2" "flake-input" "darwin"))

(defrebuild-probe
  :name      "input-lock-hash-substrate"
  :host-mode Current
  :stage     (:kind InputLockHash :input "substrate")
  :compare   JsonEqual
  :tags      ("rebuild-phase-2" "flake-input"))

(defrebuild-probe
  :name      "input-lock-hash-sops-nix"
  :host-mode Current
  :stage     (:kind InputLockHash :input "sops-nix")
  :compare   JsonEqual
  :tags      ("rebuild-phase-2" "flake-input"))

;; ── Phase 3: module-system eval ───────────────────────────────────
;;
;; This is the load-bearing module-system gap as of 2026-05-22 — sui
;; can evaluate the Nix syntax but doesn't yet realize the lattice merge
;; required to produce darwinConfigurations.<host>.system.build.toplevel.
;; M0 ships these probes to record the gap precisely; success on these
;; is the M2 ship criterion.

(defrebuild-probe
  :name      "darwin-toplevel-outpath"
  :host-mode Current
  :stage     (:kind EvalToplevel :target Darwin)
  :compare   JsonEqual
  :tags      ("rebuild-phase-3" "module-system" "darwin"))

(defrebuild-probe
  :name      "nixos-toplevel-outpath"
  :host-mode Current
  :stage     (:kind EvalToplevel :target NixOS)
  :compare   JsonEqual
  :tags      ("rebuild-phase-3" "module-system" "nixos"))

(defrebuild-probe
  :name      "home-activation-outpath"
  :host-mode Current
  :stage     (:kind EvalHomeActivation)
  :compare   JsonEqual
  :tags      ("rebuild-phase-3" "home-manager"))

;; ── Phase 4: closure shape ────────────────────────────────────────

(defrebuild-probe
  :name      "darwin-dry-run-closure"
  :host-mode Current
  :stage     (:kind DryRunClosure :target Darwin)
  :compare   StorePathSet
  :tags      ("rebuild-phase-4" "build-graph" "darwin"))

(defrebuild-probe
  :name      "nixos-dry-run-closure"
  :host-mode Current
  :stage     (:kind DryRunClosure :target NixOS)
  :compare   StorePathSet
  :tags      ("rebuild-phase-4" "build-graph" "nixos"))

(defrebuild-probe
  :name      "darwin-closure-size-sentinel"
  :host-mode Current
  :stage     (:kind ClosureSize :target Darwin)
  :compare   IntegerEqual
  :tags      ("rebuild-phase-4" "closure" "darwin" "expensive"))

(defrebuild-probe
  :name      "darwin-closure-references"
  :host-mode Current
  :stage     (:kind ClosureReferenceGraph :target Darwin)
  :compare   GraphIsomorphic
  :tags      ("rebuild-phase-4" "closure" "darwin" "expensive"))

(defrebuild-probe
  :name      "nixos-closure-references"
  :host-mode Current
  :stage     (:kind ClosureReferenceGraph :target NixOS)
  :compare   GraphIsomorphic
  :tags      ("rebuild-phase-4" "closure" "nixos" "expensive"))
