//! `reconcile::env` — the Observe/Act boundary the reconcile loop drives.
//!
//! The pleme-io **default delivery method**: the seven-beat brain
//! ([`super::SystemReconciler`]) is pure over an injected observation, and
//! *every* side effect — evaluating + building the desired toplevel, reading the
//! node's active generation, and activating a new generation — lives behind ONE
//! injectable trait ([`ReconcileEnvironment`]). Tests drive
//! [`mock::MockReconcileEnv`] with no eval, no build, no `/nix` — so the whole
//! reconcile brain is proved mock-green (the TYPED-SPEC + INTERPRETER contract's
//! Environment seam, applied to a controller instead of a spec interpreter).
//!
//! The real impl ([`LocalReconcileEnv`]) **composes the shipped rebuild
//! pipeline, never re-rolls it**: `desired_toplevel` is
//! [`SystemOrchestrator::build_toplevel`], `converge` is
//! [`SystemOrchestrator::activate`] (the atomic profile-swap — the *streamed
//! link change into place*), and `current_toplevel` reads the active generation
//! from the shipped [`sui_store::ProfileManager`].

use async_trait::async_trait;

use crate::system::SystemOrchestrator;

use super::ReconcileConfig;

/// A typed error at the reconcile Observe/Act boundary. These are **not**
/// propagated out of the controller tick — the loop is resilient: an observe
/// failure becomes an `Unobservable` verdict and a converge failure a `Failed`
/// verdict, both attested + requeued, so a transient error never kills the loop
/// (the same resilience the prewarmer's Viggy tick holds).
#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    /// Building the flake's declared toplevel (the desired state) failed.
    #[error("observe desired: {0}")]
    Observe(String),
    /// Reading the node's active generation (the observed state) failed.
    #[error("observe current: {0}")]
    ObserveCurrent(String),
    /// Activating the desired toplevel (converging) failed.
    #[error("converge: {0}")]
    Converge(String),
}

/// The node's currently-active system generation — the *observed* state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveGeneration {
    /// The active profile generation number.
    pub generation: u32,
    /// The store path the active generation points to (the live toplevel).
    pub toplevel: String,
}

impl ActiveGeneration {
    /// The store basename of the active toplevel — the prefix-independent
    /// identity used for the drift comparison (`LINK-GRAPH.md` I9).
    #[must_use]
    pub fn basename(&self) -> String {
        store_basename(&self.toplevel)
    }
}

/// The receipt of one converge (activation) — what the Act beat changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvergeReceipt {
    /// The active generation before the converge (`None` if no profile existed).
    pub from_generation: Option<u32>,
    /// The active generation after the converge (`None` if it could not be read
    /// back — the converge still ran, but the readback failed).
    pub to_generation: Option<u32>,
    /// The store basename now activated.
    pub toplevel: String,
}

/// The Observe/Act seam. `desired_toplevel` + `current_toplevel` are the Observe
/// beat; `converge` is the Act beat. All side effects live here so the tick
/// brain is pure + mock-tested.
#[async_trait]
pub trait ReconcileEnvironment: Send + Sync {
    /// **Observe (desired).** Evaluate + build the flake's declared system
    /// toplevel — the desired state. Side-effect-free with respect to the live
    /// system (it only reads the flake + realizes a closure into the store).
    ///
    /// # Errors
    ///
    /// [`ReconcileError::Observe`] on any eval/build failure.
    async fn desired_toplevel(&self, config: &ReconcileConfig) -> Result<String, ReconcileError>;

    /// **Observe (current).** Read the node's active system generation — the
    /// observed state. `None` when no profile generation is active yet (a fresh
    /// system).
    ///
    /// # Errors
    ///
    /// [`ReconcileError::ObserveCurrent`] if the profile cannot be read.
    async fn current_toplevel(&self) -> Result<Option<ActiveGeneration>, ReconcileError>;

