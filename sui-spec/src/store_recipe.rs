//! Declarative pipeline composition for store operations.
//!
//! A `StoreRecipe` is a typed Lisp authoring surface that
//! composes everything below it into one declarative artifact:
//!
//!   1. Pick a slice (selector by name + pattern + max-entries)
//!   2. Apply N transforms in declared order
//!   3. Materialize the result at a destination
//!   4. Verify the round-trip
//!
//! Authored as:
//!
//! ```lisp
//! (defstore-recipe
//!   :name        "redacted-sources"
//!   :description "Take the tiny-sources slice, redact secrets,
//!                 materialize under ~/.cache/sui/recipes/."
//!   :slice       "tiny-sources"
//!   :transforms  ("redact-base64-secrets" "strip-shell-comments")
//!   :dest-suffix "redacted-sources")
//! ```
//!
//! The Rust executor resolves the slice + transform names from
//! their canonical catalogs and runs the full pipeline.

use serde::{Deserialize, Serialize};
use tatara_lisp::DeriveTataraDomain;

use crate::SpecError;

/// Typed pipeline declaration.  Operator-facing Lisp form.
#[derive(DeriveTataraDomain, Serialize, Deserialize, Debug, Clone)]
#[tatara(keyword = "defstore-recipe")]
pub struct StoreRecipe {
    /// Stable name.
    pub name: String,
    /// Operator-facing description.
    pub description: String,
    /// Slice name from `store_ops::load_canonical_slices`.
    pub slice: String,
    /// Ordered list of transform names from
    /// `store_transform::load_canonical`.
    pub transforms: Vec<String>,
    /// Suffix under `~/.cache/sui/recipes/` for the output.
    /// The final dest is `~/.cache/sui/recipes/<dest-suffix>`.
    #[serde(rename = "destSuffix")]
    pub dest_suffix: String,
}

/// Per-entry outcome of running a recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeEntryOutcome {
    pub source: std::path::PathBuf,
    pub dest: std::path::PathBuf,
    /// Total rewrites across all transforms (file + ref + entry).
    pub total_rewrites: usize,
    /// `true` if every transform was a no-op for this entry.
    pub noop: bool,
}

/// Aggregate outcome of running a recipe end-to-end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeOutcome {
    pub recipe: String,
    pub slice: String,
    pub transforms: Vec<String>,
    pub dest_root: std::path::PathBuf,
    pub entries: Vec<RecipeEntryOutcome>,
}

impl RecipeOutcome {
    /// Number of entries the recipe modified (at least one
    /// transform produced a non-zero count).
    #[must_use]
    pub fn modified_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.noop).count()
    }

    /// Total rewrites across every entry + every transform.
    #[must_use]
    pub fn total_rewrites(&self) -> usize {
        self.entries.iter().map(|e| e.total_rewrites).sum()
    }
}

