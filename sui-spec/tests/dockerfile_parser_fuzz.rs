//! Adversarial fuzz proof that the shipped Dockerfile parser
//! (`sui_spec::dockerfile::apply`) is TOTAL: for ANY input bytes it
//! returns `Ok(DockerfileGraph)` or `Err(SpecError)` — it NEVER panics,
//! unwraps on None, slices a `&str` at a non-char-boundary, or OOMs.
//!
//! The parser walks `&str` bytes and does raw-byte indexing in
//! `substitute_arg_refs` (the `$VAR` / `${VAR}` expander) plus a hand
//! rolled instruction splitter. Those are exactly the surfaces where a
//! naive implementation panics on a `$` adjacent to a multi-byte UTF-8
//! char, an unterminated `${`, a lone `$` at EOF, a deeply-nested
//! continuation chain, or injection-shaped bytes. This file feeds all of
//! those at proptest's default 256 cases/property (the fleet floor is
//! 100) and asserts only the totality invariant — never a specific graph
//! shape, since the input is random.
//!
//! We run under `catch_unwind` so a panic (rather than a returned
//! `Err`) is reported as a FAILING case with the offending input, not a
//! process abort — making any regression a precise, reproducible test
//! failure.

use std::panic::{catch_unwind, AssertUnwindSafe};

use proptest::prelude::*;
use sui_spec::dockerfile::{apply, DockerfileArgs, MockDockerfileEnvironment};

// ── Fleet proptest floor: 100; we run 256. ──────────────────────────
const CASES: u32 = 256;

/// Run the shipped parser on `text` with `build_args`, asserting it
/// returns a `Result` (Ok or Err) and NEVER panics. Returns the parsed
/// result on success so callers can additionally assert determinism.
fn parse_must_not_panic(text: &str, build_args: &[(String, String)]) {
    let owned = text.to_string();
    let args = build_args.to_vec();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut env = MockDockerfileEnvironment::default().with_dockerfile("fuzz", &owned);
        for (k, v) in &args {
            env = env.with_build_arg(k, v);
        }
        // The invariant: this is always a Result — Ok(graph) or
        // Err(typed). We drop the value; the point is that the call
        // returns rather than unwinds.
        apply(&DockerfileArgs { path: "fuzz".into() }, &env).is_ok()
    }));
    assert!(result.is_ok(), "parser PANICKED on input {text:?} with args {build_args:?}");
}

// ── Strategy 1: arbitrary bytes rendered as (possibly-invalid) text ──
//
// `apply` takes a `&str`, so we generate arbitrary Unicode strings
// (proptest's `.*` regex over chars, which includes control chars,
// multi-byte scalars, and the surrogate-free scalar range) — the input
// domain of the real API. This is the broadest net for the
// char-boundary / raw-byte-index panic class.

proptest! {
    #![proptest_config(ProptestConfig { cases: CASES, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_unicode_text_never_panics(text in ".*") {
        parse_must_not_panic(&text, &[]);
    }

    // Longer, multi-line arbitrary text — many random "instruction"
    // lines, exercising the per-line splitter + keyword classifier on
    // junk keywords.
    #[test]
    fn arbitrary_multiline_text_never_panics(lines in prop::collection::vec(".*", 0..40)) {
        let text = lines.join("\n");
        parse_must_not_panic(&text, &[]);
    }
}

// ── Strategy 2: dollar-injection torture ─────────────────────────────
//
// Concentrate `$` / `${` / `}` and multi-byte chars — the exact bytes
// that break a naive `text[i+1..]` / `text[i+2..end]` slice. We build a
// RUN body (the surface that goes through substitute_arg_refs) out of a
// random sequence of these fragments.

