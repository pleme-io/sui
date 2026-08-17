//! The names Nix resolves as **bare identifiers** — one list, shared by every
//! engine.
//!
//! # Why this is shared rather than mirrored
//!
//! Nix exposes a small subset of `builtins` at the top level, so `map`,
//! `throw`, `import` and friends work unqualified. Every engine needs that set,
//! and until now each carried its own hand-written copy:
//!
//! | engine | site | count | had `break`? |
//! |---|---|---|---|
//! | tree-walker | `sui-eval/src/builtins/mod.rs` `DEFAULT_SCOPE` | 21 | **no** |
//! | `sui-ir` | `sui-ir/src/builtins.rs` — comment: *"mirrored from sui-eval"* | 21 | **no** |
//! | bytecode VM | `sui-bytecode/src/compiler.rs` `is_global_builtin` | 19 | **yes** |
//!
//! They had already drifted, and the drift was a real wrong answer:
//! `with { break = "LIB"; }; break` evaluated to `"LIB"` on the tree-walker
//! while nix and the VM both say `false`. A genuine global cannot be shadowed
//! by a `with`, so the walker letting one through changes the meaning of a
//! program.
//!
//! Measured against nix 2.31.5 — `break` resolves to a `lambda`, so the VM was
//! right and the other two were missing it:
//!
//! ```text
//! $ nix eval --impure --raw --expr 'builtins.typeOf break'   → lambda
//! ```
//!
//! This is the third instance of one shape. `IMPERSONATED_NIX_VERSION`
//! (`crate::versions`) was the first — the VM sat two minor versions behind the
//! walker and silently forked the derivation graph — and the `builtins` attrset
//! name set is the second. Each was N hand-maintained copies of one fact, free
//! to disagree, that did.
//!
//! # The two groups, and why engines treat them differently
//!
//! [`STRUCTURAL_GLOBALS`] are resolved by *construction* rather than by name
//! lookup: `true`/`false`/`null` are compiled as literals and `builtins` is the
//! attrset itself. An engine handles them before it ever consults a name list,
//! which is why the VM's list is 19 where the walker's is 21 — the counts
//! differ for a correct reason, and folding them into one list would force
//! every consumer to re-filter.
//!
//! [`CALLABLE_GLOBALS`] are the rest: names looked up in the `builtins` attrset
//! and bound into the initial scope.

/// Globals an engine resolves structurally, not by looking up a name.
///
/// `true`/`false`/`null` compile to literals; `builtins` is the attrset being
/// built. Listed here so the full global set is derivable in one place, not so
/// that consumers iterate it — most handle these earlier and by other means.
pub const STRUCTURAL_GLOBALS: &[&str] = &["builtins", "false", "null", "true"];

/// Globals resolved by looking the name up in the `builtins` attrset.
///
/// Sorted, so a diff against `builtins.attrNames builtins` reads directly.
///
/// Two entries carry history worth keeping:
/// - **`break`** — the one that had drifted. A real nix global; the walker and
///   `sui-ir` were both missing it, so a `with` could shadow it.
/// - **`placeholder`** — nixpkgs uses `(placeholder "out")` unqualified (e.g.
///   cpython's `--enable-framework=${placeholder "out"}`); without it the bare
///   reference resolves to null inside the module fixpoint and eval breaks.
///
/// The fetchers and `fromTOML` are here for the same reason: nixpkgs uses bare
/// `fetchGit` / `fetchTree` / `fromTOML` unqualified.
pub const CALLABLE_GLOBALS: &[&str] = &[
    "abort",
    "baseNameOf",
    "break",
    "derivation",
    "derivationStrict",
    "dirOf",
    "fetchGit",
    "fetchMercurial",
    "fetchTarball",
    "fetchTree",
    "fromTOML",
    "import",
    "isNull",
    "map",
    "placeholder",
    "removeAttrs",
    "scopedImport",
    "throw",
    "toString",
];

/// Every name nix resolves as a bare identifier.
#[must_use]
pub fn is_nix_global(name: &str) -> bool {
    CALLABLE_GLOBALS.contains(&name) || STRUCTURAL_GLOBALS.contains(&name)
}

/// Total size of the global scope — the number to compare against nix.
#[must_use]
pub fn global_count() -> usize {
    CALLABLE_GLOBALS.len() + STRUCTURAL_GLOBALS.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_groups_are_disjoint() {
        for s in STRUCTURAL_GLOBALS {
            assert!(
                !CALLABLE_GLOBALS.contains(s),
                "`{s}` is in both groups — a consumer filtering by group would \
                 either bind it twice or not at all"
            );
        }
    }

    #[test]
    fn callable_globals_are_sorted_and_unique() {
        // Sorted so a diff against `builtins.attrNames builtins` reads
        // directly; unique because a duplicate would silently inflate
        // `global_count()` and make the nix comparison wrong in the safe-
        // looking direction.
        let mut sorted = CALLABLE_GLOBALS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, CALLABLE_GLOBALS, "CALLABLE_GLOBALS is not sorted");
        sorted.dedup();
        assert_eq!(sorted.len(), CALLABLE_GLOBALS.len(), "duplicate entry");
    }

    #[test]
    fn break_is_present() {
        // The regression that motivated the extraction, pinned by name. The
        // tree-walker and sui-ir were both missing it, so `with { break = …; }`
        // could shadow a real nix global and change what a program means.
        assert!(is_nix_global("break"));
    }

    #[test]
    fn the_count_is_pinned() {
        // Measured against nix 2.31.5: 23 names resolve as bare identifiers.
        // If this moves, re-measure with
        //   nix eval --impure --raw --expr 'builtins.typeOf <name>'
        // per candidate rather than adjusting the number to match.
        assert_eq!(global_count(), 23);
    }

    #[test]
    fn a_non_global_builtin_is_not_a_global() {
        // Calibration. `attrNames` and `elemAt` are real builtins that nix does
        // NOT expose bare — without this row, a list that accidentally
        // contained every builtin would satisfy every assertion above.
        assert!(!is_nix_global("attrNames"));
        assert!(!is_nix_global("elemAt"));
        assert!(!is_nix_global("definitelyNotABuiltin"));
    }
}
