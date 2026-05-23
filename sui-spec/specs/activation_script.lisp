;; sui-spec/specs/activation_script.lisp — typed border for the
;; cppnix activation-script algorithms (NixOS / nix-darwin /
;; home-manager).  Three sibling algorithms, one per target.
;; Implementation is M3 work; this file names the contract.

;; ── NixOS (systemd, /run/current-system, switch-to-configuration) ─

(defactivation-script-algorithm
  :name   "cppnix-nixos"
  :target NixOS
  :phases ((:kind ResolveSystemBuildToplevel  :bind "toplevel")
           (:kind GenerateSystemdUnits        :from "toplevel" :bind "units")
           (:kind GenerateEtcSymlinks         :from "toplevel" :bind "etc")
           (:kind ResolveSecretRefs           :from "toplevel" :bind "secrets")
           (:kind ComposeActivationScript     :from "toplevel" :bind "script")
           (:kind WriteActivationDerivation   :from "script"   :bind "drv")))

;; ── nix-darwin (launchd, darwin-rebuild) ──────────────────────────

(defactivation-script-algorithm
  :name   "cppnix-darwin"
  :target Darwin
  :phases ((:kind ResolveSystemBuildToplevel  :bind "toplevel")
           (:kind GenerateLaunchdPlists       :from "toplevel" :bind "plists")
           (:kind GenerateEtcSymlinks         :from "toplevel" :bind "etc")
           (:kind ResolveSecretRefs           :from "toplevel" :bind "secrets")
           (:kind ComposeActivationScript     :from "toplevel" :bind "script")
           (:kind WriteActivationDerivation   :from "script"   :bind "drv")))

;; ── home-manager (per-user, no /etc, no system units) ─────────────

(defactivation-script-algorithm
  :name   "cppnix-home-manager"
  :target HomeManager
  :phases ((:kind ResolveSystemBuildToplevel  :bind "toplevel")
           (:kind GenerateLaunchdPlists       :from "toplevel" :bind "user-agents")
           (:kind ResolveSecretRefs           :from "toplevel" :bind "secrets")
           (:kind ComposeActivationScript     :from "toplevel" :bind "script")
           (:kind WriteActivationDerivation   :from "script"   :bind "drv")))