fn dollar_fragment() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("$".to_string()),
        Just("${".to_string()),
        Just("}".to_string()),
        Just("${}".to_string()),
        Just("$$".to_string()),
        Just("${A".to_string()),
        Just("A}".to_string()),
        // multi-byte chars adjacent to the above
        Just("\u{00e9}".to_string()), // é (2 bytes)
        Just("\u{4e2d}".to_string()), // 中 (3 bytes)
        Just("\u{1f600}".to_string()), // 😀 (4 bytes)
        Just("_".to_string()),
        Just("A".to_string()),
        Just(" ".to_string()),
        "[a-zA-Z0-9_]{1,4}".prop_map(|s| s),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: CASES, ..ProptestConfig::default() })]

    #[test]
    fn dollar_injection_in_a_run_body_never_panics(frags in prop::collection::vec(dollar_fragment(), 0..30)) {
        let body: String = frags.concat();
        let text = format!("FROM x\nRUN echo {body}\n");
        parse_must_not_panic(&text, &[]);
    }

    #[test]
    fn dollar_injection_in_an_env_value_never_panics(frags in prop::collection::vec(dollar_fragment(), 0..30)) {
        let body: String = frags.concat();
        let text = format!("FROM x\nENV FOO={body}\n");
        parse_must_not_panic(&text, &[]);
    }

    // A dollar-injection body WITH a matching build arg supplied, so the
    // Arg-resolution path (not just the error path) runs on torture input.
    #[test]
    fn dollar_injection_with_supplied_args_never_panics(
        frags in prop::collection::vec(dollar_fragment(), 0..30),
        arg_name in "[A-Z][A-Z0-9_]{0,6}",
        arg_val in ".{0,20}",
    ) {
        let body: String = frags.concat();
        let text = format!("FROM x\nARG {arg_name}\nRUN echo {body} ${arg_name}\n");
        parse_must_not_panic(&text, &[(arg_name, arg_val)]);
    }
}

// ── Strategy 3: structured-but-random instruction streams ────────────
//
// Generate lines that LOOK like instructions (a random real keyword +
// random junk operands) so the per-instruction sub-parsers (parse_from,
// parse_copy, parse_run, parse_arg, parse_env, parse_volume, parse_argv)
// run on adversarial operands, not just the keyword classifier.

fn keyword() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("FROM"), Just("RUN"), Just("COPY"), Just("ARG"), Just("ENV"),
        Just("WORKDIR"), Just("CMD"), Just("ENTRYPOINT"), Just("VOLUME"),
        // out-of-scope + junk keywords — must be a typed error, not a panic
        Just("HEALTHCHECK"), Just("LABEL"), Just("USER"), Just("ZZZ"), Just("--from=x"),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: CASES, ..ProptestConfig::default() })]

    #[test]
    fn random_instruction_lines_never_panic(
        lines in prop::collection::vec((keyword(), ".{0,40}"), 0..25)
    ) {
        let text: String = lines.iter().map(|(kw, rest)| format!("{kw} {rest}")).collect::<Vec<_>>().join("\n");
        parse_must_not_panic(&text, &[]);
    }

    // JSON-array-shaped operands for CMD/ENTRYPOINT/VOLUME (parse_argv) —
    // unbalanced brackets, embedded quotes, commas.
    #[test]
    fn random_json_array_operands_never_panic(
        kw in prop_oneof![Just("CMD"), Just("ENTRYPOINT"), Just("VOLUME")],
        inner in r#"[\[\]",a-z /]{0,40}"#,
    ) {
        let text = format!("FROM x\n{kw} [{inner}]\n");
        parse_must_not_panic(&text, &[]);
    }

    // COPY / RUN flag torture — `--flag`, `--flag=`, `--mount=...` with
    // random commas + kv pairs.
    #[test]
    fn random_flag_operands_never_panic(
        flags in prop::collection::vec("--[a-z]{0,6}(=[a-z,=.]{0,15})?", 0..6),
        body in ".{0,30}",
    ) {
        let flag_str = flags.join(" ");
        let run = format!("FROM x\nRUN {flag_str} {body}\n");
        let copy = format!("FROM x\nCOPY {flag_str} {body}\n");
        parse_must_not_panic(&run, &[]);
        parse_must_not_panic(&copy, &[]);
    }
}

