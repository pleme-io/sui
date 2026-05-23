;; sui CLI coverage catalog — every subcommand sui exposes vs the
;; equivalent nix invocation, classified by maturity gate.
;;
;; Substrate-invariant test enforces: every Commands:: pattern in
;; sui/src/main.rs has a catalog entry here.  Drift between code
;; and catalog fails the build, so this stays truthful.
;;
;; Operator query: `sui-spec-inventory --coverage`.

;; ── Top-level commands ─────────────────────────────────────────

(defsui-command
  :name "eval"
  :nix-equivalent "nix eval"
  :maturity Working
  :substrate ("flake")
  :notes "Expression + flake evaluation, JSON / raw output, --apply, -f")

(defsui-command
  :name "build"
  :nix-equivalent "nix build"
  :maturity Working
  :substrate ("derivation" "fetcher" "substituter")
  :notes "Derivation realization with --no-link / --print-out-paths / --dry-run / --out-link")

(defsui-command
  :name "develop"
  :nix-equivalent "nix develop"
  :maturity Working
  :substrate ("flake")
  :notes "Enter devShell via flake reference + attribute")

(defsui-command
  :name "run"
  :nix-equivalent "nix run"
  :maturity Working
  :substrate ("flake" "derivation")
  :notes "Execute a flake app installable")

(defsui-command
  :name "repl"
  :nix-equivalent "nix repl"
  :maturity Working
  :substrate ()
  :notes "Interactive evaluator")

(defsui-command
  :name "copy"
  :nix-equivalent "nix copy"
  :maturity Stub
  :substrate ("substituter" "narinfo")
  :notes "Cross-store path-set copy — not wired")

(defsui-command
  :name "path-info"
  :nix-equivalent "nix path-info"
  :maturity Stub
  :substrate ("store_layout" "narinfo")
  :notes "Top-level path-info — not wired (store path-info works)")

(defsui-command
  :name "collect-garbage"
  :nix-equivalent "nix-collect-garbage"
  :maturity Stub
  :substrate ("gc")
  :notes "Top-level GC; store gc is wired separately")

(defsui-command
  :name "show-config"
  :nix-equivalent "nix show-config"
  :maturity Working
  :substrate ()
  :notes "Effective configuration dump (--json)")

(defsui-command
  :name "why"
  :nix-equivalent "nix why-depends"
  :maturity Working
  :substrate ("derivation")
  :notes "Reverse-closure dependency explanation")

(defsui-command
  :name "path-from-hash-part"
  :nix-equivalent "nix store path-from-hash-part"
  :maturity Working
  :substrate ("store_layout")
  :notes "Lookup full store path by hash prefix")

(defsui-command
  :name "edit"
  :nix-equivalent "nix edit"
  :maturity Working
  :substrate ()
  :notes "Open derivation source in $EDITOR")

(defsui-command
  :name "log"
  :nix-equivalent "nix log"
  :maturity Working
  :substrate ("derivation")
  :notes "Print build log for a derivation")

(defsui-command
  :name "store-diff-closures"
  :nix-equivalent "nix store diff-closures"
  :maturity Working
  :substrate ("store_layout" "derivation")
  :notes "Diff two closure reference sets")

(defsui-command
  :name "upgrade-nix"
  :nix-equivalent "nix upgrade-nix"
  :maturity Working
  :substrate ()
  :notes "Self-upgrade hook — sui-native cutover")

(defsui-command
  :name "fmt"
  :nix-equivalent "nix fmt"
  :maturity Working
  :substrate ()
  :notes "Format Nix files via flake formatter attr")

(defsui-command
  :name "search"
  :nix-equivalent "nix search"
  :maturity Stub
  :substrate ("flake")
  :notes "Package search across flake registries")

(defsui-command
  :name "doctor"
  :nix-equivalent "nix doctor"
  :maturity Working
  :substrate ()
  :notes "Environment health check")

(defsui-command
  :name "print-dev-env"
  :nix-equivalent "nix print-dev-env"
  :maturity Working
  :substrate ("flake")
  :notes "Print devShell env vars + functions as a shell script")

