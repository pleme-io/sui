# Sui (粋) — Rust-Native Nix Replacement

> **★★★ CSE / Knowable Construction.** This repo operates under
> **Constructive Substrate Engineering** — canonical specification at
> [`pleme-io/theory/CONSTRUCTIVE-SUBSTRATE-ENGINEERING.md`](https://github.com/pleme-io/theory/blob/main/CONSTRUCTIVE-SUBSTRATE-ENGINEERING.md).
> The Compounding Directive (operational rules: solve once, load-bearing
> fixes only, idiom-first, models stay current, direction beats velocity)
> is in the org-level pleme-io/CLAUDE.md ★★★ section. Read both before
> non-trivial changes. Pure-Rust Nix replacement; an in-progress Layer-1
> normalization platform replacing CppNix with construction guarantees.

<!-- Blackmatter alignment: pillars 1, 9 -->
<!-- See ~/code/github/pleme-io/BLACKMATTER.md for pillar definitions. -->

## Blackmatter pillars upheld

- **Pillar 1** (Rust + tatara-lisp + WASM/WASI): Sui is the Rust half of the language stack, taken all the way down — a pure-Rust evaluator + bytecode VM + build system that replaces CppNix. Exceeds CppNix 3x on 45/48 benchmarks with a 16-byte Value and 8-byte NanBox.
- **Pillar 9** (SDLC): Sui extends the SDLC from single-node to distributed. `sui build` replaces `nix build`; NATS-triggered rebuilds shard across a cluster; Attic is the shared convergence memory. Store paths stay the content-hash proof in either mode.

Pure-Rust Nix evaluator + build system. Drop-in `nix` CLI replacement.

## ★★ North star — the nix→sui flip (destination-first, no hacks in the evaluator)

sui replaces nix at the **binary level**: the fleet's default rebuild
(`nix run .#rebuild`, darwin-/nixos-rebuild) will invoke sui over the **same
shared `/nix/store`** — byte-identical hash/NAR/drv/binary-cache formats
(the `sui parity` corpus gate — its enforcement status is stated
tier-honestly below, not rounded up). The flip is the destination, not a
maybe; a change moves toward it or is off-course.

**The bar is 100%, byte for byte — table stakes, not a goal.** A replacement that
matches 99% of the ecosystem is not a nix replacement; the missing 1% is exactly
where a real system silently mis-builds. Standing invariant, now and across every
future version of sui as a binary-delivery platform: for the ENTIRE ecosystem sui
touches — nixpkgs, darwin-nix, nixos, home-manager — sui's `outPath` / `.drv` /
NAR / realized-store bytes are **identical to nix's, or it is a defect**, never an
acceptable state. **Silent divergence** — a wrong value that still evaluates — is
the worst failure of all, worse than a crash, because it corrupts a build with no
signal; that is why a band-aid that "makes eval proceed" is forbidden (it trades a
loud gap for a silent one).

**Proven, not asserted — the enforced form of this fact is `sui parity` extended
to a corpus gate** (the typed vocabulary + phased plan: [`docs/BYTE-PARITY-TYPESCAPE.md`](docs/BYTE-PARITY-TYPESCAPE.md))**:** a differential that evaluates a growing corpus (nixpkgs
packages → whole nixpkgs → system closures) in sui *and* nix and byte-compares
every artifact, going red on a single differing byte. `sui == nix` is then a
theorem the gate re-proves on every change — the mechanical observation of this
fact over sui's evolution. Grow the gate's coverage with the evaluator so a green
build never outruns the corpus it actually proves.

**Tier-honest on the gate itself (2026-07-18 — never round up the enforcer).**
The corpus is *split* by where it actually proves anything, and the doc must say
so. The ~40 pure-builtins rows (hash / derivation / context / attrset-merge
algebra) are environment-independent — genuine CI theorems, reproducible on any
runner. The ~35 ecosystem rows (`hello`/`stdenv`/`neovim`/… + the darwin
`currentSystem` seeds) import **impure `<nixpkgs>`** — an *unpinned,
machine-dependent* oracle — so on CI (which resolves a different nixpkgs rev than
the one those rows were byte-closed against) sui **errors** on them and they count
as regressions. Net: `sui parity` was **green locally (against the operator's
`<nixpkgs>`) but inert on CI — 0 successes in the last 40 runs — and releases
cut through the red.**

> **★ CORRECTED 2026-08-09 — the paragraph above is STALE and destination (3)
> ALREADY SHIPPED. Re-measured, do not plan against the old text.** The corpus
> is **77/77 sealed, 0 regressions** (`sui parity`, identical with and without
> `SUI_PARITY_PUREONLY`), and `.github/workflows/parity.yml` already
> implements the per-row-skip design described below as pending:
> `SUI_PARITY_PUREONLY=1` reclassifies rows that *cannot evaluate* as Skipped,
> while a wrong byte (`Diverge`) still fails. So the gate BLOCKS on the
> environment-independent rows rather than being inert. Destinations (1)
> flake-locked oracle and (2) per-row `EnvCapability` remain open.
>
> **And the more important correction: a sealed corpus is not evidence the
> evaluator is correct.** The same day this was re-measured, evaluating the
> operator's REAL fleet flake found two silent divergences the 77 rows pass
> straight over — one of which renamed every NixOS system. The corpus is
> shaped so it cannot see the failure classes the flip depends on. Read
> [`docs/SILENT-DIVERGENCE-HUNT.md`](docs/SILENT-DIVERGENCE-HUNT.md) before
> planning any flip work; it carries the instrument, not just the status. A gate cannot be a "theorem re-proven on every change" while
its oracle floats: **★★ DETERMINISTIC INSTANTIATION is violated at the gate's own
ground truth,** and a chronically-red gate is *worse than none* because it blinds
the ~40 rows that would otherwise catch a real regression on every push. The
destination (one campaign, shared with the darwin/eval-memory work and
super-cache-ci's hermetic oracle): (1) the corpus imports a **flake-locked**
nixpkgs, not `<nixpkgs>`, so every machine evals the same bytes and red means a
*real* sui divergence; (2) each row carries a typed `EnvCapability` (arch + memory)
so it runs only where it can and **skips honestly** elsewhere (a linux runner skips
the darwin rows; a small runner skips the OOM-heavy rows) instead of
`sui-err`-as-regression; (3) CI then proves the reproducible subset as a
**blocking** gate and a pinned-oracle big-mem job proves the full corpus. The law
this taught, fleet-wide: *a seal is only real if its oracle is reproducible where
the seal is enforced.* Tracked as the parity-gate task.

Getting there is empirical — stock nix is the differential **oracle** (see
*The sui rhythm* below). Every fix, **especially in the subtle
evaluator/fixpoint code**, is elegant Rust + tatara-lisp, macro vocabulary
where a pattern repeats, no tech debt. A band-aid in the evaluator — returning
`null` "so eval proceeds", memoizing a transient fixpoint partial, an
env-var-gated sentinel — is **not a fix**: it hides a divergence the oracle
will resurface downstream. Fix the load-bearing cause (e.g. the genuine
`Promise(NixAttrs)` handling of a blackholed *lexical* binding in the
module-system fixpoint), never paper over it. This is Operating Principle #0
(path-of-least-resistance is a cardinal sin) at the evaluator.

## Core Philosophy: Construction Guarantees

Sui's architecture makes entire categories of bugs **impossible by construction**.
Rust's type system enforces invariants at compile time — no runtime checks needed.

| Guarantee | Mechanism | What it prevents |
|-----------|-----------|-----------------|
| **Laziness** | `Lazy<T>` wrapper — `.demand()` required to access | Accidental eager evaluation |
| **Value size** | `assert!(size_of::<Value>() <= 16)` at compile time | Cache-hostile value bloat |
| **Thunk memoization** | `OnceCell` fast-path — evaluated exactly once | Redundant computation |
| **Env sharing** | `im_rc::HashMap` HAMT — O(1) structural clone | Expensive deep copies |
| **String identity** | `Symbol(u32)` interning — comparison is `==` on u32 | String allocation in hot loops |
| **Memory safety** | Zero `unsafe` in evaluator logic (19 justified blocks in value.rs) | Use-after-free, data races |

**The pattern:** encode the invariant in the TYPE. The compiler enforces it. Bad states are unrepresentable.

## Workspace — the core crates

**Counted 2026-08-08: the workspace has 30 members; the table below is the
evaluator core plus the surfaces you are most likely to touch, not the whole
list.** The header used to read "11 crates" and was read as a total — it was a
subset that stopped being refreshed, which is the downward-rot shape the
org-level dated-claim rule warns about (a stale count reads as modest, so
nothing ever flags it as wrong). `Cargo.toml`'s `members` is the real roster and
carries a comment per non-obvious crate; count there, never here.

| Crate | Purpose |
|-------|---------|
| `sui` (root) | CLI binary — nix-compatible interface |
| `sui-eval` | Tree-walker evaluator + `Lazy<T>` primitives |
| `sui-bytecode` | Bytecode VM (NanBox 8B, 44+ opcodes, TAILCALL) |
| `sui-intern` | String interning (Symbol u32, thread-local Interner) |
| `sui-cache-eval` | Content-addressed eval cache (BLAKE3 keys) |
| `sui-compat` | Nix formats (NAR, store paths, ATerm, derivations) |
| `sui-store` | Store abstraction (SeaORM/SQLite) |
| `sui-build` | Build execution (sandboxed builder) |
| `sui-cache` | Binary cache (S3, local, redb) |
| `sui-daemon` | Daemon mode (worker protocol) |
| `sui-orchestrate` | System rebuild + fleet deployment |
| `sui-lsp` | Nix language server over sui's own parser + lowering (M0: diagnostics) |

### `sui-lsp` — the editor face, and why it is not a third front end

`nil` and `nixd` each re-implement a Nix front end so an editor can answer
questions about a file. That leaves the editor's idea of a file and the
evaluator's idea of the same file as two implementations that agree only by
effort. sui already owns the parser, the resolver, the lowering pass and the
evaluator — so the move is not writing a third front end, it is **exposing the
one that already evaluates the file**. A diagnostic from `sui-lsp` is not a
lookalike of the build's opinion; it is the build's opinion.

Shape: `diagnostics.rs` is pure (`&str` → `Vec<Diagnostic>`, no async, no LSP
types, no IO) and holds every decision worth testing; `server.rs` is the
`tower-lsp` shell and holds almost none. Positions come from `zahyou`, shared
with `escriba-lsp-client`, so both ends of the wire do the same UTF-16
arithmetic.

Two things worth knowing before extending it:

- **Most of the error surface has no span.** Five of rnix 0.14's eight
  `ParseError` variants carry a `TextRange`; three do not. One of `sui-ir`'s
  five `LowerError` variants carries byte offsets. `Anchor` exists so "we were
  not told where this is" is a represented state with a per-case answer —
  defaulting a span-less error to line 0 puts a squiggle on the first character
  for an unclosed brace at the bottom of a 400-line file.
- **`rnix::ParseError` is `#[non_exhaustive]`**, so the `match` over it is *not*
  a compile-time guard and a new upstream variant will fall into the wildcard.
  That is why the wildcard produces `Finding::UnrecognizedParseError` rather
  than quietly labelling it "unexpected token". Tier: only-mitigated (C2 — the
  variant set is upstream's and it opted out of the check).

**M0 scope, tier-honest:** diagnostics on open and change, proven end-to-end
against the real binary over stdio (`tests/stdio_smoke.rs` — handshake, a
correctly-positioned diagnostic on an emoji-bearing file, and the empty publish
that clears them). **No hover, goto-definition, completion or workspace
symbols** — those need the resolver's scope chain surfaced, and `initialize`
advertises only what is implemented so the editor never offers a feature that
silently does nothing.

## Laziness-First Evaluation

The evaluator's #1 principle: **never compute anything until a consumer demands it.**

CppNix forces 67 thunks for `(import <nixpkgs> {}).lib.version`. Every excess force
cascades into thousands of eval_expr calls. Maximum laziness = minimum work.

### Construction-Guaranteed Lazy Types

```rust
// Lazy<T> — impossible to access without explicit demand
let val = Lazy::defer(|| expensive_computation());
val.is_ready();  // false — no computation happened
val.demand();    // NOW it computes, caches, returns &T
val.demand();    // cached — returns immediately

// The type system prevents this:
// let x: i64 = val;  // ERROR: Lazy<i64> is not i64
// You MUST go through .demand()
```

### Evaluation Pipeline

```
Source → rnix::parse → AST
  → eval_expr (tree-walker, 16B Values, HAMT env)
    → maybe_thunk: wrap non-trivial exprs as Lazy (defer evaluation)
    → force only when consumer calls .demand() / force_value()
  OR
  → Compiler → Chunk → VM::run (8B NanBox, slot locals, TAILCALL)
    → fallback bridge: VM → tree-walker on error
```

### Critical Laziness Points

| Operation | Lazy? | Why |
|-----------|-------|-----|
| let-in binding values | YES (thunked) | Forward references need deferral |
| Attrset values | YES (thunked) | Only force when attribute accessed |
| Function arguments | YES (thunked for lambdas) | Call-by-need semantics |
| `foldl'` accumulator | Force ONE level | Strict fold — force attrset structure, NOT values |
| `//` merge | Force structure | Need keys for merge, NOT values |
| `if` condition | Force to bool | Must know which branch |
| `.` selection base | Force to attrset | Must check key exists |
| `.` selection result | **NO** — return as-is | Let caller decide when to force |

## Build & Test

```bash
cargo test --workspace          # all tests (~1500+)
cargo test -p sui-eval --lib    # eval unit tests (~1200)
cargo build --release           # optimized binary
SUI_EVAL_PERF=1 sui eval ...   # profiling (expression breakdown + thunk waste)
SUI_VM_TRACE=1 sui eval ...    # VM diagnostics (fixpoint detection, condition errors)
```

## ★★ THE INSTRUMENT RULE — before debugging sui, verify the instrument can say "no"

**In this mission the blocker has repeatedly been the measuring instrument, not
sui.** Counted from one 24-hour stretch (2026-07-20/21) alone:

1. `parity.yml` set `SUI_PARITY_PUREONLY=1` unconditionally — a **total loss of
   nixpkgs evaluation shipped as a green check** (35 regressions rewritten to
   "skipped"). The year's darwin rows were recorded as matching while they were
   live regressions.
2. `coverage_at_100.rs` asserted `== 100%`, so grading a command honestly turned
   CI red — `build --json` stayed "Working" while it discards every flag and
   breaks `darwin-rebuild`. The only green moves were misgrade or delete.
3. A catalog↔source invariant was **documented in two files and implemented in
   neither** — no test read `main.rs` at all.
4. `--show-trace` was declared and read by nothing; operator type errors carried
   **no file** (every op routed through `op_type`, which never appended
   `eval_file_ctx()`, and `NixTraceGuard::drop` empties the frame stack during
   unwind). Four parallel investigations burned most of their budget on
   localisation because the one flag whose job is localisation was inert.
5. The stale-symbol guard for the ident-cache bug **existed** — on the wrong
   side of the keyword check, and only on one of the two Ident arms. The
   instrument-shaped fix was present and mis-plumbed.
6. A 71-minute marquee eval died rc=137 with empty stdout/stderr — read exactly
   like an OOM or evaluator crash; it was `teiki rust-cleanup` deleting
   `target/` under a running binary (macOS SIGKILLs on text-file unlink).

**The rule:** before attributing a failure to the evaluator — and before
trusting a green — prove the instrument can represent the failure it exists to
catch. Concretely:

- **A gate must be shown to fire.** When adding/trusting any parity/coverage
  gate, reintroduce a known-bad case and watch it go red *for the right
  reason* (a syntax error in your probe also exits non-zero — read the output,
  not the exit code).
- **Reclassification is budgeted and attributed, never silent.** Any "skip"
  path states whose fault it was and is capped (`PUREONLY_SUI_ERROR_BUDGET`).
  An unexplained aggregate skip tally is how #1 hid.
- **Honest grades must be legal moves.** A ratchet (raise the committed budget
  in the same commit, reason in the diff) — never an `== 100%` equality.
- **An advertised capability is tested or it does not exist.** A doc-comment
  invariant with no test is #3; a CLI flag nothing reads is #4.
- **rc=137 with no output is not evidence about sui.** Check the janitors
  (`~/Library/Logs/rust-cleanup.log`) before profiling the evaluator; build
  long-running binaries outside `target/` (`CARGO_TARGET_DIR`) — seibi's
  `--min-age-hours` guard now defaults to 24h, but only on current builds.

Same family as the fleet's false-green traps (`| tail` before an exit check,
zsh `$pipestatus`, `grep -r` from the org root returning empty instead of
erroring). **A tool whose failure mode is indistinguishable from success cannot
verify anything** — and in a byte-parity project, the instruments ARE the
product. Fixing the instruments first is what let a year-old eval blocker fall
in hours: R1 un-blinded the gates, the gates localised the ident-cache bug, the
fix took the corpus from 41/35-red to 77/77-green on darwin the same day.

## The sui rhythm — the empirical fix loop

The cadence for any sui/nix divergence: **repro → oracle → instrument → pin →
root-fix → verify+test → commit → next.** Each pass drains one bug class and
leaves a passing regression test. Never guess-patch the evaluator.

1. **Repro small.** Reduce to the cheapest expression that still fails. For the
   module fix-point, the leaf
   `(builtins.getFlake "path:.../nix").darwinConfigurations.<host>.config.system.stateVersion`
   forces the full merge without package realization (minutes, not the whole system).
2. **Oracle.** Stock nix is ground truth — `nix eval … --raw` and
   `builtins.typeOf <name>` nix-vs-sui. Kill wrong hypotheses with minimal-expression
   probes (`sui eval --no-vm -E '<expr>'` — `--no-vm` is the tree-walker; drop it for
   the VM path) before touching the evaluator.
3. **Instrument at the site, env-gated** — `if std::env::var_os("SUI_DBG_X").is_some() { eprintln!(…) }`;
   `cargo build --release --bin sui` (~3.5 min); run; read. Techniques that cracked
   real bugs: instrument the **builtin itself** with `crate::eval::current_eval_file()`
   — a builtin error's file is often where it was *bound* (`inherit (lib) …`), not
   called; check **every** failure path (`concatLists` has two `as_list()` — the null
   was an *element*, not the arg); dump the exact expr across files via
   `ident.syntax().ancestors().nth(2).text()`, or a scope's names via a `debug_keys()`
   env probe.
4. **Force-order test.** A value that's `null` forced lazily but a list forced
   eagerly (`map builtins.typeOf` trace) is a fix-point order-dependence — the M2.6
   Promise/Blackhole partial-value class. Deep; don't hack it.
5. **Missing-global diff.** For `UndefinedVar`, diff nix bare-globals against sui's
   `builtins/mod.rs` `DEFAULT_SCOPE`; promote genuine nix globals that exist under
   `builtins.` but aren't bare (`placeholder`, `fetchGit`, `fromTOML`, …).
6. **Root-fix, no hacks** (the north-star): a band-aid — return null so eval
   proceeds, memoize a transient partial, an env-sentinel — is not a fix.
7. **Verify + regression-test.** Revert every diagnostic; keep only the fix; run
   the `sui-eval` suite. Prove any remaining failures are pre-existing by checking
   out the pre-session commit and re-running (env-dependent flake/NIX_PATH tests
   fail regardless). Add one passing test per fix (unit-test a private fn via
   `rnix::Root::parse(s).tree().expr()`). Commit.

**Cache-edit discipline.** To site-trace inside nixpkgs: `cp` the cached lib file
to a backup, `chmod u+w`, add `builtins.trace` wrappers (no rebuild — nix-file
change), run, then force-restore. Always leave `~/.cache/sui` pristine.

**Crash → lldb.** Stack overflow / segfault: ad-hoc-codesign the binary
(get-task-allow), run under `lldb -k` (run-on-crash), on a
`CARGO_PROFILE_RELEASE_STRIP=false DEBUG=1` build for symbols.

**★★ The Parity Method — typescape the class, don't just fix the instance
(canonical: [`theory/BUILD.md` §II.1](https://github.com/pleme-io/theory/blob/main/BUILD.md)).**
The fix loop above (repro → root-fix → test → commit) is the *detect* half. A
parity fix is not *done* until its whole **class** is sealed: (1) reduce to a
smallest pure-builtins repro proving the root is **general**, not
package-specific (the cascade a general fix produces is the confirmation — this
arc: single fixes closed 7–10 packages each); (2) **typescape the class** as a
TYPED-SPEC + INTERPRETER TRIPLET in `sui-spec` (Rust border + `(def…)` Lisp spec
+ `apply` behind a mockable `Environment`), so the illegal state is a typed
invariant, not a runtime guard (`laziness`/`coercion` are the worked examples);
(3) **regenerate across channels** — both engines drive the one authored spec,
repeating impl-shapes absorb into the generated macro vocabulary
([`docs/MACRO-VOCABULARY.md`](docs/MACRO-VOCABULARY.md)); (4) **seal** — a
sealed `sui parity` corpus row (a Match that regresses fails CI) + a regression
test + the CATALOG REFLECTION entry in the same commit. Standing rule: every
parity fix advances a domain + a sealed corpus row, or carries a typed
`parity-oneoff: <reason>` note naming why the divergence is genuinely
instance-specific. A one-off patch that leaves the class able to recur is the
deviation this method forbids.

## Performance Architecture

- **Value:** 16 bytes — 2 per cache line (compile-time enforced)
- **Env:** HAMT with O(1) structural sharing — clones don't copy data
- **Thunk:** OnceCell fast-path — 150M+ cache hits skip state machine entirely
- **Strings:** SmolStr (22B inline) + Symbol(u32) interned keys
- **Allocator:** mimalloc — arena-aware, thread-caching
- **foldl':** Force accumulator after each step (matches CppNix forceValue)
- **maybe_thunk:** Literals, paths, idents, lambdas evaluated directly; everything else deferred

## Key Patterns

- **Builtin bridge:** VM delegates to tree-walker via `StringKeyedValue` conversion
- **Import fallback:** VM → tree-walker per-file on CompileError/RuntimeError
- **With-scope capture:** Thunks inside `with` blocks capture scope as upvalues
- **Fixpoint support:** `force_value` stores partial result before chain unwrap
- **Force tracking:** `force_value_tracked(val, "site_name")` for perf profiling

## Conventions

- Edition 2024, Rust 1.89.0+, MIT, `clippy::pedantic`
- Release: `codegen-units = 1`, `lto = true`, `opt-level = 3`, `strip = true`
- All code clean-room — no vendored GPL code
- `#[inline(always)]` on force_value, eval_expr fast paths
- Construction guarantees: make bad states unrepresentable via types
