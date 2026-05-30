;; Typed border for the L1 AstGraph — the parsed, canonicalized,
;; content-addressed form of a single Nix expression (one `.nix` file
;; or one inline expression).
;;
;; Mirrors lockfile_graph.lisp's pattern: an `AstGraph` is a dense
;; `Vec<AstNode>` keyed by `NodeId` (u32), where `nodes[0]` is the root
;; expression. Every reference between nodes is a `NodeId`, so the
;; archive form is flat, pointer-free, and mmap-and-cast-able.
;;
;; Why a typed AST and not the rnix CST:
;;
;; * rnix gives a lossless rowan CST (every byte of the source is
;;   reachable). That's the right shape for tooling (LSP, formatter),
;;   the wrong shape for archiving (heavy green tree, parse-specific,
;;   re-parses on every access).
;; * The L1 AstGraph is what every downstream cares about: which
;;   identifiers, which attrset paths, which lambda formals, which
;;   imports. The byte-level rendering is recoverable from the source
;;   text (which is also content-addressed in the store).
;;
;; Coverage:
;;
;;   - Literals: Int, Float, Str (including interpolation), Path,
;;     IndentedStr, NullLit, BoolLit
;;   - References: Ident, Select (attrset.a.b.c with optional fallback)
;;   - Containers: List, AttrSet (recursive or not, including `inherit`)
;;   - Bindings: LetIn, With, Assert
;;   - Functions: Lambda (with formal-args destructuring), Apply
;;   - Control: IfThenElse
;;   - Operators: BinOp (+, -, *, /, ==, !=, <, <=, >, >=, &&, ||, ->,
;;                 //, ++), UnaryOp (Neg, Not), HasAttr (a ? b)
;;   - Forward-compat: Unknown { kind, source_text } — preserves the
;;     source verbatim so future versions can re-parse it without losing
;;     information.
;;
;; This file holds *fixtures* used by ast_graph tests. Production
;; AstGraphs are built from real `.nix` files via
;; `AstGraph::from_source` and persisted as rkyv blobs in
;; sui-graph-store.

;; ── Fixture 1: single integer literal ──────────────────────────────
;; `42`

(defast-graph-fixture
  :name        "literal-int"
  :source      "42"
  :root-kind   "Int"
  :node-count  1
  :notes       "Smallest possible expression. Anchors the literal path.")

;; ── Fixture 2: let-in binding ──────────────────────────────────────
;; `let x = 1; in x + 2`

(defast-graph-fixture
  :name        "let-in-with-binop"
  :source      "let x = 1; in x + 2"
  :root-kind   "LetIn"
  :node-count  6   ;; LetIn, binding(x=Int(1)), BinOp(+), Ident(x), Int(2), Int(1)
  :notes       "Exercises let-bindings + arithmetic + identifier references.")

;; ── Fixture 3: nixos-module style attrset ──────────────────────────
;; `{ config, lib, pkgs, ... }: { networking.hostName = "rio"; }`

(defast-graph-fixture
  :name        "nixos-module-skeleton"
  :source      "{ config, lib, pkgs, ... }: { networking.hostName = \"rio\"; }"
  :root-kind   "Lambda"
  :node-count  6   ;; Lambda, Formals(config/lib/pkgs/...), AttrSet, AttrPath(networking.hostName), Str("rio")
  :notes       "Canonical NixOS module shape. Validates lambda destructuring + nested attr paths.")
