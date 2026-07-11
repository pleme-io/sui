;; sui-spec/specs/laziness.lisp — the canonical evaluation model both
;; sui engines (tree-walker + bytecode VM) MUST drive.  Authored once
;; here so the two interpreters cannot diverge (the ★★ TYPED-SPEC +
;; INTERPRETER TRIPLET guarantee).  Doctrine: theory/BUILD.md §II.
;;
;; The force hot-path stays in sui-eval for byte-parity + performance;
;; this spec is the authoring surface + the decision discipline that
;; the real engine must satisfy.

;; ── The parity-correct model (Referential sharing + Tracked context) ─
;; This is the ONLY byte-parity-capable model.  The two invariants:
;;   sharing = Referential  → evaluation order can never change a drv
;;   string-context = Tracked → paths carry their store deps
(deflaziness-model
  :name           "cppnix"
  :force-strategy Whnf
  :sharing        Referential
  :string-context Tracked
  :demand-order   "nix")

;; ── Per-thunk-kind disciplines ──────────────────────────────────────

;; A non-recursive thunk re-entered while under evaluation is a genuine
;; cycle → InfiniteRecursion (nix's blackhole).
(defthunk-discipline
  :name             "non-recursive"
  :recursive        #f
  :reentry          Blackhole
  :memoize-on-force  #t
  :recursion-kind   Syntactic)

;; A self-recursive binding (`rec { … }`, mutually-referential attrs)
;; re-entered yields a promise cell rather than erroring — nix's
;; fixpoint tolerance.  Detectable syntactically (RHS names its binding).
(defthunk-discipline
  :name             "recursive-binding"
  :recursive        #t
  :reentry          Promise
  :memoize-on-force  #t
  :recursion-kind   Syntactic)

;; The nixpkgs OVERLAY FIXPOINT (`self:super:` threading through
;; callPackage across files) — semantically recursive but NOT
;; syntactically detectable.  This is the byte-parity root (sui frontier
;; 2026-07-10, byte-verified): sui misclassifies it as non-recursive →
;; hard Blackhole → `libxcrypt.nativeBuildInputs` drops its perl dep →
;; the drv diverges.  The fix, stated as data: it MUST be recursive +
;; Promise (a Fixpoint discipline that isn't fails is_correctly_classified).
(defthunk-discipline
  :name             "overlay-fixpoint"
  :recursive        #t
  :reentry          Promise
  :memoize-on-force  #t
  :recursion-kind   Fixpoint)

;; ── Dynamic attrpath-key construction force-site ────────────────────
;; A dynamic key `${e}` at the HEAD of an attrpath (`{ ${e} = v; }`) is
;; forced at construction — nix must know the key-set to reach WHNF.
(defattrkey-forcesite
  :name       "head-dynamic"
  :position   Head
  :force-site Construction)

;; A dynamic key AFTER the head (`{ a.${e} = v; }`) is DEFERRED: nix
;; builds `{ a = <thunk {${e}=v}>; }`, so `e` forces only when `.a` is
;; demanded.  Byte-parity root fixed 2026-07-11 (module-system frontier,
;; byte-verified): sui forced tail keys eagerly at construction, so in
;; the NixOS fixpoint `config.homes.${cfg.userName}` read `config` while
;; it was mid-force → empty-Promise partial → softened `null` key →
;; `homes.null` instead of `homes.<name>`.  The fix, as data: a TAIL key
;; MUST be HeadDemand (a `Construction` pairing fails is_parity_correct).
(defattrkey-forcesite
  :name       "tail-dynamic"
  :position   Tail
  :force-site HeadDemand)
