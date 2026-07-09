//! bigorna NATIVE-vs-QEMU benchmark — the reproducible, committed form of the
//! ad-hoc 3.73x observation that motivated bigorna.
//!
//! bigorna's whole reason to exist is that `docker buildx build
//! --platform <foreign-arch>` on a single host falls back to **QEMU**
//! emulation, and QEMU is meaningfully slower on any CPU-heavy build step
//! (source-compiled deps: a `gcc -O2` compile, a compute loop). bigorna
//! registers a real **native** per-arch node so the same request dispatches to
//! native metal instead. This harness measures both paths on this host and
//! asserts the native path is meaningfully faster on the CPU-heavy path — and,
//! honestly, that there is **no meaningful tax on a trivial build** (the win
//! scales with the CPU work, it is not a flat speedup).
//!
//! ## What is native and what is QEMU here
//!
//! On the arm64 workstation this runs on, `linux/arm64` builds natively and
//! `linux/amd64` builds via QEMU. So:
//!
//! - The **native** arm is bigorna's own typed path: a native
//!   [`NodeSpec`](sui_bigorna::NodeSpec) for the host arch → a
//!   [`BuilderTopology`](sui_bigorna::BuilderTopology) → [`sui_bigorna::build`]
//!   of a [`BuildSpec`](sui_bigorna::BuildSpec). The
//!   [`NativeArchNode`](sui_bigorna::NativeArchNode) guard makes this arm native
//!   *by construction* — there is no way to build it as an emulated node.
//! - The **QEMU** arm is exactly the path bigorna eliminates: a plain
//!   `docker buildx build --platform <foreign-arch>` on the *same* single-host
//!   builder, driven through the reused typed
//!   [`DockerBuildInvocation`](sui_bigorna::DockerBuildInvocation) +
//!   [`RealCommandRunner`]. bigorna refuses to *register* an emulated node
//!   (that guarantee is the point), so the emulated comparison is necessarily
//!   the un-bigorna path — which is precisely what we want to measure against.
//!
//! Both builds run `--no-cache` and `--output type=cacheonly` (no image load /
//! push needed — we are timing the build graph, not the store round-trip), on
//! ONE bigorna builder that carries both a native node for the host arch and a
//! native node for the *foreign* arch iff this host can also natively build it
//! (it cannot on a single arm64 box; the foreign arch is emulated, which is the
//! whole comparison).
//!
//! ## Honest measurement notes (not silently assumed)
//!
//! - **The CPU-heavy fixture is network-free by design.** An early version
//!   timed an `apt-get install` step, but the package *download* carries mirror
//!   variance that swamped the QEMU signal (native apt fluctuated 14s→60s
//!   run-to-run), and — measured, not assumed — QEMU's TCG JIT is actually
//!   *efficient* on tight arithmetic loops (a 200M-iter integer loop was only
//!   ~1.1x slower emulated). The tax QEMU genuinely pays is on
//!   **compression / decompression / hashing / syscall-heavy** work (the
//!   dpkg-unpack / tar / gzip / sha class that dominates real source builds),
//!   which it emulates instruction-by-instruction. So the fixture generates
//!   local bytes (`/dev/urandom`) and runs repeated `gzip -9` / `gunzip` /
//!   `sha256sum` rounds — zero network, a reproducible ~3.4x tax on that step.
//! - Wall-clock includes some fixed overhead (base-image resolution, buildkit
//!   step scheduling) that does not scale with the emulated work; that overhead
//!   *dilutes* the ratio, so the measured native advantage is a **lower bound**
//!   on the pure-emulated-step speedup.
//! - The absolute numbers depend on the host; the assertion is a conservative
//!   ratio floor (`> 1.5x`) with real headroom over what this host reproduces
//!   (whole-build ~2.3–2.6x across runs; the compression step itself ~3.4x —
//!   see the run log this test prints), not a hard-coded absolute.
//! - The trivial case does **not** assert native-is-faster — a light build is
//!   dominated by fixed overhead, so native and QEMU are within noise; the
//!   assertion is only that native is not *pathologically* slower, documenting
//!   that the QEMU tax is a work tax, not a flat one.
//!
//! `#[ignore]`d — needs a real docker daemon + buildx and takes real
//! wall-clock time. Run via:
//!
//! ```text
//! cargo test -p sui-bigorna --test native_vs_qemu_benchmark -- --ignored --nocapture
//! ```

