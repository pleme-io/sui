//! GHA-side integration point for Phase 3b of `supa-charge-akeyless-ci`.
//!
//! **Scoped-gap note (see the phase's final report):** the fleet
//! convention for shipping a GitHub Actions integration point is a
//! typed `pleme-io/actions` action (Rust binary + `action.yml`,
//! possibly a `(defaction ...)` tatara-lisp form — per the
//! `pleme-actions` skill). That repo exists locally
//! (`~/code/github/pleme-io/actions`), but wiring a new action into it
//! (crate + `action.yml` + flake + release plumbing) is out of scope
//! for this pass; this binary is the equivalent detection-and-fallback
//! logic, built inside this workspace so the behavior ships and is
//! tested now. **Follow-up gap:** port this into a
//! `pleme-io/actions/sui-dockerfile-node-cache` action that shells out
//! to (or vendors) this same logic.
//!
//! What it does: probes for the well-known node-local daemon socket
//! (see [`sui_dockerfile_node_cache_daemon::default_socket_path`]); if
//! present, configures [`sui_dockerfile_wrapper::WrapperConfig`] to
//! route through [`sui_dockerfile_wrapper::DaemonAwareCacheClient`]; if
//! absent, configures the direct-to-remote-tier path unchanged (today's
//! default) — never requiring the daemon.

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use sui_cache::config::BackendConfig;
use sui_cache::storage::{build_backend, StorageBackend};
use sui_dockerfile_node_cache_daemon::default_socket_path;
use sui_dockerfile_wrapper::{run_wrapper, DaemonAwareCacheClient, FilesystemDockerfileEnvironment, RealCommandRunner, WrapperConfig};

/// The decision this entrypoint exists to make, pulled out as a pure
/// function so it's testable without a real socket, a real remote
/// backend, or a real `docker` binary.
///
/// # Errors
///
/// Never errors — this is a pure detection step; kept `Result`-shaped
/// for symmetry with the rest of the crate's fallible surfaces and to
/// leave room for a future fallible probe (e.g. a permission check)
/// without a signature break.
fn resolve_daemon_socket_path(env_override: Option<&str>, probe_exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let candidate = env_override.map_or_else(default_socket_path, PathBuf::from);
    if probe_exists(&candidate) {
        Some(candidate)
    } else {
        None
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let Ok(dockerfile_path_raw) = env::var("SUI_DOCKERFILE_PATH") else {
        eprintln!("SUI_DOCKERFILE_PATH is required");
        return ExitCode::FAILURE;
    };
    let dockerfile_path = PathBuf::from(dockerfile_path_raw);
    let context_dir = env::var("SUI_BUILD_CONTEXT").map_or_else(|_| PathBuf::from("."), PathBuf::from);
    let Ok(image_tag) = env::var("SUI_IMAGE_TAG") else {
        eprintln!("SUI_IMAGE_TAG is required");
        return ExitCode::FAILURE;
    };

    let socket_path =
        resolve_daemon_socket_path(env::var("SUI_NODE_CACHE_SOCKET_PATH").ok().as_deref(), Path::exists);

    let Ok(backend_config_raw) = env::var("SUI_CACHE_BACKEND_CONFIG") else {
        eprintln!("SUI_CACHE_BACKEND_CONFIG is required");
        return ExitCode::FAILURE;
    };
    let backend_config = match serde_json::from_str::<BackendConfig>(&backend_config_raw) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("invalid SUI_CACHE_BACKEND_CONFIG: {e}");
            return ExitCode::FAILURE;
        }
    };
    let remote = match build_backend(&backend_config).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("failed to build remote cache backend: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Daemon-aware substitution: the ONLY place this phase changes
    // Phase 2 behavior. Absent a daemon, `cache` is byte-for-byte the
    // remote backend Phase 2 already used.
    let cache: Arc<dyn StorageBackend> = match &socket_path {
        Some(path) => Arc::new(DaemonAwareCacheClient::new(Some(path.clone()), remote)),
        None => remote,
    };

    let config = WrapperConfig {
        dockerfile_path: dockerfile_path.clone(),
        context_dir,
        build_args: std::collections::BTreeMap::new(),
        image_tag,
        daemon_socket_path: socket_path,
    };
    let env_impl = FilesystemDockerfileEnvironment { build_args: config.build_args.clone() };
    let runner = RealCommandRunner;

    match run_wrapper(&config, &env_impl, &cache, &runner).await {
        Ok(receipt) => match receipt.to_json() {
            Ok(json) => {
                println!("{json}");
                if matches!(receipt.outcome, sui_dockerfile_wrapper::WrapperOutcome::BuildFailed { .. }) {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
            }
            Err(e) => {
                eprintln!("failed to render receipt: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("wrapper run failed: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_env_override_and_socket_present_resolves_to_default_path() {
        let resolved = resolve_daemon_socket_path(None, |p| p == default_socket_path());
        assert_eq!(resolved, Some(default_socket_path()));
    }

    #[test]
    fn no_env_override_and_socket_absent_resolves_to_none() {
        let resolved = resolve_daemon_socket_path(None, |_| false);
        assert_eq!(resolved, None, "no daemon on this node ⇒ direct-to-remote-tier, unchanged Phase 2 behavior");
    }

    #[test]
    fn env_override_path_is_honored_when_present() {
        let resolved =
            resolve_daemon_socket_path(Some("/custom/socket.sock"), |p| p == Path::new("/custom/socket.sock"));
        assert_eq!(resolved, Some(PathBuf::from("/custom/socket.sock")));
    }

    #[test]
    fn env_override_path_absent_still_resolves_to_none() {
        let resolved = resolve_daemon_socket_path(Some("/custom/socket.sock"), |_| false);
        assert_eq!(resolved, None);
    }
}
