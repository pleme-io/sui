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

;; ── Lazy evaluation edge cases ────────────────────────────────────
;;
;; Each probe pins down a different load-bearing laziness property
;; of the evaluator.  Any divergence here would surface a serious
;; semantics bug.

(defprobe
  :name     "lazy-forward-reference-in-let"
  :expr     "let _ = \"$FLAKE\"; in let a = b; b = 7; in a"
  :classify JsonEqual
  :tags     ("lazy" "let-in"))

(defprobe
  :name     "lazy-recursive-attrset"
  :expr     "let _ = \"$FLAKE\"; in rec { a = 1; b = a + 1; c = b * 2; }.c"
  :classify JsonEqual
  :tags     ("lazy" "recursive-attrs"))

(defprobe
  :name     "lazy-fixpoint-fold"
  :expr     "let _ = \"$FLAKE\"; in builtins.foldl' (acc: x: acc + x) 0 [ 1 2 3 4 5 6 7 8 9 10 ]"
  :classify JsonEqual
  :tags     ("lazy" "foldl"))

(defprobe
  :name     "lazy-only-touches-demanded-keys"
  :expr     "let _ = \"$FLAKE\"; in (let s = { a = 1; b = throw \"poisoned\"; }; in s.a)"
  :classify JsonEqual
  :tags     ("lazy" "selection" "regression"))

(defprobe
  :name     "lazy-thunk-memoization-no-double-eval"
  :expr     "let _ = \"$FLAKE\"; t = 1 + 2; in [ t t t ]"
  :classify JsonEqual
  :tags     ("lazy" "memoization"))

(defprobe
  :name     "lazy-with-scope-no-shadow-eager"
  :expr     "let _ = \"$FLAKE\"; in (with { a = 1; b = throw \"poison\"; }; a)"
  :classify JsonEqual
  :tags     ("lazy" "with-scope" "regression"))

;; ── Builtin coverage gaps (beyond the smoke corpus) ──────────────

(defprobe
  :name     "lib-length-of-empty"
  :expr     "let _ = \"$FLAKE\"; in builtins.length []"
  :classify JsonEqual
  :tags     ("builtins" "lists"))

(defprobe
  :name     "lib-elem-membership"
  :expr     "let _ = \"$FLAKE\"; in builtins.elem 3 [ 1 2 3 4 5 ]"
  :classify JsonEqual
  :tags     ("builtins" "lists"))

(defprobe
  :name     "lib-listToAttrs-roundtrip"
  :expr     "let _ = \"$FLAKE\"; in builtins.listToAttrs [ { name = \"a\"; value = 1; } { name = \"b\"; value = 2; } ]"
  :classify JsonEqual
  :tags     ("builtins" "attrs"))

(defprobe
  :name     "lib-mapAttrs-doubles-values"
  :expr     "let _ = \"$FLAKE\"; in builtins.mapAttrs (_: v: v * 2) { a = 1; b = 2; c = 3; }"
  :classify JsonEqual
  :tags     ("builtins" "attrs"))

(defprobe
  :name     "lib-attrValues-orders"
  :expr     "let _ = \"$FLAKE\"; in builtins.attrValues { c = 3; a = 1; b = 2; }"
  :classify JsonEqual
  :tags     ("builtins" "attrs" "ordering"))

(defprobe
  :name     "lib-genList-shape"
  :expr     "let _ = \"$FLAKE\"; in builtins.genList (i: i * i) 5"
  :classify JsonEqual
  :tags     ("builtins" "lists"))

(defprobe
  :name     "lib-concatMap-flattens"
  :expr     "let _ = \"$FLAKE\"; in builtins.concatMap (n: [ n n ]) [ 1 2 3 ]"
  :classify JsonEqual
  :tags     ("builtins" "lists"))

(defprobe
  :name     "lib-filter-empty"
  :expr     "let _ = \"$FLAKE\"; in builtins.filter (_: false) [ 1 2 3 ]"
  :classify JsonEqual
  :tags     ("builtins" "lists"))

(defprobe
  :name     "lib-replaceStrings-shape"
  :expr     "let _ = \"$FLAKE\"; in builtins.replaceStrings [ \"a\" \"b\" ] [ \"X\" \"Y\" ] \"abcab\""
  :classify JsonEqual
  :tags     ("builtins" "strings"))

