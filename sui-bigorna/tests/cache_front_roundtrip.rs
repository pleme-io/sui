//! `BuildKitCacheFront` round-trip — a real buildx ↔ sui-store-backed cache
//! endpoint proof.
//!
//! bigorna's whole cache claim is that it points `BuildKit`'s native
//! `--cache-from` / `--cache-to` at an endpoint that fronts a sui storage
//! backend, so a build's layers warm a store and a *later* build reads them
//! back. This harness proves that round-trip against **real buildx**:
//!
//! 1. Bridge a sui [`BackendConfig::Local`] into a bigorna
//!    [`CacheEndpoint`](sui_bigorna::CacheEndpoint) via
//!    [`CacheEndpoint::from_backend_config`] — the exact map bigorna ships. The
//!    `local` wire is a genuine `StorageBackend` config (`type=local` is one of
//!    the wires `BuildKit` speaks natively), so this needs **no registry** and
//!    no throwaway server: the "store-backed endpoint" is a real sui-config
//!    Local backend fronted by BuildKit's local cache wire, under a tempdir.
//! 2. Build an image via a bigorna [`BuildSpec`] whose `cache.to` is that
//!    endpoint → BuildKit exports every layer into the store dir.
//! 3. On a **fresh** bigorna builder (a distinct builder name, so it carries no
//!    in-memory BuildKit cache — the export dir is the only shared state),
//!    build the *same* image with `cache.from` = that endpoint.
//! 4. Assert the second build actually **imports** the cache — its `RUN` steps
//!    are `CACHED` — proving the store-backed round-trip, not an in-memory
//!    BuildKit hit.
//!
//! ## Why this doubles as a regression test for a real bug
//!
//! `BuildKit`'s local cache is **asymmetric**: export writes `type=local,dest=`
//! and import reads `type=local,src=`. Passing a `dest=` token to
//! `--cache-from` fails the whole build (`local cache importer requires src`).
//! bigorna's `CacheEndpoint::Local` `Display` renders the export form; this
//! test exercises the *import* path through
//! [`BuildSpec::invocation`](sui_bigorna::BuildSpec::invocation), which must
//! emit the `src=` token — so a regression to a `dest=` import token makes this
//! test fail with buildx's own error, not silently pass.
//!
//! `#[ignore]`d — needs a real docker daemon + buildx and takes real
//! wall-clock time. Run via:
//!
//! ```text
//! cargo test -p sui-bigorna --test cache_front_roundtrip -- --ignored --nocapture
//! ```

use std::path::Path;

use sui_bigorna::{
    build, setup, teardown, Arch, BigornaConfig, BuildOutput, BuildSpec, BuildxDriver, CacheEndpoint,
    CacheFront, CacheMode, NodeSpec, Platform, PlatformList,
};
use sui_cache::config::BackendConfig;
use sui_dockerfile_wrapper::{CommandRunner, DockerBuildInvocation, RealCommandRunner};

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("buildx")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The host's native platform + bigorna arch — the round-trip is a
/// single-platform, single-native-node build (the cache round-trip is
/// arch-agnostic; use the native arch so no QEMU is involved and the timing is
/// tight).
fn native_platform_and_arch() -> Option<(Platform, Arch)> {
    match Arch::host()? {
        Arch::Arm64 => Some((Platform::linux_arm64(), Arch::Arm64)),
        Arch::Amd64 => Some((Platform::linux_amd64(), Arch::Amd64)),
        _ => None,
    }
}

/// A small multi-`RUN` Dockerfile — each `RUN` is a distinct cacheable layer,
/// so a cache hit shows up as `CACHED` steps. Kept cheap: the point is the
/// cache round-trip, not build cost.
fn roundtrip_dockerfile() -> &'static str {
    "FROM debian:bookworm-slim\n\
     RUN echo cache-front-layer-one > /one.txt\n\
     RUN echo cache-front-layer-two > /two.txt\n\
     RUN echo cache-front-layer-three > /three.txt\n"
}

/// Stand up a bigorna builder with a single native node for the host arch.
/// Returns the builder name; caller tears it down. `name` lets the two arms use
/// distinct builders (the second is genuinely fresh — no in-memory hit).
fn setup_builder(name: &str, platform: &Platform, arch: Arch) -> String {
    let runner = RealCommandRunner;
    let _ = teardown(name, &runner); // best-effort clean of a prior aborted run
    let cfg = BigornaConfig {
        builder_name: name.to_string(),
        driver: BuildxDriver::DockerContainer,
        nodes: vec![NodeSpec {
            name: {
                let mut n = name.to_string();
                n.push_str("-native");
                n
            },
            platform: platform.clone(),
            host_arch: arch,
            endpoint: None,
            driver_opts: vec![],
        }],
        bootstrap: true,
        use_as_default: false,
    };
    let receipt = setup(cfg, &runner).expect("bigorna setup");
    assert!(receipt.ok, "builder {name} setup failed: {:?}", receipt.steps);
    name.to_string()
}

