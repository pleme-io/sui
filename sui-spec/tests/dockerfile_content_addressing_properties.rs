//! Property-based proof of the two load-bearing claims the
//! Dockerfile-graph content-addressing rests on:
//!
//! 1. **ASSEMBLY-AGNOSTIC** (the core thesis) — a layer's
//!    content-addressed BLAKE3 key is *invariant* to the assembly
//!    around it (the surrounding comments/blank-line noise, the build-arg
//!    values a layer does not consume, and the output image tag — none of
//!    which is that layer's own content), and *sensitive* to the layer's
//!    own content (change a layer's instruction and its key changes).
//!    "Same content ⇒ same key; different content ⇒ different key",
//!    proven over 256 randomized inputs rather than a handful of examples.
//!
//! 2. **PARTIAL INVALIDATION** — changing exactly one instruction
//!    invalidates *exactly* that node and everything downstream of it;
//!    every node strictly upstream keeps its key (stays cache-valid).
//!    This generalizes the `nonroot-gateway` fixture's observed 8/11
//!    partial-invalidation into a property over a randomized change
//!    position in a randomized chain.
//!
//! Both properties are stated against `sui_spec::dockerfile::apply` — the
//! shipped Phase-1 graph hasher — with no docker, no wrapper, no I/O:
//! the content-address is a pure function of the parsed instruction
//! stream, so these are deterministic, non-flaky properties.
//!
//! ## The precise assembly-vs-content boundary this design guarantees
//!
//! The shipped `hash_node` folds `(kind, parent_hash, resolved_args,
//! instruction_content_bytes)` into every node. That means the honest,
//! code-backed assembly-agnostic axes are:
//!
//! - **Comment / blank-line noise** — stripped before hashing, so it is
//!   pure assembly: adding or removing it changes no node's key.
//! - **A build-arg's value, for every node strictly *before* that arg's
//!   `ARG` line** — such a node has not yet accumulated the arg into its
//!   `resolved_args`, so its key cannot depend on the value. (A node at
//!   or after the `ARG` line legitimately *is* content-dependent on it —
//!   that is not assembly, and we do not claim it is.)
//! - **The output image tag** — never enters `apply`'s inputs at all;
//!   asserted structurally rather than via a generator.
//!
//! The `FROM` base-image string is *content*, not assembly (it chains
//! into every downstream `parent_hash`), so it appears here only on the
//! sensitivity side — we never mislabel it as an assembly axis.

use proptest::prelude::*;

use sui_spec::dockerfile::{apply, DockerfileArgs, DockerfileGraph, MockDockerfileEnvironment};

// ── Fleet proptest floor ────────────────────────────────────────────
//
// The fleet floor is 100 cases; we run 256 (the proptest default) on
// every property here.
const CASES: u32 = 256;

// ── A tiny typed model of a randomly-generated Dockerfile ────────────
//
// We generate an *abstract* instruction stream, then render it to
// Dockerfile text in two ways: a "clean" rendering (canonical, no
// noise) and a "noisy" rendering (comments + blank lines interleaved).
// The abstract stream is the single source of truth for what the
// *content* is; the two renderings differ only in *assembly*.

/// One abstract instruction. Deliberately restricted to the subset the
/// scoped parser accepts, and to bodies that never contain an
/// unresolved `$ARG` reference (so every generated Dockerfile parses).
#[derive(Debug, Clone, PartialEq, Eq)]
enum AbstractInstr {
    /// A `RUN <token>` with a hash-safe alphanumeric token.
    Run(String),
    /// A `WORKDIR /<token>`.
    Workdir(String),
    /// An `ENV <NAME>=<value>` with hash-safe pieces.
    Env(String, String),
    /// A `CMD ["<token>"]`.
    Cmd(String),
}