(defprobe
  :name     "lib-isInt-true"
  :expr     "let _ = \"$FLAKE\"; in builtins.isInt 42"
  :classify JsonEqual
  :tags     ("builtins" "type-predicates"))

(defprobe
  :name     "lib-isAttrs-true"
  :expr     "let _ = \"$FLAKE\"; in builtins.isAttrs { x = 1; }"
  :classify JsonEqual
  :tags     ("builtins" "type-predicates"))

(defprobe
  :name     "lib-isList-true"
  :expr     "let _ = \"$FLAKE\"; in builtins.isList [ 1 2 ]"
  :classify JsonEqual
  :tags     ("builtins" "type-predicates"))

;; ── Numeric edge cases ────────────────────────────────────────────

(defprobe
  :name     "math-integer-overflow-bound"
  :expr     "let _ = \"$FLAKE\"; in 9223372036854775807 - 1"
  :classify JsonEqual
  :tags     ("math" "boundary"))

(defprobe
  :name     "math-float-precision"
  :expr     "let _ = \"$FLAKE\"; in builtins.div 10.0 3.0"
  :classify JsonEqual
  :tags     ("math" "float"))

(defprobe
  :name     "math-bitwise-and"
  :expr     "let _ = \"$FLAKE\"; in builtins.bitAnd 12 10"
  :classify JsonEqual
  :tags     ("math" "bitwise"))

(defprobe
  :name     "math-bitwise-or"
  :expr     "let _ = \"$FLAKE\"; in builtins.bitOr 5 3"
  :classify JsonEqual
  :tags     ("math" "bitwise"))

;; ── String coercion + interpolation ──────────────────────────────

(defprobe
  :name     "str-interpolation-mixed"
  :expr     "let _ = \"$FLAKE\"; x = 7; y = \"world\"; in \"hello ${y} ${toString x}\""
  :classify JsonEqual
  :tags     ("strings" "interpolation"))

(defprobe
  :name     "str-concatStringsSep"
  :expr     "let _ = \"$FLAKE\"; in builtins.concatStringsSep \"/\" [ \"a\" \"b\" \"c\" ]"
  :classify JsonEqual
  :tags     ("strings" "compose"))

(defprobe
  :name     "str-substring-out-of-range"
  :expr     "let _ = \"$FLAKE\"; in builtins.substring 0 100 \"short\""
  :classify JsonEqual
  :tags     ("strings" "boundary"))

;; ── Path operations ──────────────────────────────────────────────

(defprobe
  :name     "path-baseNameOf-trailing-slash"
  :expr     "let _ = \"$FLAKE\"; in builtins.baseNameOf \"/nix/store/abc/\""
  :classify JsonEqual
  :tags     ("paths" "boundary"))

(defprobe
  :name     "path-dirOf-relative"
  :expr     "let _ = \"$FLAKE\"; in builtins.dirOf \"a/b/c\""
  :classify JsonEqual
  :tags     ("paths" "boundary"))

;; ── Function semantics ───────────────────────────────────────────

(defprobe
  :name     "fn-partial-application"
  :expr     "let _ = \"$FLAKE\"; add = a: b: a + b; in (add 3) 4"
  :classify JsonEqual
  :tags     ("functions" "currying"))

(defprobe
  :name     "fn-args-destructure-with-default"
  :expr     "let _ = \"$FLAKE\"; f = { a, b ? 7 }: a + b; in f { a = 3; }"
  :classify JsonEqual
  :tags     ("functions" "args"))

(defprobe
  :name     "fn-args-ellipsis"
  :expr     "let _ = \"$FLAKE\"; f = { a, ... }@args: args.b or 0; in f { a = 1; b = 2; }"
  :classify JsonEqual
  :tags     ("functions" "args"))

;; ── Conditional + boolean logic ──────────────────────────────────

(defprobe
  :name     "if-then-else-strict-condition"
  :expr     "let _ = \"$FLAKE\"; in if 1 < 2 then \"yes\" else \"no\""
  :classify JsonEqual
  :tags     ("control" "if"))

(defprobe
  :name     "bool-short-circuit-and"
  :expr     "let _ = \"$FLAKE\"; in false && (throw \"poison\")"
  :classify JsonEqual
  :tags     ("control" "short-circuit" "regression"))

(defprobe
  :name     "bool-short-circuit-or"
  :expr     "let _ = \"$FLAKE\"; in true || (throw \"poison\")"
  :classify JsonEqual
  :tags     ("control" "short-circuit" "regression"))

