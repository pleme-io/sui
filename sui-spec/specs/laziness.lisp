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