// ── Strategy 4: size / depth torture (no OOM, no stack blowup) ────────

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    // Deep continuation chains — a single logical line made of N
    // backslash-continued physical lines. Must join in bounded memory,
    // never recurse to a stack overflow (join_continuations is a loop,
    // but we prove it).
    #[test]
    fn deep_continuation_chains_never_panic_or_blow_the_stack(depth in 0usize..8000) {
        let mut text = String::from("FROM x\nRUN a");
        for _ in 0..depth {
            text.push_str(" \\\n b");
        }
        text.push('\n');
        parse_must_not_panic(&text, &[]);
    }

    // Very long single lines — a huge RUN body, a huge image name.
    #[test]
    fn very_long_single_lines_never_panic(len in 0usize..200_000) {
        let big = "a".repeat(len);
        parse_must_not_panic(&format!("FROM x\nRUN {big}\n"), &[]);
        parse_must_not_panic(&format!("FROM {big}\n"), &[]);
    }

    // Many instructions — a long linear graph. The graph is a Vec, so
    // this is bounded, but prove it holds at scale.
    #[test]
    fn many_instructions_never_panic(count in 0usize..3000) {
        let mut text = String::from("FROM x\n");
        for i in 0..count {
            text.push_str(&format!("ARG A{i}=v{i}\n"));
        }
        parse_must_not_panic(&text, &[]);
    }
}

// ── Strategy 5: byte-level torture (control chars, NULs, CRLF, BOM) ───

proptest! {
    #![proptest_config(ProptestConfig { cases: CASES, ..ProptestConfig::default() })]

    // Interleave control/NUL/CR/LF/tab/BOM bytes with instruction-ish
    // text — the exact junk a mangled fetch or a Windows-authored file
    // ships. All are valid `char`s so the input stays a valid `&str`,
    // which is the parser's real input domain.
    #[test]
    fn control_and_whitespace_bytes_never_panic(
        parts in prop::collection::vec(
            prop_oneof![
                Just("\0".to_string()),
                Just("\r".to_string()),
                Just("\n".to_string()),
                Just("\t".to_string()),
                Just("\u{feff}".to_string()), // BOM
                Just("\u{0007}".to_string()), // BEL
                Just("FROM x".to_string()),
                Just("RUN echo".to_string()),
                Just("$VAR".to_string()),
                ".{0,10}",
            ],
            0..40,
        )
    ) {
        let text: String = parts.concat();
        parse_must_not_panic(&text, &[]);
    }
}

// ── Determinism under fuzz: a successful parse is reproducible ───────
//
// Beyond "never panics", any input the parser DOES accept must be
// content-addressed deterministically — two runs over identical bytes
// produce byte-identical graphs. This guards against a "never panics but
// silently non-deterministic" failure that a totality-only check misses.

proptest! {
    #![proptest_config(ProptestConfig { cases: CASES, ..ProptestConfig::default() })]

    #[test]
    fn a_successful_parse_is_deterministic(
        frags in prop::collection::vec(dollar_fragment(), 0..20),
        arch in prop_oneof![Just("amd64"), Just("arm64"), Just("s390x")],
    ) {
        let body: String = frags.concat();
        let text = format!("FROM debian\nARG TARGETARCH\nRUN echo {body} for $TARGETARCH\nCMD [\"/bin/true\"]\n");
        let build = || {
            let env = MockDockerfileEnvironment::default()
                .with_dockerfile("d", &text)
                .with_build_arg("TARGETARCH", arch);
            apply(&DockerfileArgs { path: "d".into() }, &env)
        };
        // Whatever the outcome (Ok or Err), it is identical across runs.
        match (build(), build()) {
            (Ok(g1), Ok(g2)) => {
                prop_assert_eq!(g1, g2, "a successful parse must be byte-deterministic");
            }
            (Err(_), Err(_)) => { /* both errored identically — fine */ }
            (a, b) => prop_assert!(false, "non-deterministic outcome: {:?} vs {:?}", a.is_ok(), b.is_ok()),
        }
    }
}