(defsui-command
  :name "bundle"
  :nix-equivalent "nix bundle"
  :maturity Working
  :substrate ("derivation")
  :notes "Bundle a derivation via a bundler installable")

;; ── store commands ─────────────────────────────────────────────

(defsui-command
  :name "store path-info"
  :nix-equivalent "nix store path-info"
  :maturity Working
  :substrate ("store_layout" "narinfo")
  :notes "Show store path metadata")

(defsui-command
  :name "store paths"
  :nix-equivalent "nix store paths"
  :maturity Working
  :substrate ("store_layout")
  :notes "List store paths (--limit)")

(defsui-command
  :name "store gc"
  :nix-equivalent "nix store gc"
  :maturity Working
  :substrate ("gc")
  :notes "Garbage collect with age + roots + dry-run flags")

(defsui-command
  :name "store verify"
  :nix-equivalent "nix store verify"
  :maturity Working
  :substrate ("store_layout" "hash")
  :notes "Verify store path integrity")

(defsui-command
  :name "store optimise"
  :nix-equivalent "nix store optimise"
  :maturity Working
  :substrate ("store_layout")
  :notes "Hardlink-dedup store contents")

(defsui-command
  :name "store info"
  :nix-equivalent "nix store info"
  :maturity Working
  :substrate ("store_layout")
  :notes "Daemon URL + version + trust info")

(defsui-command
  :name "store ping"
  :nix-equivalent "nix store ping"
  :maturity Working
  :substrate ()
  :notes "Connectivity smoke test")

(defsui-command
  :name "store delete"
  :nix-equivalent "nix store delete"
  :maturity Stub
  :substrate ("gc")
  :notes "Delete paths --ignore-liveness — not wired")

(defsui-command
  :name "store ls"
  :nix-equivalent "nix store ls"
  :maturity Stub
  :substrate ("store_layout")
  :notes "List store path contents — not wired")

(defsui-command
  :name "store cat"
  :nix-equivalent "nix store cat"
  :maturity Stub
  :substrate ("store_layout")
  :notes "Cat file from a store path — not wired")

(defsui-command
  :name "store dump-path"
  :nix-equivalent "nix store dump-path"
  :maturity Stub
  :substrate ("nar")
  :notes "Emit NAR for a store path — not wired")

(defsui-command
  :name "store make-content-addressed"
  :nix-equivalent "nix store make-content-addressed"
  :maturity Stub
  :substrate ("realisation" "hash")
  :notes "Convert input-addressed paths to CA — not wired")

(defsui-command
  :name "store add-path"
  :nix-equivalent "nix store add-path"
  :maturity Stub
  :substrate ("store_layout" "nar")
  :notes "Import a directory into the store — not wired")

(defsui-command
  :name "store add-file"
  :nix-equivalent "nix store add-file"
  :maturity Stub
  :substrate ("store_layout" "hash")
  :notes "Import a single file into the store — not wired")

(defsui-command
  :name "store prefetch-file"
  :nix-equivalent "nix store prefetch-file"
  :maturity Stub
  :substrate ("fetcher" "hash")
  :notes "Download + hash a URL into the store — not wired")

(defsui-command
  :name "store sign"
  :nix-equivalent "nix store sign"
  :maturity Stub
  :substrate ("store_layout" "hash" "narinfo")
  :notes "Ed25519-sign store paths — not wired")

(defsui-command
  :name "store repair"
  :nix-equivalent "nix store repair"
  :maturity Stub
  :substrate ("substituter" "store_layout")
  :notes "Re-fetch corrupted store paths — not wired")

;; ── flake commands ─────────────────────────────────────────────

(defsui-command
  :name "flake show"
  :nix-equivalent "nix flake show"
  :maturity Working
  :substrate ("flake")
  :notes "Render the flake's output schema as a tree")

(defsui-command
  :name "flake update"
  :nix-equivalent "nix flake update"
  :maturity Working
  :substrate ("flake" "lock_file" "registry")
  :notes "Update flake.lock — all inputs or one named input")

(defsui-command
  :name "flake check"
  :nix-equivalent "nix flake check"
  :maturity Working
  :substrate ("flake" "module_system")
  :notes "Evaluate every flake check + module-system shape")

