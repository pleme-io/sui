# SUI SUPREMACY ROADMAP — from the current state to a complete, behavior-compatible nix replacement

> Grounded 2026-07-11 by a read-only recon of the actual owning crates (not the
> coverage-status field). Every claim below cites the fn/test it rests on.
> Companion source of truth: [`sui-spec/specs/nix_replacement_coverage.lisp`](../sui-spec/specs/nix_replacement_coverage.lisp)
> (38 surfaces: 21 Done · 10 InProgress · 7 Queued). This doc is the
> **dependency-ordered assault plan** to full supremacy plus a **tier-honest
> scorecard** that corrects the coverage catalog where the code disagrees.

The **marquee supremacy proof** this roadmap drives to: **sui rebuilds the
operator's Mac (`darwinConfigurations.cid`) end-to-end — eval → build → store →
activate — byte/behavior-identical to nix.** Everything is ordered by what that
proof needs.

> **Runtime companion — running the proof forever.** This roadmap drives the
> *one-shot* rebuild (`sui system rebuild`). The **continuous** form —
> `sui system converge [--watch]`, the node kept *always rebuilt into place* via
> a Viggy seven-beat reconcile loop — is [`CONVERGENCE.md`](./CONVERGENCE.md). It
> reuses the exact `rebuild_native` pipeline behind a mockable Environment seam
> and **rides this same M2.6 gate**: the loop is real + tested today, but a
> *byte-identical* cid convergence is blocked on the module-system fixpoint just
> as the one-shot rebuild is. The loop inherits the gate; it does not remove it.

---

## 0. The one blocker that gates the marquee proof

Everything on the critical path is *already wired* except one surface: the
**NixOS/Darwin module-system fixed-point** (`lib.evalModules` / `lib.nixosSystem`
/ `lib.darwinSystem`). Confirmed by:

- `docs/M2.6-MODULE-SYSTEM-FIXPOINT.md` — status **OPEN**. `lib.nixosSystem` hits
  `Eval(InfiniteRecursion)` through `extraArgs ↔ matchedOptions` (16-frame force
  chain captured) even with `modules = []`.
- `sui-eval/tests/system_eval_parity.rs` — the drvPath-of-toplevel test is
  **`#[ignore]`d**, with the in-file comment: *"When M2.6 closes, remove the
  `#[ignore]` and the assertion flips: success becomes mandatory."* Today it only
  navigates to `darwinConfigurations.cid…toplevel.drvPath` informationally.

This is the surface **being worked now in the `module-parity` worktree**
(`.claude/worktrees/module-parity`, branch `sui-module-parity-a6e84c`). It is the
sole hard gate. Skip it in this plan per the brief — but note: **it unblocks the
entire critical path** (system-rebuild ×3 + the `system_eval_parity` assertion
flip). Nothing else on the marquee path is missing.

---

## 1. Tier-honest scorecard

Legend: **REAL** = implemented + tested, verified by reading the fn/test ·
**REAL-gated** = full impl exists, hard proof `#[ignore]`d behind M2.6 ·
**PARTIAL** = works unprivileged / for the common case, named gap · **SPEC-ONLY**
= typed border + parser, no realizer · **PLANNED** = crate exists, surface absent.

### Spot-checks of "Done" surfaces (verify, don't trust)

| Surface | Claimed | Verified reading | Verdict |
|---|---|---|---|
| `eval-language-builtins` | Done | `sui-eval/tests/lang_corpus.rs` + `vs_cppnix.rs` + `drv_path_parity.rs` (real `nix eval` vs `sui_eval_drv_path`, `assert_eq`) exist and are live (not ignored). | **REAL** ✅ |
| `derivation-hash-parity` | Done | `sui-spec/specs/derivation.lisp` drives both engines; `drv_path_parity_simple_tools` asserts sui drvPath == `nix eval`. `derivation.rs` is the typed IA+FOD pipeline. | **REAL** ✅ |
| `substituter-narinfo-pull` | Done | `sui-store/src/binary_cache.rs` + `http.rs` + `substitute.rs` — real narinfo/NAR pull, consumed by `rebuild_native` via `Substitutor::new`. | **REAL** ✅ |
| `daemon-worker-protocol-cppnix` | Done | `sui-daemon/src/{connection,server}.rs` present; separate graph server too. | **REAL** (not deep-read; no stubs found) |