    /// **Act (converge).** Activate `desired` — set the system profile (the
    /// atomic generation-symlink swap, the streamed link change) and run the
    /// closure's activate script per `config.action`. Returns the receipt.
    ///
    /// # Errors
    ///
    /// [`ReconcileError::Converge`] if activation fails (including the
    /// fail-closed root gate off-root).
    async fn converge(
        &self,
        config: &ReconcileConfig,
        desired: &str,
    ) -> Result<ConvergeReceipt, ReconcileError>;
}

/// The store basename of a path — everything after the last `/`. A nix store
/// path `/nix/store/<hash>-<name>` yields `<hash>-<name>`, the
/// prefix-independent node identity the drift compare uses.
#[must_use]
pub fn store_basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

// ───────────────────────────────────────────────────────────────────────────
// The real environment — composes the shipped rebuild pipeline, never re-rolls
// ───────────────────────────────────────────────────────────────────────────

/// The live [`ReconcileEnvironment`] over the shipped `SystemOrchestrator` +
/// `ProfileManager`. Owns one orchestrator (which detects the platform once);
/// every beat delegates to an already-shipped, already-tested primitive.
pub struct LocalReconcileEnv {
    orch: SystemOrchestrator,
}

impl LocalReconcileEnv {
    /// Build over a fresh platform-detecting orchestrator.
    ///
    /// # Errors
    ///
    /// [`crate::system::SystemError::UnsupportedPlatform`] if the platform is
    /// neither Darwin nor NixOS.
    pub fn new() -> Result<Self, crate::system::SystemError> {
        Ok(Self {
            orch: SystemOrchestrator::new()?,
        })
    }

    /// Build over an explicit orchestrator (tests / custom platform).
    #[must_use]
    pub fn with_orchestrator(orch: SystemOrchestrator) -> Self {
        Self { orch }
    }
}

#[async_trait]
impl ReconcileEnvironment for LocalReconcileEnv {
    async fn desired_toplevel(&self, config: &ReconcileConfig) -> Result<String, ReconcileError> {
        self.orch
            .build_toplevel(&config.flake)
            .await
            .map_err(|e| ReconcileError::Observe(e.to_string()))
    }

    async fn current_toplevel(&self) -> Result<Option<ActiveGeneration>, ReconcileError> {
        let pm = sui_store::ProfileManager::system();
        let generations = pm
            .list_generations()
            .map_err(|e| ReconcileError::ObserveCurrent(e.to_string()))?;
        Ok(generations
            .into_iter()
            .find(|g| g.current)
            .map(|g| ActiveGeneration {
                generation: g.number,
                toplevel: g.path.to_string_lossy().into_owned(),
            }))
    }

    async fn converge(
        &self,
        config: &ReconcileConfig,
        desired: &str,
    ) -> Result<ConvergeReceipt, ReconcileError> {
        let pm = sui_store::ProfileManager::system();
        let from_generation = pm.current_generation().ok().flatten();
        // The atomic profile-swap + activate script — the streamed link change.
        self.orch
            .activate(desired, config.action)
            .await
            .map_err(|e| ReconcileError::Converge(e.to_string()))?;
        let to_generation = sui_store::ProfileManager::system()
            .current_generation()
            .ok()
            .flatten();
        Ok(ConvergeReceipt {
            from_generation,
            to_generation,
            toplevel: store_basename(desired),
        })
    }
}

// ───────────────────────────────────────────────────────────────────────────
// The mock environment — the pleme-io default delivery method's test seam
// ───────────────────────────────────────────────────────────────────────────

/// A recording, configurable [`ReconcileEnvironment`] for unit tests — no eval,
/// no build, no `/nix`. Drives the pure reconcile brain over hand-built
/// observations, and records every `converge` call so a test can assert the Act
/// beat ran (or, for a shadow tick, did not).
pub mod mock {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::{ActiveGeneration, ConvergeReceipt, ReconcileEnvironment, ReconcileError};
    use crate::reconcile::ReconcileConfig;
    use crate::system::RebuildAction;