(defsui-command
  :name "flake lock"
  :nix-equivalent "nix flake lock"
  :maturity Working
  :substrate ("flake" "lock_file")
  :notes "Write the flake.lock if missing")

(defsui-command
  :name "flake metadata"
  :nix-equivalent "nix flake metadata"
  :maturity Working
  :substrate ("flake" "lock_file")
  :notes "Print the flake's locked metadata")

(defsui-command
  :name "flake init"
  :nix-equivalent "nix flake init"
  :maturity Stub
  :substrate ("flake")
  :notes "Initialize a flake from a template — not wired")

(defsui-command
  :name "flake new"
  :nix-equivalent "nix flake new"
  :maturity Stub
  :substrate ("flake")
  :notes "Create a flake from a template at a destination — not wired")

(defsui-command
  :name "flake archive"
  :nix-equivalent "nix flake archive"
  :maturity Stub
  :substrate ("flake" "store_layout")
  :notes "Archive a flake + all its inputs — not wired")

(defsui-command
  :name "flake clone"
  :nix-equivalent "nix flake clone"
  :maturity Stub
  :substrate ("flake")
  :notes "Clone a flake to a local destination — not wired")

(defsui-command
  :name "flake prefetch"
  :nix-equivalent "nix flake prefetch"
  :maturity Stub
  :substrate ("flake" "fetcher")
  :notes "Prefetch flake inputs to the store — not wired")

;; ── system commands (sui-native + darwin/nixos rebuild) ─────────

(defsui-command
  :name "system rebuild"
  :nix-equivalent "darwin-rebuild switch (or nixos-rebuild)"
  :maturity Partial
  :substrate ("activation_script" "module_system" "derivation")
  :notes "End-to-end host activation. Partial: M3.2 bridge chain incomplete.")

(defsui-command
  :name "system status"
  :nix-equivalent "darwin-rebuild --list-generations / nixos-version"
  :maturity Working
  :substrate ("profile")
  :notes "Current generation + system version")

(defsui-command
  :name "system rollback"
  :nix-equivalent "darwin-rebuild rollback / nixos-rebuild --rollback"
  :maturity Working
  :substrate ("activation_script" "profile")
  :notes "Roll back to the previous generation")

;; ── fleet commands (sui-native) ─────────────────────────────────

(defsui-command
  :name "fleet nodes"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ()
  :notes "List cluster nodes (no nix equivalent)")

(defsui-command
  :name "fleet deploy"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("activation_script")
  :notes "Push activation to fleet (no nix equivalent)")

(defsui-command
  :name "fleet status"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ()
  :notes "Fleet health summary (no nix equivalent)")

;; ── cache commands ─────────────────────────────────────────────

(defsui-command
  :name "cache serve"
  :nix-equivalent "nix serve"
  :maturity Working
  :substrate ("substituter" "narinfo")
  :notes "Local HTTP binary cache (--listen, --priority)")

(defsui-command
  :name "cache push"
  :nix-equivalent "nix copy --to"
  :maturity Working
  :substrate ("substituter" "narinfo" "hash")
  :notes "Push paths to a remote cache with signing key")

(defsui-command
  :name "cache gc"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("gc")
  :notes "Sui-native local cache GC")

(defsui-command
  :name "cache info"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ()
  :notes "Sui-native cache metadata")

;; ── profile commands ────────────────────────────────────────────

(defsui-command
  :name "profile list"
  :nix-equivalent "nix profile list"
  :maturity Stub
  :substrate ("profile")
  :notes "Not wired")

(defsui-command
  :name "profile install"
  :nix-equivalent "nix profile install"
  :maturity Stub
  :substrate ("profile" "derivation")
  :notes "Not wired")

(defsui-command
  :name "profile remove"
  :nix-equivalent "nix profile remove"
  :maturity Stub
  :substrate ("profile")
  :notes "Not wired")

(defsui-command
  :name "profile upgrade"
  :nix-equivalent "nix profile upgrade"
  :maturity Stub
  :substrate ("profile" "derivation")
  :notes "Not wired")