**No "Done"-but-stub caught.** The 21 Done surfaces that were spot-checked are
genuine. One honesty nuance on the boundary (see `store-add-path` below): the
*Done*-adjacent store write path is REAL at the daemon/`LocalStore` layer but the
default *unprivileged CLI* is a PARTIAL shim.

### InProgress — corrected to what the code actually is

| Surface | Catalog | Actual (grounded) | Verdict |
|---|---|---|---|
| `store-add-path` | InProgress | `LocalStore::add_to_store` (sui-store/local.rs:275) does full NAR-unpack + correct `source:sha256:…` fingerprint store path + DB `register_path` (refs + deriver edges). **BUT** the default `sui store add-path` CLI (src/main.rs:1544) computes the right path yet writes to `~/.cache/sui/added-paths` and prints *"daemon write requires sudo/root"* — privileged `/nix/store` write goes through the daemon. | **REAL (store layer) / PARTIAL (unprivileged CLI)** |
| `store-gc` | InProgress | `LocalStore::collect_garbage` (local.rs:328) — real mark-and-sweep: `find_gc_roots` scans `/nix/var/nix/{gcroots,profiles}`, walks the reachable closure via `query_path_info` refs, computes dead set. Wired to `sui store gc` (main.rs:4840). | **REAL** — mislabelled; closer to Done |
| `module-graph`/`-compiler`/`-solver`/`ast-evaluator`/`eval-module-system` | InProgress | Types + AST tree-walker (61+ tests) exist; the **fixpoint through real nixpkgs `evalModules` recurses** (M2.6 OPEN). Being worked NOW. | **REAL (synthetic) / gated (real modules)** — the true blocker |
| `system-rebuild-{nixos,darwin,home-manager}` | InProgress | `SystemOrchestrator::rebuild_native` (orchestrate/system.rs:184) is a **fully native** pipeline: `evaluate_flake` → `navigate_attrs` to `…system.build.toplevel` → extract `drvPath` → `BuildClosure::compute` → `LocalBuilder::build_closure` (with `Substitutor`) → `ProfileManager::set` + run `/activate`(+`/activate-user` on Darwin). Wired to `sui system rebuild` (main.rs:5302). The *legacy* delegating `rebuild()` is `#[deprecated]`. | **REAL pipeline / gated** — every step exists; only the eval step's toplevel drvPath is blocked by M2.6 |

### Queued — sized honestly

