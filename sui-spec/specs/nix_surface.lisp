;; sui-spec/specs/nix_surface.lisp — the typed board of "every possible nix use
;; case", one (defnix-surface) row per surface. This is the SURFACE axis of the
;; STRATOSPHERE use-case vocabulary (docs/STRATOSPHERE.md §2). The Surface enum is
;; compile-exhaustive (surfaces_are_complete); each row's :tier is honesty-gated
;; against what its :row-form + :reflects + :oracle earn (nix_surface::validate) —
;; so no row can round its tier up. Every row here is tier-honest as of 2026-07-22.
;;
;; :tier ladder (ascending): Absent < Design < Enumerated < ParityWired.
;;   ParityWired = a live oracle is bound; NOT "proven equal to nix" (that is C2,
;;   forever external). Today only S4 + S6 are wireable at Enumerated; the rest are
;;   honestly Design/Absent until their per-item catalog + gate ship (the M1..M6 path).

;; ── S1 — the Nix language ─────────────────────────────────────────
(defnix-surface
  :id       S1Language
  :covers   "The Nix language: grammar x operators x ~120 builtins x error paths"
  :row-form ""                         ;; (defbuiltincase)/(deflangcase) not authored yet
  :reflects BuiltinRegistry
  :oracle   ExpFixture                 ;; lang_corpus 88 active + 49 known_broken .exp fixtures
  :tier     Design                     ;; corpus exists (88/137) but no per-builtin bijection gate
  :blocker  None
  :notes    "88/137 eval-okay corpus passing; ~89/120 builtins implemented, per-builtin parity far thinner. Becomes Enumerated when (defbuiltincase) + the registry<->catalog bijection gate ship (STRATOSPHERE M0).")

;; ── S2 — whole-closure byte-parity ────────────────────────────────
(defnix-surface
  :id       S2Closure
  :covers   "Per-node ATerm + NAR byte-diff across a real dependency graph"
  :row-form ""
  :reflects ClosureWalk
  :oracle   Absent                     ;; bisect_drv descends only the first diverging child
  :tier     Design
  :blocker  MemoryWall                 ;; cid-scale (20,827 drvs) swap-deaths; cannot reach ParityWired there
  :notes    "bisect_drv is DFS-to-first-leaf; build-parity is a 2-row scaffold. A visit-all walk (M5) reaches Enumerated on a SMALL closure; the cid subject stays CannotComplete (memory wall).")

;; ── S4 — the CLI contract ─────────────────────────────────────────
(defnix-surface
  :id       S4Cli
  :covers   "Every subcommand x flag x JSON shape x exit code vs cppnix"
  :row-form "defsui-command"           ;; SHIPPED: 109 rows in cli_coverage.lisp
  :reflects SuiCommandsEnum            ;; the Commands:: scan (SUI's own surface, not nix's)
  :oracle   ParityCheck
  :tier     Enumerated                 ;; command-NAME bijection is live + green today
  :blocker  None
  :notes    "Command-name level 100% (every Commands:: has a defsui-command row). Flag/exit/JSON contract ~0% typed (165 #[arg] defs live only in :notes). ParityWired awaits the per-flag rows + a real oracle (M1). Name presence != behavioral parity.")

;; ── S5 — config parsing ───────────────────────────────────────────
(defnix-surface
  :id       S5Config
  :covers   "nix.conf / NIX_CONFIG / --option / NIX_PATH typed parsing"
  :row-form ""
  :reflects None                       ;; no parser exists; nix show-config is the future gate
  :oracle   Absent
  :tier     Absent                     ;; 0 rows, no parser — honestly nothing yet
  :blocker  NoParser
  :notes    "nix.conf/NIX_PATH never parsed. SuiDaemonConfig (sui.yaml) is orthogonal and MUST NOT be counted. The parser is the largest new build (M6); reflects nix show-config --json keys once it exists.")

;; ── S6 — the daemon worker protocol (server side) ─────────────────
(defnix-surface
  :id       S6Daemon
  :covers   "The daemon worker protocol, server side (nix clients connecting to sui)"
  :row-form "defworker-op"             ;; SHIPPED: worker_protocol.lisp models the wire shape
  :reflects WireOpcodeCatalog          ;; name-bridged to sui_compat::wire::WorkerOp
  :oracle   Absent                     ;; real_nix_client.rs is opt-in, silent no-op in CI
  :tier     Enumerated                 ;; opcode-shape catalog is live; no server-status field yet
  :blocker  HarnessOnlyNoOracle
  :notes    "Server is real (12/32 opcodes handled). Catalog models wire-SHAPE only, no server-status field, ~20 opcodes runtime-stub. essential_opcodes_present is a 10-name self-consistency check, not a cppnix reflection. ParityWired awaits promoting real_nix_client.rs to a gated corpus (M2).")

;; ── S7 — drop-in PATH ─────────────────────────────────────────────
(defnix-surface
  :id       S7Path
  :covers   "nix + legacy nix-* entrypoints resolve to sui shims on PATH"
  :row-form ""
  :reflects None                       ;; pinned cppnix bin/ is the future gate
  :oracle   Absent
  :tier     Design                     ;; one entrypoint (nix) shimmed with a real lock gate
  :blocker  NoShims
  :notes    "nix shimmed with a real lock gate (lock_100_percent.rs); legacy nix-* unshimmed, 0 PATH-shadow rows. (defpath-entrypoint) + a which-resolves-to-shim probe reach Enumerated (M3).")

;; ── U — the performance matrix ────────────────────────────────────
(defnix-surface
  :id       UPerf
  :covers   "The twelve-class performance matrix U01-U12, each at its gate tier"
  :row-form ""                         ;; (defuse-case) + UseCaseClass enum do NOT exist yet
  :reflects UseCaseCatalog
  :oracle   PerfSeal                   ;; perf_seal work-budget ratchet + use-case-baseline.json
  :tier     Design                     ;; evidence exists (3/12 honest G2 wins) but no typed catalog
  :blocker  None
  :notes    "As typed catalog: 0/12 (UseCaseClass + use_case_matrix.rs absent). As evidence: 3/12 honest G2 wins (U01/U02/U04), U05 9.4x LOSS, U10 G0-RED DNF, rest unmeasured. Becomes Enumerated when (defuse-case) per class + the coverage/gate ratchet ship (M4), reusing perf::Delta.")
