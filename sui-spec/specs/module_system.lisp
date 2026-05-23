;; sui-spec/specs/module_system.lisp — typed border for the Nix
;; module system.  Three authoring surfaces compose:
;;
;;   (defoption-type ...)           — one entry per lib.types.<X>
;;   (defpriority ...)              — one rank in the priority lattice
;;   (defmodule-eval-algorithm ...) — the fixed-point pipeline
;;
;; Implementation is M2 work.  This file names the contract;
;; sui-eval and sui-bytecode will both drive this exact spec when
;; the impl lands, so they cannot drift.

;; ── Priority lattice ──────────────────────────────────────────────
;;
;; Numerically, lower :level = higher priority.  Matches cppnix's
;; lib.modules priority convention.

(defpriority
  :name   "mkForce"
  :level  50
  :origin MkForce)

(defpriority
  :name   "normal"
  :level  100
  :origin Normal)

(defpriority
  :name   "mkDefault"
  :level  1000
  :origin MkDefault)

(defpriority
  :name   "mkOptionDefault"
  :level  1500
  :origin MkOptionDefault)

;; ── Option-type registry — the cppnix-canonical types ────────────
;;
;; Every NixOS / nix-darwin / home-manager option lives in one of
;; these types (or a composition of them).  Each declares a merge
;; strategy + acceptance check.

(defoption-type
  :name           "bool"
  :merge-strategy LastWins
  :check-kind     Bool)

(defoption-type
  :name           "int"
  :merge-strategy LastWins
  :check-kind     Int)

(defoption-type
  :name           "str"
  :merge-strategy LastWins
  :check-kind     Str)

(defoption-type
  :name           "path"
  :merge-strategy LastWins
  :check-kind     Path)

(defoption-type
  :name           "null"
  :merge-strategy LastWins
  :check-kind     Null)

(defoption-type
  :name           "any"
  :merge-strategy AnyLastWins
  :check-kind     Any)

(defoption-type
  :name           "package"
  :merge-strategy LastWins
  :check-kind     Package)

(defoption-type
  :name           "attrs"
  :merge-strategy AttrsetMerge
  :check-kind     Attrs)

(defoption-type
  :name           "listOf"
  :merge-strategy Concatenate
  :check-kind     ListOf
  :element-type   "any")

(defoption-type
  :name           "attrsOf"
  :merge-strategy AttrsetMerge
  :check-kind     AttrsOf
  :element-type   "any")

(defoption-type
  :name           "submodule"
  :merge-strategy SubmoduleMerge
  :check-kind     Submodule)

(defoption-type
  :name           "nullOr"
  :merge-strategy LastWins
  :check-kind     NullOr
  :element-type   "any")

(defoption-type
  :name           "oneOf"
  :merge-strategy Disjoint
  :check-kind     OneOf
  :member-types   ("bool" "int" "str"))

(defoption-type
  :name           "functionTo"
  :merge-strategy Custom
  :check-kind     FunctionTo
  :element-type   "any")

;; ── Algorithm: cppnix module-eval fixed point ────────────────────
;;
;; Eight phases left-to-right.  Each operates on the typed
;; EvalModulesArgs scratchpad; later phases consume slots earlier
;; phases populate.  M2 interpreter implements each phase against
;; sui-eval's lazy Value type.

(defmodule-eval-algorithm
  :name "cppnix-module-eval"
  :phases ((:kind CollectModules :bind "modules")
           (:kind PartitionOptionsAndConfig :from "modules" :bind "partition")
           (:kind BuildOptionTree :from "partition" :bind "option-tree")
           (:kind GroupDefinitions :from "partition" :bind "definitions")
           (:kind ResolveConditionals :from "definitions" :bind "active-defs")
           (:kind ResolvePriorities :from "active-defs" :bind "winning-defs")
           (:kind MergePerOption :from "winning-defs" :bind "merged")
           (:kind TypeCheck :from "merged" :bind "checked")
           (:kind EvaluateRecursive :from "checked" :bind "resolved")
           (:kind EmitConfig :from "resolved" :bind "config")))
