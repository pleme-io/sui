;; sui-spec/specs/perf.lisp — the PERFORMANCE-LEVER CLAIM LEDGER for
;; sui's eval hot path.  Every optimization applied to the tree-walker /
;; bytecode VM is authored here as a typed, honesty-gated claim
;; (technique → the proof-tier its CLASS earns; status → its measured
;; delta; a sealed strictly-positive Delta).  Doctrine: theory/BUILD.md
;; §II + sui/docs/PERF-CLAIM-LEDGER.md.
;;
;; This types the claim SHAPE.  It does NOT generate optimizations and
;; does NOT verify parity soundness — whether an edit is force-order-
;; neutral stays the `sui parity` byte oracle's job over a PARTIAL corpus
;; (C2, forever).  A lever whose `:technique` is mislabeled passes the
;; honesty gate while being byte-unsound; the type gates the CLASS.
;;
;; Every authored lever below is in its HONEST record — the corrected
;; form.  The mistakes the ledger catches (a null recorded as a win, a
;; regression proposed as shippable, a resolution change claiming byte-
;; sufficiency) are exercised as unit tests in src/perf.rs, never authored
;; here (exactly as laziness.lisp authors only correctly-classified
;; disciplines and tests the bug forms separately).

;; ── #1  NanBox upvalues (fa86b77) ──────────────────────────────────
;; VMClosure / ThunkState upvalues `Vec<VMValue>` → `Vec<NanBox>`, killing
;; the per-call deep-clone round-trip.  A pure representation swap —
;; nothing observable changes — so a byte-identical corpus fully
;; establishes it (ByteSufficient, the tier ReprSwap earns).  Merged +
;; parity 171/171, but the isolated perf delta was NOT measured this
;; session, so the honest record is Landed / Pending — never rounded to
;; Proven (which would require a measured Improved).
(defperf-lever
  :name       "nanbox-upvalues"
  :attacks    ("vm-closure-upvalue-clone" "thunk-upvalue-clone")
  :technique  ReprSwap
  :proof-tier ByteSufficient
  :status     Landed
  :measured   Pending
  :speedup-bp 0
  :ceiling    NotApplicable)

;; ── sui-resolve M0 (98a4771) ───────────────────────────────────────
;; A lexical pre-resolution fast-path for Ident, behind SUI_RESOLVE=1,
;; parity-by-construction (same binding; fail-safe to Dynamic at every
;; `with` barrier), so the dropped with-chain walk is an unobserved-order
;; drop → ByteSufficient.  Measured NULL (−0.5 % on fib20) → Deferred,
;; measured NoImprovement.  Authoring it as Proven would fire
;; ProvenWithoutImprovement — the null-measurement mistake, eval-caught
;; (unit test `null_measurement_recorded_as_proven_is_caught`).
(defperf-lever
  :name       "m0-resolver"
  :attacks    ("ident-lookup" "with-chain-walk")
  :technique  DropUnobservedOrder
  :proof-tier ByteSufficient
  :status     Deferred
  :measured   NoImprovement
  :speedup-bp 0
  :ceiling    NotApplicable)

;; ── positional-frames M1 (design doc §6b, never merged) ────────────
;; A positional-frame overlay replacing the HAMT env probe.  Coupling was
;; PROVEN airtight — but measured NET-NEGATIVE (+7 % fib / +32–39 %
;; call-heavy) because the frame ALLOCATION costs more than the probe it
;; removes, inherent to sui's no-GC persistent-lazy design.  DISCARDED
;; (never merged; never ship a regression).  As a technique CLASS it
;; changes resolution → earns Rejected; authoring it ByteSufficient /
;; CouplingProof would fire TierOverclaim (unit test
;; `a_resolution_change_claiming_byte_sufficiency_is_caught`).  The
;; Rejected tier requires a named ceiling: the persistent-lazy design.
(defperf-lever
  :name       "positional-frames"
  :attacks    ("ident-lookup" "env-child-alloc")
  :technique  ResolutionChange
  :proof-tier Rejected
  :status     Discarded
  :measured   Regressed
  :speedup-bp 0
  :ceiling    PersistentLazyDesign)

;; ── redundant-store-elision (57da0d79) ─────────────────────────────
;; Skip the second thunk store when it is provably redundant
;; (value.rs:1685).  Observationally neutral → ByteSufficient.  Already
;; landed; the isolated delta was not re-measured this session → Landed /
;; Pending (the Care #9 correction that this was ALREADY landed, not a new
;; lever — the already-landed drift the ledger does NOT yet catch is the
;; named M2 addition, `no_two_landed_levers_share_a_primary_counter`).
(defperf-lever
  :name       "redundant-store-elision"
  :attacks    ("thunk-store-redundant")
  :technique  SkipRedundantStore
  :proof-tier ByteSufficient
  :status     Landed
  :measured   Pending
  :speedup-bp 0
  :ceiling    NotApplicable)