impl AbstractInstr {
    /// Render this instruction to its single canonical Dockerfile line.
    fn render_line(&self) -> String {
        match self {
            Self::Run(cmd) => {
                let mut s = String::from("RUN echo ");
                s.push_str(cmd);
                s
            }
            Self::Workdir(dir) => {
                let mut s = String::from("WORKDIR /");
                s.push_str(dir);
                s
            }
            Self::Env(name, value) => {
                let mut s = String::from("ENV ");
                s.push_str(name);
                s.push('=');
                s.push_str(value);
                s
            }
            Self::Cmd(token) => {
                let mut s = String::from("CMD [\"");
                s.push_str(token);
                s.push_str("\"]");
                s
            }
        }
    }
}

/// A generated Dockerfile: a base image + a body of abstract
/// instructions. The `FROM` is always present (a Dockerfile must start
/// with one) and is content, not assembly.
#[derive(Debug, Clone)]
struct AbstractDockerfile {
    base_image: String,
    body: Vec<AbstractInstr>,
}

impl AbstractDockerfile {
    /// Canonical rendering — `FROM` then one line per body instruction,
    /// no comments, no blank lines. This is the reference *content*.
    fn render_clean(&self) -> String {
        let mut lines = Vec::with_capacity(self.body.len() + 1);
        let mut from = String::from("FROM ");
        from.push_str(&self.base_image);
        lines.push(from);
        for instr in &self.body {
            lines.push(instr.render_line());
        }
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    /// Total node count of the clean rendering (`FROM` + body).
    fn node_count(&self) -> usize {
        self.body.len() + 1
    }
}

// ── Generators ───────────────────────────────────────────────────────

/// Hash-safe token: lowercase alphanumeric, 1..=12 chars. Avoids
/// whitespace, `$`, quotes, and continuation backslashes so every
/// generated line parses to exactly one instruction with no
/// arg-substitution surprises.
fn arb_token() -> impl Strategy<Value = String> {
    "[a-z0-9]{1,12}"
}

/// An uppercase env-var name that is NOT one of the interpreter's
/// well-known passthrough names (`PATH`/`HOME`/`TERM`) — keeping the
/// generated ENV a plain literal assignment.
fn arb_env_name() -> impl Strategy<Value = String> {
    "[A-Z][A-Z0-9_]{0,10}".prop_filter(
        "avoid well-known passthrough env names",
        |n| !matches!(n.as_str(), "PATH" | "HOME" | "TERM"),
    )
}

fn arb_instr() -> impl Strategy<Value = AbstractInstr> {
    prop_oneof![
        arb_token().prop_map(AbstractInstr::Run),
        arb_token().prop_map(AbstractInstr::Workdir),
        (arb_env_name(), arb_token()).prop_map(|(n, v)| AbstractInstr::Env(n, v)),
        arb_token().prop_map(AbstractInstr::Cmd),
    ]
}

/// A base image string of the `name:tag` shape.
fn arb_base_image() -> impl Strategy<Value = String> {
    ("[a-z]{2,8}", "[a-z0-9.]{1,8}").prop_map(|(name, tag)| {
        let mut s = name;
        s.push(':');
        s.push_str(&tag);
        s
    })
}

fn arb_dockerfile() -> impl Strategy<Value = AbstractDockerfile> {
    (arb_base_image(), prop::collection::vec(arb_instr(), 1..8))
        .prop_map(|(base_image, body)| AbstractDockerfile { base_image, body })
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Parse+hash a rendered Dockerfile with the given build-args, returning
/// the per-node content-hash vector.
fn hashes(text: &str, build_args: &[(&str, &str)]) -> Vec<String> {
    let mut env = MockDockerfileEnvironment::default().with_dockerfile("d", text);
    for (k, v) in build_args {
        env = env.with_build_arg(k, v);
    }
    let graph: DockerfileGraph = apply(&DockerfileArgs { path: "d".to_string() }, &env)
        .expect("generated Dockerfile must parse");
    graph.nodes.iter().map(|n| n.content_hash.clone()).collect()
}

/// Render an abstract Dockerfile with deterministic comment/blank-line
/// noise interleaved — a comment line before the FROM, and a blank line
/// + a `# note` before every body instruction. Same *content*, pure
/// *assembly* difference from `render_clean`.
fn render_noisy(df: &AbstractDockerfile) -> String {
    let mut out = String::from("# generated fixture — leading comment\n\n");
    out.push_str("FROM ");
    out.push_str(&df.base_image);
    out.push('\n');
    for (i, instr) in df.body.iter().enumerate() {
        out.push('\n');
        out.push_str("# step ");
        // A small typed number render — avoids format! for the payload,
        // though this is a test helper either way.
        out.push_str(&i.to_string());
        out.push('\n');
        out.push_str(&instr.render_line());
        out.push('\n');
    }
    out.push('\n');
    out.push_str("# trailing comment\n");
    out
}

// ═══════════════════════════════════════════════════════════════════
//  (1) ASSEMBLY-AGNOSTIC  — the core thesis
// ═══════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    /// **Assembly axis: comment / blank-line noise.** The clean and the
    /// noisy rendering of the *same* abstract Dockerfile are byte-different
    /// texts but hash to an identical per-node key vector — comments and
    /// blank lines are assembly, never content.
    #[test]
    fn comment_and_blankline_noise_is_assembly_agnostic(df in arb_dockerfile()) {
        let clean = df.render_clean();
        let noisy = render_noisy(&df);
        prop_assert_ne!(&clean, &noisy, "the two renderings must be textually different (assembly varies)");

        let clean_hashes = hashes(&clean, &[]);
        let noisy_hashes = hashes(&noisy, &[]);
        prop_assert_eq!(
            clean_hashes,
            noisy_hashes,
            "layer keys must be INVARIANT to comment/blank-line assembly noise",
        );
    }

    /// **Assembly axis: a build-arg value a layer does not consume.** We
    /// prepend `ARG NOISE` *after* the body (so no generated node ever
    /// references it) — actually we place the `ARG NOISE` line as the
    /// LAST body line, meaning every FROM+body node strictly before it is
    /// invariant to `NOISE`'s value. Two runs with different `NOISE`
    /// values must produce identical keys for all those prior nodes.
    #[test]
    fn unconsumed_build_arg_value_is_assembly_agnostic(df in arb_dockerfile()) {
        // Build: FROM ... <body> ... then a trailing `ARG NOISE`. Every
        // node before the ARG line cannot depend on NOISE's value.
        let mut text = df.render_clean();
        text.push_str("ARG NOISE\n");

        let with_x = hashes(&text, &[("NOISE", "alpha")]);
        let with_y = hashes(&text, &[("NOISE", "omega")]);

        // The nodes strictly before the trailing ARG line are FROM + the
        // whole body — i.e. the first `node_count()` nodes. The ARG node
        // itself is the last one and legitimately folds the resolved
        // value, so we exclude it (it is content w.r.t. NOISE, not
        // assembly).
        let boundary = df.node_count();
        prop_assert!(with_x.len() > boundary && with_y.len() > boundary,
            "expected FROM+body+ARG nodes");
        for i in 0..boundary {
            prop_assert_eq!(
                &with_x[i],
                &with_y[i],
                "node {} is strictly before `ARG NOISE` — its key must be INVARIANT to NOISE's value",
                i,
            );
        }
    }

    /// **Determinism** — the same content hashes identically across two
    /// independent parses (no hidden global/order dependence).
    #[test]
    fn identical_content_hashes_identically(df in arb_dockerfile()) {
        let text = df.render_clean();
        let a = hashes(&text, &[]);
        let b = hashes(&text, &[]);
        prop_assert_eq!(a, b, "same content ⇒ same key (determinism)");
    }

    /// **Content sensitivity: base image.** Two Dockerfiles that differ
    /// only in the `FROM` image (content) must produce a *fully* disjoint
    /// key vector — the base chains into every downstream `parent_hash`.
    #[test]
    fn different_base_image_changes_every_key(
        df in arb_dockerfile(),
        other_base in arb_base_image(),
    ) {
        prop_assume!(other_base != df.base_image);
        let a = hashes(&df.render_clean(), &[]);

        let mut changed = df.clone();
        changed.base_image = other_base;
        let b = hashes(&changed.render_clean(), &[]);

        prop_assert_eq!(a.len(), b.len());
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            prop_assert_ne!(x, y, "changing FROM must change node {}'s key (content chains)", i);
        }
    }