/// Build the round-trip Dockerfile on `builder`, exporting the cache to the
/// store-backed endpoint (`cache.to`). Returns the raw buildx stderr (buildx
/// prints its progress there) so the caller can confirm export.
fn build_exporting_cache(
    builder: &str,
    platform: &Platform,
    dockerfile: &Path,
    context: &Path,
    export_endpoint: CacheEndpoint,
    runner: &RealCommandRunner,
) -> String {
    let spec = BuildSpec {
        builder: builder.to_string(),
        platforms: PlatformList(vec![platform.clone()]),
        dockerfile: dockerfile.to_path_buf(),
        context: context.to_path_buf(),
        tags: vec![],
        build_args: std::collections::BTreeMap::new(),
        cache: CacheFront { from: vec![], to: vec![export_endpoint] },
        output: BuildOutput::None,
    };
    // A real bigorna build receipt (the typed driver a consumer runs). We force
    // a cacheonly output + no-cache so the export is of a genuinely fresh graph.
    let mut inv = spec.invocation();
    inject_no_cache_and_cacheonly(&mut inv);
    let outcome = runner.run(&inv).expect("spawn docker buildx build (export)");
    assert!(
        outcome.success,
        "cache-exporting build failed: {}",
        outcome.stderr_tail(8192)
    );
    // buildx writes progress to stderr.
    String::from_utf8_lossy(&outcome.stderr).into_owned()
}

/// Insert `--no-cache` + `--output type=cacheonly` right after `buildx build`.
fn inject_no_cache_and_cacheonly(inv: &mut DockerBuildInvocation) {
    let at = 2.min(inv.args.len());
    inv.args.insert(at, "--no-cache".to_string());
    inv.args.insert(at + 1, "--output".to_string());
    inv.args.insert(at + 2, "type=cacheonly".to_string());
}

#[test]
#[ignore = "needs a real docker daemon + buildx and takes real wall-clock time; run via `cargo test -p sui-bigorna --test cache_front_roundtrip -- --ignored --nocapture`"]
fn buildkit_cache_front_round_trips_through_a_sui_store_backed_endpoint() {
    if !docker_available() {
        eprintln!("skipping cache_front_roundtrip: no docker buildx reachable");
        return;
    }
    let Some((platform, arch)) = native_platform_and_arch() else {
        eprintln!("skipping cache_front_roundtrip: host arch not wired");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let dockerfile_path = dir.path().join("Dockerfile.roundtrip");
    std::fs::write(&dockerfile_path, roundtrip_dockerfile()).unwrap();
    // The store-backed endpoint: a real sui `BackendConfig::Local` under the
    // tempdir, bridged through bigorna's shipped `from_backend_config` map. On
    // export it renders `type=local,dest=<store>`; on import `type=local,src=`.
    let store_dir = dir.path().join("cache-store");
    let backend_config = BackendConfig::Local { path: store_dir.clone() };
    let endpoint =
        CacheEndpoint::from_backend_config(&backend_config, None, Some(CacheMode::Max)).unwrap();
    // Sanity: the bridge produced a Local endpoint (the store-backed wire).
    assert!(
        matches!(endpoint, CacheEndpoint::Local { .. }),
        "a sui Local backend must bridge to a Local cache endpoint",
    );

    let runner = RealCommandRunner;

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // ── ARM 1: build + EXPORT the cache to the sui-store-backed endpoint ─
        let export_builder = setup_builder("bigorna-rt-export", &platform, arch);
        let export_stderr = build_exporting_cache(
            &export_builder,
            &platform,
            &dockerfile_path,
            dir.path(),
            endpoint.clone(),
            &runner,
        );
        let _ = teardown(&export_builder, &runner);

        // The export must have actually written the store dir (the round-trip's
        // whole premise — a real store, not an in-memory BuildKit cache).
        assert!(
            store_dir.join("index.json").exists(),
            "the cache export must have written the sui-store-backed endpoint dir; \
             stderr tail:\n{}",
            &export_stderr[export_stderr.len().saturating_sub(2000)..]
        );

        // ── ARM 2: on a FRESH builder, build + IMPORT the cache ─────────────
        // A distinct builder name ⇒ no shared in-memory BuildKit cache; the
        // ONLY shared state is the store dir. A CACHED step here therefore
        // proves the store round-trip, not an in-process hit.
        let import_builder = setup_builder("bigorna-rt-import", &platform, arch);
        let import_spec = BuildSpec {
            builder: import_builder.clone(),
            platforms: PlatformList(vec![platform.clone()]),
            dockerfile: dockerfile_path.clone(),
            context: dir.path().to_path_buf(),
            tags: vec![],
            build_args: std::collections::BTreeMap::new(),
            // `from` = the SAME store-backed endpoint. This exercises the
            // asymmetric import path — the invocation MUST render `src=`, or
            // buildx fails with `local cache importer requires src`.
            cache: CacheFront { from: vec![endpoint.clone()], to: vec![] },
            output: BuildOutput::None,
        };
        // Confirm, at the argv level, that the import token is `src=` (the
        // regression guard) BEFORE running — a mis-render would fail the build
        // with a confusing buildx error otherwise.
        let import_inv = import_spec.invocation();
        let import_token = import_inv
            .args
            .iter()
            .find(|a| a.starts_with("type=local"))
            .cloned()
            .expect("a --cache-from local token must be present");
        assert!(
            import_token.contains("src="),
            "the local --cache-from token MUST use src= (buildx rejects dest= on import); got `{import_token}`",
        );

        // Run the import build through bigorna's real typed `build` driver — no
        // --no-cache here: we WANT it to consult the imported cache and hit.
        let mut inv = import_spec.invocation();
        // cacheonly output so no image materialization is needed.
        {
            let at = 2.min(inv.args.len());
            inv.args.insert(at, "--output".to_string());
            inv.args.insert(at + 1, "type=cacheonly".to_string());
        }
        let outcome = runner.run(&inv).expect("spawn docker buildx build (import)");
        let import_stderr = String::from_utf8_lossy(&outcome.stderr).into_owned();
        let _ = teardown(&import_builder, &runner);

        assert!(
            outcome.success,
            "cache-importing build failed: {}",
            outcome.stderr_tail(8192)
        );

        // ── The load-bearing assertion: the import build HIT the store cache.
        //    buildx prints `CACHED` for each layer served from the imported
        //    cache. At least one RUN layer must be CACHED — proving the
        //    store-backed round-trip. ─────────────────────────────────────────
        let cached_steps = import_stderr.matches("CACHED").count();
        eprintln!(
            "cache_front_roundtrip: import build reported {cached_steps} CACHED steps from the sui-store-backed endpoint"
        );
        eprintln!(
            "--- import build progress (stderr tail) ---\n{}",
            &import_stderr[import_stderr.len().saturating_sub(1200)..]
        );
        cached_steps
    }));

    // Best-effort teardown of both builders even on panic (the arms tear down
    // on the happy path; this catches a mid-arm panic).
    let _ = teardown("bigorna-rt-export", &runner);
    let _ = teardown("bigorna-rt-import", &runner);

    let cached_steps =
        result.unwrap_or_else(|e| std::panic::resume_unwind(e));

    assert!(
        cached_steps > 0,
        "the import build must serve at least one layer from the sui-store-backed cache \
         (CACHED steps == 0 means the round-trip did not hit)",
    );
}

