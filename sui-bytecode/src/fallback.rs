//! Fallback accounting — and the strict latch that makes a laundered answer
//! impossible to mistake for a computed one.
//!
//! # The problem this exists to solve
//!
//! The VM does not evaluate alone. It delegates to the tree-walker at **three**
//! independent granularities, and until now none of them was observable in
//! practice:
//!
//! | layer | where | granularity |
//! |---|---|---|
//! | [`Layer::Builtin`] | `builtins.rs`, the bridge call | one builtin call |
//! | [`Layer::ImportedFile`] | `vm.rs`, `import_file` | one imported file |
//! | [`Layer::WholeExpression`] | the CLI's VM arm | the entire expression |
//!
//! The consequence is that a test comparing "the VM" against the tree-walker
//! can be answered *by the tree-walker on both sides*. That is not theoretical:
//! `tests/vm_cli.rs` (36 cases) and `tests/vm_capabilities.rs` (23) go through
//! the CLI, whose VM arm falls back on any error — so **a VM failing 100% of
//! those expressions passed 36/36**. A green run meant "the VM did not produce
//! a *different* answer", never "the VM computed this".
//!
//! A counter for the middle layer already existed (`vm_fallback_count()`) and
//! **nothing in the repo ever read it**.
//!
//! # What strict mode does, and what it deliberately does not
//!
//! `SUI_VM_STRICT=1` makes [`Layer::ImportedFile`] and
//! [`Layer::WholeExpression`] **hard errors**. Both mean the same thing — the
//! VM could not do its job and the walker covered for it — and that is exactly
//! what a measurement must not silently absorb.
//!
//! [`Layer::Builtin`] is counted but **never fatal, at any setting**. Bridging
//! a builtin is the VM's *architecture*, not a failure: it has no native
//! `getEnv`, `match`, `split`, `fromTOML`, `genericClosure`, `readDir` or
//! `hashFile`, and it is not supposed to. Making that arm fatal would leave
//! strict mode unable to evaluate anything at all, which is a strict mode
//! nobody can use. A caller that wants full purity asserts
//! `count(Layer::Builtin) == 0` for itself.
//!
//! Being explicit about that asymmetry is the point: a latch that conflates
//! "the VM delegated by design" with "the VM failed" would produce a red that
//! nobody could act on, and reds nobody can act on get switched off.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// Which delegation boundary was crossed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    /// A builtin with no native VM implementation went to the tree-walker.
    /// Architectural — counted, never fatal.
    Builtin,
    /// An imported file failed to compile or to run, and the tree-walker
    /// evaluated it instead. Fatal under strict.
    ImportedFile,
    /// The whole top-level expression failed and was re-run on the
    /// tree-walker. Fatal under strict.
    WholeExpression,
}

impl Layer {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Layer::Builtin => "builtin",
            Layer::ImportedFile => "imported-file",
            Layer::WholeExpression => "whole-expression",
        }
    }

    /// Whether strict mode refuses this layer.
    #[must_use]
    pub fn is_fatal_under_strict(self) -> bool {
        match self {
            Layer::Builtin => false,
            Layer::ImportedFile | Layer::WholeExpression => true,
        }
    }

    /// Every layer, so a caller can report totals without hand-listing —
    /// and so a new layer cannot be added without appearing in the report.
    pub const ALL: &'static [Layer] = &[
        Layer::Builtin,
        Layer::ImportedFile,
        Layer::WholeExpression,
    ];
}

static BUILTIN: AtomicU64 = AtomicU64::new(0);
static IMPORTED_FILE: AtomicU64 = AtomicU64::new(0);
static WHOLE_EXPRESSION: AtomicU64 = AtomicU64::new(0);

fn cell(layer: Layer) -> &'static AtomicU64 {
    match layer {
        Layer::Builtin => &BUILTIN,
        Layer::ImportedFile => &IMPORTED_FILE,
        Layer::WholeExpression => &WHOLE_EXPRESSION,
    }
}

/// `true` when `SUI_VM_STRICT=1`.
///
/// Latched once, like the other env gates in this workspace, so a mid-run
/// change cannot make one half of an evaluation strict and the other half not.
#[must_use]
pub fn strict() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("SUI_VM_STRICT").as_deref() == Ok("1"))
}

/// Record that `layer` was crossed.
///
/// Returns `Err` with an operator-facing message when strict mode refuses this
/// layer; the caller must propagate rather than fall back. Always counts,
/// strict or not — the counts are useful on their own, and a caller that wants
/// to know "did the VM really do this" reads them.
///
/// # Errors
///
/// When [`strict`] is on and the layer [`Layer::is_fatal_under_strict`].
pub fn record(layer: Layer, detail: &str) -> Result<(), String> {
    cell(layer).fetch_add(1, Ordering::Relaxed);
    if strict() && layer.is_fatal_under_strict() {
        return Err(format!(
            "SUI_VM_STRICT: refusing to fall back to the tree-walker at the \
             {} boundary: {detail}. Strict mode exists so a measurement cannot \
             silently become the walker's answer — if you want the fallback, \
             unset SUI_VM_STRICT; if you want the VM to handle this, that is \
             the bug.",
            layer.name()
        ));
    }
    Ok(())
}

/// How many times `layer` was crossed this process.
#[must_use]
pub fn count(layer: Layer) -> u64 {
    cell(layer).load(Ordering::Relaxed)
}

/// Total across every layer.
#[must_use]
pub fn total() -> u64 {
    Layer::ALL.iter().map(|l| count(*l)).sum()
}

/// One line per layer, for a diagnostic dump.
#[must_use]
pub fn report() -> String {
    Layer::ALL
        .iter()
        .map(|l| format!("{}={}", l.name(), count(*l)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Zero every counter. Test-support only.
///
/// The counters are process-global `AtomicU64`s, so a test that asserts on a
/// count must reset first AND must not run concurrently with another test that
/// evaluates. Prefer asserting a *delta* you captured yourself.
pub fn reset() {
    for l in Layer::ALL {
        cell(*l).store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_bridging_is_never_fatal() {
        // Holds regardless of the latch: bridging a builtin is architecture,
        // not failure. If this ever starts erroring, strict mode becomes
        // unusable rather than stricter.
        assert!(!Layer::Builtin.is_fatal_under_strict());
        assert!(record(Layer::Builtin, "getEnv").is_ok());
    }

    #[test]
    fn the_two_failure_layers_are_fatal_under_strict() {
        assert!(Layer::ImportedFile.is_fatal_under_strict());
        assert!(Layer::WholeExpression.is_fatal_under_strict());
    }

    #[test]
    fn counting_happens_whether_or_not_strict_is_on() {
        // The count is the instrument; the latch only decides whether crossing
        // is fatal. A caller reading counts must work with strict off.
        let before = count(Layer::Builtin);
        let _ = record(Layer::Builtin, "probe");
        assert_eq!(count(Layer::Builtin), before + 1);
    }

    #[test]
    fn report_names_every_layer() {
        // Anti-vacuity for the layer SET: adding a variant without adding it
        // to ALL would silently drop it from every report and total.
        let r = report();
        for l in Layer::ALL {
            assert!(r.contains(l.name()), "report() omits {}", l.name());
        }
        assert_eq!(Layer::ALL.len(), 3, "a layer was added or removed");
    }
}