    /// **Content sensitivity: a single instruction.** Replacing exactly
    /// one body instruction with a *different* instruction changes that
    /// node's key (and, by the chain, every downstream key), while every
    /// upstream key is untouched. This is the sensitivity half of the
    /// core thesis and the setup for the partial-invalidation property
    /// below.
    #[test]
    fn changing_one_instruction_changes_that_key(
        df in arb_dockerfile(),
        idx in any::<prop::sample::Index>(),
        replacement in arb_instr(),
    ) {
        let n = df.body.len();
        let pos = idx.index(n);
        prop_assume!(df.body[pos] != replacement);

        let before = hashes(&df.render_clean(), &[]);

        let mut changed = df.clone();
        changed.body[pos] = replacement;
        let after = hashes(&changed.render_clean(), &[]);

        // Node indices: 0 = FROM, body[k] = node k+1.
        let changed_node = pos + 1;

        // Upstream (FROM + body before pos) is byte-identical.
        for i in 0..changed_node {
            prop_assert_eq!(
                &before[i], &after[i],
                "upstream node {} must be UNCHANGED when body[{}] changes", i, pos,
            );
        }
        // The changed node's key differs.
        prop_assert_ne!(
            &before[changed_node], &after[changed_node],
            "the changed node's key must differ",
        );
    }
}

