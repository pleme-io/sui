//! Contract tests for the OperatorView trait.
//!
//! Every implementor must:
//! - report a non-empty subject + header_label
//! - render a body that produces output (no panic, no early return)
//! - optionally render a summary; defaults are tolerated
//!
//! These tests use lightweight fixtures rather than the real
//! sui-spec-inventory views (the inventory tool lives in a bin,
//! not a lib).  The fixtures share the same OperatorView trait
//! the bin's three view structs implement.

use sui_spec::operator_view::{render, OperatorView};

struct MinimalView;

impl OperatorView for MinimalView {
    fn subject(&self) -> &str { "minimal" }
    fn header_label(&self) -> &str { "minimal" }
    fn render_body(&self) {
        println!("body called");
    }
}

#[test]
fn render_succeeds_with_default_summary() {
    let v = MinimalView;
    render(&v);
}

struct LoudView;

impl OperatorView for LoudView {
    fn subject(&self) -> &str { "loud" }
    fn header_label(&self) -> &str { "loud-thing" }
    fn render_body(&self) {
        for i in 0..5 {
            println!("  body row {i}");
        }
    }
    fn render_summary(&self) {
        println!("  loud summary line");
    }
}

#[test]
fn render_works_with_explicit_summary() {
    let v = LoudView;
    render(&v);
}

#[test]
fn subject_and_label_are_returnable() {
    let v = MinimalView;
    assert_eq!(v.subject(), "minimal");
    assert_eq!(v.header_label(), "minimal");
}

#[test]
fn pick_format_finds_named_entry() {
    use sui_spec::operator_view::pick_format;
    let formats = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
    let beta = pick_format(formats, |s| s == "beta", "beta").unwrap();
    assert_eq!(beta, "beta");
}

#[test]
fn pick_format_errors_with_typed_phase() {
    use sui_spec::SpecError;
    use sui_spec::operator_view::pick_format;
    let formats = vec!["a".to_string(), "b".to_string()];
    let err = pick_format(formats, |s| s == "missing", "missing-label").unwrap_err();
    match err {
        SpecError::Interp { phase, message } => {
            assert_eq!(phase, "operator-view");
            assert!(message.contains("missing-label"));
        }
        other => panic!("expected Interp error, got {other:?}"),
    }
}