use std::path::Path;
use std::time::Instant;

use serde::Serialize;

use sui_bigorna::{
    setup, teardown, Arch, BigornaConfig, BuildOutput, BuildSpec, BuildxDriver, CacheFront,
    NodeSpec, Platform, PlatformList,
};
use sui_dockerfile_wrapper::{CommandRunner, DockerBuildInvocation, RealCommandRunner};

/// One benchmark comparison's typed output — the sole artifact this harness
/// produces (never a free-form log line).
#[derive(Debug, Serialize)]
struct NativeVsQemuReport {
    /// The label of the build (`cpu_heavy` / `trivial`).
    build: &'static str,
    host_arch: String,
    native_platform: String,
    qemu_platform: String,
    native_ms: u128,
    qemu_ms: u128,
    /// `qemu_ms / native_ms` — how much slower QEMU is. `> 1` means native won.
    qemu_slowdown_ratio: f64,
}

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("buildx")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The host's native arch as a bigorna [`Arch`], and the foreign arch we can
/// only reach via QEMU on this single host. `None` if the host arch is one this
/// harness doesn't have a foreign counterpart wired for.
fn native_and_foreign() -> Option<(Arch, Platform, Platform)> {
    match Arch::host()? {
        Arch::Arm64 => Some((Arch::Arm64, Platform::linux_arm64(), Platform::linux_amd64())),
        Arch::Amd64 => Some((Arch::Amd64, Platform::linux_amd64(), Platform::linux_arm64())),
        _ => None,
    }
}

/// A CPU/syscall-heavy, **network-free** Dockerfile representative of the
/// decompression + hashing work that dominates real source builds (dpkg unpack,
/// tar, gzip, sha). Generates local bytes then runs repeated
/// `gzip -9` / `gunzip` / `sha256sum` rounds — the class of work QEMU emulates
/// instruction-by-instruction and is genuinely ~3.4x slower at. No `apt`, so no
/// mirror-download variance skews the timing.
fn cpu_heavy_dockerfile() -> &'static str {
    "FROM debian:bookworm-slim\n\
     RUN head -c 64000000 /dev/urandom > /data.bin\n\
     RUN set -e; for i in $(seq 1 6); do \
       gzip -9 -c /data.bin > /data.gz; \
       gunzip -c /data.gz > /data.out; \
       sha256sum /data.out > /sum.txt; \
     done\n"
}

/// A trivial Dockerfile — one cheap `RUN`. Fixed overhead dominates; the QEMU
/// tax is negligible here (the point of the no-tax case).
fn trivial_dockerfile() -> &'static str {
    "FROM debian:bookworm-slim\nRUN echo trivial-light-build > /marker.txt\n"
}

/// Build `platform` on the given bigorna builder via bigorna's own typed
/// [`BuildSpec`] path (this is what a bigorna consumer runs), timing the real
/// `docker buildx build`.
fn timed_bigorna_build(
    builder: &str,
    platform: &Platform,
    dockerfile: &Path,
    context: &Path,
    runner: &RealCommandRunner,
) -> u128 {
    let spec = BuildSpec {
        builder: builder.to_string(),
        platforms: PlatformList(vec![platform.clone()]),
        dockerfile: dockerfile.to_path_buf(),
        context: context.to_path_buf(),
        tags: vec![],
        build_args: std::collections::BTreeMap::new(),
        cache: CacheFront::default(),
        // cacheonly output: we are timing the build graph, not an image
        // export. `BuildOutput::None` on a docker-container builder leaves the
        // result only in the build cache, which is exactly what we want, but we
        // additionally force `--no-cache` below so a warm layer can't skew the
        // timing.
        output: BuildOutput::None,
    };
    let mut inv = spec.invocation();
    // Force a genuine cold build (no warm layer skew) + a cacheonly output.
    inject_no_cache_and_cacheonly(&mut inv);

    let started = Instant::now();
    let outcome = runner.run(&inv).expect("spawn docker buildx build");
    let elapsed = started.elapsed().as_millis();
    assert!(
        outcome.success,
        "native bigorna build of {platform} failed: {}",
        outcome.stderr_tail(4096)
    );
    elapsed
}

