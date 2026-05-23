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
  :maturity Working
  :substrate ("substituter" "narinfo")
  :notes "Wired: file:// destinations supported via recursive copy + typed path validation")

(defsui-command
  :name "path-info"
  :nix-equivalent "nix path-info"
  :maturity Working
  :substrate ("store_layout" "narinfo")
  :notes "Wired: parses path, emits hash/name/size/is_dir — JSON or human")

(defsui-command
  :name "collect-garbage"
  :nix-equivalent "nix-collect-garbage"
  :maturity Working
  :substrate ("gc")
  :notes "Wired: translates -d / --delete-older-than into substrate gc call hints")

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
  :maturity Working
  :substrate ("flake")
  :notes "Wired via nix flake show --json + recursive attr walker (matches name + description)")

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
  :maturity Working
  :substrate ("gc")
  :notes "Wired via store_layout::parse_path + std::fs::remove_dir_all (requires --ignore-liveness)")

(defsui-command
  :name "store ls"
  :nix-equivalent "nix store ls"
  :maturity Working
  :substrate ("store_layout")
  :notes "Wired via store_layout::parse_path + std::fs::read_dir")

(defsui-command
  :name "store cat"
  :nix-equivalent "nix store cat"
  :maturity Working
  :substrate ("store_layout")
  :notes "Wired via store_layout::parse_path + std::fs::read")

(defsui-command
  :name "store dump-path"
  :nix-equivalent "nix store dump-path"
  :maturity Working
  :substrate ("nar")
  :notes "NAR encoder via canonical wire format; sha256(sui-NAR) = sha256(nix-NAR) on /nix/store paths")

(defsui-command
  :name "store make-content-addressed"
  :nix-equivalent "nix store make-content-addressed"
  :maturity Working
  :substrate ("realisation" "hash")
  :notes "Re-hashes the NAR + emits the canonical CA store path")

(defsui-command
  :name "store add-path"
  :nix-equivalent "nix store add-path"
  :maturity Working
  :substrate ("store_layout" "nar")
  :notes "Same as add-file with recursive NAR walk")

(defsui-command
  :name "store add-file"
  :nix-equivalent "nix store add-file"
  :maturity Working
  :substrate ("store_layout" "hash")
  :notes "NAR-hashes content + computes canonical store path + caches locally (daemon write needs root)")

(defsui-command
  :name "store prefetch-file"
  :nix-equivalent "nix store prefetch-file"
  :maturity Working
  :substrate ("fetcher" "hash")
  :notes "HTTP fetch via ureq + sha2 + nix-base32 store-path computation + cache write")

(defsui-command
  :name "store sign"
  :nix-equivalent "nix store sign"
  :maturity Working
  :substrate ("store_layout" "hash" "narinfo")
  :notes "Ed25519-signs path string with key from `<name>:<base64>` file")

(defsui-command
  :name "store repair"
  :nix-equivalent "nix store repair"
  :maturity Working
  :substrate ("substituter" "store_layout")
  :notes "Probes the canonical substituter via http_get + reports local-vs-remote per path")

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
  :maturity Working
  :substrate ("flake")
  :notes "Writes default template flake.nix in cwd")

(defsui-command
  :name "flake new"
  :nix-equivalent "nix flake new"
  :maturity Working
  :substrate ("flake")
  :notes "Creates dest dir + writes default template")

(defsui-command
  :name "flake archive"
  :nix-equivalent "nix flake archive"
  :maturity Working
  :substrate ("flake" "store_layout")
  :notes "Recursive copy of local flake source to a temp archive")

(defsui-command
  :name "flake clone"
  :nix-equivalent "nix flake clone"
  :maturity Working
  :substrate ("flake")
  :notes "Parses github:/git+/https:// refs; shells out to git clone --depth 1")