;; ── Flake-input shape probes ─────────────────────────────────────

(defprobe
  :name     "getflake-inputs-self-is-derivation-like"
  :expr     "((builtins.getFlake \"path:$FLAKE\").inputs.self.outPath or \"missing\") != \"\""
  :classify JsonEqual
  :tags     ("flake-input" "self"))

(defprobe
  :name     "getflake-result-has-required-type"
  :expr     "((builtins.getFlake \"path:$FLAKE\")._type or \"missing\")"
  :classify JsonEqual
  :tags     ("flake-shape" "regression"))

;; ── M0 rebuild-flow expansion — added 2026-05-23 ──────────────────
;;
;; Forty additional probes targeting the surface a fleet rebuild
;; actually touches.  Each captures a question the rebuild flow asks
;; of nix that sui must mirror byte-for-byte to drive
;; `darwin-rebuild switch` end-to-end.  Grouped by category for
;; targeted include/exclude via `sui-sweep --tag <name>`.

;; ── Module-system depth: darwinConfigurations.<host>.config ─────

(defprobe
  :name     "rebuild-darwin-systemPackages-length"
  :expr     "builtins.length (builtins.getFlake \"path:$FLAKE\").darwinConfigurations.$HOST.config.environment.systemPackages"
  :classify JsonEqual
  :tags     ("rebuild" "module-system" "darwin"))

(defprobe
  :name     "rebuild-darwin-systemPackages-pnames"
  :expr     "map (p: p.pname or p.name or \"unknown\") (builtins.getFlake \"path:$FLAKE\").darwinConfigurations.$HOST.config.environment.systemPackages"
  :classify JsonEqual
  :tags     ("rebuild" "module-system" "darwin"))

(defprobe
  :name     "rebuild-darwin-environment-variables-keys"
  :expr     "builtins.attrNames (builtins.getFlake \"path:$FLAKE\").darwinConfigurations.$HOST.config.environment.variables"
  :classify JsonEqual
  :tags     ("rebuild" "module-system" "darwin"))

(defprobe
  :name     "rebuild-darwin-activationScripts-keys"
  :expr     "builtins.attrNames (builtins.getFlake \"path:$FLAKE\").darwinConfigurations.$HOST.config.system.activationScripts"
  :classify JsonEqual
  :tags     ("rebuild" "module-system" "darwin" "activation"))

(defprobe
  :name     "rebuild-darwin-system-build-keys"
  :expr     "builtins.attrNames (builtins.getFlake \"path:$FLAKE\").darwinConfigurations.$HOST.config.system.build"
  :classify JsonEqual
  :tags     ("rebuild" "module-system" "darwin" "build"))

(defprobe
  :name     "rebuild-darwin-launchd-daemons-keys"
  :expr     "builtins.attrNames ((builtins.getFlake \"path:$FLAKE\").darwinConfigurations.$HOST.config.launchd.daemons or {})"
  :classify JsonEqual
  :tags     ("rebuild" "module-system" "darwin" "launchd"))

(defprobe
  :name     "rebuild-darwin-nix-package-pname"
  :expr     "(builtins.getFlake \"path:$FLAKE\").darwinConfigurations.$HOST.config.nix.package.pname or \"unknown\""
  :classify JsonEqual
  :tags     ("rebuild" "module-system" "darwin"))

;; ── home-manager activation depth ───────────────────────────────

(defprobe
  :name     "rebuild-hm-packages-length"
  :expr     "builtins.length ((builtins.getFlake \"path:$FLAKE\").homeConfigurations.$USER.config.home.packages or [])"
  :classify JsonEqual
  :tags     ("rebuild" "module-system" "home-manager"))

(defprobe
  :name     "rebuild-hm-packages-pnames"
  :expr     "map (p: p.pname or p.name or \"unknown\") ((builtins.getFlake \"path:$FLAKE\").homeConfigurations.$USER.config.home.packages or [])"
  :classify JsonEqual
  :tags     ("rebuild" "module-system" "home-manager"))

(defprobe
  :name     "rebuild-hm-sessionVariables-keys"
  :expr     "builtins.attrNames ((builtins.getFlake \"path:$FLAKE\").homeConfigurations.$USER.config.home.sessionVariables or {})"
  :classify JsonEqual
  :tags     ("rebuild" "module-system" "home-manager"))