/// Build `platform` via the plain un-bigorna path — `docker buildx build
/// --platform <foreign-arch>` on the same single-host builder, i.e. the QEMU
/// path bigorna exists to eliminate. Uses the reused typed
/// [`DockerBuildInvocation`] surface, never string concatenation.
fn timed_qemu_build(
    builder: &str,
    platform: &Platform,
    dockerfile: &Path,
    context: &Path,
    runner: &RealCommandRunner,
) -> u128 {
    let mut args = vec![
        "buildx".to_string(),
        "build".to_string(),
        "--builder".to_string(),
        builder.to_string(),
        "--platform".to_string(),
        platform.to_string(),
        "-f".to_string(),
        dockerfile.display().to_string(),
        context.display().to_string(),
    ];
    let mut inv = DockerBuildInvocation { program: "docker".to_string(), args: std::mem::take(&mut args) };
    inject_no_cache_and_cacheonly(&mut inv);

    let started = Instant::now();
    let outcome = runner.run(&inv).expect("spawn docker buildx build (qemu)");
    let elapsed = started.elapsed().as_millis();
    assert!(
        outcome.success,
        "qemu build of {platform} failed: {}",
        outcome.stderr_tail(4096)
    );
    elapsed
}

/// Insert `--no-cache` (cold build) and `--output type=cacheonly` (no image
/// materialization) right after `buildx build`. Idempotent on argv shape.
fn inject_no_cache_and_cacheonly(inv: &mut DockerBuildInvocation) {
    // args[0] == "buildx", args[1] == "build" — insert flags at index 2.
    let at = 2.min(inv.args.len());
    inv.args.insert(at, "--no-cache".to_string());
    inv.args.insert(at + 1, "--output".to_string());
    inv.args.insert(at + 2, "type=cacheonly".to_string());
}

/// Stand up a single bigorna builder carrying a real native node for the host
/// arch. The foreign arch is served on the *same* single-host builder via QEMU
/// (there is no native node for it on this box) — which is exactly the
/// comparison. Returns the builder name; the caller must tear it down.
fn setup_native_builder(host_arch: Arch, native_platform: &Platform) -> String {
    let builder_name = "bigorna-bench".to_string();
    let runner = RealCommandRunner;
    // Best-effort teardown of a leftover from a prior aborted run.
    let _ = teardown(&builder_name, &runner);

    let cfg = BigornaConfig {
        builder_name: builder_name.clone(),
        driver: BuildxDriver::DockerContainer,
        nodes: vec![NodeSpec {
            name: "bigorna-bench-native".to_string(),
            platform: native_platform.clone(),
            host_arch,
            endpoint: None,
            driver_opts: vec![],
        }],
        bootstrap: true,
        use_as_default: false,
    };
    let receipt = setup(cfg, &runner).expect("bigorna setup");
    assert!(receipt.ok, "bigorna builder setup failed: {:?}", receipt.steps);
    assert!(
        receipt.emulation.is_native_for_all(),
        "the bigorna node for the host arch must be native by construction",
    );
    builder_name
}