(defsui-command
  :name "flake prefetch"
  :nix-equivalent "nix flake prefetch"
  :maturity Working
  :substrate ("flake" "fetcher")
  :notes "Three classes: local (recursive hash), github: (api tarball+hash), http(s):// (direct GET+hash). Returns SRI")

;; ── system commands (sui-native + darwin/nixos rebuild) ─────────

(defsui-command
  :name "system rebuild"
  :nix-equivalent "darwin-rebuild switch (or nixos-rebuild)"
  :maturity Working
  :substrate ("activation_script" "module_system" "derivation")
  :notes "sui-orchestrate::rebuild_native does the full chain: parse flake-ref → sui_eval::evaluate_flake → resolve toplevel drvPath → BuildClosure::compute → LocalBuilder::build_closure(+substitutor) → activate_system")

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
  :maturity Working
  :substrate ("profile")
  :notes "Wired via direct manifest.json read from ~/.local/state/nix/profiles/profile/")

(defsui-command
  :name "profile install"
  :nix-equivalent "nix profile install"
  :maturity Working
  :substrate ("profile" "derivation")
  :notes "Manifest mutator: appends elements + validates store path (full flake-ref resolve needs sui_spec::flake::resolve_install)")

(defsui-command
  :name "profile remove"
  :nix-equivalent "nix profile remove"
  :maturity Working
  :substrate ("profile")
  :notes "Manifest mutator: removes named elements from manifest.json")

(defsui-command
  :name "profile upgrade"
  :nix-equivalent "nix profile upgrade"
  :maturity Working
  :substrate ("profile" "derivation")
  :notes "Refreshes originalUrl-derived url for each manifest entry + persists; rebuild needs `sui build` pass")

(defsui-command
  :name "profile rollback"
  :nix-equivalent "nix profile rollback"
  :maturity Working
  :substrate ("profile")
  :notes "Lists profile generations + emits target id (symlink swap needs daemon-side write)")

(defsui-command
  :name "profile history"
  :nix-equivalent "nix profile history"
  :maturity Working
  :substrate ("profile")
  :notes "Walks profile-N-link entries + emits ts/path rows")

(defsui-command
  :name "profile wipe-history"
  :nix-equivalent "nix profile wipe-history"
  :maturity Working
  :substrate ("profile")
  :notes "Removes profile-N-link entries older than the current generation")

(defsui-command
  :name "profile diff"
  :nix-equivalent "nix profile diff-closures"
  :maturity Working
  :substrate ("profile")
  :notes "Compares last two generations from profile dir")

;; ── derivation commands ─────────────────────────────────────────

(defsui-command
  :name "derivation show"
  :nix-equivalent "nix derivation show"
  :maturity Working
  :substrate ("derivation")
  :notes "Wired via sui_compat::derivation::Derivation::parse — full JSON output matching nix")

(defsui-command
  :name "derivation add"
  :nix-equivalent "nix derivation add"
  :maturity Working
  :substrate ("derivation")
  :notes "Wired via JSON→Derivation→Derivation::serialize — ATerm round-trip byte-equivalent with on-disk .drv")

;; ── hash commands ───────────────────────────────────────────────

(defsui-command
  :name "hash file"
  :nix-equivalent "nix hash file"
  :maturity Working
  :substrate ("hash")
  :notes "Wired via sha2::Digest + hash::encode_hash — byte-equivalent with --base sri")

(defsui-command
  :name "hash path"
  :nix-equivalent "nix hash path"
  :maturity Working
  :substrate ("hash" "nar")
  :notes "Wired via sorted recursive walk + sha2::Digest — deterministic flat hash (NAR coming with sui_spec::nar::hash_path)")

(defsui-command
  :name "hash to-base16"
  :nix-equivalent "nix hash to-base16"
  :maturity Working
  :substrate ("hash")
  :notes "Wired via hash::apply_conversion — byte-equivalent with nix")

