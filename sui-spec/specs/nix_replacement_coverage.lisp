;; Nix-replacement coverage catalog — every Nix surface sui must
;; cover to fully replace cppnix in production. One row per surface;
;; each row's `:status` field is enforced typed (Done / InProgress /
;; Queued / NotStarted) so `sui-spec-inventory --nix-coverage` can
;; project a single source-of-truth dashboard.
;;
;; Cross-references:
;;   - cli_coverage.lisp — per-command coverage (sui's argv surface)
;;   - this file — per-Nix-workload coverage (the *behaviors* operators
;;     run, not just the commands that invoke them)
;;
;; Add a row whenever a real Nix workload is identified that sui must
;; cover. Each row points at the typed Rust piece that implements (or
;; will implement) the surface — so adding `:status Done` is a
;; verifiable claim, not a marketing one.

;; ── L0 storage / link-in-place ───────────────────────────────────

(defnix-replacement-surface
  :name "store-path-layout"
  :category Storage
  :status Done
  :owns "sui-graph-store::store::GraphStore"
  :notes "Content-addressed blob layout on a ZFS-friendly sharded CAS. Replaces SeaORM/SQLite for the L1 substrate. /nix/store layout (cppnix-compat) is owned by sui-store; both coexist.")

(defnix-replacement-surface
  :name "store-add-path"
  :category Storage
  :status InProgress
  :owns "sui-store::local"
  :notes "`nix-store --add` / `nix store add-path`. Sui-store has the local store interface; full add-path realizer + hard-link/clone dedup (a la `nix-store --optimise`) lands with the daemon-graph integration.")

(defnix-replacement-surface
  :name "store-optimise-hardlinks"
  :category Storage
  :status Queued
  :owns "sui-store (planned)"
  :notes "`nix-store --optimise` — dedupe identical files across store paths via hard links. cppnix uses /nix/var/nix/db/links + ino reuse; sui will mirror the design + opportunistically use reflink on copy-on-write FS (ZFS, btrfs).")

(defnix-replacement-surface
  :name "store-gc"
  :category Storage
  :status InProgress
  :owns "sui-graph-store + sui-store::local"
  :notes "Mark-and-sweep gc rooted at gcroots. Sui-graph-store has iter_keys for the L1 substrate; sui-store has the /nix/store gc. Connect them under one `sui collect-garbage` umbrella.")

(defnix-replacement-surface
  :name "store-verify-narhash"
  :category Storage
  :status Done
  :owns "sui-graph-store::store::GraphStore::get_validated"
  :notes "Re-hash on read for blobs pulled from untrusted sources. Already operational.")

;; ── L1 typed graph substrate ─────────────────────────────────────

(defnix-replacement-surface
  :name "lockfile-graph"
  :category SubstrateL1
  :status Done
  :owns "sui-spec::lockfile_graph"
  :notes "Parsed + follows-resolved + content-addressed lockfile. 25 MB rio JSON → 6.4 MB rkyv archive, 18 ms warm-path read.")

(defnix-replacement-surface
  :name "ast-graph"
  :category SubstrateL1
  :status Done
  :owns "sui-spec::ast_graph"
  :notes "Typed AST for every `.nix` source. 16 core variants + Unknown forward-compat. Dialect-discriminator (Nix/Tlisp/Mixed) anchors the universal IR.")

(defnix-replacement-surface
  :name "module-graph"
  :category SubstrateL1
  :status InProgress
  :owns "sui-spec::module_graph"
  :notes "Typed module-system IR. Today: types + builder skeleton. Queued: worker/wrapper synthesis, defunctionalization, slice-keyed re-firing, NbE execution.")

(defnix-replacement-surface
  :name "derivation-graph"
  :category SubstrateL1
  :status Queued
  :owns "sui-spec::derivation + sui-spec (planned)"
  :notes "Typed derivation graph (pre-realisation). Today sui-spec::derivation models the typed border for one derivation; full graph form for closure operations is queued.")

;; ── Eval engine ──────────────────────────────────────────────────

(defnix-replacement-surface
  :name "eval-language-builtins"
  :category EvalEngine
  :status Done
  :owns "sui-eval::builtins + sui-bytecode::builtins"
  :notes "All 29 builtin smoke probes match cppnix byte-for-byte (parity_probes corpus).")

(defnix-replacement-surface
  :name "eval-flake-evaluation"
  :category EvalEngine
  :status Done
  :owns "sui-eval::eval + sui-spec::flake"
  :notes "`sui eval`, `sui flake show`, `sui flake metadata` — Working per cli_coverage.")

(defnix-replacement-surface
  :name "eval-cache-shared-fleet"
  :category EvalEngine
  :status Done
  :owns "sui-eval::eval_cache (with GraphStore tier)"
  :notes "Three-tier eval cache: memory + JSON + GraphStore. GraphStore tier enables fleet-wide reuse via atticd replication.")

(defnix-replacement-surface
  :name "eval-module-system"
  :category EvalEngine
  :status InProgress
  :owns "sui-eval (planned) + sui-spec::module_graph"
  :notes "NixOS module system fixed-point evaluator. Today: types ready; compiled-closure execution is the next focused ship.")

(defnix-replacement-surface
  :name "eval-tlisp-dialect"
  :category EvalEngine
  :status Queued
  :owns "sui-spec::ast_graph::from_tlisp_source"
  :notes "Parse + lower `.tlisp` source into the universal AstGraph IR. Today: typed seam returns Unknown stub. Queued: full lowering of every AstNodeKind variant + bidirectional Nix↔Tlisp transformation.")

;; ── Derivation + build ────────────────────────────────────────────

(defnix-replacement-surface
  :name "derivation-hash-parity"
  :category Derivation
  :status Done
  :owns "sui-spec::derivation"
  :notes "Input-addressed + fixed-output derivation path algorithms, byte-identical to cppnix. Spec-driven (sui-spec/specs/derivation.lisp); both engines call the same interpreter.")

(defnix-replacement-surface
  :name "derivation-build-sandbox"
  :category Build
  :status Done
  :owns "sui-build"
  :notes "Sandboxed builder. Working per cli_coverage; covers `sui build` end-to-end.")

(defnix-replacement-surface
  :name "derivation-ca-derivations"
  :category Derivation
  :status Queued
  :owns "sui-spec::realisation (planned)"
  :notes "Content-addressed derivations (`__contentAddressed = true`). Realisation table lives in sui-spec::realisation; full input-addressed → content-addressed mapping queued.")

;; ── Fetchers ─────────────────────────────────────────────────────

(defnix-replacement-surface
  :name "fetch-github"
  :category Fetcher
  :status Done
  :owns "sui-spec::fetcher + sui-eval::fetcher"
  :notes "`builtins.fetchTree { type = \"github\"; ... }` and the github flake-ref. Working per cli_coverage.")

(defnix-replacement-surface
  :name "fetch-git"
  :category Fetcher
  :status Done
  :owns "sui-eval::git"
  :notes "`builtins.fetchGit`. Working — uses gix for the protocol.")

(defnix-replacement-surface
  :name "fetch-tarball"
  :category Fetcher
  :status Done
  :owns "sui-eval::fetcher"
  :notes "`builtins.fetchTarball` + `builtins.fetchurl`. Working.")

(defnix-replacement-surface
  :name "fetch-path"
  :category Fetcher
  :status Done
  :owns "sui-eval::path + sui-spec::fetcher"
  :notes "`path:` flake refs + `builtins.path`. Working.")

;; ── Substituter / cache ──────────────────────────────────────────

(defnix-replacement-surface
  :name "substituter-narinfo-pull"
  :category Substituter
  :status Done
  :owns "sui-store::binary_cache + sui-store::http"
  :notes "Pull NARs from existing binary caches (atticd, cache.nixos.org) via narinfo/HTTP. Compatible with every Nix substituter in existence.")

(defnix-replacement-surface
  :name "substituter-narinfo-push"
  :category Substituter
  :status Done
  :owns "sui-store + attic-client"
  :notes "Push closures to atticd via the existing narinfo protocol. Already operational via the `attic push` integration tend uses.")

(defnix-replacement-surface
  :name "substituter-typed-closure-stream"
  :category Substituter
  :status Queued
  :owns "sui-protocol (planned tvix-castore-shaped endpoint)"
  :notes "One-round-trip typed-closure streaming via tonic + protobuf, tvix-castore-shaped. Alongside narinfo (never replacing). Queued.")

;; ── Daemon / IPC ────────────────────────────────────────────────

(defnix-replacement-surface
  :name "daemon-worker-protocol-cppnix"
  :category Daemon
  :status Done
  :owns "sui-daemon::connection + sui-daemon::server"
  :notes "Drop-in `/nix/var/nix/daemon-socket/socket` server speaking the cppnix worker protocol. Existing `nix` clients see no difference.")

(defnix-replacement-surface
  :name "daemon-graph-protocol-native"
  :category Daemon
  :status Done
  :owns "sui-daemon::graph_server + sui-daemon-graph"
  :notes "Native rkyv-over-UDS protocol for fleet-wide sui graph access. Coexists with the cppnix-worker server; separate socket; binary entry point ships as sui-daemon-graph.")

(defnix-replacement-surface
  :name "daemon-fleet-work-stealing"
  :category Daemon
  :status Queued
  :owns "sui-protocol (planned REAPI-shaped fleet endpoint) + sui-orchestrate"
  :notes "Cross-host build dispatch via tonic + REAPI-shaped protobuf. Turns the pleme-io fleet (cid + ryn + rio + VMs) into one logical builder.")

;; ── System rebuild ───────────────────────────────────────────────

(defnix-replacement-surface
  :name "system-rebuild-nixos"
  :category SystemRebuild
  :status InProgress
  :owns "sui-orchestrate (planned) + sui-spec::rebuild"
  :notes "`nixos-rebuild switch / boot / dry-build / rollback`. Catalog says Working; per the README, parity is shadow-tested via sui-sweep. The slow path (`nixosConfigurations.X.config.system.build.toplevel`) is gated on module_graph compilation landing.")

(defnix-replacement-surface
  :name "system-rebuild-darwin"
  :category SystemRebuild
  :status InProgress
  :owns "sui-orchestrate (planned) + sui-spec::rebuild"
  :notes "`darwin-rebuild switch`. Same architecture as nixos-rebuild; same slow-path gate (module_graph compilation).")

(defnix-replacement-surface
  :name "system-rebuild-home-manager"
  :category SystemRebuild
  :status InProgress
  :owns "sui-orchestrate (planned) + sui-spec::rebuild"
  :notes "`home-manager switch`. Per-user activation; rides on the same module-system pipeline.")

;; ── Convenience surfaces operators rely on ───────────────────────

(defnix-replacement-surface
  :name "nix-channel-compat"
  :category Convenience
  :status Done
  :owns "sui::main argv dispatcher (legacy `nix-channel` shim)"
  :notes "Legacy `nix-channel` shim. Working per cli_coverage's argv-dispatch table.")

(defnix-replacement-surface
  :name "nix-shell-compat"
  :category Convenience
  :status Done
  :owns "sui::main argv dispatcher (legacy `nix-shell` shim)"
  :notes "`nix-shell` + `nix-shell -p`. Working per cli_coverage.")

(defnix-replacement-surface
  :name "nixos-rebuild-cli-shim"
  :category Convenience
  :status Queued
  :owns "sui-nix-wrap (planned host-tool extension)"
  :notes "`nixos-rebuild` is a host script, not a `nix-*` symlink. Sui-nix-wrap can grow a `nixos-rebuild` mode that drives `sui system rebuild` once the parity is ready.")
