;; sui-spec/specs/eager_class.lisp — authored eager/lazy steering rules.
;;
;; Each (defeager-class …) declares a shape-class sui may force EAGERLY instead
;; of the default lazy, and the byte-safety `:technique` of that reordering. The
;; enforcement border (eager_class::validate) REFUSES any rule whose technique
;; is not `ByteSufficient` in the perf ledger — so every rule here is byte-safe
;; by construction, and steering can only change HOW FAST, never THE BYTES.
;;
;; The M7 fluidity beachhead: this is the vocabulary the runtime hot-reload path
;; (sui-daemon shikumi ConfigStore) will load a live conversation over. Today
;; these are the byte-safe seed rules the border proves it accepts; a force-order
;; rule authored here would fail `canonical_specs_parse_and_every_authored_rule_is_byte_safe`.

;; ── seed rules (byte-safe) ────────────────────────────────────────

(defeager-class
  :name      "small-static-attrset-literal"
  :shape     "attrset-literal-all-static-keys-le-8"
  :technique DropUnobservedOrder
  :notes     "A small attrset literal with only statically-known keys and no
              `rec`/`with`/interpolation self-reference forces its fields eagerly;
              the field-evaluation order is never observed (no field can read
              another mid-construction), so dropping the lazy defer is
              byte-invisible — DropUnobservedOrder, ByteSufficient.")

(defeager-class
  :name      "interned-symbol-key-lookup"
  :shape     "select-on-interned-static-key"
  :technique ReprSwap
  :notes     "A `attrs.staticKey` select over an interned-symbol-keyed attrset
              swaps the string-materialized lookup for the interned u64 probe —
              same value, different storage of the key. ReprSwap, ByteSufficient.")
