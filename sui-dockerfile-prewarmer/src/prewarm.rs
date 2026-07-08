//! The pre-warm seam: wraps [`sui_dockerfile_wrapper::run_wrapper`]
//! behind a trait so the poll-cycle logic can be tested without a real
//! `docker` binary or cache backend.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use sui_cache::storage::StorageBackend;
use sui_dockerfile_wrapper::{WrapperConfig, WrapperError, WrapperReceipt};

/// The one injectable seam over "run the wrapper against this fetched
/// Dockerfile content" — production wires the real `run_wrapper` against
/// a real cache backend + `docker`; tests inject a
/// [`RecordingPrewarmRunner`].
#[async_trait]
pub trait PrewarmRunner: Send + Sync {
    /// Pre-warm the cache for one watched Dockerfile whose content was
    /// just fetched from GitHub.
    ///
    /// # Errors
    ///
    /// Propagates any [`WrapperError`] from the underlying `run_wrapper`
    /// call (graph computation or cache-backend I/O failure — a failing
    /// `docker build` is not an error here either, same contract as
    /// `run_wrapper`).
    async fn prewarm(&self, dockerfile_path: &str, dockerfile_content: &str, image_tag: &str) -> Result<WrapperReceipt, WrapperError>;
}

/// The production [`PrewarmRunner`] — real cache backend, real
/// `docker` subprocess (via [`sui_dockerfile_wrapper::command::RealCommandRunner`]).
pub struct RealPrewarmRunner {
    cache: Arc<dyn StorageBackend>,
    context_dir: PathBuf,
}

impl RealPrewarmRunner {
    #[must_use]
    pub fn new(cache: Arc<dyn StorageBackend>, context_dir: PathBuf) -> Self {
        Self { cache, context_dir }
    }
}

#[async_trait]
impl PrewarmRunner for RealPrewarmRunner {
    async fn prewarm(&self, dockerfile_path: &str, dockerfile_content: &str, image_tag: &str) -> Result<WrapperReceipt, WrapperError> {
        let env = sui_spec::dockerfile::MockDockerfileEnvironment::default().with_dockerfile(dockerfile_path, dockerfile_content);
        let config = WrapperConfig {
            dockerfile_path: PathBuf::from(dockerfile_path),
            context_dir: self.context_dir.clone(),
            build_args: std::collections::BTreeMap::new(),
            image_tag: image_tag.to_string(),
        };
        let runner = sui_dockerfile_wrapper::command::RealCommandRunner;
        sui_dockerfile_wrapper::run_wrapper(&config, &env, &self.cache, &runner).await
    }
}

#[cfg(test)]
pub mod mock {
    //! A recording [`PrewarmRunner`] — proves the poll cycle invokes
    //! pre-warming with the expected Dockerfile content, without
    //! touching a real cache or `docker`.

    use std::sync::Mutex;

    use super::{PrewarmRunner, WrapperError, WrapperReceipt};
    use async_trait::async_trait;
    use sui_dockerfile_wrapper::WrapperOutcome;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RecordedPrewarmCall {
        pub dockerfile_path: String,
        pub dockerfile_content: String,
        pub image_tag: String,
    }

    #[derive(Default)]
    pub struct RecordingPrewarmRunner {
        calls: Mutex<Vec<RecordedPrewarmCall>>,
    }

    impl RecordingPrewarmRunner {
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        #[must_use]
        pub fn recorded(&self) -> Vec<RecordedPrewarmCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl PrewarmRunner for RecordingPrewarmRunner {
        async fn prewarm(&self, dockerfile_path: &str, dockerfile_content: &str, image_tag: &str) -> Result<WrapperReceipt, WrapperError> {
            self.calls.lock().unwrap().push(RecordedPrewarmCall {
                dockerfile_path: dockerfile_path.to_string(),
                dockerfile_content: dockerfile_content.to_string(),
                image_tag: image_tag.to_string(),
            });
            Ok(WrapperReceipt {
                outcome: WrapperOutcome::CacheMiss { docker_build_duration_ms: 0, nodes_cached: 1 },
                nodes: Vec::new(),
                total_wall_clock_ms: 0,
                docker_ran: true,
            })
        }
    }
}