;; ── overlay-base-move (fan-out 2026-07-18, attempted + DISCARDED) ───
;; A /fan-out found the //-merge's `as_flat` deep-clone (value.rs:2226,
;; `let mut result = left.as_flat().clone()`) was the biggest MEASURED
;; self-time slice (~33-50% under a self-time profile).  The lever: MOVE
;; left's fully-merged map out (it is released right after) instead of
;; deep-cloning it — O(1) vs O(n) HashMap::clone.  Byte-parity verified
;; (1384/1384 relevant tests + the vm_vs_treewalker byte-for-byte oracle).
;; BUT the rigorous INTERLEAVED A/B measured NEUTRAL (base 57.3 vs change
;; 58.3 iters/15s — within noise), NOT a win: `overlay()` takes `self` by
;; value so the cached attrs are cloned through a fold, which SHARES the
;; `cache` Rc → `Rc::get_mut(cache)` fails → the steal falls back to clone
;; anyway; and the real throughput cost is the 1.2M inserts + Value::clones
;; + drops, which moving the base map does not touch.  A byte-safe change
;; with no measured win on the SACRED hot path is DISCARDED (the
;; never-ship-a-regression rule; the positional-frames precedent).  The
;; first "HashMap::clone 8.9%->0.08%" signal was an apples-to-oranges
;; mis-attribution (mimalloc vs system-malloc builds) the A/B corrected.
(defperf-lever
  :name       "overlay-base-move"
  :attacks    ("overlay-flatten-build" "attrset-copy-on-merge")
  :technique  SkipRedundantStore
  :proof-tier ByteSufficient
  :status     Discarded
  :measured   NoImprovement
  :speedup-bp 0
  :ceiling    NotApplicable)

;; ── Round 2 (fan-out Workflow, 2026-07-18): 3 candidates tried in
;; parallel worktree agents, each byte-verified + interleaved-A/B on a
;; foldl'-over-1M-list (ident/apply/force-heavy) workload.  ONE Proven, two
;; Discarded — the discipline landing real ground + refusing two non-wins.

;; ★ ident-intern — THE FIRST PROVEN LEVER.  The strict + maybe_thunk Ident
;; arms (eval.rs ~971 / ~786) allocated a fresh String per lookup
;; (`ident_text().to_string()`) then re-hashed it (`intern(&name)`) even on a
;; hit.  Rerouted through the existing-but-unused (source_id, text_offset)→
;; Symbol cache via a new lazy `intern_cached_with` (value.rs) — the u64-keyed
;; hit pays no alloc + no string-hash; the text is materialized only on the
;; once-per-offset cold miss / rare deferral.  Keyword check preserved via
;; zero-copy `with_resolved`; `lookup_fast(sym, "")` (its name arg is a dead
;; param).  ReprSwap → ByteSufficient (same Symbol by construction → same
;; binding → same value → same drv).  Byte-parity: 1384/1384 + 1 pre-existing
;; net fail (agent AND my worktree, independently).  MEASURED: +9.5% (agent,
;; mimalloc, fine-grained, every interleaved round) / +16.7% (mine, system-
;; malloc, coarse, 6→7 iters 4/4 rounds).  Conservative recorded delta = the
;; agent's fine-grained +9.5%.
(defperf-lever
  :name       "ident-intern"
  :attacks    ("ident-text-alloc" "ident-intern-rehash")
  :technique  ReprSwap
  :proof-tier ByteSufficient
  :status     Proven
  :measured   Improved
  :speedup-bp 950
  :ceiling    NotApplicable)

;; apply-trace-clone — drop the always-on dead current_eval_file() PathBuf
;; clone per lambda call (the trace frame's current_file is dead; Display
;; already derives from closure_env).  Byte-verified (1384/1384 + 1), but
;; interleaved A/B measured NEUTRAL (-0.01%): the clone was not a measurable
;; slice of throughput.  Byte-safe + neutral on the SACRED path → Discarded.
(defperf-lever
  :name       "apply-trace-clone"
  :attacks    ("lambda-trace-frame-pathbuf-clone")
  :technique  ReprSwap
  :proof-tier ByteSufficient
  :status     Discarded
  :measured   NoImprovement
  :speedup-bp 0
  :ceiling    NotApplicable)

;; force-roundtrip (#6) — eliminate the Concrete→Value→Concrete round-trip on
;; the memoized thunk cache-hit path.  Byte-verified (isomorphism + 1384/1384
;; + 1), but interleaved A/B measured NEUTRAL (+1.15%, within run-to-run
;; noise) — not a reliable win.  Byte-safe + neutral → Discarded.
(defperf-lever
  :name       "force-roundtrip"
  :attacks    ("thunk-cache-hit-concrete-value-roundtrip")
  :technique  ReprSwap
  :proof-tier ByteSufficient
  :status     Discarded
  :measured   NoImprovement
  :speedup-bp 0
  :ceiling    NotApplicable)
