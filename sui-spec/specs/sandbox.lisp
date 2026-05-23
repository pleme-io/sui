;; sui-spec/specs/sandbox.lisp — typed sandbox specs for the build
;; isolation layer.

;; ── Linux: strict (no network, no host paths) ────────────────────

(defsandbox-spec
  :name              "cppnix-linux-strict"
  :platform          Linux
  :isolation-tier    Strict
  :allowed-paths     ("/nix/store" "/dev/null")
  :network-allowed   #f
  :seccomp-profile   "deny-network-syscalls"
  :user-namespacing  #t)

;; ── Linux: FOD-allowed (network for fetchurl) ────────────────────

(defsandbox-spec
  :name              "cppnix-linux-fod"
  :platform          Linux
  :isolation-tier    Relaxed
  :allowed-paths     ("/nix/store" "/dev/null" "/etc/resolv.conf" "/etc/ssl/certs")
  :network-allowed   #t
  :seccomp-profile   "allow-network-syscalls"
  :user-namespacing  #t)

;; ── Darwin: strict (sandbox-exec profile) ────────────────────────

(defsandbox-spec
  :name              "cppnix-darwin-strict"
  :platform          Darwin
  :isolation-tier    Strict
  :allowed-paths     ("/nix/store" "/dev/null")
  :network-allowed   #f
  :user-namespacing  #f)

;; ── Darwin: FOD (sandbox-exec with network) ──────────────────────

(defsandbox-spec
  :name              "cppnix-darwin-fod"
  :platform          Darwin
  :isolation-tier    Relaxed
  :allowed-paths     ("/nix/store" "/dev/null")
  :network-allowed   #t
  :user-namespacing  #f)

;; ── Permissive (Darwin .app builds requiring host paths) ─────────

(defsandbox-spec
  :name              "cppnix-darwin-app-build"
  :platform          Darwin
  :isolation-tier    Permissive
  :allowed-paths     ("/nix/store" "/Applications" "/Library" "/private/tmp")
  :network-allowed   #t
  :user-namespacing  #f)

;; ── Off (last resort, e.g. nix-shell --pure builds that fail otherwise) ──

(defsandbox-spec
  :name              "no-sandbox"
  :platform          NoSandbox
  :isolation-tier    Off
  :allowed-paths     ()
  :network-allowed   #t
  :user-namespacing  #f)