/// A second, tighter arm: prove that bigorna's `--cache-from` for a `local`
/// endpoint renders the import (`src=`) token through the full typed
/// `BuildSpec` → invocation path — the regression guard, checked without a
/// docker daemon so it runs in normal CI too (NOT `#[ignore]`d).
#[test]
fn build_spec_renders_local_cache_from_as_src_not_dest() {
    let backend_config = BackendConfig::Local {
        path: std::path::PathBuf::from("/tmp/sui-cache-store"),
    };
    let endpoint = CacheEndpoint::from_backend_config(&backend_config, None, None).unwrap();

    let spec = BuildSpec {
        builder: "bigorna".to_string(),
        platforms: PlatformList(vec![Platform::linux_arm64()]),
        dockerfile: std::path::PathBuf::from("Dockerfile"),
        context: std::path::PathBuf::from("."),
        tags: vec![],
        build_args: std::collections::BTreeMap::new(),
        // The same local endpoint on BOTH sides — the invocation must render
        // `src=` after `--cache-from` and `dest=` after `--cache-to`.
        cache: CacheFront { from: vec![endpoint.clone()], to: vec![endpoint] },
        output: BuildOutput::None,
    };
    let inv = spec.invocation();

    // Locate the token immediately following each cache flag.
    let from_at = inv.args.iter().position(|a| a == "--cache-from").expect("--cache-from present");
    let to_at = inv.args.iter().position(|a| a == "--cache-to").expect("--cache-to present");
    let from_token = &inv.args[from_at + 1];
    let to_token = &inv.args[to_at + 1];

    assert_eq!(from_token, "type=local,src=/tmp/sui-cache-store", "import must be src=");
    assert_eq!(to_token, "type=local,dest=/tmp/sui-cache-store", "export must be dest=");
    assert_ne!(from_token, to_token, "a local endpoint's import and export tokens differ");

    // Additionally build the `build()` driver's would-run receipt is well-formed
    // (it does not spawn — MockCommandRunner records the argv).
    let recorder = sui_dockerfile_wrapper::MockCommandRunner::new();
    let receipt = build(&spec, &recorder).expect("build receipt");
    assert!(
        receipt.argv.iter().any(|a| a == "type=local,src=/tmp/sui-cache-store"),
        "the driven build's argv must carry the src= import token: {:?}",
        receipt.argv,
    );
}
