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

;; ── Round 3 (discovery fan-out, 2026-07-18): profile 3 shapes, discover +
;; try the cleanest lever per shape.  2 Proven (LANDED), 1 Deferred
;; (soundness), plus the BIG-LEVER //-merge investigation → Discarded.

;; ★ select-ident-token — PROVEN, LANDED (+12.6% on select-heavy).  `ident_text`
;; walked the whole ident node's descendant span via `syntax().text()` (a
;; SyntaxText Display over a rowan PreorderWithTokens cursor walk + NodeData
;; allocs) on EVERY call — ~40% of the eval_select subtree.  Fix: fetch the
;; single TOKEN_IDENT's text directly via `ident.ident_token().text()`, with a
;; byte-identical fallback to the full walk for the `or` ident (lexed as a
;; nested TOKEN_OR).  ReprSwap/ByteSufficient (same text → same key → same drv).
;; WIDENS: every ident_text caller (idents, has-attr, attrset keys) inherits the
;; cheap path.  NO source-id cache (a first source-id-cache attempt was caught
;; UNSOUND — cross-source stale Symbol → InfiniteRecursion — and reverted).
(defperf-lever
  :name       "select-ident-token"
  :attacks    ("ident-text-rowan-walk" "select-key-materialization")
  :technique  ReprSwap
  :proof-tier ByteSufficient
  :status     Proven
  :measured   Improved
  :speedup-bp 1260
  :ceiling    NotApplicable)

;; ★ string-concat — PROVEN, LANDED (+16.5% on string-heavy).  The
;; (String,String) arm of eval_binop's Add built the result with
;; `format!("{}{}", a.chars, b.chars)` — the core::fmt runtime dispatch was the
;; #1 self-time frame + grows the buffer twice.  Fix: one exact-capacity String
;; + two push_str (reserve the final size once).  ByteSufficient (identical
;; bytes + context).  Bonus: removes a `format!` — a TYPED EMISSION win too.
(defperf-lever
  :name       "string-concat"
  :attacks    ("string-add-format-runtime")
  :technique  ReprSwap
  :proof-tier ByteSufficient
  :status     Proven
  :measured   Improved
  :speedup-bp 1650
  :ceiling    NotApplicable)

;; attrset-symcache — measured +25.4% (attrset-construction) but HELD (Deferred),
;; NOT landed.  It caches static attrset keys' Symbol by (source_id, offset) off
;; CURRENT_SOURCE_ID — the SAME mechanism the select agent DEMONSTRATED can
;; produce wrong bytes for a lazily-forced cross-source node.  It passes the full
;; 1384-suite (incl the cross-source flake test), but a demonstrated failure of
;; the mechanism is a real yellow flag; pending a source-id-soundness audit
;; (why is it sound for ident-eval + attrset-construction but not select-
;; traversal?).  Its single_attr + insert_sym parts are safe; a source-id-free
;; rework is the follow-up.  Ceiling: PartialCorpus (the oracle can't prove the
;; uncovered cross-source attrset case).
(defperf-lever
  :name       "attrset-symcache"
  :attacks    ("attrset-static-key-materialization")
  :technique  ResolutionChange
  :proof-tier Rejected
  :status     Deferred
  :measured   Improved
  :speedup-bp 2540
  :ceiling    PartialCorpus)

;; overlay-merge-structural — THE BIG LEVER, investigated + DISCARDED.  The
;; //-merge LOOKED like 33-50% self-time on a synthetic //-tower smoke, but
;; that shape is PATHOLOGICAL: docs/EVAL-CORE-DOMINANCE.md measures overlay-
;; flatten at ≤2.1% of the REAL nixosSystem deep eval (82.5% cache-hit).  Three
;; sub-levers proven dead by instrumentation + micro-bench: (A) in-place base-
;; move NEVER fires (the bound accumulator retains the Rc — 0/505k uniquely
;; owned, explaining overlay-base-move's neutral); (B) inert (right = 1 key/
;; merge, base clone is 99%); (C) im_rc persistent map is a DOUBLE-LOSS TRAP
;; (5-11× slower merge AND 1.1-3.4× slower lookup — the std flat-table memcpy is
;; already near-optimal at realistic ≤300-key sizes).  Byte-parity held; all 3
;; axes within noise → Neutral → Discarded.  REDIRECT: the marquee's real
;; bottleneck is thunk-waste (51.8% never-forced) + retained HAMTs, NOT //-merge.
(defperf-lever
  :name       "overlay-merge-structural"
  :attacks    ("overlay-flatten-base-clone")
  :technique  ResolutionChange
  :proof-tier Rejected
  :status     Discarded
  :measured   NoImprovement
  :speedup-bp 0
  :ceiling    PersistentLazyDesign)

