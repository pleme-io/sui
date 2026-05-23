;; sui-spec/specs/store_layout.lisp — typed convention for the
;; on-disk shape of a nix store.

;; ── cppnix (the canonical layout) ────────────────────────────────

(defstore-layout
  :name        "cppnix"
  :store-root  "/nix/store"
  :state-root  "/nix/var/nix"
  :path-format HashDashName
  :aux-dirs ((:name "db"            :purpose Db)
             (:name "gcroots"       :purpose GcRoots)
             (:name "profiles"      :purpose Profiles)
             (:name "daemon-socket" :purpose DaemonSocket)
             (:name "temproots"     :purpose Temp)
             (:name "userpool"      :purpose UserState)
             (:name "eval-cache-v5" :purpose EvalCache)))

;; ── Single-user store (non-root, e.g. macOS without daemon) ──────

(defstore-layout
  :name        "cppnix-single-user"
  :store-root  "/nix/store"
  :state-root  "/nix/var/nix"
  :path-format HashDashName
  :aux-dirs ((:name "db"            :purpose Db)
             (:name "gcroots"       :purpose GcRoots)
             (:name "profiles"      :purpose Profiles)
             (:name "userpool"      :purpose UserState)
             (:name "eval-cache-v5" :purpose EvalCache)))
