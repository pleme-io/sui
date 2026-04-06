# Fuzzing sui

sui ships two levels of adversarial testing:

## Level 1: fuzz-lite (ships in `cargo test`)

**Where**: `sui-eval/tests/fuzz_lite.rs` and `sui-compat/tests/fuzz_lite.rs`.

These are proptest-driven panic-hunters. They generate random byte
sequences (and, where applicable, length-prefixed or grammar-shaped
inputs) and assert the parser does not *panic*. Returning an `Err` is
fine; the goal is to catch crashes, array OOBs, integer overflows,
and unchecked unwraps.

Run with:

```bash
cargo test -p sui-eval   --test fuzz_lite
cargo test -p sui-compat --test fuzz_lite
```

Default: 256 random cases per property, sub-second total. No nightly
toolchain required.

## Level 2: cargo-fuzz (coverage-guided, persistent, opt-in)

fuzz-lite is a good smoke test, but a real coverage-guided fuzzer
(`cargo-fuzz` / libFuzzer) will find deeper bugs. Wire it up like
this when you want serious adversarial testing:

```bash
cargo install cargo-fuzz
cd sui-compat && cargo +nightly fuzz init
```

Then in `sui-compat/fuzz/fuzz_targets/`, create one target per
parser:

```rust
// fuzz_targets/fuzz_aterm.rs
#![no_main]
use libfuzzer_sys::fuzz_target;
use sui_compat::derivation::Derivation;
fuzz_target!(|data: &[u8]| { let _ = Derivation::parse(data); });
```

Same pattern for `NarReader::read_complete`, `FlakeLock::parse`, and
(in sui-eval) `rnix::Root::parse` / `sui_eval::eval`.

Run with:

```bash
cargo +nightly fuzz run fuzz_aterm
```

Corpus files go under `sui-compat/fuzz/corpus/fuzz_aterm/` and should
be committed so CI can use them as seeds.

**Why level 2 is not yet wired up**: cargo-fuzz requires nightly Rust
and an install step, which would break the no-setup promise of
`cargo test`. The fuzz-lite layer ships *in* the regular test suite
and catches the most common panic regressions without that friction.
Upgrade to level 2 when a fuzz campaign is on the roadmap.