;; thunk-waste-elision — THE post-//-merge "real big lever", ESTIMATED + CLOSED
;; as CAPPED (2026-07-18).  51.8% of thunks are created-but-never-forced, but
;; that is laziness WORKING; the only byte-safe capture is to NOT create the
;; thunk for a provably-TOTAL cheap expr (literal / already-forced ident) so
;; eager eval is byte-identical.  MEASURED eager-eligible fraction of the
;; never-forced thunks = ~0%: the only total kind left (Paren) always wraps an
;; Apply (eliding it just moves the thunk inward — net zero), and the lambda-arg
;; site's `eval_pure_constant_arg` already eager-takes Literal/Str/Path with 0
;; remaining pure-constants (cross-confirmed on 2 byte-verified workloads).  The
;; 3 landed levers + prior M2 eager cuts already captured ALL byte-safe eager-
;; elidable thunk-waste; the rest wrap potentially-diverging exprs (Apply /
;; Select / Ident-with-scope / BinOp / recursive-let / the //-fixpoint merge)
;; whose laziness is LOAD-BEARING for correctness — eager-evaluating any breaks
;; byte-parity.  ACHIEVABLE-GAIN UPPER BOUND ~0%.  The real remaining lever is a
;; cheaper thunk REPRESENTATION (arena / bump-alloc Env chain / thunk-store
;; collapse) — a memory-model change touching every thunk, NOT elision; bigger +
;; riskier, named-not-attempted.  ForceOrderChange (eager-vs-lazy) earning
;; ForceOrderProof; Discarded because the safe fraction is ~0 (NoImprovement).
(defperf-lever
  :name       "thunk-waste-elision"
  :attacks    ("thunk-creation-never-forced")
  :technique  ForceOrderChange
  :proof-tier ForceOrderProof
  :status     Discarded
  :measured   NoImprovement
  :speedup-bp 0
  :ceiling    PersistentLazyDesign)

;; thunk-env-repr — the campaign's biggest swing (a cheaper thunk/Env memory
;; representation), ESTIMATED + CLOSED as CAPPED (2026-07-18).  An exact dhat
;; profile DISPROVED the premise: the dominant memory term is NOT
;; Rc<ThunkInner> (2.7% bytes / 7.6% peak) nor Rc<EnvInner>/child (2.8% / 1.8%)
;; — it is the im_rc HAMT-node COW on `Env::bind` under bind_param←apply
;; (42.7% churn, 80.8% live peak: every lambda application clones a ~1KB HAMT
;; chunk to insert one param), which is the flat-env project already
;; /twin-reasoning-ruled-out (flattening → O(n) scope-push wrecks the 6.24M
;; env_lookups).  Byte-safe upper bounds: thunk-repr ≤2.3% wall (72B is a cheap
;; mimalloc small-block; shrink = same-size-class no-op); env-repr ≤~1% AND
;; net-negative (ENV-RESOLVE M1 already measured a parent/positional-frame
;; chain at +7% fib / +32-39% call-heavy — the frame ALLOC costs more than the
;; interned-Symbol HAMT probe it removes).  Every candidate hits a PROVEN trap
;; (parent-chain net-negative; whole-eval arena = RSS regression, no GC to
;; free; slab/pool = allocator+Rc-touch, unproven).  The ~9× deep-recursion gap
;; is the inherent PRICE of persistent-lazy GC-free design (Rc/call + Rc/arg +
;; HAMT-COW/bind — what cppnix's tracing GC gets free).  REDIRECT: the profile
;; surfaced the real next big lever — the ROWAN AST RE-WALK (40.7% bytes / 51%
;; alloc-calls / 21% wall self-time; NodeData cursor re-allocation per node
;; re-visit) — a SEPARATE AST-caching axis, NOT thunk/env repr.
(defperf-lever
  :name       "thunk-env-repr"
  :attacks    ("env-hamt-cow-on-bind" "rc-thunkinner-per-thunk" "rc-envinner-per-child")
  :technique  ReprSwap
  :proof-tier ByteSufficient
  :status     Discarded
  :measured   NoImprovement
  :speedup-bp 0
  :ceiling    PersistentLazyDesign)

;; ── canonicalize-memo (2026-07-21) ─────────────────────────────────
;; Found by SAMPLING THE LIVE cid marquee eval (/usr/bin/sample): 61% of
;; the eval thread's wall-clock inside `__getattrlist` — macOS `realpath`
;; issuing one metadata syscall per path component.  Root: `dematerialize`
;; (called from EVERY path literal via eval_expr's Path arms) re-ran
;; `read_dir.canonicalize()` inside its per-entry loop over ~60 registered
;; flake inputs, on every call; `source_name_for_read_dir` same shape.
;; Fix: canonicalize each read_dir ONCE at registration + a SUCCESS-ONLY
;; probe memo (failures uncached so an IFD output that appears later still
;; resolves).  The named frozen-inputs ASSUMPTION (the class's honesty
;; condition per the Technique doc): registered trees are immutable
;; fetcher caches; probe paths are frozen sources — the same assumption
;; CppNix makes copying a tree to the store.
;; Measured: syscall share 2143 -> 37 samples (61% -> ~2%) live, and
;; parity 77/77 exit 0 post-fix.  Wall-clock delta honestly PENDING — the
;; cid eval has NEVER completed (memory-bound at 25GB footprint), so no
;; before/after wall number exists; recording Improved on the sample share
;; alone would be the null-as-win mistake in a new costume.
(defperf-lever
  :name       "canonicalize-memo"
  :attacks    ("realpath-per-path-literal" "read-dir-canon-per-lookup")
  :technique  MemoizeIdempotentQuery
  :proof-tier ByteSufficient
  :status     Landed
  :measured   Pending
  :speedup-bp 0
  :ceiling    NotApplicable)

;; ── sym-keyed-attrs (2026-07-21, lever 1 of the 20s campaign) ──────
;; The live-sampled #1 CPU sink: iter_unsorted materialized a String per
;; key (resolve alloc + whole-map Vec collect per call) and intersectAttrs
;; re-interned each back to the Symbol it started as, third intern inside
;; insert — Sym→String→hash+memcmp→Sym per key per call; 63/70 intern
;; leaves attributed to that one closure.  Fix: iter_syms()/insert_sym()
;; (Symbol is Copy; as_flat() borrows) — intersectAttrs fully sym-keyed;
;; filterAttrs/mapAttrs keep exactly ONE resolve (the lambda's key arg)
;; and drop the collect + insert-intern.  MEASURED same-phase sample
;; (t≈97s): interner cluster 27-39% -> <=6.7% upper bound, intern
;; self-time 11.8-16.9% -> 0.5%.  Parity 77/77 exit 0.  Wall-clock delta
;; honestly PENDING — the cid eval still cannot COMPLETE (the ~140MB/s
;; retained-allocation memory wall, levers 3-5), so no end-to-end number
;; exists yet; the profile share is the measured fact.
(defperf-lever
  :name       "sym-keyed-attrs"
  :attacks    ("iter-unsorted-string-per-key" "contains-key-reintern" "insert-reintern")
  :technique  ReprSwap
  :proof-tier ByteSufficient
  :status     Landed
  :measured   Pending
  :speedup-bp 0
  :ceiling    NotApplicable)

;; ── path-memo L2 (2026-07-21, tried + DISCARDED same session) ──────
;; SPEED.md L2 targeted the ~5% std::path::Components cluster with memos
;; on normalize/canon_abs (pure string fns — trivially sound).  Interleaved
;; A/B, TWO harnesses, byte-identical outputs throughout: hello.drvPath
;; (5 rounds: +9/+17/-9/+13/-35ms on ~1.55s = noise) and the kataFleet
;; flake eval (3 rounds: -0.03/+0.06/+0.06s on ~23.7s = noise).  Measured
;; NoImprovement -> Discarded per the overlay-base-move precedent (never
;; ship a no-win on the sacred path).  HONEST RESIDUAL: neither harness
;; registers cid-scale flake inputs (~60), so the cluster's real home may
;; be the materialize/dematerialize strip_prefix loops that only a
;; completing cid eval exercises — L2 may return post-S6 with that
;; harness.  Session bonus datum recorded in the baseline: kataFleet cold
;; measured 22.8-24.5s today post-582cede (recorded prior: 35.2s under
;; recon load) — suggestive of sym-keyed-attrs' wall effect on U02, not
;; claimed as its A/B.
(defperf-lever
  :name       "path-memo"
  :attacks    ("path-components-parse" "canon-abs-parse")
  :technique  MemoizeIdempotentQuery
  :proof-tier ByteSufficient
  :status     Discarded
  :measured   NoImprovement
  :speedup-bp 0
  :ceiling    NotApplicable)

;; ── dead-binding-elim (2026-07-21, KILLED BY CENSUS pre-implementation) ─
;; The S1 spike settled L1's motivating payload before a line was wired:
;; textually-dead LET elimination is SOUND (14 probes x 2 engines: let-only,
;; plain-inherit excluded, __findFile/__nixPath blanket-kept — the one
;; dynamic channel, since <x> desugars to __findFile with NO Ident node) but
;; THREE ORDERS OF MAGNITUDE too small: all-nixpkgs census (41,959 files,
;; rnix 0.14.0, mirroring eval.rs's own analysis) = 40,271 lets, 444 dead,
;; 417 thunk-allocating, vs 2.2M static thunk sites — ~0.02%.  The
;; 51.8%-never-forced population is attrset VALUES: structurally required,
;; enumerable (attrNames probe), unaddressable by elimination.  The 154MB/s
;; retention wall is alive-but-unforced attrset-value thunks pinning HAMT
;; env chains — the L7 env-capture-shrink track, a different lever.
;; Killing a lever by census before building it is the cheapest Discard the
;; ledger has recorded.  Bonus finding, recorded not fixed: `let x =
;; undefinedVar; in 1` is a PRE-EXISTING divergence (nix: static error;
;; sui: 1) — a KnownDiverge candidate for the corpus, independent of this.
(defperf-lever
  :name       "dead-binding-elim"
  :attacks    ("never-forced-thunk-creation" "dead-let-env-pin")
  :technique  ForceOrderChange
  :proof-tier ForceOrderProof
  :status     Discarded
  :measured   NoImprovement
  :speedup-bp 0
  :ceiling    NotApplicable)

;; ── ir-eval-subset — L3 slice 2 (2026-07-21, branch ir/l3-eval-slice2) ─
;; Eval-through-IR for the PURE EXPRESSION SUBSET: sui-ir::eval_ir walks
;; the lower()ed flat Program (ExprId arena) instead of re-walking rowan
;; per eval — the SPEED.md L3 lower-once thesis, first executable cut.
;; Dual-engine, differential-gated: every parity-corpus row + the 94-row
;; supplement + a 90-row closed-value seed + 256-case proptest generation
;; evaluate on BOTH engines (tree-walker = semantic oracle) and the
;; rendered results byte-compare; typed shrink-only known-gap allowlist
;; (17 corpus + 12 supplement rows: derivation/builtins/paths/legacy-let).
;; Technique authored ReprSwap with the caveat stated plainly: the
;; program REPRESENTATION swaps (rowan AST → flat IR), but the walker is
;; RE-IMPLEMENTED against it (mirror value/env/thunk types — Closure and
;; ThunkRepr hold rowan nodes and cannot be reused), so the byte claim
;; rests on the external differential over a PARTIAL corpus (C2 forever),
;; not on structural neutrality.  Micro A/B (release, interleaved ×5,
;; 2000 evals/round, synthetic let/apply/binop expression, byte-agreeing
;; result): tree-walker 172-175ms vs eval_ir 68-70ms per round — ~2.5×
;; (+150% → 15000 bp).  HONEST SCOPE: that is the dual-engine micro
;; harness, NOT a sacred-path measurement — the subset engine is wired
;; into nothing (Proposed, not Landed); the at-scale win (lower-once
;; killing the 40.7%-bytes/51%-alloc rowan churn) stays the L3 thesis to
;; be proven when the full engine flips behind --ir.
(defperf-lever
  :name       "ir-eval-subset"
  :attacks    ("rowan-per-eval-rewalk" "rowan-nodedata-alloc-churn")
  :technique  ReprSwap
  :proof-tier ByteSufficient
  :status     Proposed
  :measured   Improved
  :speedup-bp 15000
  :ceiling    NotApplicable)

;; ── ir-file-eval — L3 slice 3 (2026-07-21, branch ir/l3-file-slice3) ──
;; eval_ir goes FILE-CAPABLE.  Three additions, all differential-gated
;; against the tree-walker oracle: (1) PATH literals — abs/rel/home with
;; interpolation, resolved through byte-mirrors of the walker's
;; canon_abs/normalize/resolve_import (sui-ir::path, parity-tested
;; against sui-eval::path in tests/path_parity.rs — the walker fns are
;; pub but sui-eval is dev-dep-only, so mirror-plus-parity-gate is the
;; honest shape); (2) IMPORT — a thread-local canonical-path-keyed
;; ProgramCache (parse+lower ONCE per file: the SPEED.md L3 lower-once
;; destination in miniature, proven by tests/file_differential.rs'
;; program_cache_len assertions), a walker-mirroring import VALUE cache
;; (identity equality holds: import ./a == import ./a), the
;; directory→default.nix rule, and TYPED circular-import detection
;; (IrEvalError::ImportCycle — the walker/CppNix would recurse to stack
;; death; IR-only test); (3) the BUILTINS BRIDGE — 36 pure builtins
;; natively on IrValue (attrNames/attrValues/hasAttr/getAttr/length/
;; head/tail/elemAt/filter/concatLists/listToAttrs/mapAttrs/removeAttrs/
;; intersectAttrs/toString/typeOf/is* ×9/seq/deepSeq/foldl'/genList/
;; substring/stringLength/split/concatStringsSep/replaceStrings/map/
;; import), constants (currentSystem/storeDir/nixVersion/langVersion),
;; and EVERY unimplemented walker builtin pre-seeded as a typed
;; MissingBuiltin failed thunk — builtins KEY-SET parity with the walker
;; is byte-gated (builtins_registry_parity).  Known-gap allowlists
;; SHRANK: corpus 17→7 rows (derivation ×4 + elem + concatMap), supplement
;; 12→4 (search-path ×2, legacy-let ×2); closed seed grew ~90 rows.  File
;; A/B (release, interleaved ×5, 300 evals/round, ab_file.nix importing
;; lib.nix + dir/, byte-agreeing result): walker 134-147ms/round vs
;; ir-cold (ALL caches cleared per eval — pays parse+lower each time) vs
;; ir-warm (programs cached, value cache cleared — every file's code
;; re-runs, lower-once amortized) 41-44ms → 3.0-3.4× (+220% → 22000 bp;
;; round-0 warm outlier 93ms recorded as warmup).  Micro A/B re-run this
;; slice: 2.54-2.61× (slice-2's 2.49-2.57× held).
;; :speedup-bp is the WARM number — the lever's actual thesis (lower once,
;; eval many).  COLD is DELIBERATELY NOT the claim: the adversarial re-run
;; measured cold at 0.95-1.3× (one round SLOWER than the walker) — within
;; round-to-round noise, i.e. parse+lower cost ≈ the rowan-rewalk it saves
;; on a first eval.  Cold neutrality is expected and honest; the win is
;; the amortized re-eval the warm-daemon/incremental case delivers.
;; TWO TEST-COVERAGE FOLLOW-UPS the adversary named (recorded, not blocking
;; a Proposed row): (a) per-builtin kill-power is thin — ~1 differential
;; row kills an elemAt off-by-one, and the proptest generator has no
;; builtin-application node, so ≥2 rows/builtin (value + OOB/error) or a
;; generator extension is owed before the --ir flip trusts the 36 natives;
;; (b) adopt the ProgramCache byte-mutation probe (overwrite the cached
;; file, confirm the stale Program still serves) as a permanent test — the
;; shipped len-stability assertion alone would miss a re-parse-same-key
;; regression.  HONEST SCOPE: dual-engine harness numbers over the pure
;; subset; the engine is wired into nothing (Proposed, not Landed); the
;; string-interpolated-path copy-to-store coercion stays a typed gap
;; (Unsupported("path-copy-to-store") — needs the store).
(defperf-lever
  :name       "ir-file-eval"
  :attacks    ("rowan-per-eval-rewalk" "per-import-reparse-relower")
  :technique  ReprSwap
  :proof-tier ByteSufficient
  :status     Proposed
  :measured   Improved
  :speedup-bp 22000
  :ceiling    NotApplicable)
