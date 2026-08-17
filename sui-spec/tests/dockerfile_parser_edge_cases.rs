//! Dockerfile parser edge-case coverage matching REAL vendor
//! Dockerfile shapes (multi-stage builds, `RUN --mount=type=bind`,
//! `ARG TARGETARCH` multi-arch, `${VAR}` interpolation, comments,
//! line-continuations, heredocs).
//!
//! Every case asserts one of exactly two outcomes against the SHIPPED
//! `sui_spec::dockerfile::apply`:
//!
//!   * a **correct typed graph** — the parsed [`DockerfileInstruction`]s
//!     have the exact fields the shape demands (a `--from=` reference is
//!     captured, a `--mount=type=bind` source/target is captured, a
//!     `${TARGETARCH}` is substituted, a comment is stripped), OR
//!   * a **clean typed [`SpecError::Interp`]** — for shapes deliberately
//!     out of this scoped parser's domain (heredocs, unsupported
//!     instructions), the error names the phase and offending token.
//!
//! Never a panic, never a silent WRONG graph. These tests do not rewrite
//! the parser; they pin its behavior at the exact edges a real vendor
//! image build exercises.

use sui_spec::dockerfile::{apply, DockerfileArgs, DockerfileGraph, DockerfileInstruction, InstructionKind, MockDockerfileEnvironment};
use sui_spec::SpecError;

/// Parse `text` through the shipped interpreter with the given build args.
fn parse(text: &str, build_args: &[(&str, &str)]) -> Result<DockerfileGraph, SpecError> {
    let mut env = MockDockerfileEnvironment::default().with_dockerfile("edge", text);
    for (k, v) in build_args {
        env = env.with_build_arg(k, v);
    }
    apply(&DockerfileArgs { path: "edge".into() }, &env)
}

/// The parsed instruction stream, kinds only — a compact shape assertion.
fn kinds(g: &DockerfileGraph) -> Vec<InstructionKind> {
    g.nodes.iter().map(|n| n.kind).collect()
}

// ── Multi-stage: FROM x AS build; FROM y; COPY --from=build ──────────

