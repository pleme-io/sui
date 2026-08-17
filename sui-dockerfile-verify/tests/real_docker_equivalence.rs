//! Real-docker integration test for the equivalence checker.
//!
//! Builds two tiny fixture images once (a vendor-shaped nonroot
//! gateway-style Dockerfile) — one pair that must compare *equivalent*
//! (same Dockerfile, rebuilt) and one pair that must compare
//! *different* (one instruction changed) — then runs
//! `sui_dockerfile_verify::compare_images` against the real, local
//! `docker` binary.
//!
//! `#[ignore]`d because it needs a real docker daemon and takes real
//! wall-clock time. Run explicitly via:
//!
//! ```text
//! cargo test -p sui-dockerfile-verify -- --ignored
//! ```
//!
//! Skips cleanly (prints why, does not fail) if `docker` isn't
//! reachable — never fakes a result.

use std::path::PathBuf;

use sui_dockerfile_wrapper::command::{CommandRunner, DockerBuildInvocation, RealCommandRunner};
use sui_dockerfile_verify::{compare_images, VolatileAllowlist};

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn build(runner: RealCommandRunner, dockerfile_path: &std::path::Path, context: &std::path::Path, tag: &str) {
    // `--load` is required so the built image lands in the local docker
    // image store (the default buildx `docker-container` driver only
    // leaves it in the build cache) — a hand-typed extra arg here since
    // DockerBuildInvocation::build() targets the classic/portable
    // invocation shape; --load is appended for this test's local-only
    // verification use case.
    let mut invocation = DockerBuildInvocation::build(dockerfile_path, context, tag, &std::collections::BTreeMap::new());
    invocation.args.insert(1, "--load".to_string());
    let outcome = runner.run(&invocation).expect("failed to spawn docker build");
    assert!(outcome.success, "docker build failed: {}", outcome.stderr_tail(4096));
}

#[test]
#[ignore = "needs a real docker daemon; run via `cargo test -p sui-dockerfile-verify -- --ignored`"]
fn identical_rebuild_is_equivalent_and_changed_instruction_is_not() {
    if !docker_available() {
        eprintln!("skipping identical_rebuild_is_equivalent_and_changed_instruction_is_not: no docker daemon reachable");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let dockerfile_a = dir.path().join("Dockerfile.a");
    let dockerfile_b_same = dir.path().join("Dockerfile.b_same");
    let dockerfile_c_changed = dir.path().join("Dockerfile.c_changed");

    let base = "FROM debian:bookworm-slim\nRUN apt-get update && apt-get install -y ca-certificates\nCMD [\"/bin/true\"]\n";
    let changed = "FROM debian:bookworm-slim\nRUN apt-get update && apt-get install -y ca-certificates curl\nCMD [\"/bin/true\"]\n";
    std::fs::write(&dockerfile_a, base).unwrap();
    std::fs::write(&dockerfile_b_same, base).unwrap();
    std::fs::write(&dockerfile_c_changed, changed).unwrap();

    let runner = RealCommandRunner;
    build(runner, &dockerfile_a, dir.path(), "sui-verify-test-a:latest");
    build(runner, &dockerfile_b_same, dir.path(), "sui-verify-test-b-same:latest");
    build(runner, &dockerfile_c_changed, dir.path(), "sui-verify-test-c-changed:latest");

    let workdir_same = tempfile::tempdir().unwrap();
    let report_same = compare_images(
        &runner,
        "sui-verify-test-a:latest",
        "sui-verify-test-b-same:latest",
        &VolatileAllowlist::default_docker_volatile(),
        workdir_same.path(),
    )
    .expect("compare_images against identical rebuild must succeed");
    assert!(report_same.layer_count_match, "identical Dockerfiles must have the same layer count");
    assert!(
        report_same.equivalent,
        "identical Dockerfiles rebuilt must compare equivalent; got differences: {:#?}",
        report_same.differences
    );

    let workdir_changed = tempfile::tempdir().unwrap();
    let report_changed = compare_images(
        &runner,
        "sui-verify-test-a:latest",
        "sui-verify-test-c-changed:latest",
        &VolatileAllowlist::default_docker_volatile(),
        workdir_changed.path(),
    )
    .expect("compare_images against a changed instruction must still succeed");
    assert!(
        !report_changed.equivalent,
        "a changed RUN instruction (added `curl`) must be detected as a difference"
    );

    // Clean up the images this test created — best-effort, do not fail
    // the test on cleanup errors.
    for tag in ["sui-verify-test-a:latest", "sui-verify-test-b-same:latest", "sui-verify-test-c-changed:latest"] {
        let _ = std::process::Command::new("docker").args(["rmi", "-f", tag]).output();
    }
    let _ = PathBuf::from(&dir.path()); // keep dir alive until here
}