(defprobe
  :name     "rebuild-hm-stateVersion"
  :expr     "(builtins.getFlake \"path:$FLAKE\").homeConfigurations.$USER.config.home.stateVersion or \"unknown\""
  :classify JsonEqual
  :tags     ("rebuild" "module-system" "home-manager"))

;; ── Flake metadata depth ────────────────────────────────────────

(defprobe
  :name     "rebuild-flake-input-narHash-nixpkgs"
  :expr     "(builtins.getFlake \"path:$FLAKE\").inputs.nixpkgs.narHash or \"missing\""
  :classify JsonEqual
  :tags     ("rebuild" "flake-input" "narhash"))

(defprobe
  :name     "rebuild-flake-input-lastModified-nixpkgs"
  :expr     "(builtins.getFlake \"path:$FLAKE\").inputs.nixpkgs.lastModified or 0"
  :classify JsonEqual
  :tags     ("rebuild" "flake-input"))

(defprobe
  :name     "rebuild-flake-input-rev-nixpkgs"
  :expr     "(builtins.getFlake \"path:$FLAKE\").inputs.nixpkgs.rev or \"\""
  :classify JsonEqual
  :tags     ("rebuild" "flake-input"))

(defprobe
  :name     "rebuild-flake-self-narHash"
  :expr     "(builtins.getFlake \"path:$FLAKE\").inputs.self.narHash or \"missing\""
  :classify JsonEqual
  :tags     ("rebuild" "flake-input" "self"))

(defprobe
  :name     "rebuild-flake-self-shortRev"
  :expr     "(builtins.getFlake \"path:$FLAKE\").inputs.self.shortRev or \"\""
  :classify JsonEqual
  :tags     ("rebuild" "flake-input" "self"))

;; ── Derivation algorithm: outPath round-trips ───────────────────

(defprobe
  :name     "rebuild-deriv-trivial-outpath"
  :expr     "(derivation { name = \"trivial-probe\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; args = [\"-c\" \"exit 0\"]; }).outPath"
  :classify JsonEqual
  :tags ("rebuild" "derivation" "outpath" "context-free"))

(defprobe
  :name     "rebuild-deriv-drv-path"
  :expr     "(derivation { name = \"trivial-probe\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; args = [\"-c\" \"exit 0\"]; }).drvPath"
  :classify JsonEqual
  :tags ("rebuild" "derivation" "drvpath" "context-free"))

(defprobe
  :name     "rebuild-deriv-with-env-outpath"
  :expr     "(derivation { name = \"env-probe\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; args = [\"-c\" \"exit 0\"]; FOO = \"bar\"; }).outPath"
  :classify JsonEqual
  :tags ("rebuild" "derivation" "outpath" "context-free"))

(defprobe
  :name     "rebuild-deriv-fod-outpath"
  :expr     "(derivation { name = \"fod-probe\"; system = \"aarch64-darwin\"; builder = \"/bin/sh\"; args = [\"-c\" \"exit 0\"]; outputHash = \"0000000000000000000000000000000000000000000000000000\"; outputHashAlgo = \"sha256\"; outputHashMode = \"flat\"; }).outPath"
  :classify JsonEqual
  :tags ("rebuild" "derivation" "fod" "outpath" "context-free"))

;; ── Builtin coverage gaps the rebuild exercises ─────────────────

(defprobe
  :name     "builtin-hashString-sha256-empty"
  :expr     "builtins.hashString \"sha256\" \"\""
  :classify JsonEqual
  :tags ("rebuild" "builtin" "hash" "context-free"))

(defprobe
  :name     "builtin-hashString-sha256-hello"
  :expr     "builtins.hashString \"sha256\" \"hello\""
  :classify JsonEqual
  :tags ("rebuild" "builtin" "hash" "context-free"))

(defprobe
  :name     "builtin-hashString-md5-hello"
  :expr     "builtins.hashString \"md5\" \"hello\""
  :classify JsonEqual
  :tags ("rebuild" "builtin" "hash" "context-free"))

(defprobe
  :name     "builtin-toFile-roundtrip"
  :expr     "builtins.readFile (builtins.toFile \"x\" \"hello world\")"
  :classify JsonEqual
  :tags ("rebuild" "builtin" "io" "context-free"))

(defprobe
  :name     "builtin-pathExists-flake-self"
  :expr     "builtins.pathExists \"$FLAKE/flake.nix\""
  :classify JsonEqual
  :tags     ("rebuild" "builtin" "io"))