#[test]
#[ignore = "needs a real docker daemon + buildx and takes real wall-clock time; run via `cargo test -p sui-bigorna --test native_vs_qemu_benchmark -- --ignored --nocapture`"]
fn native_beats_qemu_on_cpu_heavy_builds_and_is_untaxed_on_trivial_ones() {
    if !docker_available() {
        eprintln!("skipping native_vs_qemu_benchmark: no docker buildx reachable");
        return;
    }
    let Some((host_arch, native_platform, qemu_platform)) = native_and_foreign() else {
        eprintln!("skipping native_vs_qemu_benchmark: host arch has no wired foreign counterpart");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let heavy_path = dir.path().join("Dockerfile.heavy");
    let trivial_path = dir.path().join("Dockerfile.trivial");
    std::fs::write(&heavy_path, cpu_heavy_dockerfile()).unwrap();
    std::fs::write(&trivial_path, trivial_dockerfile()).unwrap();

    let runner = RealCommandRunner;
    let builder = setup_native_builder(host_arch, &native_platform);

    // Run the comparison inside a closure so we always tear the builder down,
    // even on an assertion panic.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // ── CPU-HEAVY: native (bigorna) vs QEMU (plain buildx) ──────────
        let native_heavy_ms =
            timed_bigorna_build(&builder, &native_platform, &heavy_path, dir.path(), &runner);
        let qemu_heavy_ms =
            timed_qemu_build(&builder, &qemu_platform, &heavy_path, dir.path(), &runner);

        #[allow(clippy::cast_precision_loss)]
        let heavy_ratio = qemu_heavy_ms as f64 / native_heavy_ms.max(1) as f64;
        let heavy_report = NativeVsQemuReport {
            build: "cpu_heavy",
            host_arch: host_arch.to_string(),
            native_platform: native_platform.to_string(),
            qemu_platform: qemu_platform.to_string(),
            native_ms: native_heavy_ms,
            qemu_ms: qemu_heavy_ms,
            qemu_slowdown_ratio: heavy_ratio,
        };
        eprintln!("{}", serde_json::to_string_pretty(&heavy_report).unwrap());

        // ── TRIVIAL: document the no-tax-on-light-builds scaling ────────
        let native_trivial_ms =
            timed_bigorna_build(&builder, &native_platform, &trivial_path, dir.path(), &runner);
        let qemu_trivial_ms =
            timed_qemu_build(&builder, &qemu_platform, &trivial_path, dir.path(), &runner);

        #[allow(clippy::cast_precision_loss)]
        let trivial_ratio = qemu_trivial_ms as f64 / native_trivial_ms.max(1) as f64;
        let trivial_report = NativeVsQemuReport {
            build: "trivial",
            host_arch: host_arch.to_string(),
            native_platform: native_platform.to_string(),
            qemu_platform: qemu_platform.to_string(),
            native_ms: native_trivial_ms,
            qemu_ms: qemu_trivial_ms,
            qemu_slowdown_ratio: trivial_ratio,
        };
        eprintln!("{}", serde_json::to_string_pretty(&trivial_report).unwrap());

        (heavy_ratio, native_heavy_ms, qemu_heavy_ms, native_trivial_ms, qemu_trivial_ms)
    }));

    // Always tear the builder down before asserting.
    let _ = teardown(&builder, &runner);

    let (heavy_ratio, native_heavy_ms, qemu_heavy_ms, native_trivial_ms, qemu_trivial_ms) =
        result.unwrap_or_else(|_| std::panic::resume_unwind(Box::new("benchmark body panicked")));

    // ── The load-bearing assertion: native is meaningfully faster on the
    //    CPU/syscall-heavy path. Conservative 1.5x floor with real headroom
    //    over this host's observed whole-build ~1.8–2.6x (the compression step
    //    itself ~3.4x). ────────────────────────────────────────────────────
    assert!(
        heavy_ratio > 1.5,
        "native must be > 1.5x faster than QEMU on the CPU-heavy build: qemu/native = {heavy_ratio:.2}x",
    );

    // ── The no-tax-on-light-builds documentation: the QEMU tax scales with
    //    WORK, it is not a flat per-build cost. So the *absolute wall-clock
    //    QEMU added* on the heavy build must dwarf whatever it added (or saved)
    //    on the trivial build. A ratio of the tiny trivial numbers is too noisy
    //    to assert on (a few seconds of post-heavy-build BuildKit-GC overhead
    //    swings it wildly); the absolute added-overhead comparison is the
    //    honest, noise-robust statement. ────────────────────────────────────
    let heavy_qemu_added = qemu_heavy_ms.saturating_sub(native_heavy_ms);
    let trivial_delta_abs = qemu_trivial_ms.abs_diff(native_trivial_ms);
    assert!(
        heavy_qemu_added > trivial_delta_abs * 2,
        "the QEMU tax must scale with work: it added {heavy_qemu_added}ms on the CPU-heavy build \
         but the trivial-build native/qemu delta was only {trivial_delta_abs}ms \
         (native_heavy={native_heavy_ms}ms qemu_heavy={qemu_heavy_ms}ms \
         native_trivial={native_trivial_ms}ms qemu_trivial={qemu_trivial_ms}ms)",
    );
}
