//! Sanity checks for the shared oracle helpers in `common/`.
//!
//! Offline mode: verifies the module links, corpus functions don't panic,
//! and sui's own evaluator round-trips simple expressions.
//!
//! Online mode (SUI_TEST_ONLINE=1): verifies that `assert_eq_nix` agrees
//! with real nix on a handful of trivial expressions. If this test
//! fails the rest of the differential suite cannot be trusted.

mod common;

#[test]
fn sui_eval_is_reachable() {
    let v = common::sui_eval_json("1 + 1");
    assert_eq!(v, serde_json::json!(2));
}

#[test]
fn sui_eval_error_is_wrapped() {
    let v = common::sui_eval_json("let x = x; in x");
    assert!(common::is_error_json(&v), "expected error, got {v}");
}

#[test]
fn pleme_io_sample_is_stable() {
    let a = common::pleme_io_flake_nix_sample(5);
    let b = common::pleme_io_flake_nix_sample(5);
    assert_eq!(a, b, "sample must be deterministic");
}

#[test]
fn nix_store_drv_sample_only_returns_drv_files() {
    for p in common::nix_store_drv_sample(10) {
        assert_eq!(
            p.extension().and_then(|s| s.to_str()),
            Some("drv"),
            "non-drv path in drv sample: {}",
            p.display()
        );
    }
}

#[test]
fn sanity_oracle_agrees_on_arithmetic() {
    if common::skip_if_offline("sanity_oracle_agrees_on_arithmetic") {
        return;
    }
    common::assert_eq_nix("1 + 1");
    common::assert_eq_nix("2 * 3");
    common::assert_eq_nix("let x = 5; in x * x");
}

#[test]
fn sanity_oracle_agrees_on_lists_and_attrs() {
    if common::skip_if_offline("sanity_oracle_agrees_on_lists_and_attrs") {
        return;
    }
    common::assert_eq_nix("builtins.length [1 2 3]");
    common::assert_eq_nix(r#"{ a = 1; b = 2; }.a + { a = 1; b = 2; }.b"#);
}