/// Resolve + run a recipe end-to-end.  Lifts the slice
/// definition from `store_ops::load_canonical_slices`, the
/// transform definitions from `store_transform::load_canonical`,
/// runs the materialize → transform → re-materialize pipeline.
///
/// # Errors
///
/// - `recipe-unknown-slice` if `recipe.slice` isn't authored.
/// - `recipe-unknown-transform` if any transform name isn't authored.
/// - Whatever the underlying primitives return on failure.
pub fn run(
    recipe: &StoreRecipe,
    dest_root_base: &std::path::Path,
) -> Result<RecipeOutcome, SpecError> {
    use crate::store_ops::{self, ParsedNar};
    use crate::store_transform;
    use crate::nar;

    let slices = store_ops::load_canonical_slices()?;
    let slice = slices.iter().find(|s| s.name == recipe.slice)
        .ok_or_else(|| SpecError::Interp {
            phase: "recipe-unknown-slice".into(),
            message: format!("recipe `{}` references unknown slice `{}`",
                recipe.name, recipe.slice),
        })?;

    let all_transforms = store_transform::load_canonical()?;
    let resolved: Vec<_> = recipe.transforms.iter().map(|name| {
        all_transforms.iter().find(|t| &t.name == name)
            .cloned()
            .ok_or_else(|| SpecError::Interp {
                phase: "recipe-unknown-transform".into(),
                message: format!("recipe `{}` references unknown transform `{}`",
                    recipe.name, name),
            })
    }).collect::<Result<Vec<_>, _>>()?;

    let dest_root = dest_root_base.join(&recipe.dest_suffix);
    std::fs::create_dir_all(&dest_root).map_err(|e| SpecError::Interp {
        phase: "recipe-mkdir".into(),
        message: format!("mkdir {}: {e}", dest_root.display()),
    })?;

    let plans = store_ops::build_materialization_plan(slice, &dest_root)?;
    let mut entries: Vec<RecipeEntryOutcome> = Vec::with_capacity(plans.len());

    for plan in &plans {
        // 1. Encode source.
        let nar_bytes = nar::encode(&plan.source).map_err(|e| SpecError::Interp {
            phase: "recipe-encode".into(),
            message: format!("encode {}: {e}", plan.source.display()),
        })?;
        // 2. Parse to typed tree.
        let mut parsed = ParsedNar::parse(&nar_bytes).map_err(|e| SpecError::Interp {
            phase: "recipe-parse".into(),
            message: format!("parse {}: {e}", plan.source.display()),
        })?;
        // 3. Apply every transform.
        let outcomes = store_transform::apply_all(&mut parsed, &resolved)?;
        let total = outcomes.iter().map(|o|
            o.file_rewrites + o.ref_rewrites + o.entries_renamed
        ).sum::<usize>();
        // 4. Serialize → decode → dest.
        let new_nar = parsed.serialize();
        if plan.dest.exists() {
            std::fs::remove_dir_all(&plan.dest).ok();
        }
        nar::decode(&new_nar, &plan.dest).map_err(|e| SpecError::Interp {
            phase: "recipe-decode".into(),
            message: format!("decode → {}: {e}", plan.dest.display()),
        })?;
        entries.push(RecipeEntryOutcome {
            source: plan.source.clone(),
            dest: plan.dest.clone(),
            total_rewrites: total,
            noop: total == 0,
        });
    }

    Ok(RecipeOutcome {
        recipe: recipe.name.clone(),
        slice: recipe.slice.clone(),
        transforms: recipe.transforms.clone(),
        dest_root,
        entries,
    })
}

// ── Canonical Lisp spec ────────────────────────────────────────────

pub const CANONICAL_STORE_RECIPES_LISP: &str =
    include_str!("../specs/store_recipes.lisp");

/// Compile every authored recipe.
///
/// # Errors
///
/// Returns an error if the Lisp source can't be parsed.
pub fn load_canonical() -> Result<Vec<StoreRecipe>, SpecError> {
    crate::loader::load_all::<StoreRecipe>(CANONICAL_STORE_RECIPES_LISP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_recipes_parse() {
        let rs = load_canonical().expect("recipes must compile");
        assert!(!rs.is_empty());
    }

    #[test]
    fn recipe_references_known_slice() {
        // Substrate-wide invariant: every recipe's slice exists
        // in the canonical slice catalog.
        let recipes = load_canonical().unwrap();
        let slices = crate::store_ops::load_canonical_slices().unwrap();
        let slice_names: std::collections::HashSet<String> =
            slices.iter().map(|s| s.name.clone()).collect();
        for r in &recipes {
            assert!(slice_names.contains(&r.slice),
                "recipe `{}` references unknown slice `{}`", r.name, r.slice);
        }
    }

    #[test]
    fn recipe_references_known_transforms() {
        let recipes = load_canonical().unwrap();
        let xforms = crate::store_transform::load_canonical().unwrap();
        let xform_names: std::collections::HashSet<String> =
            xforms.iter().map(|t| t.name.clone()).collect();
        for r in &recipes {
            for t in &r.transforms {
                assert!(xform_names.contains(t),
                    "recipe `{}` references unknown transform `{}`", r.name, t);
            }
        }
    }
}