| Surface | Owner state | Load-bearing gap | Size |
|---|---|---|---|
| `store-optimise-hardlinks` | `optimise_store` (local.rs:499) **already exists** — real `std::fs::hard_link` dedup by content hash. Catalog says "planned" but the impl is there + CLI-wired (main.rs:4861). | Reflink/CoW fast path (ZFS/btrfs) + `links/`-dir parity with cppnix; correctness harness vs `nix-store --optimise`. | **S** (mostly done; needs oracle + reflink) |
| `derivation-graph` | `sui-spec::derivation` models ONE derivation (typed IA+FOD). No graph/closure form. | A typed multi-derivation closure structure (drv → input-drvs edges) for closure ops; `store-gc` already walks the *output* ref graph, this is the *derivation* graph. | **M** |
| `derivation-ca-derivations` | `sui-spec::realisation.rs` (168 LoC) is a **parse-only** format spec — parses `.doi` realisation JSON (`SpecError::Interp` on bad input). No IA→CA drvPath rewrite, no `__contentAddressed` build path, no realisation-store write. | The whole CA machinery: CA drvPath algorithm, floating-CA output resolution, realisation table write + query, deferred-input rewriting on build. cppnix's most involved subsystem. | **XL** |
| `substituter-typed-closure-stream` | `sui-protocol` ships **wire types + version negotiation only** (local rkyv frame). tvix-castore-shaped tonic/protobuf endpoint is roadmap-in-doc, not built. | The tonic gRPC service + castore-shaped protobuf schema + streaming closure server/client; extends the working narinfo substituter, never replaces it. | **L** |
| `daemon-fleet-work-stealing` | `sui-orchestrate` has real fleet/node/topo-sort machinery (`fleet.rs` 2309 LoC, `topo_sort`); `sui-protocol` roadmap names a REAPI-shaped remote endpoint (absent). | REAPI-shaped protobuf + tonic-over-HTTPS (Tailscale identity) dispatch service; work-stealing scheduler across cid+ryn+rio+VMs. | **L** |
| `eval-tlisp-dialect` | `ast_graph::from_tlisp_source` returns an **Unknown stub** (typed seam only). | Full `.tlisp` → `AstGraph` lowering for every `AstNodeKind` variant + bidirectional Nix↔Tlisp. Not on the nix-parity critical path — it's the *superset* dialect. | **L** (off-critical-path) |
| `nixos-rebuild-cli-shim` | `sui-nix-wrap` (275 LoC) is a **routing wrapper** — argv → catalog-maturity → `→sui` / `✘gap`. It routes `nix-*` symlinks; a `nixos-rebuild` mode is absent (it's a host script, not a `nix-*` symlink). | A `nixos-rebuild`/`darwin-rebuild` arg-parse mode that drives `sui system rebuild`. Trivial once system-rebuild parity lands. | **S** (gated on system-rebuild) |

---

## 2. The critical path to the cid-rebuild supremacy proof

`sui system rebuild switch --flake .#cid` must produce the same activated system
as `darwin-rebuild switch --flake .#cid`. Every surface on that path, in order,
with status:

| # | Surface | Crate/fn | Status |
|---|---|---|---|
| 1 | **Flake eval → outputs attrset** | `sui_eval::builtins::evaluate_flake` | ✅ REAL (`eval-flake-evaluation` Done; flake_eval_parity live) |
| 2 | **Navigate to `darwinConfigurations.cid.system.build.toplevel`** | `sui_eval::builtins::navigate_attrs` | ✅ REAL — navigation reaches the node (`system_eval_parity` levels 1–4) |
| 3 | **Module-system fixpoint forces `toplevel` → `drvPath`** | `sui-spec::module_{graph,compiler,solver}` + `ast_evaluator` + `lib.evalModules` | 🔴 **BLOCKED (M2.6)** — real nixpkgs `evalModules` infinite-recurses. *Being worked now.* THE gate. |
| 4 | **drvPath byte-parity vs nix** | `derivation.rs` + `drv_path_parity.rs` | ✅ REAL for direct derivations; ⚠️ the *toplevel* drvPath assertion is `#[ignore]`d until #3 closes |
| 5 | **Compute build closure** | `sui_build::BuildClosure::compute` | ✅ REAL |
| 6 | **Substitute from binary caches** | `sui_store::Substitutor` + `binary_cache`/`http` | ✅ REAL (`substituter-narinfo-pull` Done) |
| 7 | **Build the closure (sandboxed)** | `sui_build::LocalBuilder::build_closure` + `DarwinSandbox` | ✅ REAL (`derivation-build-sandbox` Done) |
| 8 | **Register outputs in the store** | `LocalStore` register / daemon | ✅ REAL at daemon/store layer (⚠️ unprivileged CLI shim is PARTIAL — root write via daemon) |
| 9 | **Set the system profile** | `sui_store::ProfileManager::set` | ✅ REAL |
| 10 | **Run `/activate` (+`/activate-user`)** | `SystemOrchestrator::activate_system` | ✅ REAL |
| 11 | **CLI dispatch `sui system rebuild`** | `src/main.rs:5302` → `rebuild_native` | ✅ REAL (wired) |

**Read this table plainly: 10 of 11 surfaces on the marquee path are REAL and
wired. The proof is one surface away — module-system fixpoint parity (#3).** When
M2.6 closes, #4's ignored assertion flips to mandatory and the whole path is
provable end-to-end.

---

## 3. Dependency-ordered assault plan (the phases)

### Phase A — Module-system fixpoint parity ← **IN FLIGHT (module-parity worktree)**
**Unblocks:** system-rebuild ×3, the marquee cid proof, `system_eval_parity`
assertion flip, `nixos-rebuild-cli-shim`.
- **Target:** `sui eval --no-vm '(nixpkgs.lib.nixosSystem { … }).config.system.name'`
  returns the real value (no `InfiniteRecursion`), and
  `darwinConfigurations.cid…toplevel.drvPath` == `nix eval …toplevel.drvPath`.
- **Oracle:** `nix eval --raw .#darwinConfigurations.cid.config.system.build.toplevel.drvPath`
  vs sui; then remove `#[ignore]` in `system_eval_parity.rs`.
- **Gap:** the `extraArgs ↔ matchedOptions` recursion in `lib/modules.nix` under
  sui-eval (M2.6 force chain). **XL** (the hardest surface in the whole plan).

### Phase B — Marquee proof: cid rebuilds byte-identical
**Depends on:** Phase A. Everything else is done (§2).
- **Target:** `sui system rebuild build --flake .#cid` yields a `system_path`
  whose `outPath` == `darwin-rebuild build`'s; then `switch` activates identically.
- **Oracle:** compare `sui system rebuild build` outPath to
  `nix build .#darwinConfigurations.cid.system.build.toplevel --print-out-paths`;
  diff the activated `/run/current-system` symlink target.
- **Gap:** **S** — pure verification once A lands; the pipeline (`rebuild_native`)
  already exists.

### Phase C — Store write path hardening (the two InProgress→Done conversions)
Parallelizable with A (no dependency on module-system).
- **C0 IFD realize on a multi-user store — ✅ LANDED (daemon-store-write pivot).**
  On the operator's daemon-based Mac (`/nix/store` + `db.sqlite` root-owned, sui
  runs uid 501) the IFD realize hook's `LocalBuilder`/`open_rw` path could not
  write, so `import (stylix-fonts)` computed the right `.drv` then died at
  `cannot read <drv>`. The pivot routes IFD realize through the running nix daemon
  (worker protocol): `AddTextToStore` the computed `.drv` closure (byte-identical
  to nix) → `BuildPaths` (substitute-or-build) → `IsValidPath` attestation. Two
  seals in `sui-store/src/daemon_realize.rs`:
  - **`StoreAccess` dispatch** (`Direct(WritableStore)` | `Daemon(DaemonStore)`),
    chosen at construction. `WritableStore`'s only constructor probes writability,
    so `Direct` over a read-only store is *unconstructable* → the `cannot read
    <drv>` failure class is **truly-unrepresentable on the dispatch axis** (not a
    runtime guard). The IFD hook (`src/main.rs`) dispatches on it.
  - **`Realized` proof** — sole constructor requires the daemon to attest the
    output valid at its content-addressed store path; a wrong/absent output is
    **parse-time-rejected**. Tier: **C2 external-observation ceiling** — sui
    observes the daemon's content-addressing attestation rather than re-hashing
    root-owned bytes it cannot read.
  - **Proven live end-to-end**: `sui eval` on a `pathExists "${pkgs.hello}/bin/…"`
    IFD flake realizes hello via the daemon (both fast-path and, after GC'ing the
    output, the full AddTextToStore+BuildPaths **substitute** path), returning the
    same `true` as the nix oracle. Regression test:
    `sui-store/tests/daemon_realize_oracle.rs`. `sui parity` = 39 match, 0
    regressions, `hello` byte-identical throughout.
  - **What this unlocks:** IFD realize now works on the operator's multi-user
    store (the build-half's store-write dependency) and it is the reusable
    daemon-write substrate C1 should consume. **What remains:** the cid
    *full-toplevel* eval still does **not** reach IFD at all — it blocks in the
    Phase A module-system fixpoint (0% CPU, no drv instantiated, no `sui-drv-cache`
    written), exactly where §0 says it does. So IFD-on-cid is gated on Phase A
    landing first, then on sui's darwin-eval byte-parity for that closure (the
    pivot's aarch64-darwin IFD probe surfaced a real `perl-5.42.0-devdoc`
    output-path divergence — a pre-existing eval-parity gap the daemon correctly
    rejects, not a pivot defect). The pivot is proven on the *isolated* IFD flake;
    the cid demonstration waits on the upstream gate.
- **C1 store-add-path:** promote the unprivileged CLI shim to a real
  daemon-mediated write. **Target:** `sui store add-path <dir>` produces the same
  `/nix/store/<hash>-<name>` + registers in the DB as `nix store add-path`.
  **Oracle:** `nix store add-path <dir> --name n` vs sui path string + DB row.
  **Gap: M** (the `add_to_store` realizer exists; wire the CLI through the daemon;
  the C0 pivot's `DaemonConn` worker-protocol client is the reusable substrate).
- **C2 store-gc → Done:** re-label + add the oracle. **Target:** sui's dead set ==
  `nix-store --gc --print-dead`. **Oracle:** exactly that. **Gap: S** (impl done;
  needs the differential test).
- **C3 store-optimise → Done:** **Target:** sui's dedup matches
  `nix-store --optimise` (same freed bytes, links/-dir parity) + reflink fast path.
  **Oracle:** `nix-store --optimise` freed-bytes vs `sui store optimise`.
  **Gap: S**.

### Phase D — Closure + CA (the derivation tail)
- **D1 derivation-graph (M):** typed multi-drv closure structure. **Unblocks**
  closure operations for D2 and the fleet dispatch payload (F).
  **Oracle:** `nix-store --query --requisites <drv>` vs sui closure.
- **D2 ca-derivations (XL):** the big one. **Target:** a `__contentAddressed =
  true` derivation's drvPath + realisation match nix. **Oracle:**
  `nix derivation show <ca.drv>` + `nix path-info --json` realisation vs sui.
  **Depends on:** D1 + the realisation-store write path (today parse-only).

### Phase E — Typed closure streaming (L)
Extends the working narinfo substituter; never replaces it.
- **Target:** one-round-trip closure fetch over tonic/protobuf (tvix-castore-shaped).
- **Oracle:** the streamed closure NAR bytes == the narinfo-pulled closure.
- **Depends on:** D1 (closure structure) + `sui-protocol` gRPC surface.

### Phase F — Fleet work-stealing (L)
- **Target:** `sui build` on cid dispatches a drv to ryn/rio and returns the same
  output as a local build. **Oracle:** local-build outPath == remote-dispatched
  outPath.
- **Depends on:** D1 (closure payload) + `sui-protocol` REAPI endpoint +
  `sui-orchestrate` fleet machinery (already real).

### Phase G — Off-critical-path superset: tlisp dialect (L)
Not required for nix supremacy — it's the *superset* dialect. Schedule after F.
- **Target:** `from_tlisp_source` lowers every `AstNodeKind`; Nix↔Tlisp round-trips.

---

## 4. Highest-leverage next 3 waves after module-system (fire immediately)

Ordered by leverage-per-effort; all three are independent of Phase A and can run
concurrently with the module-parity work.

### Wave 1 — Close the marquee proof the instant M2.6 lands (Phase B verification)
- Have the `nix build .#…cid…toplevel --print-out-paths` vs `sui system rebuild
  build` differential test **written and `#[ignore]`d now**, so the moment Phase A
  goes green it un-ignores and the supremacy proof is a CI gate, not a manual run.
- **Why highest:** zero new engine work; it converts the whole eval→build→store→
  activate pipeline (10 REAL surfaces) into a single attested claim.

### Wave 2 — Convert the two mislabelled store surfaces to Done (Phase C2 + C3)
- `store-gc` and `store-optimise` are **already implemented and CLI-wired**; they
  are InProgress/Queued only because the *oracle differential* isn't written.
  Ship `sui store gc`-vs-`nix-store --gc --print-dead` and `sui store optimise`-vs-
  `nix-store --optimise` differentials.
- **Why:** two catalog rows flip to Done for the cost of two tests + the store
  write path (the #8 critical-path surface) gets its correctness harness.

### Wave 3 — Real daemon-mediated store-add-path (Phase C1)
- Promote `sui store add-path` from the unprivileged `~/.cache` shim to the
  daemon-mediated `LocalStore::add_to_store` write (the realizer already exists).
- **Why:** this is the *last* PARTIAL surface on the marquee critical path (#8) —
  closing it makes the entire cid-rebuild path REAL end-to-end without the
  "requires sudo/root" caveat, and it's the write half every future surface (D2
  realisation store, E closure stream) builds on.

**Deferred (bigger than they look):** `derivation-ca-derivations` (XL — a
parse-only spec today, needs the whole CA subsystem) and the tonic/REAPI protocol
work (L each — `sui-protocol` ships wire types only) are real engineering, not
oracle-writing. They come after the marquee proof is banked.

---

## 5. One-line honest summary

**sui is one surface — the NixOS/Darwin module-system fixpoint (M2.6, in
flight) — away from proving it rebuilds `darwinConfigurations.cid` byte/behavior-
identical to nix.** 10 of the 11 marquee-path surfaces are REAL and wired; the
store write path's only gap is an unprivileged-CLI shim (the daemon realizer
exists); the genuinely-large remaining work (CA derivations XL, typed-closure/
fleet protocols L×2, tlisp dialect L) is all *off* the marquee critical path.
