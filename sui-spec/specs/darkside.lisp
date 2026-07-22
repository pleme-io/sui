;; darkside.lisp — the typed catalog of dark-side optimization levers.
;;
;; Each (defdarkside-lever …) is ONE typed row about ONE byte-risky (or candidate
;; byte-safe) perf optimization on sui's eval hot path. The border in
;; sui-spec/src/darkside.rs enforces honesty at load time — a byte-RISKY lever
;; claiming ByteSafe, or a Promoted lever missing its Verified-grade gate / named
;; backstop / named ceiling, is REFUSED (SpecError::Interp{phase:"darkside::honesty"}).
;; So adding a lever is: write the row + the row cannot lie.
;;
;; Grounding: docs/DARK-SIDE-DESIGN.md (ranked levers) + docs/DARK-SIDE-PROFILE.md
;; (the confirming profile) + docs/ENV-RESOLVE-DESIGN.md (the measured prior art).
;; Axes: Representation | RedundantWrite (byte-safe candidates) · ForceOrder |
;; Resolution | PartialShape | Lifetime (always byte-risky). earned_tier reused
;; from perf.rs: ReprSwap/DropUnobservedOrder/SkipRedundantStore → ByteSufficient;
;; ForceOrderChange → ForceOrderProof; ResolutionChange → Rejected.

;; ── #3 batch-bind — the byte-SAFE M0 warm-up ──────────────────────────────
;; Collapse N Rc::make_mut+im_rc::insert COWs in bind_param into one make_mut +
;; N inserts on the owned HAMT. Identical final HAMT, fewer path-copies.
(defdarkside-lever
  :name       "batch-bind"
  :flag       "SUI_BATCH_BIND"
  :technique  SkipRedundantStore
  :axis       RedundantWrite
  :byte-risk  ByteSafe
  :attacks    "env-cow-per-bind"
  :cost-share 0.427
  :gate       SingleByteCheck
  :status     DarkGated
  :ceiling    NotApplicable)

;; ── #1 eval_ir — the byte-RISKY headline (already built, measured 2.58x warm) ─
;; Lower rowan→flat ExprId arena once/file; the walker is RE-IMPLEMENTED vs mirror
;; IrValue/IrEnv, so the byte claim rests on the differential, not structure.
;; WARM only; cold ~= neutral (parse+lower ~= the rewalk it saves).
(defdarkside-lever
  :name       "eval-ir-subset"
  :flag       "SUI_IR"
  :technique  ReprSwap
  :axis       Representation
  :byte-risk  ByteRisky
  :attacks    "rowan-ast-re-walk"
  :cost-share 0.407
  :gate       DifferentialOracle
  :status     DarkGated
  :ceiling    PartialCorpus)

;; ── #2 env-capture-shrink — the lever that attacks the cid-DNF (memory) ───────
;; Narrow each Suspended{env}/Closure{env} to capture only the free vars its body
;; reaches. Over-approximates (blanket-keep with-scopes + every dynamic channel),
;; so it is ByteRisky though the technique is DropUnobservedOrder.
(defdarkside-lever
  :name       "env-capture-shrink"
  :flag       "SUI_CAPTURE_SHRINK"
  :technique  DropUnobservedOrder
  :axis       Resolution
  :byte-risk  ByteRisky
  :attacks    "env-cow-per-bind+captured-env-retention"
  :cost-share 0.808
  :gate       DifferentialOracle
  :status     DarkGated
  :ceiling    PartialCorpus)

;; ── #4 arena-thunks — the enabling half of the coupled top lever ──────────────
;; Per-eval arena/index allocation of thunks & frames; malloc/free → bump; bulk-
;; free at eval end. Changes finalization timing/lifetime → ByteRisky. Must keep
;; SUI_LIVE_CENSUS on during rollout (peak MUST drop) + never outlive its eval.
(defdarkside-lever
  :name       "arena-thunks"
  :flag       "SUI_ARENA_THUNKS"
  :technique  ReprSwap
  :axis       Lifetime
  :byte-risk  ByteRisky
  :attacks    "rc-thunkinner-malloc-free-churn"
  :cost-share 0.238
  :gate       DifferentialOracle
  :status     DarkGated
  :ceiling    PartialCorpus)

;; ── #5 debruijn-frames — REJECTED ALONE (the proven trap) ─────────────────────
;; {up,slot} positional resolution. ALONE this IS positional-frames: measured
;; net-negative (+7% fib / +32-39% call-heavy). Pays ONLY coupled with #4.
(defdarkside-lever
  :name       "debruijn-frames-alone"
  :flag       "SUI_RESOLVE"
  :technique  ResolutionChange
  :axis       Resolution
  :byte-risk  ByteRisky
  :attacks    "env-hamt-lookup-probe"
  :cost-share 0.062
  :gate       DifferentialOracle
  :status     Rejected
  :ceiling    PersistentLazyDesign)

;; ── #6 nanbox-value — 16B→8B Value word ───────────────────────────────────────
(defdarkside-lever
  :name       "nanbox-value"
  :flag       "SUI_NANBOX"
  :technique  ReprSwap
  :axis       Representation
  :byte-risk  ByteRisky
  :attacks    "per-value-bandwidth-every-clone"
  :gate       DifferentialOracle
  :status     DarkGated
  :ceiling    PartialCorpus)

;; ── #7 thunk-elision — general form DISCARDED (byte-safe eager fraction ~0%) ──
(defdarkside-lever
  :name       "thunk-elision-general"
  :flag       "SUI_EAGER"
  :technique  ForceOrderChange
  :axis       ForceOrder
  :byte-risk  ByteRisky
  :attacks    "thunk-alloc-waste-51pct-never-forced"
  :gate       DifferentialOracle
  :status     Discarded
  :ceiling    NotApplicable)

;; ── #8 champ-hamt — byte-SAFE compact HAMT node layout ────────────────────────
(defdarkside-lever
  :name       "champ-hamt"
  :flag       "SUI_CHAMP"
  :technique  ReprSwap
  :axis       Representation
  :byte-risk  ByteSafe
  :attacks    "env-hamt-cache-hostility"
  :gate       SingleByteCheck
  :status     DarkGated
  :ceiling    NotApplicable)

;; ── #9 value-intern — byte-SAFE singleton caching (small ints, empty coll) ────
(defdarkside-lever
  :name       "value-intern"
  :flag       "SUI_VALUE_INTERN"
  :technique  ReprSwap
  :axis       Representation
  :byte-risk  ByteSafe
  :attacks    "small-value-allocation"
  :gate       SingleByteCheck
  :status     DarkGated
  :ceiling    NotApplicable)

;; ── #10 superinstructions — byte-SAFE dispatch fusion on the Ir arena ─────────
(defdarkside-lever
  :name       "superinstructions"
  :flag       "SUI_SUPEROPS"
  :technique  ReprSwap
  :axis       Representation
  :byte-risk  ByteSafe
  :attacks    "ir-dispatch-count"
  :gate       SingleByteCheck
  :status     DarkGated
  :ceiling    NotApplicable)