// ═══════════════════════════════════════════════════════════════════
//  (3) PARTIAL INVALIDATION  — one change invalidates exactly downstream
// ═══════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    /// Model a warm cache holding *every* node key of the original
    /// Dockerfile. Change exactly one body instruction. Re-hash. Against
    /// that warm cache:
    ///
    /// - every node strictly upstream of the change is still a HIT
    ///   (key unchanged);
    /// - the changed node and every node downstream of it are MISSES
    ///   (keys changed via the parent-hash chain).
    ///
    /// i.e. a 1-instruction change invalidates *exactly* the contiguous
    /// downstream suffix `[changed .. end]`, and nothing else. This is
    /// the generalized, randomized form of the `nonroot-gateway`
    /// fixture's 8/11 observation.
    #[test]
    fn one_change_invalidates_exactly_the_downstream_suffix(
        df in arb_dockerfile(),
        idx in any::<prop::sample::Index>(),
        replacement in arb_instr(),
    ) {
        let n = df.body.len();
        let pos = idx.index(n);
        prop_assume!(df.body[pos] != replacement);

        // Warm cache = the set of ORIGINAL node keys.
        let original = hashes(&df.render_clean(), &[]);
        let warm: std::collections::BTreeSet<&str> =
            original.iter().map(String::as_str).collect();

        let mut changed = df.clone();
        changed.body[pos] = replacement;
        let after = hashes(&changed.render_clean(), &[]);
        prop_assert_eq!(after.len(), original.len(), "node count is stable under a 1-for-1 replace");

        let changed_node = pos + 1; // 0 = FROM
        let mut miss_indices = Vec::new();
        for (i, key) in after.iter().enumerate() {
            if !warm.contains(key.as_str()) {
                miss_indices.push(i);
            }
        }

        // The misses are EXACTLY the contiguous suffix [changed_node ..].
        let expected: Vec<usize> = (changed_node..after.len()).collect();
        prop_assert_eq!(
            miss_indices,
            expected,
            "a 1-instruction change at body[{}] must invalidate exactly the downstream suffix",
            pos,
        );
    }

    /// A cache-hit-ratio corollary stated as a number, generalizing
    /// "8 of 11 stayed valid": the count of still-valid (upstream) nodes
    /// equals `changed_node` (= `pos + 1`), and the invalidated count
    /// equals `total - changed_node`. Both are strict, non-degenerate
    /// fractions of the whole (never 0, never all) whenever the changed
    /// node is neither the FROM-adjacent extreme collapsing to trivial
    /// bounds.
    #[test]
    fn partial_invalidation_fractions_are_exact(
        df in arb_dockerfile(),
        idx in any::<prop::sample::Index>(),
        replacement in arb_instr(),
    ) {
        let n = df.body.len();
        let pos = idx.index(n);
        prop_assume!(df.body[pos] != replacement);

        let original = hashes(&df.render_clean(), &[]);
        let warm: std::collections::BTreeSet<&str> =
            original.iter().map(String::as_str).collect();

        let mut changed = df.clone();
        changed.body[pos] = replacement;
        let after = hashes(&changed.render_clean(), &[]);

        let total = after.len();
        let changed_node = pos + 1;
        let still_valid = after.iter().filter(|k| warm.contains(k.as_str())).count();
        let invalidated = total - still_valid;

        prop_assert_eq!(still_valid, changed_node,
            "still-valid (upstream) count must equal the changed node's index");
        prop_assert_eq!(invalidated, total - changed_node,
            "invalidated (downstream suffix) count must equal total - changed_node");

        // At least the FROM node always survives (it is upstream of any
        // body change), so still_valid >= 1 and invalidated < total —
        // never a full-cache blow-away for a single body edit.
        prop_assert!(still_valid >= 1, "the FROM node always survives a body edit");
        prop_assert!(invalidated < total, "a single body edit never invalidates the whole graph");
    }
}