#[test]
fn multi_stage_from_as_build_and_copy_from_build_is_a_correct_typed_graph() {
    let text = "\
FROM golang:1.22 AS build
WORKDIR /src
COPY . .
RUN go build -o /out/gateway ./cmd/gateway
FROM debian:bookworm-slim
COPY --from=build /out/gateway /usr/local/bin/gateway
ENTRYPOINT [\"/usr/local/bin/gateway\"]
";
    let g = parse(text, &[]).expect("multi-stage must parse to a graph");
    assert_eq!(
        kinds(&g),
        vec![
            InstructionKind::From,
            InstructionKind::Workdir,
            InstructionKind::Copy,
            InstructionKind::Run,
            InstructionKind::From,
            InstructionKind::Copy,
            InstructionKind::Entrypoint,
        ]
    );

    // The build-stage FROM carries its `AS build` alias.
    match &g.nodes[0].instruction {
        DockerfileInstruction::From { image, stage_alias } => {
            assert_eq!(image, "golang:1.22");
            assert_eq!(stage_alias.as_deref(), Some("build"));
        }
        other => panic!("node 0 should be a FROM with an alias, got {other:?}"),
    }
    // The runtime-stage FROM has no alias.
    match &g.nodes[4].instruction {
        DockerfileInstruction::From { image, stage_alias } => {
            assert_eq!(image, "debian:bookworm-slim");
            assert_eq!(*stage_alias, None);
        }
        other => panic!("node 4 should be a bare FROM, got {other:?}"),
    }
    // The cross-stage COPY captures `--from=build`.
    match &g.nodes[5].instruction {
        DockerfileInstruction::Copy { sources, destination, from_stage } => {
            assert_eq!(from_stage.as_deref(), Some("build"), "COPY --from must be captured");
            assert_eq!(sources, &vec!["/out/gateway".to_string()]);
            assert_eq!(destination, "/usr/local/bin/gateway");
        }
        other => panic!("node 5 should be a COPY --from, got {other:?}"),
    }
}

#[test]
fn from_as_alias_is_case_insensitive_on_the_as_keyword() {
    // Real Dockerfiles use both `AS` and `as`.
    let g = parse("FROM alpine as builder\nFROM scratch\nCOPY --from=builder /a /b\n", &[]).expect("parses");
    match &g.nodes[0].instruction {
        DockerfileInstruction::From { stage_alias, .. } => assert_eq!(stage_alias.as_deref(), Some("builder")),
        other => panic!("expected FROM alias, got {other:?}"),
    }
}

// ── RUN --mount=type=bind ────────────────────────────────────────────

#[test]
fn run_mount_type_bind_captures_source_and_target() {
    let text = "\
FROM debian
RUN --mount=type=bind,source=.,target=/build echo building in /build
";
    let g = parse(text, &[]).expect("mount bind must parse");
    match &g.nodes[1].instruction {
        DockerfileInstruction::Run { command, mount_bind_source, mount_bind_target } => {
            assert_eq!(mount_bind_source.as_deref(), Some("."));
            assert_eq!(mount_bind_target.as_deref(), Some("/build"));
            assert!(command.contains("echo building"), "command body preserved: {command}");
        }
        other => panic!("expected a RUN --mount, got {other:?}"),
    }
}

#[test]
fn run_mount_type_cache_without_bind_source_target_is_still_a_run() {
    // `--mount=type=cache,target=/root/.cache` (no source) — the parser
    // captures target, leaves source None, and still yields a RUN.
    let g = parse("FROM debian\nRUN --mount=type=cache,target=/root/.cache go build\n", &[]).expect("parses");
    match &g.nodes[1].instruction {
        DockerfileInstruction::Run { mount_bind_source, mount_bind_target, command } => {
            assert_eq!(*mount_bind_source, None);
            assert_eq!(mount_bind_target.as_deref(), Some("/root/.cache"));
            assert!(command.contains("go build"));
        }
        other => panic!("expected RUN, got {other:?}"),
    }
}

// ── ARG TARGETARCH + multi-arch ──────────────────────────────────────

#[test]
fn arg_targetarch_substitutes_the_build_arg_value_into_a_downstream_run() {
    let text = "\
FROM debian
ARG TARGETARCH
RUN echo building for $TARGETARCH
";
    let amd = parse(text, &[("TARGETARCH", "amd64")]).expect("amd64 parses");
    let arm = parse(text, &[("TARGETARCH", "arm64")]).expect("arm64 parses");

    match &amd.nodes[2].instruction {
        DockerfileInstruction::Run { command, .. } => assert!(command.contains("amd64"), "amd64 substituted: {command}"),
        other => panic!("expected RUN, got {other:?}"),
    }
    match &arm.nodes[2].instruction {
        DockerfileInstruction::Run { command, .. } => assert!(command.contains("arm64"), "arm64 substituted: {command}"),
        other => panic!("expected RUN, got {other:?}"),
    }
    // Multi-arch: the two architectures produce different downstream
    // content hashes (correct cache-invalidation), never a silent
    // collapse to one graph.
    assert_ne!(
        amd.nodes[2].content_hash, arm.nodes[2].content_hash,
        "different TARGETARCH must diverge the downstream hash"
    );
}

#[test]
fn arg_with_default_value_resolves_without_a_passed_build_arg() {
    let text = "FROM debian\nARG FIPS=false\nRUN echo fips is $FIPS\n";
    let g = parse(text, &[]).expect("ARG default resolves");
    match &g.nodes[2].instruction {
        DockerfileInstruction::Run { command, .. } => assert!(command.contains("false"), "default substituted: {command}"),
        other => panic!("expected RUN, got {other:?}"),
    }
}

#[test]
fn a_passed_build_arg_overrides_the_arg_default() {
    let text = "FROM debian\nARG FIPS=false\nRUN echo fips is $FIPS\n";
    let g = parse(text, &[("FIPS", "true")]).expect("override resolves");
    match &g.nodes[2].instruction {
        DockerfileInstruction::Run { command, .. } => assert!(command.contains("true"), "override wins: {command}"),
        other => panic!("expected RUN, got {other:?}"),
    }
}

// ── build-arg interpolation: ${VAR} form ─────────────────────────────

#[test]
fn brace_form_variable_interpolation_substitutes_the_same_as_bare_form() {
    let brace = parse("FROM debian\nARG V=x\nRUN echo ${V}\n", &[]).expect("brace parses");
    let bare = parse("FROM debian\nARG V=x\nRUN echo $V\n", &[]).expect("bare parses");
    let brace_cmd = match &brace.nodes[2].instruction {
        DockerfileInstruction::Run { command, .. } => command.clone(),
        other => panic!("expected RUN, got {other:?}"),
    };
    let bare_cmd = match &bare.nodes[2].instruction {
        DockerfileInstruction::Run { command, .. } => command.clone(),
        other => panic!("expected RUN, got {other:?}"),
    };
    assert!(brace_cmd.contains('x') && !brace_cmd.contains("${V}"), "brace form substituted: {brace_cmd}");
    assert_eq!(brace_cmd, bare_cmd, "${{V}} and $V resolve identically");
}

#[test]
fn an_unresolved_arg_reference_is_a_clean_typed_error_not_a_panic() {
    // `$NEVER` is never declared as an ARG and never passed — a real
    // author mistake. The parser rejects it with a typed
    // unresolved-arg-reference error naming the variable.
    let err = parse("FROM debian\nRUN echo $NEVER_DECLARED\n", &[]).expect_err("unresolved arg must error");
    match err {
        SpecError::Interp { phase, message } => {
            assert_eq!(phase, "unresolved-arg-reference");
            assert!(message.contains("NEVER_DECLARED"), "error names the variable: {message}");
        }
        other => panic!("expected a typed Interp error, got {other:?}"),
    }
}

#[test]
fn env_self_referencing_path_prepend_is_left_literal_not_an_arg_error() {
    // The real vendor base image shape: prepend to $PATH. $PATH is a
    // well-known env var (never a build ARG) so it is left literal, NOT
    // treated as an unresolved ARG reference.
    let g = parse(
        "FROM ubuntu:24.04\nENV PATH=\"/opt/venv/bin:/opt/app/bin:${PATH}\"\n",
        &[],
    )
    .expect("PATH prepend must not be an ARG error");
    match &g.nodes[1].instruction {
        DockerfileInstruction::Env { name, value } => {
            assert_eq!(name, "PATH");
            assert!(value.contains("${PATH}"), "self-PATH left literal: {value}");
        }
        other => panic!("expected ENV, got {other:?}"),
    }
}

// ── comments ─────────────────────────────────────────────────────────

#[test]
fn comments_and_blank_lines_are_stripped_and_do_not_appear_as_nodes() {
    let text = "\
# syntax=docker/dockerfile:1
# Build the gateway
FROM debian:bookworm-slim

# install deps
RUN apt-get update

# done
CMD [\"/bin/true\"]
";
    let g = parse(text, &[]).expect("comments must be stripped");
    assert_eq!(kinds(&g), vec![InstructionKind::From, InstructionKind::Run, InstructionKind::Cmd], "only 3 real instructions");
}

#[test]
fn comment_only_dockerfile_yields_an_empty_graph_not_a_panic() {
    let g = parse("# just a comment\n# and another\n", &[]).expect("comment-only parses");
    assert!(g.nodes.is_empty(), "no instructions -> empty graph");
    assert_eq!(g.root_hash(), None);
}

// ── line-continuations (\) ───────────────────────────────────────────

#[test]
fn backslash_line_continuation_joins_a_wrapped_run_into_one_node() {
    let text = "\
FROM debian
RUN apt-get update && \\
    apt-get install -y \\
    ca-certificates curl
CMD [\"/bin/true\"]
";
    let g = parse(text, &[]).expect("continuation must join");
    // FROM, one joined RUN, CMD — the wrapped RUN is a single node.
    assert_eq!(kinds(&g), vec![InstructionKind::From, InstructionKind::Run, InstructionKind::Cmd]);
    match &g.nodes[1].instruction {
        DockerfileInstruction::Run { command, .. } => {
            assert!(command.contains("apt-get update"), "start preserved: {command}");
            assert!(command.contains("ca-certificates curl"), "wrapped tail preserved: {command}");
        }
        other => panic!("expected one joined RUN, got {other:?}"),
    }
}

#[test]
fn continuation_of_a_run_mount_flag_across_lines_still_captures_the_mount() {
    // A real shape: the `--mount` flag and the command wrap.
    let text = "\
FROM debian
RUN --mount=type=bind,source=.,target=/build \\
    echo built
";
    let g = parse(text, &[]).expect("wrapped mount parses");
    match &g.nodes[1].instruction {
        DockerfileInstruction::Run { mount_bind_source, mount_bind_target, command } => {
            assert_eq!(mount_bind_source.as_deref(), Some("."));
            assert_eq!(mount_bind_target.as_deref(), Some("/build"));
            assert!(command.contains("echo built"), "wrapped command body: {command}");
        }
        other => panic!("expected RUN --mount, got {other:?}"),
    }
}

// ── heredocs (explicitly out of scope — must be a CLEAN typed error) ──

#[test]
fn a_heredoc_run_body_is_a_clean_typed_error_never_a_panic_or_wrong_graph() {
    // BuildKit heredoc syntax is documented OUT OF SCOPE for this parser.
    // The interior shell lines don't start with an instruction keyword,
    // so the parser must reject one of them with a typed parse-line
    // error — never a panic, never a silently-accepted wrong graph.
    let text = "\
FROM debian
RUN <<EOF
apt-get update
apt-get install -y curl
EOF
";
    let err = parse(text, &[]).expect_err("heredoc interior must be a typed error, not a wrong graph");
    match err {
        SpecError::Interp { phase, .. } => assert_eq!(phase, "parse-line", "heredoc interior -> parse-line error"),
        other => panic!("expected a typed parse-line error, got {other:?}"),
    }
}

#[test]
fn a_copy_heredoc_is_a_clean_typed_error() {
    // `COPY <<EOF ... EOF` — same out-of-scope class. The `<<EOF` token
    // is treated as a COPY body and the interior lines fail typed.
    let text = "\
FROM debian
COPY <<EOF /etc/config
key=value
EOF
";
    let err = parse(text, &[]).expect_err("copy-heredoc must be a typed error");
    assert!(matches!(err, SpecError::Interp { .. }), "must be a typed Interp error: {err:?}");
}

// ── unsupported instructions (out of scope — CLEAN typed error) ──────

#[test]
fn healthcheck_is_a_clean_typed_unsupported_instruction_error() {
    let err = parse("FROM debian\nHEALTHCHECK CMD curl -f http://localhost/ || exit 1\n", &[])
        .expect_err("HEALTHCHECK is out of scope");
    match err {
        SpecError::Interp { phase, message } => {
            assert_eq!(phase, "parse-line");
            assert!(message.contains("HEALTHCHECK"), "error names the instruction: {message}");
        }
        other => panic!("expected parse-line error, got {other:?}"),
    }
}

#[test]
fn label_and_user_and_expose_are_typed_errors_not_silently_dropped() {
    // These are common real instructions this scoped parser does NOT
    // model — each must be a NAMED typed error, never silently skipped
    // (silent-skip would produce a wrong graph).
    for instr in ["LABEL maintainer=example", "USER nonroot", "EXPOSE 8080", "SHELL [\"/bin/bash\", \"-c\"]", "STOPSIGNAL SIGTERM"] {
        let text = format!("FROM debian\n{instr}\n");
        let err = parse(&text, &[]).expect_err("unsupported instruction must be a typed error");
        match err {
            SpecError::Interp { phase, .. } => assert_eq!(phase, "parse-line", "{instr} -> parse-line"),
            other => panic!("{instr}: expected parse-line error, got {other:?}"),
        }
    }
}

// ── malformed-but-in-scope instructions (CLEAN typed error) ──────────

#[test]
fn copy_with_only_one_token_is_a_clean_typed_error() {
    let err = parse("FROM debian\nCOPY onlyonetoken\n", &[]).expect_err("COPY needs src + dest");
    match err {
        SpecError::Interp { phase, message } => {
            assert_eq!(phase, "parse-line");
            assert!(message.contains("COPY"), "error names COPY: {message}");
        }
        other => panic!("expected parse-line error, got {other:?}"),
    }
}

#[test]
fn env_without_equals_is_a_clean_typed_error() {
    let err = parse("FROM debian\nENV JUSTANAME\n", &[]).expect_err("ENV needs NAME=value");
    assert!(matches!(err, SpecError::Interp { .. }), "typed error: {err:?}");
}

#[test]
fn arg_with_no_name_is_a_clean_typed_error() {
    let err = parse("FROM debian\nARG \n", &[]).expect_err("ARG needs a name");
    assert!(matches!(err, SpecError::Interp { .. }), "typed error: {err:?}");
}

#[test]
fn from_with_no_image_is_a_clean_typed_error() {
    let err = parse("FROM \n", &[]).expect_err("FROM needs an image");
    assert!(matches!(err, SpecError::Interp { .. }), "typed error: {err:?}");
}

#[test]
fn empty_volume_is_a_clean_typed_error() {
    let err = parse("FROM debian\nVOLUME\n", &[]).expect_err("VOLUME needs a path");
    assert!(matches!(err, SpecError::Interp { .. }), "typed error: {err:?}");
}

// ── a realistic composite vendor-shaped Dockerfile ───────────────────

#[test]
fn a_composite_multi_stage_arch_gated_dockerfile_parses_to_the_expected_shape() {
    // Exercises multi-stage + AS-alias + ARG TARGETARCH multi-arch +
    // --mount=type=bind + ${VAR} interpolation + comments + line
    // continuation + COPY --from, all in one file — the shape a real
    // vendor gateway image build hits.
    let text = "\
# syntax=docker/dockerfile:1
FROM golang:1.22 AS build
ARG TARGETARCH
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/go/pkg/mod \\
    GOARCH=${TARGETARCH} go build -o /out/gateway ./cmd/gateway

# runtime stage
FROM gcr.io/distroless/base-debian12
ENV GATEWAY_HOME=/opt/gateway
COPY --from=build /out/gateway /opt/gateway/gateway
ENTRYPOINT [\"/opt/gateway/gateway\"]
CMD [\"--config\", \"/opt/gateway/config.yaml\"]
";
    let g = parse(text, &[("TARGETARCH", "arm64")]).expect("composite must parse");
    assert_eq!(
        kinds(&g),
        vec![
            InstructionKind::From,       // golang AS build
            InstructionKind::Arg,        // TARGETARCH
            InstructionKind::Workdir,
            InstructionKind::Copy,       // . .
            InstructionKind::Run,        // --mount=cache, wrapped, ${TARGETARCH}
            InstructionKind::From,       // distroless
            InstructionKind::Env,
            InstructionKind::Copy,       // --from=build
            InstructionKind::Entrypoint,
            InstructionKind::Cmd,
        ]
    );
    // The wrapped, arch-gated RUN substituted GOARCH=arm64.
    match &g.nodes[4].instruction {
        DockerfileInstruction::Run { command, mount_bind_target, .. } => {
            assert!(command.contains("arm64"), "TARGETARCH substituted into GOARCH: {command}");
            assert_eq!(mount_bind_target.as_deref(), Some("/go/pkg/mod"));
        }
        other => panic!("node 4 should be the arch RUN, got {other:?}"),
    }
    // The cross-stage COPY.
    match &g.nodes[7].instruction {
        DockerfileInstruction::Copy { from_stage, .. } => assert_eq!(from_stage.as_deref(), Some("build")),
        other => panic!("node 7 should be COPY --from, got {other:?}"),
    }
    // Determinism: re-parsing the same input reproduces byte-identical hashes.
    let g2 = parse(text, &[("TARGETARCH", "arm64")]).expect("re-parse");
    assert_eq!(g, g2, "composite parse is deterministic");
}
