//! `OperatorView` — the shape every operator-facing introspection
//! mode shares.
//!
//! Three callers reach this shape in `sui-spec-inventory`:
//!
//! - `--flake-lock <path>` (lock_file primitive)
//! - `--narinfo <path>` (narinfo primitive)
//! - `--registry-resolve <ref>` (registry primitive)
//!
//! Each one (a) consumes a string input, (b) calls a typed
//! substrate primitive that returns `Result<T, SpecError>`, and
//! (c) renders T as a Nord-styled banner + body + optional
//! summary line.  Third-site rule per the PRIME DIRECTIVE — the
//! trait extraction earns its keep when the next mode (e.g.
//! `--realisation-parse`, `--substituter-probe`) adds **one**
//! impl block instead of replicating banner / table / summary
//! plumbing.
//!
//! ## Anatomy of an operator view
//!
//! ```text
//!   <glyph>  <header>  <subject>     <- emit_banner
//!   (blank)
//!   <body row>
//!   <body row>                       <- emit_body
//!   ...
//!   (blank)
//!   <summary line>                   <- emit_summary (optional)
//! ```
//!
//! Implementors only own `parse()` + `body()`.  Banner + summary
//! defaults match the cppnix-style output the inventory tool
//! already emits.
//!
//! ## Why a trait
//!
//! - One place for the banner shape — operators see consistent
//!   output across every mode.
//! - New introspection modes are 1 impl + 1 `match` arm in CLI
//!   plumbing.  No more cut-paste of the Nord styling.
//! - Test harness in CSE style: a single property test asserts
//!   every implementor emits a banner first, body second,
//!   summary last (or nothing).

use crate::SpecError;

/// One operator-facing introspection view.  Implementors plug
/// into the inventory CLI to surface a typed substrate primitive.
pub trait OperatorView {
    /// Source the implementor reports against (file path, ref
    /// string, etc.).  Used in the banner.
    fn subject(&self) -> &str;

    /// One-word noun for the header, e.g. `"flake.lock"`,
    /// `"narinfo"`, `"registry resolve"`.
    fn header_label(&self) -> &str;

    /// Render the body rows under the banner.  Pure I/O via
    /// stdout writes — implementors print row-by-row.
    fn render_body(&self);

    /// Optional summary line printed below the body.  Default
    /// emits nothing.
    fn render_summary(&self) {}
}

/// Helper that renders `view` end-to-end: banner, blank, body,
/// blank, summary.
pub fn render<V: OperatorView>(view: &V) {
    use crate::style::{glyph_snowflake, header, muted};
    println!(
        "{}  {}  {}",
        glyph_snowflake(),
        header(view.header_label()),
        muted(view.subject()),
    );
    println!();
    view.render_body();
    println!();
    view.render_summary();
}

/// Common error path for inventory CLIs.  Lifts `SpecError` and
/// `io::Error` into a single `Box<dyn Error>` consumers can `?`
/// against.
pub type ViewResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Helper that loads a substrate primitive by name from a
/// `load_canonical()` Vec.  Most inventory modes do exactly this
/// to obtain the format spec they parse against.
pub fn pick_format<F, P>(
    formats: Vec<F>,
    predicate: P,
    label: &'static str,
) -> Result<F, SpecError>
where
    P: Fn(&F) -> bool,
{
    formats
        .into_iter()
        .find(predicate)
        .ok_or_else(|| SpecError::Interp {
            phase: "operator-view".into(),
            message: format!("missing format spec: {label}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        subj: String,
        body_called: std::cell::RefCell<bool>,
        summary_called: std::cell::RefCell<bool>,
    }

    impl OperatorView for Fixture {
        fn subject(&self) -> &str { &self.subj }
        fn header_label(&self) -> &str { "fixture" }
        fn render_body(&self) {
            *self.body_called.borrow_mut() = true;
        }
        fn render_summary(&self) {
            *self.summary_called.borrow_mut() = true;
        }
    }

    #[test]
    fn render_calls_body_and_summary() {
        let f = Fixture {
            subj: "x".into(),
            body_called: std::cell::RefCell::new(false),
            summary_called: std::cell::RefCell::new(false),
        };
        render(&f);
        assert!(*f.body_called.borrow());
        assert!(*f.summary_called.borrow());
    }

    #[test]
    fn pick_format_finds_match() {
        let formats = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let result = pick_format(formats, |s| s == "b", "test").unwrap();
        assert_eq!(result, "b");
    }

    #[test]
    fn pick_format_errors_when_missing() {
        let formats = vec!["a".to_string(), "b".to_string()];
        let err = pick_format(formats, |s| s == "c", "test-thing")
            .unwrap_err();
        match err {
            SpecError::Interp { phase, message } => {
                assert_eq!(phase, "operator-view");
                assert!(message.contains("test-thing"));
            }
            _ => panic!("expected Interp error, got {err:?}"),
        }
    }
}
