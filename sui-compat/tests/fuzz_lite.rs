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
use sui_compat::nar::NarReader;

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

    /// `NarReader::read_complete` must not panic on arbitrary bytes.
    /// Restored after Gap 11 fix added a 4 GiB cap on length prefixes.
    #[test]
    fn nar_read_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let result = std::panic::catch_unwind(|| {
            let mut cursor = std::io::Cursor::new(bytes.as_slice());
            NarReader::read_complete(&mut cursor)
        });
        prop_assert!(result.is_ok(), "NarReader panicked on {} bytes", bytes.len());
    }

    /// NAR reader with a valid magic header (stress the body parser).
    #[test]
    fn nar_read_never_panics_with_magic(tail in prop::collection::vec(any::<u8>(), 0..512)) {
        let magic = b"nix-archive-1";
        let mut bytes = Vec::with_capacity(24 + tail.len());
        bytes.extend_from_slice(&(magic.len() as u64).to_le_bytes());
        bytes.extend_from_slice(magic);
        // Pad to 8-byte alignment.
        while bytes.len() % 8 != 0 {
            bytes.push(0);
        }
        bytes.extend(tail);
        let result = std::panic::catch_unwind(|| {
            let mut cursor = std::io::Cursor::new(bytes.as_slice());
            NarReader::read_complete(&mut cursor)
        });
        prop_assert!(result.is_ok(), "NarReader panicked with magic on {} bytes", bytes.len());
    }

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