(defsui-command
  :name "profile rollback"
  :nix-equivalent "nix profile rollback"
  :maturity Stub
  :substrate ("profile")
  :notes "Not wired")

(defsui-command
  :name "profile history"
  :nix-equivalent "nix profile history"
  :maturity Stub
  :substrate ("profile")
  :notes "Not wired")

(defsui-command
  :name "profile wipe-history"
  :nix-equivalent "nix profile wipe-history"
  :maturity Stub
  :substrate ("profile")
  :notes "Not wired")

(defsui-command
  :name "profile diff"
  :nix-equivalent "nix profile diff-closures"
  :maturity Stub
  :substrate ("profile")
  :notes "Not wired")

;; ── derivation commands ─────────────────────────────────────────

(defsui-command
  :name "derivation show"
  :nix-equivalent "nix derivation show"
  :maturity Stub
  :substrate ("derivation")
  :notes "Not wired")

(defsui-command
  :name "derivation add"
  :nix-equivalent "nix derivation add"
  :maturity Stub
  :substrate ("derivation")
  :notes "Not wired")

;; ── hash commands ───────────────────────────────────────────────

(defsui-command
  :name "hash file"
  :nix-equivalent "nix hash file"
  :maturity Stub
  :substrate ("hash")
  :notes "Not wired (sui-spec-inventory --hash-decode covers reverse direction)")

(defsui-command
  :name "hash path"
  :nix-equivalent "nix hash path"
  :maturity Stub
  :substrate ("hash" "nar")
  :notes "Not wired")

(defsui-command
  :name "hash to-base16"
  :nix-equivalent "nix hash to-base16"
  :maturity Stub
  :substrate ("hash")
  :notes "Not wired")

(defsui-command
  :name "hash to-base32"
  :nix-equivalent "nix hash to-base32"
  :maturity Stub
  :substrate ("hash")
  :notes "Not wired")

(defsui-command
  :name "hash to-base64"
  :nix-equivalent "nix hash to-base64"
  :maturity Stub
  :substrate ("hash")
  :notes "Not wired")

(defsui-command
  :name "hash to-sri"
  :nix-equivalent "nix hash to-sri"
  :maturity Stub
  :substrate ("hash")
  :notes "Not wired")

;; ── key commands ────────────────────────────────────────────────

(defsui-command
  :name "key generate-secret"
  :nix-equivalent "nix key generate-secret"
  :maturity Stub
  :substrate ("trust_model")
  :notes "Not wired")

(defsui-command
  :name "key convert-secret-to-public"
  :nix-equivalent "nix key convert-secret-to-public"
  :maturity Stub
  :substrate ("trust_model")
  :notes "Not wired")

;; ── registry commands ───────────────────────────────────────────

(defsui-command
  :name "registry list"
  :nix-equivalent "nix registry list"
  :maturity Stub
  :substrate ("registry")
  :notes "Not wired (sui-spec-inventory --registry-resolve covers lookup)")

(defsui-command
  :name "registry add"
  :nix-equivalent "nix registry add"
  :maturity Stub
  :substrate ("registry")
  :notes "Not wired")

(defsui-command
  :name "registry remove"
  :nix-equivalent "nix registry remove"
  :maturity Stub
  :substrate ("registry")
  :notes "Not wired")

(defsui-command
  :name "registry pin"
  :nix-equivalent "nix registry pin"
  :maturity Stub
  :substrate ("registry")
  :notes "Not wired")

;; ── sui-native primitives ───────────────────────────────────────

(defsui-command
  :name "serve"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ()
  :notes "REST + GraphQL + gRPC API server")

(defsui-command
  :name "daemon"
  :nix-equivalent "nix-daemon"
  :maturity Working
  :substrate ("worker_protocol")
  :notes "Worker-protocol-compatible daemon over Unix socket")

(defsui-command
  :name "agent"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("derivation" "fetcher" "substituter")
  :notes "NATS-driven distributed build agent")

(defsui-command
  :name "cache-warm"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("flake" "derivation")
  :notes "Pre-warm derivation cache for K8s shipping")

(defsui-command
  :name "rebuild-shadow"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ()
  :notes "Run parity probes vs cppnix; emit typed ShadowReport")