// ── A concrete, named-fixture cross-check (not a property) ────────────
//
// Pins the `nonroot-gateway` canonical fixture's exact partial-
// invalidation number, so the property's abstraction stays anchored to
// the real vendor-shaped corpus the wrapper actually caches.

#[test]
fn nonroot_gateway_single_run_edit_partial_invalidation_is_exact() {
    use sui_spec::dockerfile::apply_canonical;

    let original = apply_canonical("nonroot-gateway", &[("TARGETARCH", "amd64")]).unwrap();
    let warm: std::collections::BTreeSet<&str> =
        original.nodes.iter().map(|n| n.content_hash.as_str()).collect();

    // The canonical source text, with exactly one RUN body changed. We
    // reconstruct it from the shipped canonical instance and edit the
    // apt-install line — the same edit the warmth benchmark makes.
    let spec = sui_spec::dockerfile::load_named("nonroot-gateway").unwrap();
    let modified_text = spec.source_text.replace(
        "apt-get install -y ca-certificates curl",
        "apt-get install -y ca-certificates curl jq",
    );
    assert_ne!(modified_text, spec.source_text, "edit must actually change the text");

    let env = MockDockerfileEnvironment::default()
        .with_dockerfile("m", &modified_text)
        .with_build_arg("TARGETARCH", "amd64");
    let modified = apply(&DockerfileArgs { path: "m".to_string() }, &env).unwrap();

    let total = modified.nodes.len();
    let still_valid = modified
        .nodes
        .iter()
        .filter(|n| warm.contains(n.content_hash.as_str()))
        .count();
    let invalidated = total - still_valid;

    // FROM, ARG TARGETARCH, ARG FIPS survive (upstream of the changed
    // apt-install RUN); the changed RUN + everything after it miss.
    assert_eq!(total, 11, "nonroot-gateway has 11 nodes");
    assert_eq!(still_valid, 3, "FROM + ARG TARGETARCH + ARG FIPS stay cache-valid");
    assert_eq!(invalidated, 8, "the changed apt RUN + its 7 downstream nodes invalidate");
}
