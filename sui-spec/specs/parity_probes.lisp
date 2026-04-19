;; sui-spec/specs/parity_probes.lisp — cross-repo parity probe corpus.
;;
;; Each `(defprobe …)` is a typed question of the form "does sui
;; agree with CppNix when you ask this?".  The runner
;; (`src/bin/sui-sweep.rs`) walks every pleme-io flake, substitutes
;; `$FLAKE` with that flake's absolute path, runs both engines,
;; classifies the result, and reports.
;;
;; Adding a new probe = adding one (defprobe …) form.  Promoting a
;; probe to a permanent regression guard = adding "regression" to
;; its :tags.
;;
;; Starter corpus — the same seven probes the first session's bash
;; sweep ran, now authored declaratively:

(defprobe
  :name     "getflake-outPath"
  :expr     "(builtins.getFlake \"path:$FLAKE\").outPath"
  :classify JsonEqual
  :tags     ("smoke" "drop-in-replacement"))

(defprobe
  :name     "getflake-inputs-keys"
  :expr     "builtins.attrNames ((builtins.getFlake \"path:$FLAKE\").inputs or {})"
  :classify JsonEqual
  :tags     ("smoke" "flake-shape"))

(defprobe
  :name     "getflake-outputs-keys"
  :expr     "builtins.attrNames (builtins.getFlake \"path:$FLAKE\")"
  :classify JsonEqual
  :tags     ("smoke" "flake-shape" "regression"))

(defprobe
  :name     "getflake-packages-keys"
  :expr     "builtins.attrNames ((builtins.getFlake \"path:$FLAKE\").packages.aarch64-darwin or {})"
  :classify JsonEqual
  :tags     ("smoke" "flake-outputs"))

(defprobe
  :name     "getflake-devShells-keys"
  :expr     "builtins.attrNames ((builtins.getFlake \"path:$FLAKE\").devShells.aarch64-darwin or {})"
  :classify JsonEqual
  :tags     ("smoke" "flake-outputs"))

(defprobe
  :name     "getflake-overlays-keys"
  :expr     "builtins.attrNames ((builtins.getFlake \"path:$FLAKE\").overlays or {})"
  :classify JsonEqual
  :tags     ("smoke" "flake-outputs"))

(defprobe
  :name     "getflake-homeManagerModules-keys"
  :expr     "builtins.attrNames ((builtins.getFlake \"path:$FLAKE\").homeManagerModules or {})"
  :classify JsonEqual
  :tags     ("smoke" "flake-outputs"))