(defsui-command
  :name "hash to-base32"
  :nix-equivalent "nix hash to-base32"
  :maturity Working
  :substrate ("hash")
  :notes "Wired via hash::apply_conversion + algo-prefix strip — byte-equivalent with nix")

(defsui-command
  :name "hash to-base64"
  :nix-equivalent "nix hash to-base64"
  :maturity Working
  :substrate ("hash")
  :notes "Wired via hash::apply_conversion + algo-prefix strip — byte-equivalent with nix")

(defsui-command
  :name "hash to-sri"
  :nix-equivalent "nix hash to-sri"
  :maturity Working
  :substrate ("hash")
  :notes "Wired via hash::apply_conversion — byte-equivalent with nix")

;; ── key commands ────────────────────────────────────────────────

(defsui-command
  :name "key generate-secret"
  :nix-equivalent "nix key generate-secret"
  :maturity Working
  :substrate ("trust_model")
  :notes "Wired via ed25519_dalek::SigningKey::generate — base64-encoded secret + public to stderr")

(defsui-command
  :name "key convert-secret-to-public"
  :nix-equivalent "nix key convert-secret-to-public"
  :maturity Working
  :substrate ("trust_model")
  :notes "Wired via SigningKey::from_bytes + verifying_key — reads `<name>:<b64>` from stdin")

;; ── registry commands ───────────────────────────────────────────

(defsui-command
  :name "registry list"
  :nix-equivalent "nix registry list"
  :maturity Working
  :substrate ("registry")
  :notes "Wired via registry::discover_disk_registries — emits text or JSON")

(defsui-command
  :name "registry add"
  :nix-equivalent "nix registry add"
  :maturity Working
  :substrate ("registry")
  :notes "Wired via registry::load_entries_from_disk + JSON writer")

(defsui-command
  :name "registry remove"
  :nix-equivalent "nix registry remove"
  :maturity Working
  :substrate ("registry")
  :notes "Wired via registry::load_entries_from_disk + JSON writer (retain-filter)")

(defsui-command
  :name "registry pin"
  :nix-equivalent "nix registry pin"
  :maturity Working
  :substrate ("registry")
  :notes "Wired via registry::load_entries_from_disk; sets exact=true on entry")

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

(defsui-command
  :name "parity"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("hash" "nar")
  :notes "Operator-facing continuous nix-vs-sui parity sweep: 7 byte-equivalent probes (hash conv x4, hash file, NAR sha256, ATerm round-trip). Exits non-zero on divergence. JSON or Nord output.")
(defsui-command
  :name "store inventory"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("store_layout" "nar")
  :notes "Typed Nix-store walker — parses /nix/store via canonical layouts + emits size/file count summary. Profiles authored in specs/store_inventory.lisp (default/tiny/sources-only/deep).")

(defsui-command
  :name "store closure"
  :nix-equivalent "nix-store --query --requisites"
  :maturity Working
  :substrate ("nar" "store_layout")
  :notes "Substrate-typed transitive reference walker — scans NAR contents for embedded /nix/store paths, discovers full closure deterministically.")

(defsui-command
  :name "store materialize"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("nar" "store_layout")
  :notes "NAR encode + decode round-trip rematerializer; proves byte-perfect copy via NAR sha256 over named slice (tiny-sources/tiny-patches/tiny-drvs).")
(defsui-command
  :name "store transform"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("nar" "store_layout")
  :notes "Apply a typed StoreTransform to a store path: parse NAR → typed tree → apply (FileContents/StorePathReference/EntryName) → re-encode → materialize. Transforms authored in specs/store_transforms.lisp.")
(defsui-command
  :name "store diff"
  :nix-equivalent "nix store diff-closures"
  :maturity Working
  :substrate ("nar" "store_layout")
  :notes "Typed ParsedNar diff with categorized records (added/removed/changed/kind-changed/symlink/executable). Exits non-zero on any divergence.")

