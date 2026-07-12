# Darwin-Parity Campaign — the cid marquee proof's last mile

> Big-bang plan (2026-07-12), adversarially verified against `sui` `main` @ `ff3daba`.
> Leads with the destination (Op#0). Every claim carries its tier; nothing rounds up.
> **Three overclaims were caught by the adversarial pass and are recorded in §6 so
> they can never silently return.**

## 1. Destination (unhedged)

`sui system rebuild build --flake .#cid` produces a `system.build.toplevel` whose
`outPath` is byte-identical to `darwin-rebuild build --flake .#cid`, and `switch`
activates identically — with the **entire cid aarch64-darwin closure's per-drv
`drvPath` byte-parity SEALED as a `RowExpect::Match` corpus** that fails CI on any
regression, and the class "a darwin drv silently diverges" surfaced as a red
`regression`/`graduated` tally, never swallowed.

**Two ceilings baked in, stated not rounded:**
- **Realize parity is a C2 external-observation ceiling.** sui (uid 501) over the
  multi-user root-owned `/nix/store` asserts the daemon attests the same
  content-addressed `out_path` (`IsValidPath` proof) — **out-path-address equality**,
  NOT built-output byte equality. It cannot re-hash root-owned bytes. Never claim
  byte equality across this boundary.
- **The apple-sdk `PROMOTION_RUNAWAY_EVAL_DEPTH=500` backstop is `only-mitigated`** —
  a tuned heuristic. Converting it toward a typed invariant is honesty-tail work (M5).

## 2. What darwin parity actually IS (the corrected framing)

**The shared half is real and load-bearing.** The eval KERNEL — parse, laziness,
the drvPath hashing algo (`derivation.rs`: `system` is just a forced attr string in
the env JSON; nothing in the hash path branches on platform), module-system merge,
`NixValue` — is genuinely arch-independent. sui-eval carries exactly **one** meaningful
platform `cfg` in eval logic (`eval_cache.rs:313`, a cache *directory*) plus the
mechanical `current_system()` arch→string selector. **Running the *same* `sui parity`
binary on cid renders aarch64-darwin store paths with zero code change** — the single
largest reuse fact of the whole campaign.

**But "darwin = small delta over linux" is an OVERCLAIM.** It conflates the shared
kernel with the darwin stdenv CORPUS. Honest picture:
1. The **102/104 byte-verified basket is x86_64-linux ONLY**, freshly-true (2026-07-11),
   still 2 short (ffmpeg, neovim). There is **no verified darwin basket** beyond a single
   darwin-native `hello` row. Darwin byte-parity is an **open, actively-worked frontier.**
2. **Most cid-closure divergences will be the SAME general eval-primitive roots already
   burned down on linux** — darwin *surfaces more of them* (IFD/apple-sdk/flit-core exercise
   paths the linux baskets never hit), but they are general roots that **cascade to linux
   too**. Worked proof: darwin `hello` graduated via ROOT #10 (`77e0e12`) which ALSO closed
   linux curl+git — the *opposite* of an isolated darwin root.
3. The **genuinely darwin-SPECIFIC residue is the IFD + store-write MECHANISM**, not a set
   of packages: daemon multi-user store write (`ff3daba`), IFD realize-during-eval (`31d1043`),
   fetched-input source materialization (`0ade005`), stylix `darwinModules` isFunction
   (`5d6e913`), filterSource root-pruning (`b5f68ee`), flake-input in-store source copy
   (`88daa3e`) — **all already closed this session** — plus the cross-system apple-sdk/flit-core
   `hello` path.

> **Correction to the workflow draft:** the draft called the M2.6 module-system fixed-point
> "the still-open sole marquee gate." **M2.6 is CLOSED** (`45aaed2`, on main; `nixosSystem`
> terminates + generalizes to darwin). The cid marquee is gated on **this darwin-parity
> descent**, not on M2.6.

**Honest characterization:** *shared kernel, LARGE untested darwin corpus surfacing a
distinct IFD/store-write class of mechanism-roots.* Default to the larger estimate — linux
eval-primitive fixes do NOT mechanically transfer to darwin's IFD/store-mediated realize paths.

## 3. Reuse map — the linux Parity Method EXTENDED, never rebuilt

The harness is already system-agnostic: `drv()` in `parity_corpus.rs` pins
`system = builtins.currentSystem`, so every existing outPath/drvPath row renders
aarch64-darwin paths on cid unchanged.

| Component | Disposition |
|---|---|
| `cmd_parity` runner + seal-gate tally (`regression`/`graduated`) | **REUSE** — CONVERGE=SEAL |
| `Expect`/`ParityVerdict` verdict algebra | **REUSE** |
| `diff_eval` (drives `--no-vm` tree-walker; the VM defers StringContext) | **REUSE** |
| `cmd_parity_bisect` → `parity-enumerate --system-closure` | **EXTEND** |
| `gen_nix::NixValue` typed AST builders (TYPED EMISSION) | **REUSE** |
| `first_store_path_matching` (scans `/nix/store`) | **REUSE** — works on darwin |
| sui-sweep `(defprobe)` + aarch64-darwin probes | **REUSE** — darwin probes present |
| Landed realize seal (`StoreAccess`/`Realized`/`realize_via_daemon`, `DarwinSandbox`) | **CONSUME, never touch** |

**EXTEND** (one function, shared builders — never a forked `generate_darwin()`, which
violates the Prime Directive): `parity_corpus::generate()` gains a darwin-frontier row
cluster using the same `drv()`/`let_()`/`attrs()`. Each diagnosed root is sealed as a
`RowExpect` row — `Match` if its fix is on main, `KnownDiverge` if on a `+1`-ahead branch
(merging the branch mechanically **graduates** the row).

## 4. Divergence-class taxonomy (typed, CATALOG REFLECTION)

Closed `enum DivergenceClass` (sum-over-product; a new class is a compile-forced arm):

| Variant | Root / fix | Axis · cascade |
|---|---|---|
| `FixpointDepDropped` | `derivation.rs:214` swallow → overlay-fixpoint demand-order (perl/libxcrypt) | eval-engine · multi-package **UNCONFIRMED** — do not assume cascade |
| `FlakeInputOutPath` / `SourceRootPruned` / `FetchedInputMaterialize` | `flake_eval.rs`/`path.rs`/`paths.rs` | eval · per-root, land branch → graduate |
| `IfdRealizeStoreWrite` | landed `StoreAccess`/`Realized` seal (stylix-fonts class) | **store-write** · consume via `diff_realize` |
| `DarwinModulesIsFunction` | stylix `darwinModules` select | eval · land branch → graduate |
| `ArchStringDrift` | duplicated `currentSystem`: `convert_helpers.rs:13` vs `sui-bytecode/builtins.rs:76` | eliminate-the-shared-cell → truly-unrep (M5) |
| `MultiOutputSelect` | **tripwire, expected EMPTY** | falsifies the "multi-output is a root" claim if hit |
| `Unclassified` | none | forces a human + a new arm; never silently bucketed |

**The swallow (`derivation.rs:214`, `Err(_)=>continue`) is the meta-risk:** it hides
*stacked* roots as "value diverges" and is why the true darwin root-count is invisible
until measured. A gated `SUI_PARITY_STRICT` collector un-blinds them **for enumeration
only** — the default is never changed (naive surfacing regresses `libxcrypt`).

## 5. Phased plan (path DOWN from the destination)

- **M0 — un-blind + MEASURE the real backlog (½–1 day; FIRST, gates everything).**
  (a) Add the `SUI_PARITY_STRICT`-gated collector to `derivation.rs` (enumeration-only,
  default unchanged). (b) Un-ignore the darwin toplevel drvPath assertion
  (`system_eval_parity.rs:135`). (c) Run `sui system rebuild build --flake .#cid` (`--no-vm`)
  with strict → emit the **measured** darwin root backlog ordered by cascade-width.
  **Deliverable: a red darwin ledger with named roots, not a projection.** No campaign
  step commits before this — it converts the root-count from a guess to a list and surfaces
  any *systemic* bootstrap root on the first closure run.
- **M1 — stdenv-bootstrap byte-parity** (clang/cctools/apple-sdk/Libsystem/libtapi/sigtool
  — deepest, most-shared, the ONLY bucket the linux descent never exercised; where the
  apple-sdk promotion-runaway backstop fires). **Single biggest uncertainty:** whether the
  bootstrap carries a *systemic* (not pointwise) divergence → one XL root pushes the timeline
  to its upper bound. perl-devdoc is the early warning the class exists but appears
  **bounded/targeted, not yet proven systemic.**
- **M2 — shared-leaf cluster descent (the bulk):** broaden-basket → cluster-by-shared-leaf →
  fix-general-root → cascade (linux strategy verbatim). **Expect most to be linux-shared roots.**
- **M3 — realize-parity primitive + IFD rows (the net-new build):** add `diff_realize`
  (offline-`Skipped`-gated), the IFD/realize corpus category; compose the landed seal; seal
  darwin IFD as out-path-address-equal.
- **M4 — cross-system + full cid toplevel gate:** flip the un-ignored toplevel assertion to
  a mandatory green gate → the cid marquee eval-half is proven → build-half.
- **M5 — typescape hardening (honesty tail):** fold the duplicated `currentSystem`
  (→ truly-unrep `ArchStringDrift`); convert the promotion-runaway backstop toward a typed
  invariant; ledger every C2 ceiling.

**Sequencing discipline:** un-blind (M0) → enumerate → seal-as-`KnownDiverge` → **land the
`+1`-ahead branch** → graduate to `Match` *in the same commit as the merge*. `+1`-ahead
land-list: `darwin-root-2`, `marquee/darwin-root-ifd`, `marquee/darwin-root3-input-materialize`,
`root-isfunction-select` — **NOT yet on main** (each graduates its row on merge). *(Note: as
of this doc, these are being cherry-picked to main root-by-root; update the land-list as they land.)*

## 5a. M0 vertical slice — one darwin package end-to-end

**Target: `hello` on aarch64-darwin, drvPath byte-identical to the darwin nix oracle, via the
extended `sui parity` harness.** Chosen because it's already CLOSED on linux AND graduated to
`Match` darwin-native (ROOT #10) — the honest *first green row that proves the pipe*, not the
frontier. Thread: un-ignore `system_eval_parity.rs:135` + add the strict collector → add one
`RowExpect::Match` row via `drv()` under `builtins.currentSystem` → `sui parity --no-vm` on cid
→ oracle `nix eval --raw nixpkgs#hello.drvPath` → `diff_eval` byte-equality → `graduated: 1,
regression: 0`. **DoD:** green `Match` row, CI fails on regression, AND the strict collector emits
the first *measured* darwin backlog for M1.

## 6. Honest root-count + timeline (PROJECTION until M0 measures)

- ~6 darwin-relevant roots already closed this session (daemon-store-write, IFD-realize,
  fetched-input materialize, darwinModules isFunction, filterSource pruning, flake-input
  in-store copy). ~1 concrete new probed: perl-devdoc (`FixpointDepDropped`).
- sui-eval is ~99.9% arch-independent, so the ~9–12 general linux roots do NOT repeat as
  darwin-*new* work — but darwin re-surfaces many of them (same shared roots), plus the
  darwin-specific IFD/store-write residue.
- **Realistic remaining darwin-SPECIFIC eval roots: ~4–10 — a PROJECTION, invisible until the
  un-blinded M0 closure run** (the swallow hides stacked roots). Each is a targeted
  laziness/coercion/materialize fix in a *shared* primitive — **no package patches.**

**Timeline, honestly:** NOT days for the full marquee (toplevel eval not yet byte-parity;
unseen roots guaranteed until M0). NOT the multi-week linux descent either (shared engine sealed,
~9–12 general roots already paid, 10/11 build surfaces real). **Honest center: M0 = ½–1 day →
M1–M2 = ~1–2 weeks focused root-grinding → M3/M4 = days** (verification against already-real
build/store/activate machinery). **Upper-bound trigger, named:** a *systemic* stdenv-bootstrap
divergence — surfaced by M0, not left latent.

## 7. Claims that MUST NOT round up (the caught overclaims — permanent record)

1. **"Darwin is a small delta over linux."** REJECTED — shared kernel, LARGE untested corpus.
2. **"Multi-output-path is one cascade-fixable class."** FALSE — per-output paths are a pure
   function of one recipe hash; stylix-fonts (store-write) vs perl-devdoc (eval-engine) are
   different axes, no cascade. Kept only as the `MultiOutputSelect` tripwire.
3. **"The ~9-package cascade from the fixpoint root."** UNCONFIRMED per sui's own ledger —
   the diverging tail has multiple independent roots.
4. **"Only a handful of darwin-specific derivations diverge."** REFUTED — most are the SAME
   general roots as linux; the darwin-specific residue is IFD/store-write mechanism.
5. **Darwin BUILT-output byte-parity.** IMPOSSIBLE from uid 501 over a root-owned store (C2
   ceiling). Claim only **out-path-address equality** (daemon `IsValidPath`).
6. **The apple-sdk backstop + duplicated `currentSystem`.** Both **only-mitigated** (M5 targets).
7. **The root-count "~4–10."** A PROJECTION, not a measurement — invisible until M0.
8. **The linux basket.** SHIPPED-partial (2 open), not "at parity."
9. **"Everything landed."** The `+1`-ahead darwin roots graduate their rows only on merge to main.
