//! Layer 12: fuzz-lite regression tests.
//!
//! These are not full cargo-fuzz targets (which require nightly +
//! an opt-in install) — instead, proptest generates random byte
//! sequences and asserts the sui parsers/evaluator do not *panic*
//! on them. They may produce errors, which is fine; what's not
//! fine is a crash.
//!
//! If you want a *real* fuzzer (persistent, coverage-guided):
//!
//!     cargo install cargo-fuzz
//!     cargo +nightly fuzz init
//!     # port the bodies below into fuzz/fuzz_targets/*.rs
//!
//! The two extra property-based tests below cover:
//!   - `rnix::Root::parse` on arbitrary ASCII
//!   - `sui_eval::eval` on arbitrary ASCII (tolerates errors,
//!     forbids panics)

mod common;

use proptest::prelude::*;

// Modest case counts — each run is fast enough that 256 is fine.

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// `rnix::Root::parse` must never panic on any ASCII input.
    #[test]
    fn rnix_parse_never_panics_on_printable_ascii(s in "[\\x20-\\x7e]{0,200}") {
        let result = std::panic::catch_unwind(|| rnix::Root::parse(&s));
        prop_assert!(result.is_ok(), "rnix panicked on input: {s:?}");
    }

    /// `sui_eval::eval` must never panic on any ASCII input, even
    /// if it returns an error.
    #[test]
    fn sui_eval_never_panics_on_printable_ascii(s in "[\\x20-\\x7e]{0,200}") {
        let result = std::panic::catch_unwind(|| sui_eval::eval(&s));
        prop_assert!(result.is_ok(), "sui_eval panicked on input: {s:?}");
    }

    /// Even with unicode + control chars mixed in, the parser must
    /// refuse to panic.
    #[test]
    fn rnix_parse_never_panics_on_utf8(s in ".{0,200}") {
        let result = std::panic::catch_unwind(|| rnix::Root::parse(&s));
        prop_assert!(result.is_ok(), "rnix panicked on input: {s:?}");
    }

    /// Feed sui_eval::eval a grammar-shaped fragment and verify
    /// nothing catches fire even when the input is intentionally
    /// partial.
    #[test]
    fn sui_eval_handles_grammar_fragments(
        n in any::<u32>(),
        s in "[a-z]{1,8}",
    ) {
        // Build a handful of semi-valid fragments.
        for expr in [
            format!("let {s} = {n}; in {s}"),
            format!("{{ {s} = {n}; }}"),
            format!("[ {n} {n} {n} ]"),
            format!("({s}: {s} + {n}) {n}"),
        ] {
            let result = std::panic::catch_unwind(|| sui_eval::eval(&expr));
            prop_assert!(result.is_ok(), "sui_eval panicked on {expr:?}");
        }
    }
}