(defprobe
  :name     "builtin-readDir-flake-self-keys"
  :expr     "builtins.attrNames (builtins.readDir \"$FLAKE\")"
  :classify JsonEqual
  :tags     ("rebuild" "builtin" "io"))

(defprobe
  :name     "builtin-baseNameOf-store-path"
  :expr     "builtins.baseNameOf \"/nix/store/abc-foo-1.2.3\""
  :classify JsonEqual
  :tags ("rebuild" "builtin" "paths" "context-free"))

(defprobe
  :name     "builtin-substring-from-mid"
  :expr     "builtins.substring 4 5 \"abcdefghij\""
  :classify JsonEqual
  :tags ("rebuild" "builtin" "strings" "context-free"))

(defprobe
  :name     "builtin-split-empty-string"
  :expr     "builtins.split \"a\" \"\""
  :classify JsonEqual
  :tags ("rebuild" "builtin" "strings" "context-free"))

(defprobe
  :name     "builtin-match-named-capture"
  :expr     "builtins.match \"(.*)@(.*)\" \"user@host\""
  :classify JsonEqual
  :tags ("rebuild" "builtin" "strings" "context-free"))

(defprobe
  :name     "builtin-replaceStrings-multi"
  :expr     "builtins.replaceStrings [\"a\" \"b\"] [\"1\" \"2\"] \"banana\""
  :classify JsonEqual
  :tags ("rebuild" "builtin" "strings" "context-free"))

(defprobe
  :name     "builtin-removeAttrs-shape"
  :expr     "builtins.attrNames (builtins.removeAttrs { a = 1; b = 2; c = 3; } [ \"b\" ])"
  :classify JsonEqual
  :tags ("rebuild" "builtin" "attrs" "context-free"))

(defprobe
  :name     "builtin-intersectAttrs-shape"
  :expr     "builtins.attrNames (builtins.intersectAttrs { a = 1; b = 2; } { a = 1; c = 3; })"
  :classify JsonEqual
  :tags ("rebuild" "builtin" "attrs" "context-free"))

(defprobe
  :name     "builtin-mapAttrs-shape"
  :expr     "builtins.mapAttrs (n: v: builtins.toString v) { a = 1; b = 2; }"
  :classify JsonEqual
  :tags ("rebuild" "builtin" "attrs" "context-free"))

(defprobe
  :name     "builtin-attrValues-order"
  :expr     "builtins.attrValues { c = 3; a = 1; b = 2; }"
  :classify JsonEqual
  :tags ("rebuild" "builtin" "attrs" "context-free"))

(defprobe
  :name     "builtin-sort-strings"
  :expr     "builtins.sort builtins.lessThan [ \"banana\" \"apple\" \"cherry\" ]"
  :classify JsonEqual
  :tags ("rebuild" "builtin" "lists" "context-free"))

(defprobe
  :name     "builtin-elem-int-yes"
  :expr     "builtins.elem 3 [ 1 2 3 4 5 ]"
  :classify JsonEqual
  :tags ("rebuild" "builtin" "lists" "context-free"))

(defprobe
  :name     "builtin-genList-length"
  :expr     "builtins.length (builtins.genList (n: n * 2) 10)"
  :classify JsonEqual
  :tags ("rebuild" "builtin" "lists" "context-free"))

(defprobe
  :name     "builtin-foldlPrime-sum"
  :expr     "builtins.foldl' (a: b: a + b) 0 [ 1 2 3 4 5 ]"
  :classify JsonEqual
  :tags ("rebuild" "builtin" "lists" "context-free"))

(defprobe
  :name     "builtin-compareVersions-rc-vs-pre"
  :expr     "builtins.compareVersions \"1.0-rc1\" \"1.0-pre1\""
  :classify JsonEqual
  :tags ("rebuild" "builtin" "versions" "context-free"))

(defprobe
  :name     "builtin-currentSystem-self"
  :expr     "builtins.currentSystem"
  :classify JsonEqual
  :tags ("rebuild" "builtin" "system" "impure" "context-free"))

(defprobe
  :name     "builtin-nixVersion-shape"
  :expr     "builtins.match \"[0-9]+\\\\.[0-9]+.*\" builtins.nixVersion != null"
  :classify JsonEqual
  :tags ("rebuild" "builtin" "system" "context-free"))

(defprobe
  :name     "builtin-storeDir-default"
  :expr     "builtins.storeDir"
  :classify JsonEqual
  :tags ("rebuild" "builtin" "system" "context-free"))