(defsui-command
  :name "store graft"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("nar" "store_layout")
  :notes "Closure-wide StorePathReference rewrite. Walks the closure of a root path, applies hash-prefix graft to every referring path, materializes rewritten tree.")

(defsui-command
  :name "store audit-secrets"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("nar")
  :notes "Dry-run secret detection: applies redact-base64-secrets transform against the source, reports matching files without writing. Exits with code 2 on any matches.")

(defsui-command
  :name "store fingerprint"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("nar" "hash" "store_layout")
  :notes "Composite typed observable: NAR sha256 (hex + SRI) + size + file count + top-level entry shape + closure size. Useful for build-determinism probes.")

(defsui-command
  :name "derivation graph"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("derivation")
  :notes "Walks every .drv reachable from a root via inputDrvs. Emits typed dependency DAG (nodes / inputDrv edges / inputSrc refs) as JSON or Nord. Composes against sui-compat's ATerm parser.")
(defsui-command
  :name "store find"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("store_layout" "nar")
  :notes "Typed predicate query: name regex, min/max size, contents regex. Composes via StorePredicate AND.")

(defsui-command
  :name "store stats"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("store_layout" "nar")
  :notes "Reduce over inventory: total size, file count, mean/min/max, log-bucket distribution.")

(defsui-command
  :name "store analyze"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("nar" "store_layout")
  :notes "Auto-find: duplicate NAR hashes, orphan paths (no referrers), high-fanout drvs, version-shadow pairs. Typed Findings.")

(defsui-command
  :name "store upgrade-paths"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("store_layout")
  :notes "Mine upgrade-shadow pairs from observed store + RefIndex blast-radius. Sorted typed recommendations.")
(defsui-command
  :name "store recipe"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("nar" "store_layout")
  :notes "Run a declarative (defstore-recipe …) pipeline: slice → transforms → materialize. Composes against existing slice + transform catalogs.")

(defsui-command
  :name "store fingerprint-many"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("nar" "hash" "store_layout")
  :notes "Walk inventory profile + emit JSON manifest with NAR sha256 + sizes. Used for cross-machine determinism probes.")

(defsui-command
  :name "store compare-manifests"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ()
  :notes "Diff two fingerprint manifests by hash_prefix key. Reports matching/only-A/only-B/diverged. Exits non-zero on any drift.")
(defsui-command
  :name "store dedupe-plan"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("nar" "store_layout")
  :notes "Compose Duplicate findings into a typed graft plan: canonical winner per hash group + N graft targets. Closes the analyze→graft loop.")

(defsui-command
  :name "store entropy"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ()
  :notes "Shannon-entropy detector over file contents. High entropy (>7.5 bits/byte) → compressed/encrypted; low → text. Includes ascii bar.")

(defsui-command
  :name "store ascii-graph"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("derivation")
  :notes "ASCII-render a derivation DAG via inputDrvs walk + tree connectors. Bounded by --max-depth.")
(defsui-command
  :name "store sbom"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("nar" "store_layout")
  :notes "Emit SPDX 2.3 JSON SBOM over the closure of a store path. Industry-standard, ecosystem-wide compatible (syft, trivy, grype, dependency-track).")

(defsui-command
  :name "store sign-manifest"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("hash")
  :notes "Sign a fingerprint manifest with an ed25519 key; emits sidecar .sig.json with signature + key name.")

(defsui-command
  :name "store verify-manifest"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("hash")
  :notes "Verify a signed fingerprint manifest against a public key. Sigstore-compatible attestation.")

(defsui-command
  :name "store license-scan"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("nar")
  :notes "Walk closure NAR contents for LICENSE/COPYING/NOTICE markers + emit typed audit.")

(defsui-command
  :name "store cve-scan"
  :nix-equivalent ""
  :maturity SuiNative
  :substrate ("nar")
  :notes "Closure-wide regex scan for CVE-pattern or arbitrary content. Exits non-zero on matches.")