    /// A mock environment with settable desired/current state and a converge
    /// call recorder. `converge` (when it succeeds) advances `current` to the
    /// desired toplevel + `converge_to_gen`, so a second tick observes the
    /// system in place — modelling real convergence.
    pub struct MockReconcileEnv {
        desired: Mutex<Result<String, String>>,
        current: Mutex<Option<ActiveGeneration>>,
        converge_to_gen: u32,
        converge_fails: bool,
        /// Every `converge` call, in order: `(desired, action)`.
        pub converge_calls: Mutex<Vec<(String, RebuildAction)>>,
    }

    impl MockReconcileEnv {
        /// A mock that is already in place: desired == current == `toplevel`.
        #[must_use]
        pub fn in_place(toplevel: &str, generation: u32) -> Self {
            Self {
                desired: Mutex::new(Ok(toplevel.to_string())),
                current: Mutex::new(Some(ActiveGeneration {
                    generation,
                    toplevel: format!("/nix/store/{toplevel}"),
                })),
                converge_to_gen: generation,
                converge_fails: false,
                converge_calls: Mutex::new(Vec::new()),
            }
        }

        /// A mock that is drifted: desired != current; a converge advances to
        /// `desired` at `converge_to_gen`.
        #[must_use]
        pub fn drifted(desired: &str, current: &str, current_gen: u32, converge_to_gen: u32) -> Self {
            Self {
                desired: Mutex::new(Ok(desired.to_string())),
                current: Mutex::new(Some(ActiveGeneration {
                    generation: current_gen,
                    toplevel: format!("/nix/store/{current}"),
                })),
                converge_to_gen,
                converge_fails: false,
                converge_calls: Mutex::new(Vec::new()),
            }
        }

        /// A fresh node: desired known, no active generation yet.
        #[must_use]
        pub fn fresh(desired: &str, converge_to_gen: u32) -> Self {
            Self {
                desired: Mutex::new(Ok(desired.to_string())),
                current: Mutex::new(None),
                converge_to_gen,
                converge_fails: false,
                converge_calls: Mutex::new(Vec::new()),
            }
        }

        /// A mock whose desired-toplevel observe fails (eval/build error).
        #[must_use]
        pub fn unobservable(error: &str, current: &str, current_gen: u32) -> Self {
            Self {
                desired: Mutex::new(Err(error.to_string())),
                current: Mutex::new(Some(ActiveGeneration {
                    generation: current_gen,
                    toplevel: format!("/nix/store/{current}"),
                })),
                converge_to_gen: current_gen,
                converge_fails: false,
                converge_calls: Mutex::new(Vec::new()),
            }
        }

        /// Make this mock's `converge` fail (activation error).
        #[must_use]
        pub fn with_failing_converge(mut self) -> Self {
            self.converge_fails = true;
            self
        }

        /// How many times `converge` has been called.
        ///
        /// # Panics
        ///
        /// Only on a poisoned mutex (unreachable in the single-threaded tick).
        #[must_use]
        pub fn converge_count(&self) -> usize {
            self.converge_calls.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl ReconcileEnvironment for MockReconcileEnv {
        async fn desired_toplevel(
            &self,
            _config: &ReconcileConfig,
        ) -> Result<String, ReconcileError> {
            self.desired
                .lock()
                .unwrap()
                .clone()
                .map_err(ReconcileError::Observe)
        }

        async fn current_toplevel(&self) -> Result<Option<ActiveGeneration>, ReconcileError> {
            Ok(self.current.lock().unwrap().clone())
        }

        async fn converge(
            &self,
            config: &ReconcileConfig,
            desired: &str,
        ) -> Result<ConvergeReceipt, ReconcileError> {
            self.converge_calls
                .lock()
                .unwrap()
                .push((desired.to_string(), config.action));
            if self.converge_fails {
                return Err(ReconcileError::Converge("mock activation failure".into()));
            }
            let from_generation = self.current.lock().unwrap().as_ref().map(|g| g.generation);
            // Model real convergence: the node is now at the desired toplevel.
            *self.current.lock().unwrap() = Some(ActiveGeneration {
                generation: self.converge_to_gen,
                toplevel: format!("/nix/store/{desired}"),
            });
            Ok(ConvergeReceipt {
                from_generation,
                to_generation: Some(self.converge_to_gen),
                toplevel: desired.to_string(),
            })
        }
    }
}
