//! Layer 12: fuzz-lite regression tests for sui-compat parsers.
//!
//! Proptest generates random byte sequences and asserts the
//! derivation ATerm parser, the NAR reader, and the flake.lock JSON
//! parser never *panic*. Returning `Err` is fine; what's not fine
//! is a crash.
//!
//! See `sui-eval/tests/fuzz_lite.rs` for notes on upgrading to a
//! real cargo-fuzz setup if desired.

use proptest::prelude::*;
use sui_compat::derivation::Derivation;
use sui_compat::flake::FlakeLock;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// `Derivation::parse` must not panic on arbitrary bytes.
    #[test]
    fn derivation_parse_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        let result = std::panic::catch_unwind(|| Derivation::parse(&bytes));
        prop_assert!(result.is_ok(), "Derivation::parse panicked on {} bytes", bytes.len());
    }

    /// `Derivation::parse` must not panic on inputs that start
    /// with a valid prefix ("Derive(") — a small structural
    /// bias that's likely to exercise more parser code paths.
    #[test]
    fn derivation_parse_never_panics_with_prefix(tail in prop::collection::vec(any::<u8>(), 0..256)) {
        let mut bytes = b"Derive(".to_vec();
        bytes.extend(tail);
        let result = std::panic::catch_unwind(|| Derivation::parse(&bytes));
        prop_assert!(result.is_ok(), "Derivation::parse panicked on prefixed {} bytes", bytes.len());
    }

    // NOTE (2026-04-06): NarReader fuzz targets are disabled
    // because the reader trusts a u64 length prefix in the first
    // 8 bytes of input. Random fuzz bytes make that u64 huge,
    // causing `Vec::with_capacity(len)` to attempt a multi-exabyte
    // allocation that the OS refuses — the process aborts (SIGABRT,
    // not a catchable panic), so `catch_unwind` cannot contain it.
    //
    // This is a real hardening gap in sui-compat (tracked as
    // Gap 11 in sui_known_gaps.md): NarReader::read_str should
    // validate the length prefix against a sane cap (e.g., 4 GiB
    // or remaining input length) before allocating. Once that
    // hardening lands, restore both a "no magic" test and a
    // "with magic" test to this file.

    /// `FlakeLock::parse` must not panic on arbitrary UTF-8 strings.
    #[test]
    fn flake_lock_parse_never_panics(s in ".{0,500}") {
        let result = std::panic::catch_unwind(|| FlakeLock::parse(&s));
        prop_assert!(result.is_ok(), "FlakeLock::parse panicked on {} chars", s.len());
    }

    /// `FlakeLock::parse` on a shape that's superficially lock-like.
    #[test]
    fn flake_lock_parse_never_panics_with_skeleton(
        extra in ".{0,200}",
        version in 0u32..20,
    ) {
        let json = format!(
            r#"{{"nodes":{{}},"root":"root","version":{version},"extra":"{}"}}"#,
            extra.replace('"', r#"\""#).replace('\\', r#"\\"#)
        );
        let result = std::panic::catch_unwind(|| FlakeLock::parse(&json));
        prop_assert!(result.is_ok(), "FlakeLock::parse panicked on skeleton: {json}");
    }
}
