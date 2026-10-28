//! Local builder — executes derivation builds on the local machine.
//!
//! [`LocalBuilder`] implements the [`Builder`] trait using a [`Sandbox`]
//! for process isolation and a [`Store`] for output registration.
//! After a successful build it scans outputs for runtime references,
//! computes NAR hashes, and registers the results in the store.

use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use sui_compat::activity::{self, Activity, ActivityType, BuilderLog, SharedSink, Verbosity, Worker};
use sui_compat::derivation::{Derivation, DerivationOutput};
use sui_compat::nar::{NarWriter, NarNode, NarEntry};
use sui_compat::store_path::{StorePath, STORE_PATH_HASH_LEN};

use crate::closure::BuildClosure;
use crate::reference_scan;
use crate::sandbox::{Sandbox, SandboxConfig};
use crate::traits::{BuildError, BuildLog, BuildOutcome, BuildResult, Builder};

use sui_store::substitute::{Availability, SubstituteResult, Substitutor};
use sui_store::traits::{PathInfo, Store};

/// Progress update emitted during [`LocalBuilder::build_closure_with_progress`].
#[derive(Debug, Clone)]
pub struct BuildProgress {
    /// Total number of derivations in the closure.
    pub total: usize,
    /// Number of derivations already processed.
    pub completed: usize,
    /// Store path of the derivation currently being processed.
    pub current_drv: String,
    /// What is happening to this derivation.
    pub action: BuildAction,
}

/// The action taken for a single derivation during closure building.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildAction {
    /// Checking whether outputs exist in the local store.
    Checking,
    /// Fetching outputs from a binary cache.
    Substituting,
    /// Building outputs locally.
    Building,
    /// All outputs already present — nothing to do.
    Skipped,
}

/// How one output of a rebuild compares with what the store records for it
/// (`--check`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputCheck {
    /// The output name.
    pub output: String,
    /// The output's store path.
    pub path: String,
    /// The NAR hash the store records.
    pub recorded_nar_hash: String,
    /// The NAR hash of the rebuild, hashes rewritten back (`sha256:<hex>`).
    pub rebuilt_nar_hash: String,
    /// The references the store records.
    pub recorded_references: Vec<String>,
    /// The references the rebuild's bytes name, by the builder's rule.
    pub rebuilt_references: Vec<String>,
}

/// How a rebuild diverged from the recorded output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Divergence {
    /// The rebuilt bytes differ (cppnix: "may not be deterministic").
    Bytes {
        /// As recorded.
        recorded: String,
        /// As rebuilt.
        rebuilt: String,
    },
    /// The bytes agree but the recorded references do not.
    References {
        /// Named by the bytes, not recorded.
        missing: Vec<String>,
        /// Recorded, not named by the bytes.
        extra: Vec<String>,
    },
}

impl OutputCheck {
    /// `None` when the rebuild agrees with the record.
    #[must_use]
    pub fn divergence(&self) -> Option<Divergence> {
        use sui_compat::hash::{HashAlgorithm, NixHash};
        let digest = |h: &str| NixHash::parse_any(HashAlgorithm::Sha256, h).ok().map(|h| h.digest);
        if digest(&self.recorded_nar_hash).is_none() || digest(&self.recorded_nar_hash) != digest(&self.rebuilt_nar_hash) {
            return Some(Divergence::Bytes { recorded: self.recorded_nar_hash.clone(), rebuilt: self.rebuilt_nar_hash.clone() });
        }
        let recorded: std::collections::BTreeSet<&String> = self.recorded_references.iter().collect();
        let rebuilt: std::collections::BTreeSet<&String> = self.rebuilt_references.iter().collect();
        let missing: Vec<String> = rebuilt.difference(&recorded).map(|s| (*s).clone()).collect();
        let extra: Vec<String> = recorded.difference(&rebuilt).map(|s| (*s).clone()).collect();
        (!missing.is_empty() || !extra.is_empty()).then_some(Divergence::References { missing, extra })
    }
}

/// Compare a rebuilt output, still at its scratch path, with the record of
/// the real one: rewrite the scratch hashes back, hash its NAR, scan it for
/// the build's candidates.
///
/// # Errors
///
/// When the scratch tree cannot be rewritten or read.
pub fn compare_rebuilt(
    output: &str,
    recorded: &PathInfo,
    scratch: &Path,
    back: &[(String, String)],
    candidates: &[String],
) -> Result<OutputCheck, BuildError> {
    if !back.is_empty() {
        rewrite_tree(scratch, back)?;
    }
    let digest = sui_compat::store_path_scan::NarDigester::of_path(scratch)
        .map_err(|e| BuildError::Failed(format!("hashing the rebuilt '{output}': {e}")))?;
    let hex: String = digest.sha256.iter().map(|b| format!("{b:02x}")).collect();
    let mut rebuilt_references = scan_refs(candidates, &scratch.to_string_lossy());
    rebuilt_references.sort();
    let mut recorded_references = recorded.references.clone();
    recorded_references.sort();
    Ok(OutputCheck {
        output: output.to_string(),
        path: recorded.path.clone(),
        recorded_nar_hash: recorded.nar_hash.clone(),
        rebuilt_nar_hash: format!("sha256:{hex}"),
        recorded_references,
        rebuilt_references,
    })
}

enum RemoteOrLocal {
    Local(BuildError),
    Connect(String),
}

/// A builder that executes derivations locally with sandbox isolation.
pub struct LocalBuilder {
    store: Arc<dyn Store>,
    sandbox: Arc<dyn Sandbox>,
    build_dir_base: String,
    sink: SharedSink,
    max_jobs: Option<usize>,
    offload: Option<Arc<crate::offload::Offload>>,
}

struct RunningBuild<'a> {
    worker: &'a Worker,
    activity: Activity,
    succeeded: bool,
}

impl<'a> RunningBuild<'a> {
    fn start(worker: &'a Worker, drv_path: &str) -> Self {
        let activity = Activity::start(
            worker.sink(),
            Verbosity::Info,
            ActivityType::Build,
            format!("building '{drv_path}'"),
            vec![drv_path.into(), "".into(), 1.into(), 1.into()],
            0,
        );
        worker.build_started();
        Self { worker, activity, succeeded: false }
    }
}

impl Drop for RunningBuild<'_> {
    fn drop(&mut self) {
        self.worker.build_finished(self.succeeded);
    }
}

impl LocalBuilder {
    /// Create a new `LocalBuilder`.
    ///
    /// # Arguments
    ///
    /// * `store` — The store backend for registering build outputs.
    /// * `sandbox` — The sandbox implementation for process isolation.
    pub fn new(store: Arc<dyn Store>, sandbox: Box<dyn Sandbox>) -> Self {
        Self {
            store,
            sandbox: Arc::from(sandbox),
            build_dir_base: std::env::temp_dir()
                .join("sui-build")
                .to_string_lossy()
                .into_owned(),
            sink: activity::noop_sink(),
            max_jobs: None,
            offload: None,
        }
    }

    #[must_use]
    pub fn with_offload(mut self, offload: crate::offload::Offload) -> Self {
        if offload.policy.active() || offload.policy.host.max_jobs == 0 {
            self.offload = Some(Arc::new(offload));
        }
        self
    }

    #[must_use]
    pub fn with_offload_from_nix_conf(self) -> Self {
        match crate::offload::OffloadPolicy::from_nix_conf() {
            Ok(policy) => self.with_offload(crate::offload::Offload::new(policy)),
            Err(e) => {
                tracing::warn!("remote builders disabled: {e}");
                self
            }
        }
    }

    /// Run at most `jobs` builds at once, in place of nix.conf's `max-jobs`.
    #[must_use]
    pub fn with_max_jobs(mut self, jobs: usize) -> Self {
        self.max_jobs = Some(jobs.max(1));
        self
    }

    /// Report builds and substitutions to `sink` as CppNix activities.
    #[must_use]
    pub fn with_sink(mut self, sink: SharedSink) -> Self {
        self.sink = sink;
        self
    }

    /// Override the base directory for build sandboxes.
    #[must_use]
    pub fn with_build_dir(mut self, base: impl Into<String>) -> Self {
        self.build_dir_base = base.into();
        self
    }

    /// Build a single derivation (not the full closure).
    ///
    /// This is the core build logic:
    /// 1. Check if outputs already exist (skip if so)
    /// 2. Create a sandbox build directory
    /// 3. Execute the builder in the sandbox
    /// 4. On success: scan references, compute NAR hashes, register outputs
    /// 5. Clean up the build directory
    async fn build_single(&self, drv: &Derivation) -> Result<BuildResult, BuildError> {
        let worker = Worker::start(&self.sink);
        self.build_one(drv, None, None, &worker, None).await
    }

    pub async fn build_basic(&self, drv: &Derivation, deriver: &str) -> Result<BuildResult, BuildError> {
        let worker = Worker::start(&self.sink);
        self.build_one(drv, None, Some(deriver), &worker, None).await
    }

    /// Rebuild `drv` and compare each output with what the store records
    /// (`nix-store --realise --check`). Every output must be valid already;
    /// the rebuild goes to scratch paths, is compared with the hashes
    /// rewritten back, and is removed. Nothing registered changes.
    ///
    /// # Errors
    ///
    /// When an output is not valid ("checking is not possible"), the build
    /// fails, or a rebuilt output cannot be read.
    pub async fn check(&self, drv: &Derivation, deriver: &str) -> Result<Vec<OutputCheck>, BuildError> {
        if !self.all_outputs_exist(drv).await? {
            return Err(BuildError::Failed(format!(
                "some outputs of '{deriver}' are not valid, so checking is not possible"
            )));
        }
        let worker = Worker::start(&self.sink);
        let mut checks = Vec::new();
        let result = self.build_one(drv, None, Some(deriver), &worker, Some(&mut checks)).await?;
        if !result.is_success() {
            return Err(BuildError::Failed(format!("rebuild of '{deriver}' failed:\n{}", result.log)));
        }
        Ok(checks)
    }

    async fn build_one(
        &self,
        drv: &Derivation,
        index: Option<&std::collections::HashMap<&str, &Derivation>>,
        deriver: Option<&str>,
        worker: &Worker,
        mut check: Option<&mut Vec<OutputCheck>>,
    ) -> Result<BuildResult, BuildError> {
        let start = std::time::Instant::now();
        let mut log = BuildLog::new();

        // 1. Check if all outputs already exist (a check rebuilds them anyway)
        if check.is_none() && self.all_outputs_exist(drv).await? {
            log.push("all outputs already exist, skipping build");
            let outputs = self.collect_output_paths(drv);
            return Ok(BuildResult::success(
                outputs,
                log.finish(),
                start.elapsed().as_secs_f64(),
            ));
        }

        let mut running = RunningBuild::start(worker, deriver.unwrap_or_default());

        // 2. Create build directory
        let build_id = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let build_dir = format!("{}/{}", self.build_dir_base, build_id);
        self.clear_invalid_outputs(drv).await?;
        let redirects = self.valid_output_redirects(drv).await;
        for (_, scratch) in redirects.values() {
            let _ = remove_path(scratch);
        }
        let rewrites: Vec<(String, String)> =
            redirects.values().map(|(real, scratch)| (hash_part(real), hash_part(scratch))).collect();
        let built = rewrite_derivation(drv, &rewrites);
        let candidates = self.reference_candidates(drv, index).await;
        std::fs::create_dir_all(&build_dir)?;
        let build_dir = std::fs::canonicalize(&build_dir)?.to_string_lossy().into_owned();
        for (path, value) in crate::export_graph::attr_files(self.store.as_ref(), &built, &build_dir)
            .await
            .map_err(|e| BuildError::Failed(format!("preparing the build directory: {e}")))?
        {
            std::fs::write(path, value)?;
        }

        // 3. Create sandbox config from derivation
        let mut config = SandboxConfig::from_derivation(&built, &build_dir);

        // 4. Handle fixed-output derivations (allow network)
        if is_fixed_output(drv) {
            config.allow_network = true;
        }

        // 5. Execute in sandbox
        log.push(&format!("executing builder: {}", config.builder));
        self.sandbox.prepare(&config)?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let sandbox = self.sandbox.clone();
        let job = config.clone();
        let execution = tokio::task::spawn_blocking(move || {
            sandbox.execute_observed(&job, &|chunk| {
                let _ = tx.send(chunk.to_vec());
            })
        });
        let mut builder_log = BuilderLog::new(&running.activity, false);
        while let Some(chunk) = rx.recv().await {
            builder_log.feed(&chunk);
        }
        builder_log.finish();
        let result = execution
            .await
            .map_err(|e| BuildError::Failed(format!("builder task for {}: {e}", deriver.unwrap_or_default())))?;
        let _ = self.sandbox.cleanup(&config);

        let result = result?;

        // 6. Check exit code
        if !result.is_success() {
            let stderr = result.stderr_lossy();
            log.push(&format!("build failed with exit code {}", result.exit_code));
            if !stderr.is_empty() {
                log.push(&stderr);
            }
            let _ = std::fs::remove_dir_all(&build_dir);
            let _ = self.clear_invalid_outputs(drv).await;
            return Ok(BuildResult::failure(
                log.finish(),
                stderr,
                result.exit_code,
                start.elapsed().as_secs_f64(),
            ));
        }

        if !result.stdout_lossy().is_empty() {
            log.push(&result.stdout_lossy());
        }

        // 7. Post-build: for each output, scan references and register
        let mut output_paths = Vec::new();
        let mut pending: Vec<(String, PathInfo)> = Vec::new();
        let back: Vec<(String, String)> = rewrites.iter().map(|(real, scratch)| (scratch.clone(), real.clone())).collect();
        for (output_name, output) in &drv.outputs {
            let output_path = &output.path;

            if let Some((_, scratch)) = redirects.get(output_name) {
                let produced = std::fs::symlink_metadata(scratch).is_ok();
                if produced && let Some(checks) = check.as_deref_mut() {
                    let recorded = match StorePath::from_absolute_path(output_path) {
                        Ok(sp) => self.store.query_path_info(&sp).await.ok().flatten(),
                        Err(_) => None,
                    };
                    let compared = match recorded {
                        Some(recorded) => compare_rebuilt(output_name, &recorded, Path::new(scratch), &back, &candidates),
                        None => Err(BuildError::Failed(format!("output '{output_name}' has no record to check against"))),
                    };
                    let _ = remove_path(scratch);
                    checks.push(compared?);
                }
                let _ = remove_path(scratch);
                if !produced {
                    let _ = std::fs::remove_dir_all(&build_dir);
                    return Ok(BuildResult::failure(
                        log.finish(),
                        format!("output '{output_name}' not created"),
                        1,
                        start.elapsed().as_secs_f64(),
                    ));
                }
                log.push(&format!("output '{output_name}' was already valid: {output_path}"));
                if let Ok(sp) = StorePath::from_absolute_path(output_path) {
                    output_paths.push(sp);
                }
                continue;
            }

            // Check if the output path actually exists on disk
            if std::fs::symlink_metadata(output_path).is_err() {
                log.push(&format!(
                    "output '{output_name}' at {output_path} was not created by builder"
                ));
                let _ = std::fs::remove_dir_all(&build_dir);
                return Ok(BuildResult::failure(
                    log.finish(),
                    format!("output '{output_name}' not created"),
                    1,
                    start.elapsed().as_secs_f64(),
                ));
            }

            // Scan references
            if !back.is_empty() {
                rewrite_tree(Path::new(output_path), &back)?;
            }
            let runtime_refs = scan_refs(&candidates, output_path);

            // Compute NAR hash of the output
            let (nar_hash, nar_size) = compute_nar_hash(output_path)?;

            // Verify fixed-output hash if applicable
            if !output.hash.is_empty() {
                verify_fixed_output(output, output_path)?;
            }

            // Build PathInfo and register
            let info = PathInfo {
                path: output_path.clone(),
                nar_hash: format!("sha256:{nar_hash}"),
                nar_size: nar_size as i64,
                references: runtime_refs,
                deriver: deriver.map(String::from),
                signatures: vec![],
                registration_time: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64,
                content_address: fixed_output_ca(output),
            };

            pending.push((output_name.clone(), info));
        }

        let ordered = topo_sort_outputs(pending).map_err(BuildError::Failed)?;
        for (output_name, info) in ordered {
            self.store.register_path(&info).await.map_err(|e| {
                BuildError::Failed(format!("failed to register output: {e}"))
            })?;
            if let Ok(sp) = StorePath::from_absolute_path(&info.path) {
                output_paths.push(sp);
            }
            log.push(&format!("registered output '{output_name}': {}", info.path));
        }

        // 8. Clean up build directory
        let _ = std::fs::remove_dir_all(&build_dir);
        running.succeeded = true;

        Ok(BuildResult::success(
            output_paths,
            log.finish(),
            start.elapsed().as_secs_f64(),
        ))
    }

    async fn build_remote(
        &self,
        offload: &Arc<crate::offload::Offload>,
        machine: usize,
        drv_path: &str,
        drv: &Derivation,
        index: &std::collections::HashMap<&str, &Derivation>,
        worker: &Worker,
    ) -> Result<BuildResult, RemoteOrLocal> {
        use crate::offload::{fs_closure, run_remote_job, RemoteFailure, RemoteJob};
        let start = std::time::Instant::now();
        let uri = offload.policy.machines[machine].store_uri.raw.clone();
        let failed = |message: String| {
            Ok(BuildResult::failure(
                format!("build of '{drv_path}' on '{uri}' failed: {message}"),
                message,
                1,
                start.elapsed().as_secs_f64(),
            ))
        };
        let mut roots = drv.input_sources.clone();
        for (input, outputs) in &drv.input_derivations {
            let parsed;
            let input_drv = match index.get(input.as_str()) {
                Some(d) => *d,
                None => match std::fs::read(input).map_err(|e| e.to_string()).and_then(|b| Derivation::parse(&b).map_err(|e| e.to_string())) {
                    Ok(d) => {
                        parsed = d;
                        &parsed
                    }
                    Err(e) => return failed(format!("reading input derivation '{input}': {e}")),
                },
            };
            for o in outputs {
                match input_drv.outputs.get(o) {
                    Some(out) if !out.path.is_empty() => roots.push(out.path.clone()),
                    _ => return failed(format!("input derivation '{input}' has no known output '{o}'")),
                }
            }
        }
        let inputs = match fs_closure(self.store.as_ref(), &roots).await {
            Ok(i) => i,
            Err(e) => return failed(e),
        };
        let drv_closure = match fs_closure(self.store.as_ref(), &[drv_path.to_string()]).await {
            Ok(i) => i,
            Err(e) => return failed(e),
        };
        let mut wanted = Vec::new();
        for out in drv.outputs.values() {
            let valid = match StorePath::from_absolute_path(&out.path) {
                Ok(sp) => self.store.is_valid_path(&sp).await.unwrap_or(false),
                Err(_) => false,
            };
            if !valid {
                wanted.push(out.path.clone());
            }
        }
        if let Err(e) = self.clear_invalid_outputs(drv).await {
            return failed(e.to_string());
        }
        let mut running = RunningBuild::start(worker, drv_path);
        let substitute = offload.policy.builders_use_substitutes;
        let job = RemoteJob { drv_path: drv_path.to_string(), drv: drv.clone(), inputs, drv_closure, wanted, substitute };
        let remote = offload.clone();
        let fetched = tokio::task::spawn_blocking(move || {
            let m = &remote.policy.machines[machine];
            let (mut conn, guard) = remote.connector.open(m, &remote.policy).map_err(|e| RemoteFailure::Connect(e.to_string()))?;
            let result = run_remote_job(&mut conn, &job, remote.policy.build_options, &remote.policy.real_store_dir);
            drop(conn);
            drop(guard);
            result
        })
        .await
        .map_err(|e| RemoteOrLocal::Local(BuildError::Failed(format!("remote build task for {drv_path}: {e}"))))?;
        let fetched = match fetched {
            Ok(f) => f,
            Err(RemoteFailure::Connect(message)) => return Err(RemoteOrLocal::Connect(message)),
            Err(RemoteFailure::Build(message)) => return failed(message),
        };
        let mut outputs = Vec::new();
        for (info, nar) in fetched {
            if let Err(e) = self.store.add_to_store_nar(&info, &nar, false).await {
                return failed(format!("copying '{}' back: {e}", info.path));
            }
        }
        for out in drv.outputs.values() {
            if let Ok(sp) = StorePath::from_absolute_path(&out.path) {
                outputs.push(sp);
            }
        }
        running.succeeded = true;
        drop(running);
        Ok(BuildResult::success(outputs, format!("built '{drv_path}' on '{uri}'"), start.elapsed().as_secs_f64()))
    }

    /// Check whether all outputs of a derivation already exist in the store.
    async fn all_outputs_exist(&self, drv: &Derivation) -> Result<bool, BuildError> {
        for output in drv.outputs.values() {
            if let Ok(sp) = StorePath::from_absolute_path(&output.path) {
                let exists = self
                    .store
                    .is_valid_path(&sp)
                    .await
                    .unwrap_or(false);
                if !exists {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Collect output StorePaths from a derivation (best-effort).
    fn collect_output_paths(&self, drv: &Derivation) -> Vec<StorePath> {
        drv.outputs
            .values()
            .filter_map(|o| StorePath::from_absolute_path(&o.path).ok())
            .collect()
    }

    async fn valid_output_redirects(&self, drv: &Derivation) -> std::collections::BTreeMap<String, (String, String)> {
        let mut redirects = std::collections::BTreeMap::new();
        for (name, output) in &drv.outputs {
            let Ok(sp) = StorePath::from_absolute_path(&output.path) else {
                continue;
            };
            if self.store.is_valid_path(&sp).await.unwrap_or(false) {
                redirects.insert(name.clone(), (output.path.clone(), scratch_path(&output.path)));
            }
        }
        redirects
    }

    /// Every path a build output may reference: the closure of the inputs plus the drv's own outputs.
    async fn reference_candidates(
        &self,
        drv: &Derivation,
        index: Option<&std::collections::HashMap<&str, &Derivation>>,
    ) -> Vec<String> {
        reference_candidates(self.store.as_ref(), drv, index).await
    }
}

/// The paths a build's output may reference, as cppnix scans for them: the
/// input closure (input sources and the wanted outputs of input derivations,
/// with everything they reference) and the derivation's own outputs.
pub async fn reference_candidates(
    store: &dyn Store,
    drv: &Derivation,
    index: Option<&std::collections::HashMap<&str, &Derivation>>,
) -> Vec<String> {
    let mut queue: Vec<String> = drv.input_sources.clone();
    for (input, wanted) in &drv.input_derivations {
        let parsed;
        let input_drv = match index.and_then(|i| i.get(input.as_str()).copied()) {
            Some(d) => d,
            None => match std::fs::read(input).ok().and_then(|b| Derivation::parse(&b).ok()) {
                Some(d) => {
                    parsed = d;
                    &parsed
                }
                None => continue,
            },
        };
        for name in wanted {
            if let Some(out) = input_drv.outputs.get(name) {
                queue.push(out.path.clone());
            }
        }
    }
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    while let Some(path) = queue.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        if let Ok(sp) = StorePath::from_absolute_path(&path)
            && let Ok(Some(info)) = store.query_path_info(&sp).await
        {
            queue.extend(info.references.into_iter().filter(|r| !seen.contains(r)));
        }
    }
    seen.extend(drv.outputs.values().map(|o| o.path.clone()));
    seen.into_iter().collect()
}

fn scan_refs(candidates: &[String], output_path: &str) -> Vec<String> {
    let hashes: Vec<String> = candidates.iter().map(|p| hash_part(p)).collect();
    let hash_strs: Vec<&str> = hashes.iter().map(String::as_str).filter(|h| h.len() == STORE_PATH_HASH_LEN).collect();
    if hash_strs.is_empty() {
        return Vec::new();
    }
    let found = if !std::fs::symlink_metadata(output_path).is_ok_and(|m| m.is_file()) {
        reference_scan::scan_directory(output_path, &hash_strs).unwrap_or_default()
    } else {
        reference_scan::scan_file(output_path, &hash_strs).unwrap_or_default()
    };
    candidates
        .iter()
        .filter(|p| found.iter().any(|h| *h == hash_part(p)))
        .cloned()
        .collect()
}

fn hash_part(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or_default();
    base.get(..STORE_PATH_HASH_LEN).unwrap_or(base).to_string()
}

fn scratch_path(real: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let digest = Sha256::digest(format!("rewrite:{real}:{nanos}:{}", std::process::id()).as_bytes());
    let hash = sui_compat::store_path::nix_base32_encode(&sui_compat::store_path::compress_hash(&digest, 20));
    let (dir, base) = real.rsplit_once('/').unwrap_or(("", real));
    let name = base.get(STORE_PATH_HASH_LEN..).unwrap_or_default();
    format!("{dir}/{hash}{name}")
}

fn rewrite_str(value: &str, rewrites: &[(String, String)]) -> String {
    rewrites.iter().fold(value.to_string(), |acc, (from, to)| acc.replace(from.as_str(), to))
}

fn rewrite_derivation(drv: &Derivation, rewrites: &[(String, String)]) -> Derivation {
    if rewrites.is_empty() {
        return drv.clone();
    }
    let mut out = drv.clone();
    out.builder = rewrite_str(&drv.builder, rewrites);
    out.args = drv.args.iter().map(|a| rewrite_str(a, rewrites)).collect();
    for value in out.env.values_mut() {
        *value = rewrite_str(value, rewrites);
    }
    for output in out.outputs.values_mut() {
        output.path = rewrite_str(&output.path, rewrites);
    }
    out
}

fn remove_path(path: &str) -> std::io::Result<()> {
    let p = Path::new(path);
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(_) => Ok(()),
    }
}

fn rewrite_bytes(data: &[u8], rewrites: &[(String, String)]) -> Option<Vec<u8>> {
    let mut out = data.to_vec();
    let mut changed = false;
    for (from, to) in rewrites {
        let (from, to) = (from.as_bytes(), to.as_bytes());
        if from.is_empty() || from.len() != to.len() {
            continue;
        }
        let mut i = 0;
        while i + from.len() <= out.len() {
            if &out[i..i + from.len()] == from {
                out[i..i + from.len()].copy_from_slice(to);
                changed = true;
                i += from.len();
            } else {
                i += 1;
            }
        }
    }
    changed.then_some(out)
}

fn rewrite_tree(path: &Path, rewrites: &[(String, String)]) -> Result<(), BuildError> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        let target = target.to_string_lossy();
        let rewritten = rewrite_str(&target, rewrites);
        if rewritten != target {
            std::fs::remove_file(path)?;
            std::os::unix::fs::symlink(rewritten, path)?;
        }
    } else if meta.is_dir() {
        for entry in std::fs::read_dir(path)? {
            rewrite_tree(&entry?.path(), rewrites)?;
        }
    } else if let Some(bytes) = rewrite_bytes(&std::fs::read(path)?, rewrites) {
        let mode = meta.permissions();
        let mut writable = mode.clone();
        std::os::unix::fs::PermissionsExt::set_mode(&mut writable, std::os::unix::fs::PermissionsExt::mode(&mode) | 0o200);
        std::fs::set_permissions(path, writable)?;
        std::fs::write(path, bytes)?;
        std::fs::set_permissions(path, mode)?;
    }
    Ok(())
}

impl LocalBuilder {
}

impl LocalBuilder {
    /// Build an entire derivation closure in topological order.
    ///
    /// For each derivation in the closure (leaves first, target last):
    /// 1. Check if all outputs already exist in the store (skip)
    /// 2. Try to substitute each output from binary caches
    /// 3. Fall back to local build if substitution is not available
    ///
    /// Stops on the first build failure.
    async fn clear_invalid_outputs(&self, drv: &Derivation) -> Result<(), BuildError> {
        for output in drv.outputs.values() {
            let path = Path::new(&output.path);
            if std::fs::symlink_metadata(path).is_err() {
                continue;
            }
            let valid = match StorePath::from_absolute_path(&output.path) {
                Ok(sp) => self.store.is_valid_path(&sp).await.unwrap_or(false),
                Err(_) => false,
            };
            if valid {
                continue;
            }
            if path.is_dir() {
                std::fs::remove_dir_all(path)?;
            } else {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    async fn plan_closure(
        &self,
        closure: &BuildClosure,
        substitutor: Option<&Substitutor>,
        worker: &Worker,
    ) -> Result<ClosurePlan, BuildError> {
        walk_plan(self.store.as_ref(), closure, substitutor, PlanMode::Realize, Some(worker)).await
    }

    pub async fn build_closure(
        &self,
        closure: &BuildClosure,
        substitutor: Option<&Substitutor>,
    ) -> Result<BuildResult, BuildError> {
        let start = std::time::Instant::now();
        let mut log = BuildLog::new();
        let mut substituted: u32 = 0;
        let mut built: u32 = 0;
        let mut skipped: u32 = 0;

        let worker = Worker::start(&self.sink);
        let plan = self.plan_closure(closure, substitutor, &worker).await?;
        worker.expect_builds(plan.to_build.len() as u64);
        let index: std::collections::HashMap<&str, &Derivation> =
            closure.derivations.iter().map(|(p, d)| (p.as_str(), d)).collect();
        skipped += plan.present;
        substituted += plan.substituted;
        let order: Vec<(&str, &Derivation)> = closure
            .derivations
            .iter()
            .filter(|(p, _)| plan.to_build.contains(p))
            .map(|(p, d)| (p.as_str(), d))
            .collect();
        let mut waiting: std::collections::HashMap<&str, usize> = order
            .iter()
            .map(|(p, d)| (*p, d.input_derivations.keys().filter(|i| plan.to_build.contains(*i)).count()))
            .collect();
        let mut dependents: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
        for (p, d) in &order {
            for input in d.input_derivations.keys().filter(|i| plan.to_build.contains(*i)) {
                dependents.entry(input.as_str()).or_default().push(p);
            }
        }
        let mut ready: std::collections::VecDeque<&str> =
            order.iter().filter(|(p, _)| waiting[p] == 0).map(|(p, _)| *p).collect();
        let jobs = match (&self.max_jobs, &self.offload) {
            (Some(j), _) => *j,
            (None, Some(o)) => o.policy.host.max_jobs,
            (None, None) => max_jobs(),
        };
        let mut running = futures::stream::FuturesUnordered::new();
        let mut local_running = 0usize;
        let mut failure: Option<(String, BuildResult)> = None;
        loop {
            let mut deferred: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
            while failure.is_none() {
                let Some(drv_path) = ready.pop_front() else { break };
                let drv = index[drv_path];
                let local_free = local_running < jobs;
                let placement = match &self.offload {
                    Some(o) => o.place(drv, local_free),
                    None => crate::offload::Placement::Local,
                };
                let index = &index;
                let worker = &worker;
                match placement {
                    crate::offload::Placement::Remote(machine) => {
                        let offload = self.offload.clone().expect("a remote placement has an offload");
                        log.push(&format!("building {drv_path} on {}", offload.policy.machines[machine].store_uri.raw));
                        tracing::info!(drv_path, machine = %offload.policy.machines[machine].store_uri.raw, "building remotely");
                        running.push(futures::FutureExt::boxed(async move {
                            let done = self.build_remote(&offload, machine, drv_path, drv, index, worker).await;
                            (drv_path, Some(machine), done)
                        }));
                    }
                    crate::offload::Placement::Local if local_free => {
                        local_running += 1;
                        log.push(&format!("building {drv_path}"));
                        tracing::info!(drv_path, "building locally");
                        running.push(futures::FutureExt::boxed(async move {
                            let done = self.build_one(drv, Some(index), Some(drv_path), worker, None).await.map_err(RemoteOrLocal::Local);
                            (drv_path, None, done)
                        }));
                    }
                    crate::offload::Placement::Local | crate::offload::Placement::Postpone => deferred.push_back(drv_path),
                    crate::offload::Placement::Unbuildable(message) => {
                        failure = Some((drv_path.to_string(), BuildResult::failure(message.clone(), message, 1, 0.0)));
                    }
                }
            }
            deferred.extend(ready.drain(..));
            ready = deferred;
            let Some((drv_path, machine, result)) = futures::StreamExt::next(&mut running).await else {
                if failure.is_none() && let Some(stuck) = ready.front() {
                    let message = format!("waiting for a machine to build '{stuck}', but no build is running that could free one");
                    failure = Some(((*stuck).to_string(), BuildResult::failure(message.clone(), message, 1, 0.0)));
                }
                break;
            };
            match machine {
                Some(m) => {
                    if let Some(o) = &self.offload {
                        o.release(m);
                    }
                }
                None => local_running -= 1,
            }
            let result = match result {
                Ok(r) => r,
                Err(RemoteOrLocal::Local(e)) => return Err(e),
                Err(RemoteOrLocal::Connect(message)) => {
                    if let (Some(o), Some(m)) = (&self.offload, machine) {
                        o.disable(m);
                        tracing::warn!("cannot build on '{}': {message}", o.policy.machines[m].store_uri.raw);
                        log.push(&format!("cannot build on '{}': {message}", o.policy.machines[m].store_uri.raw));
                    }
                    ready.push_front(drv_path);
                    continue;
                }
            };
            if !result.success {
                log.push(&format!("FAILED: {drv_path}"));
                failure.get_or_insert((drv_path.to_string(), result));
                continue;
            }
            built += 1;
            for next in dependents.get(drv_path).into_iter().flatten() {
                let n = waiting.get_mut(next).expect("every dependent is planned");
                *n -= 1;
                if *n == 0 {
                    ready.push_back(next);
                }
            }
        }
        if let Some((_, result)) = failure {
            let merged_log = format!("{}\n{}", log.finish(), result.log);
            return Ok(BuildResult { log: merged_log, ..result });
        }

        log.push(&format!(
            "closure complete: {skipped} skipped, {substituted} substituted, {built} built"
        ));

        // Return result for the target (last) derivation
        let (_target_path, target_drv) = closure.try_target()?;
        let output_paths: Vec<StorePath> = target_drv
            .outputs
            .values()
            .filter_map(|o| StorePath::from_absolute_path(&o.path).ok())
            .collect();

        Ok(BuildResult {
            outputs: output_paths,
            log: log.finish(),
            success: true,
            outcome: BuildOutcome::Success,
            duration_secs: start.elapsed().as_secs_f64(),
        })
    }

    /// Build an entire derivation closure with progress reporting.
    ///
    /// Identical to [`build_closure`](Self::build_closure) but invokes
    /// `progress` at each step so callers can display build status.
    pub async fn build_closure_with_progress(
        &self,
        closure: &BuildClosure,
        substitutor: Option<&Substitutor>,
        progress: impl Fn(BuildProgress),
    ) -> Result<BuildResult, BuildError> {
        let start = std::time::Instant::now();
        let total = closure.len();
        let mut log = BuildLog::new();
        let mut substituted: u32 = 0;
        let mut built: u32 = 0;
        let mut skipped: u32 = 0;
        let mut completed: usize = 0;

        let worker = Worker::start(&self.sink);
        let plan = self.plan_closure(closure, substitutor, &worker).await?;
        worker.expect_builds(plan.to_build.len() as u64);
        let index: std::collections::HashMap<&str, &Derivation> =
            closure.derivations.iter().map(|(p, d)| (p.as_str(), d)).collect();
        skipped += plan.present;
        substituted += plan.substituted;
        for (drv_path, drv) in &closure.derivations {
            if !plan.to_build.contains(drv_path) {
                completed += 1;
                progress(BuildProgress {
                    total,
                    completed,
                    current_drv: drv_path.clone(),
                    action: BuildAction::Skipped,
                });
                continue;
            }

            // 3. Build locally
            progress(BuildProgress {
                total,
                completed,
                current_drv: drv_path.clone(),
                action: BuildAction::Building,
            });
            log.push(&format!("building {drv_path}"));
            let result = self.build_one(drv, Some(&index), Some(drv_path.as_str()), &worker, None).await?;

            if !result.success {
                log.push(&format!("FAILED: {drv_path}"));
                let merged_log = format!("{}\n{}", log.finish(), result.log);
                return Ok(BuildResult {
                    log: merged_log,
                    ..result
                });
            }
            built += 1;
            completed += 1;
        }

        log.push(&format!(
            "closure complete: {skipped} skipped, {substituted} substituted, {built} built"
        ));

        let (_target_path, target_drv) = closure.try_target()?;
        let output_paths: Vec<StorePath> = target_drv
            .outputs
            .values()
            .filter_map(|o| StorePath::from_absolute_path(&o.path).ok())
            .collect();

        Ok(BuildResult {
            outputs: output_paths,
            log: log.finish(),
            success: true,
            outcome: BuildOutcome::Success,
            duration_secs: start.elapsed().as_secs_f64(),
        })
    }

    /// Try to substitute all outputs of a derivation. Returns `true` if
    /// every output was either already present or successfully substituted.
    async fn try_substitute_outputs(
        &self,
        substitutor: &Substitutor,
        drv: &sui_compat::derivation::Derivation,
    ) -> bool {
        for output in drv.outputs.values() {
            let sp = match StorePath::from_absolute_path(&output.path) {
                Ok(sp) => sp,
                Err(_) => return false,
            };
            match substitutor.substitute(&sp).await {
                Ok(SubstituteResult::Substituted { .. } | SubstituteResult::AlreadyPresent) => {
                    // This output is covered
                }
                _ => return false,
            }
        }
        true
    }
}

impl Builder for LocalBuilder {
    async fn build(&self, drv: &Derivation) -> Result<BuildResult, BuildError> {
        self.build_single(drv).await
    }

    async fn output_exists(&self, path: &StorePath) -> Result<bool, BuildError> {
        Ok(self.store.is_valid_path(path).await.unwrap_or(false))
    }
}

/// Check whether a derivation is a fixed-output derivation.
///
/// A FOD has exactly one output ("out") with a non-empty hash field.
#[must_use]
pub fn is_fixed_output(drv: &Derivation) -> bool {
    drv.outputs.len() == 1
        && drv
            .outputs
            .get("out")
            .is_some_and(|o| !o.hash.is_empty())
}

/// Compute the SHA-256 hash of a path's NAR serialization.
///
/// Returns `(hex_hash, nar_byte_count)`.
pub fn compute_nar_hash(path: &str) -> Result<(String, usize), BuildError> {
    let p = Path::new(path);

    // Build the NarNode from the filesystem
    let node = path_to_nar_node(p)?;

    // Serialize to NAR bytes
    let mut nar_bytes = Vec::new();
    NarWriter::write(&mut nar_bytes, &node)
        .map_err(|e| BuildError::Failed(format!("NAR serialization failed: {e}")))?;

    // SHA-256 hash
    let mut hasher = Sha256::new();
    hasher.update(&nar_bytes);
    let hash = hasher.finalize();
    let hex = hash.iter().map(|b| format!("{b:02x}")).collect::<String>();

    Ok((hex, nar_bytes.len()))
}

/// Recursively build a [`NarNode`] tree from a filesystem path.
fn path_to_nar_node(path: &Path) -> Result<NarNode, BuildError> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(BuildError::Io)?;

    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        Ok(NarNode::Symlink {
            target: target.to_string_lossy().into_owned(),
        })
    } else if meta.is_file() {
        let contents = std::fs::read(path)?;
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        Ok(NarNode::Regular {
            executable,
            contents,
        })
    } else if meta.is_dir() {
        let mut entries: Vec<NarEntry> = Vec::new();
        let mut dir_entries: Vec<_> = std::fs::read_dir(path)?
            .filter_map(|e| e.ok())
            .collect();
        dir_entries.sort_by_key(|e| e.file_name());

        for entry in dir_entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let node = path_to_nar_node(&entry.path())?;
            entries.push(NarEntry { name, node });
        }
        Ok(NarNode::Directory { entries })
    } else {
        Err(BuildError::Failed(format!(
            "unsupported file type at {}",
            path.display()
        )))
    }
}

fn fixed_output_ca(output: &DerivationOutput) -> Option<String> {
    if output.hash.is_empty() || output.hash.len() % 2 != 0 {
        return None;
    }
    let raw: Option<Vec<u8>> = (0..output.hash.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&output.hash[i..i + 2], 16).ok())
        .collect();
    let encoded = sui_compat::store_path::nix_base32_encode(&raw?);
    Some(match output.hash_algo.strip_prefix("r:") {
        Some(algo) => format!("fixed:r:{algo}:{encoded}"),
        None => format!("fixed:{}:{encoded}", output.hash_algo),
    })
}

fn digest_hex(algo: &str, bytes: &[u8]) -> Option<String> {
    use sha2::Digest;
    let digest: Vec<u8> = match algo {
        "sha256" => sha2::Sha256::digest(bytes).to_vec(),
        "sha512" => sha2::Sha512::digest(bytes).to_vec(),
        _ => return None,
    };
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

fn verify_fixed_output(output: &DerivationOutput, output_path: &str) -> Result<(), BuildError> {
    let (recursive, algo) = match output.hash_algo.strip_prefix("r:") {
        Some(algo) => (true, algo),
        None => (false, output.hash_algo.as_str()),
    };
    let bytes = if recursive {
        let mut nar = Vec::new();
        NarWriter::write(&mut nar, &path_to_nar_node(Path::new(output_path))?)
            .map_err(|e| BuildError::Failed(format!("serialize {output_path}: {e}")))?;
        nar
    } else {
        std::fs::read(output_path)
            .map_err(|e| BuildError::Failed(format!("read flat fixed output {output_path}: {e}")))?
    };
    let actual = digest_hex(algo, &bytes).ok_or_else(|| {
        BuildError::Failed(format!(
            "fixed-output {output_path} declares hash algorithm {algo:?}, which sui cannot verify"
        ))
    })?;
    if output.hash != actual {
        return Err(BuildError::Failed(format!(
            "fixed-output hash mismatch for {output_path}: expected {}, got {actual}",
            output.hash
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::test_store::MockStore;
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use sui_compat::derivation::DerivationOutput;
    use sui_compat::store_path::StorePath;

    use crate::sandbox::NoSandbox;
    use crate::traits::BuildOutcome;

    // ── MockStore ─────────────────────────────────────────────

    // ── Helper ────────────────────────────────────────────────

    /// `--check`'s comparison: a rebuild at a scratch path, hashes rewritten
    /// back, matches the record exactly when its bytes and references do;
    /// a different byte is a Bytes divergence (cppnix: "may not be
    /// deterministic"), a record short of a reference the bytes name is a
    /// References divergence.
    #[test]
    fn a_rebuild_is_compared_with_the_record_after_rewriting_hashes_back() {
        let real = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1-pkg";
        let scratch_hash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb1";
        let dep = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2-dep";
        let tmp = tempfile::tempdir().unwrap();
        let tree = |dir: &Path, own: &str, extra: &str| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("f"), format!("lib=/nix/store/{own}-pkg/lib dep={dep}/lib{extra}")).unwrap();
            std::os::unix::fs::symlink(format!("{dep}/bin"), dir.join("l")).unwrap();
        };
        let expected = tmp.path().join("expected");
        tree(&expected, &hash_part(real), "");
        let d = sui_compat::store_path_scan::NarDigester::of_path(&expected).unwrap();
        let hex: String = d.sha256.iter().map(|b| format!("{b:02x}")).collect();
        let recorded = |refs: Vec<String>, nar_hash: String| PathInfo {
            path: real.to_string(),
            nar_hash,
            nar_size: 0,
            references: refs,
            deriver: None,
            signatures: vec![],
            registration_time: 0,
            content_address: None,
        };
        let back = vec![(scratch_hash.to_string(), hash_part(real))];
        let candidates = vec![real.to_string(), dep.to_string()];
        let run = |name: &str, extra: &str, info: &PathInfo| {
            let scratch = tmp.path().join(name);
            tree(&scratch, scratch_hash, extra);
            compare_rebuilt("out", info, &scratch, &back, &candidates).unwrap()
        };

        let full = recorded(vec![dep.to_string(), real.to_string()], format!("sha256:{hex}"));
        let same = run("s1", "", &full);
        assert_eq!(same.divergence(), None, "{same:?}");
        assert_eq!(same.rebuilt_references, vec![real.to_string(), dep.to_string()]);

        let drifted = run("s2", " one more byte", &full);
        assert!(matches!(drifted.divergence(), Some(Divergence::Bytes { .. })), "{drifted:?}");

        let short = recorded(vec![], format!("sha256:{hex}"));
        assert_eq!(
            run("s3", "", &short).divergence(),
            Some(Divergence::References { missing: vec![real.to_string(), dep.to_string()], extra: vec![] })
        );
        let padded = recorded(
            vec![dep.to_string(), real.to_string(), "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa3-x".to_string()],
            format!("sha256:{hex}"),
        );
        assert_eq!(
            run("s4", "", &padded).divergence(),
            Some(Divergence::References { missing: vec![], extra: vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa3-x".to_string()] })
        );
        let nix32 = format!("sha256:{}", sui_compat::store_path::nix_base32_encode(&d.sha256));
        assert_eq!(run("s5", "", &recorded(full.references.clone(), nix32)).divergence(), None, "nix32 record");
        assert!(matches!(run("s6", "", &recorded(vec![], "garbage".into())).divergence(), Some(Divergence::Bytes { .. })));
    }

    /// `--check` refuses a derivation whose outputs are not all valid, as
    /// cppnix does, rather than building it as if it were new.
    #[tokio::test]
    async fn check_refuses_when_an_output_is_not_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store, Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());
        let drv = make_drv("/bin/sh", &["-c", "true"], "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1-pkg");
        let err = builder.check(&drv, "/nix/store/x.drv").await.unwrap_err();
        assert!(err.to_string().contains("checking is not possible"), "{err}");
    }

    /// A rebuild that fails is an error, never an empty (clean-looking)
    /// comparison. The build never writes: it exits before creating its
    /// scratch output.
    #[tokio::test]
    async fn a_failed_rebuild_is_an_error_not_an_empty_check() {
        let tmp = tempfile::tempdir().unwrap();
        let out = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1-pkg";
        let store = Arc::new(MockStore::new().with_path(PathInfo {
            path: out.to_string(),
            nar_hash: "sha256:00".to_string(),
            nar_size: 0,
            references: vec![],
            deriver: None,
            signatures: vec![],
            registration_time: 0,
            content_address: None,
        }));
        let builder = LocalBuilder::new(store, Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());
        let drv = make_drv("/bin/sh", &["-c", "exit 3"], out);
        let err = builder.check(&drv, "/nix/store/x.drv").await.unwrap_err();
        assert!(err.to_string().contains("rebuild of '/nix/store/x.drv' failed"), "{err}");
    }

    fn make_drv(
        builder: &str,
        args: &[&str],
        output_path: &str,
    ) -> Derivation {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: output_path.to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );

        Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![],
            system: "aarch64-darwin".to_string(),
            builder: builder.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
        }
    }

    // ── is_fixed_output tests ─────────────────────────────────

    #[test]
    fn is_fixed_output_true_when_hash_present() {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/abc-src".to_string(),
                hash_algo: "sha256".to_string(),
                hash: "deadbeef".to_string(),
            },
        );
        let drv = Derivation {
            outputs,
            ..Derivation::default()
        };
        assert!(is_fixed_output(&drv));
    }

    #[test]
    fn is_fixed_output_false_when_hash_empty() {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/abc-hello".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );
        let drv = Derivation {
            outputs,
            ..Derivation::default()
        };
        assert!(!is_fixed_output(&drv));
    }

    #[test]
    fn is_fixed_output_false_when_multiple_outputs() {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/abc-hello".to_string(),
                hash_algo: "sha256".to_string(),
                hash: "deadbeef".to_string(),
            },
        );
        outputs.insert(
            "dev".to_string(),
            DerivationOutput {
                path: "/nix/store/def-hello-dev".to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );
        let drv = Derivation {
            outputs,
            ..Derivation::default()
        };
        assert!(!is_fixed_output(&drv));
    }

    // ── compute_nar_hash tests ─────────────────────────────────

    #[test]
    fn nar_hash_of_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("hello.txt");
        std::fs::write(&file_path, b"hello world").unwrap();

        let (hash, size) = compute_nar_hash(file_path.to_str().unwrap()).unwrap();
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // SHA-256 hex is 64 chars
        assert!(size > 0);
    }

    #[test]
    fn nar_hash_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("det.txt");
        std::fs::write(&file_path, b"deterministic content").unwrap();

        let (hash1, size1) = compute_nar_hash(file_path.to_str().unwrap()).unwrap();
        let (hash2, size2) = compute_nar_hash(file_path.to_str().unwrap()).unwrap();
        assert_eq!(hash1, hash2);
        assert_eq!(size1, size2);
    }

    #[test]
    fn nar_hash_of_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("mydir");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"aaa").unwrap();
        std::fs::write(dir.join("b.txt"), b"bbb").unwrap();

        let (hash, size) = compute_nar_hash(dir.to_str().unwrap()).unwrap();
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64);
        assert!(size > 0);
    }

    #[test]
    fn nar_hash_of_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("empty");
        std::fs::write(&file_path, b"").unwrap();

        let (hash, size) = compute_nar_hash(file_path.to_str().unwrap()).unwrap();
        assert!(!hash.is_empty());
        assert!(size > 0); // NAR overhead even for empty file
    }

    #[test]
    fn nar_hash_nonexistent_path_fails() {
        let result = compute_nar_hash("/nonexistent/path/xyz");
        assert!(result.is_err());
    }

    // ── verify_fixed_output tests ──────────────────────────────

    fn fixed(algo: &str, hash: String) -> DerivationOutput {
        DerivationOutput { path: String::new(), hash_algo: algo.to_string(), hash }
    }

    #[test]
    fn a_flat_fixed_output_is_checked_against_the_file_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("crate.tar.gz");
        std::fs::write(&file, b"crate bytes").unwrap();
        let path = file.to_str().unwrap();
        let good = digest_hex("sha256", b"crate bytes").unwrap();
        assert!(verify_fixed_output(&fixed("sha256", good), path).is_ok());
        let err = verify_fixed_output(&fixed("sha256", "0".repeat(64)), path).unwrap_err();
        assert!(err.to_string().contains("hash mismatch"));
    }

    #[test]
    fn a_recursive_fixed_output_is_checked_against_its_nar() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("src");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("a"), b"x").unwrap();
        let mut nar = Vec::new();
        NarWriter::write(&mut nar, &path_to_nar_node(&out).unwrap()).unwrap();
        let good = digest_hex("sha256", &nar).unwrap();
        assert!(verify_fixed_output(&fixed("r:sha256", good), out.to_str().unwrap()).is_ok());
        let flat_digest_of_nothing = digest_hex("sha256", b"").unwrap();
        assert!(verify_fixed_output(&fixed("r:sha256", flat_digest_of_nothing), out.to_str().unwrap()).is_err());
    }

    #[test]
    fn a_sha512_fixed_output_is_verified_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"y").unwrap();
        let path = file.to_str().unwrap();
        assert!(verify_fixed_output(&fixed("sha512", digest_hex("sha512", b"y").unwrap()), path).is_ok());
        assert!(verify_fixed_output(&fixed("sha512", "0".repeat(128)), path).is_err());
    }

    #[test]
    fn a_fixed_output_registers_its_content_address_as_nix_writes_it() {
        let hash = "521bc38abb08001b01866da9f51eb7c5d647a19260e00054a8c7fd5f9e57f7a9".to_string();
        let raw: Vec<u8> = (0..hash.len()).step_by(2).map(|i| u8::from_str_radix(&hash[i..i + 2], 16).unwrap()).collect();
        let b32 = sui_compat::store_path::nix_base32_encode(&raw);
        assert_eq!(fixed_output_ca(&fixed("sha256", hash.clone())), Some(format!("fixed:sha256:{b32}")));
        assert_eq!(fixed_output_ca(&fixed("r:sha256", hash)), Some(format!("fixed:r:sha256:{b32}")));
        assert_eq!(fixed_output_ca(&fixed("sha256", String::new())), None);
    }

    #[test]
    fn an_unverifiable_hash_algorithm_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"z").unwrap();
        let err = verify_fixed_output(&fixed("md5", "x".into()), file.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("cannot verify"));
    }

    // ── LocalBuilder + MockStore tests ─────────────────────────

    #[tokio::test]
    async fn output_exists_returns_false_for_unknown() {
        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store, Box::new(NoSandbox));

        let path = StorePath::from_absolute_path(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap();
        assert!(!builder.output_exists(&path).await.unwrap());
    }

    #[tokio::test]
    async fn output_exists_returns_true_for_known() {
        let info = PathInfo::new(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
            "sha256:abc",
        );
        let store = Arc::new(MockStore::new().with_path(info));
        let builder = LocalBuilder::new(store, Box::new(NoSandbox));

        let path = StorePath::from_absolute_path(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1",
        )
        .unwrap();
        assert!(builder.output_exists(&path).await.unwrap());
    }

    #[tokio::test]
    async fn build_skips_when_outputs_exist() {
        let output_path = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1";
        let info = PathInfo::new(output_path, "sha256:abc");
        let store = Arc::new(MockStore::new().with_path(info));
        let builder = LocalBuilder::new(store, Box::new(NoSandbox));

        let drv = make_drv("/bin/sh", &["-c", "true"], output_path);
        let result = builder.build(&drv).await.unwrap();
        assert!(result.is_success());
        assert!(result.log.contains("already exist"));
    }

    #[tokio::test]
    async fn build_echo_produces_success() {
        let tmp = tempfile::tempdir().unwrap();
        let output_path = tmp.path().join("out");
        let output_str = output_path.to_str().unwrap().to_string();

        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store.clone(), Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());

        // The builder will create the output file
        let drv = make_drv(
            "/bin/sh",
            &[
                "-c",
                &format!("echo hello > {}", output_path.display()),
            ],
            &output_str,
        );

        let result = builder.build(&drv).await.unwrap();
        assert!(result.is_success());
        assert!(result.duration_secs >= 0.0);

        // Output is not under /nix/store/ so StorePath parsing won't work,
        // but verify registration through MockStore's internal state
        let paths = store.paths.lock().unwrap();
        assert!(paths.contains_key(&output_str));
        let info = &paths[&output_str];
        assert!(info.nar_hash.starts_with("sha256:"));
        assert!(info.nar_size > 0);
    }

    #[tokio::test]
    async fn build_failing_command_produces_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let output_path = tmp.path().join("out");

        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store, Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());

        let drv = make_drv(
            "/bin/sh",
            &["-c", "exit 42"],
            output_path.to_str().unwrap(),
        );

        let result = builder.build(&drv).await.unwrap();
        assert!(!result.is_success());
        assert!(result.outcome.is_failure());
        match &result.outcome {
            BuildOutcome::Failure { exit_code, .. } => assert_eq!(*exit_code, 42),
            other => panic!("expected Failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn build_directory_output() {
        let tmp = tempfile::tempdir().unwrap();
        let output_path = tmp.path().join("outdir");

        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store.clone(), Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());

        let drv = make_drv(
            "/bin/sh",
            &[
                "-c",
                &format!(
                    "/bin/mkdir -p {} && echo data > {}/file.txt",
                    output_path.display(),
                    output_path.display(),
                ),
            ],
            output_path.to_str().unwrap(),
        );

        let result = builder.build(&drv).await.unwrap();
        assert!(result.is_success());
    }

    #[tokio::test]
    async fn build_output_not_created_fails() {
        let tmp = tempfile::tempdir().unwrap();
        // This output path will never be created by the builder
        let output_path = tmp.path().join("never-created");

        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store, Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());

        let drv = make_drv(
            "/bin/sh",
            &["-c", "true"],
            output_path.to_str().unwrap(),
        );

        let result = builder.build(&drv).await.unwrap();
        assert!(!result.is_success());
        assert!(result.log.contains("not created"));
    }

    #[tokio::test]
    async fn build_registers_path_info_in_store() {
        let tmp = tempfile::tempdir().unwrap();
        let output_path = tmp.path().join("registered-out");
        let output_str = output_path.to_str().unwrap().to_string();

        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store.clone(), Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());

        let drv = make_drv(
            "/bin/sh",
            &[
                "-c",
                &format!("echo registered > {}", output_path.display()),
            ],
            &output_str,
        );

        let result = builder.build(&drv).await.unwrap();
        assert!(result.is_success());

        // Verify PathInfo through MockStore's internal state
        let paths = store.paths.lock().unwrap();
        assert!(paths.contains_key(&output_str));
        let info = &paths[&output_str];
        assert!(info.nar_hash.starts_with("sha256:"));
        assert!(info.nar_size > 0);
        assert_eq!(info.path, output_str);
    }

    struct ReferenceCheckingStore(MockStore);

    #[async_trait::async_trait]
    impl Store for ReferenceCheckingStore {
        async fn query_path_info(&self, path: &StorePath) -> sui_store::traits::StoreResult<Option<PathInfo>> {
            self.0.query_path_info(path).await
        }

        async fn is_valid_path(&self, path: &StorePath) -> sui_store::traits::StoreResult<bool> {
            self.0.is_valid_path(path).await
        }

        async fn query_all_valid_paths(&self) -> sui_store::traits::StoreResult<Vec<StorePath>> {
            self.0.query_all_valid_paths().await
        }

        async fn register_path(&self, info: &PathInfo) -> sui_store::traits::StoreResult<()> {
            let known = self.0.paths.lock().unwrap().keys().cloned().collect::<std::collections::BTreeSet<_>>();
            if let Some(r) = info.references.iter().find(|r| **r != info.path && !known.contains(*r)) {
                return Err(sui_store::traits::StoreError::PathNotFound(format!(
                    "cannot register {}: reference {r} is not a valid path in this store",
                    info.path
                )));
            }
            self.0.register_path(info).await
        }
    }

    #[tokio::test]
    async fn outputs_that_reference_each_other_register_in_reference_order() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2-thing");
        let lib = tmp.path().join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1-thing-lib");
        let store = Arc::new(ReferenceCheckingStore(MockStore::new()));
        let builder = LocalBuilder::new(store.clone(), Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());
        let mut drv = make_drv(
            "/bin/sh",
            &["-c", &format!("echo {o} > {o}; echo {o} {l} > {l}", o = out.display(), l = lib.display())],
            out.to_str().unwrap(),
        );
        drv.outputs.insert(
            "lib".to_string(),
            DerivationOutput { path: lib.to_str().unwrap().to_string(), hash_algo: String::new(), hash: String::new() },
        );

        let result = builder.build(&drv).await.unwrap();
        assert!(result.is_success(), "{}", result.log);
        let paths = store.0.paths.lock().unwrap();
        let lib_info = &paths[lib.to_str().unwrap()];
        assert!(lib_info.references.contains(&out.to_str().unwrap().to_string()));
        assert!(lib_info.references.contains(&lib.to_str().unwrap().to_string()));
        assert!(paths.contains_key(out.to_str().unwrap()));
    }

    #[test]
    fn a_reference_cycle_between_outputs_is_an_error_naming_both() {
        let a = PathInfo { references: vec!["/b".into(), "/a".into()], ..PathInfo::new("/a", "") };
        let b = PathInfo { references: vec!["/a".into()], ..PathInfo::new("/b", "") };
        let err = topo_sort_outputs(vec![("a".into(), a), ("b".into(), b)]).unwrap_err();
        assert!(err.contains("/a") && err.contains("/b"), "{err}");
        let lib = PathInfo { references: vec!["/out".into()], ..PathInfo::new("/lib", "") };
        let out = PathInfo::new("/out", "");
        let order: Vec<String> = topo_sort_outputs(vec![("lib".into(), lib), ("out".into(), out)])
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(order, vec!["out", "lib"]);
    }

    #[tokio::test]
    async fn build_chain_second_sees_existing() {
        // Use a real store-path-format output so all_outputs_exist works.
        // We pre-register the output in MockStore to simulate a completed build.
        let output_path = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-step1";
        let info = PathInfo::new(output_path, "sha256:abc");
        let store = Arc::new(MockStore::new().with_path(info));
        let builder = LocalBuilder::new(store, Box::new(NoSandbox));

        let drv = make_drv("/bin/sh", &["-c", "true"], output_path);

        // Since the output is already in the store, the build should skip
        let result = builder.build(&drv).await.unwrap();
        assert!(result.is_success());
        assert!(result.log.contains("already exist"));
    }

    #[tokio::test]
    async fn fixed_output_detected_and_network_allowed() {
        // This test verifies that is_fixed_output correctly identifies FODs
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: "/nix/store/abc-fetchurl".to_string(),
                hash_algo: "sha256".to_string(),
                hash: "deadbeef".to_string(),
            },
        );
        let drv = Derivation {
            outputs,
            ..Derivation::default()
        };
        assert!(is_fixed_output(&drv));

        // The SandboxConfig should allow network for FODs
        let config = SandboxConfig::from_derivation(&drv, "/tmp/build");
        let mut modified = config.clone();
        if is_fixed_output(&drv) {
            modified.allow_network = true;
        }
        assert!(modified.allow_network);
    }

    // ── path_to_nar_node tests ─────────────────────────────────

    #[test]
    fn path_to_nar_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.txt");
        std::fs::write(&file, b"test content").unwrap();

        let node = path_to_nar_node(&file).unwrap();
        match node {
            NarNode::Regular { executable, contents } => {
                assert!(!executable);
                assert_eq!(contents, b"test content");
            }
            other => panic!("expected Regular, got {other:?}"),
        }
    }

    #[test]
    fn path_to_nar_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("testdir");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("file.txt"), b"contents").unwrap();

        let node = path_to_nar_node(&dir).unwrap();
        match node {
            NarNode::Directory { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].name, "file.txt");
            }
            other => panic!("expected Directory, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn path_to_nar_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target.txt");
        std::fs::write(&target, b"target").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let node = path_to_nar_node(&link).unwrap();
        match node {
            NarNode::Symlink { target: t } => {
                assert!(t.contains("target.txt"));
            }
            other => panic!("expected Symlink, got {other:?}"),
        }
    }

    // ── Reference scanning integration ─────────────────────────

    #[test]
    fn scan_refs_finds_embedded_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let output_file = tmp.path().join("output");
        let input_path = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-dependency-1.0".to_string();
        std::fs::write(&output_file, format!("using {input_path}/lib/libfoo.so")).unwrap();
        let other = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1-other".to_string();

        let refs = scan_refs(&[input_path.clone(), other], output_file.to_str().unwrap());
        assert_eq!(refs, vec![input_path]);
    }

    #[test]
    fn scan_refs_no_matches_or_no_candidates_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let output_file = tmp.path().join("output");
        std::fs::write(&output_file, b"no store paths here").unwrap();
        let dep = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-dep-1.0".to_string();
        assert!(scan_refs(&[dep], output_file.to_str().unwrap()).is_empty());
        assert!(scan_refs(&[], output_file.to_str().unwrap()).is_empty());
    }

    #[test]
    fn scan_refs_reads_a_dangling_symlink_output_by_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("output");
        let dep = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-absent-dep".to_string();
        std::os::unix::fs::symlink(format!("{dep}/bin/x"), &output).unwrap();
        assert_eq!(scan_refs(&[dep.clone()], output.to_str().unwrap()), vec![dep]);
    }

    #[tokio::test]
    async fn a_dangling_symlink_output_is_a_produced_output() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        let builder = LocalBuilder::new(Arc::new(MockStore::new()), Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());
        let drv = make_drv(
            "/bin/sh",
            &["-c", &format!("/bin/ln -s /nonexistent/kube/config {}", out.display())],
            out.to_str().unwrap(),
        );
        let result = builder.build(&drv).await.unwrap();
        assert!(result.is_success(), "{}", result.log);
        assert_eq!(std::fs::read_link(&out).unwrap(), Path::new("/nonexistent/kube/config"));
    }

    #[tokio::test]
    async fn reference_candidates_are_the_input_closure_and_the_own_outputs() {
        let lib = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1-lib";
        let dep_out = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2-dep";
        let src = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa3-src";
        let own_dev = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa4-me-dev";
        let own_out = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa5-me";
        let mut dep_info = PathInfo::new(dep_out, "sha256:d");
        dep_info.references = vec![lib.to_string()];
        let store = Arc::new(
            MockStore::new()
                .with_path(PathInfo::new(lib, "sha256:l"))
                .with_path(dep_info),
        );
        let builder = LocalBuilder::new(store, Box::new(NoSandbox));

        let dep = make_drv("/bin/sh", &[], dep_out);
        let mut me = depends_on(make_drv("/bin/sh", &[], own_out), "dep.drv");
        me.input_sources.push(src.to_string());
        me.outputs.insert(
            "dev".to_string(),
            DerivationOutput { path: own_dev.to_string(), hash_algo: String::new(), hash: String::new() },
        );
        let index: std::collections::HashMap<&str, &Derivation> = [("dep.drv", &dep)].into_iter().collect();

        let mut got = builder.reference_candidates(&me, Some(&index)).await;
        got.sort();
        assert_eq!(got, vec![lib, dep_out, src, own_dev, own_out]);
    }

    #[test]
    fn a_valid_output_is_redirected_and_its_hash_rewritten_back() {
        let real = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa5-me";
        let scratch = scratch_path(real);
        assert_eq!(scratch.len(), real.len());
        assert!(scratch.ends_with("-me") && scratch != real);

        let rewrites = vec![(hash_part(real), hash_part(&scratch))];
        let mut drv = make_drv("/bin/sh", &["-c", &format!("echo {real} > $dev")], real);
        drv.env.insert("out".into(), real.into());
        let built = rewrite_derivation(&drv, &rewrites);
        assert_eq!(built.outputs["out"].path, scratch);
        assert_eq!(built.env["out"], scratch);
        assert!(built.args[1].contains(&scratch));

        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("dev");
        std::fs::create_dir(&dev).unwrap();
        std::fs::write(dev.join("f"), format!("lib={scratch}/lib")).unwrap();
        std::os::unix::fs::symlink(format!("{scratch}/bin"), dev.join("l")).unwrap();
        let back: Vec<(String, String)> = rewrites.iter().map(|(a, b)| (b.clone(), a.clone())).collect();
        rewrite_tree(&dev, &back).unwrap();
        assert_eq!(std::fs::read_to_string(dev.join("f")).unwrap(), format!("lib={real}/lib"));
        assert_eq!(std::fs::read_link(dev.join("l")).unwrap(), std::path::PathBuf::from(format!("{real}/bin")));
    }

    // ── Build with references ──────────────────────────────────

    #[tokio::test]
    async fn build_with_references_records_them() {
        let tmp = tempfile::tempdir().unwrap();
        let output_path = tmp.path().join("with-refs-out");
        let output_str = output_path.to_str().unwrap().to_string();

        // The input source that will be referenced
        let input_hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let input_path = format!("/nix/store/{input_hash}-dep-1.0");

        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store.clone(), Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());

        // Build a derivation whose output file contains the input store path
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            DerivationOutput {
                path: output_str.clone(),
                hash_algo: String::new(),
                hash: String::new(),
            },
        );
        let drv = Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: vec![input_path.clone()],
            system: "aarch64-darwin".to_string(),
            builder: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                format!("echo 'depends on {input_path}' > {}", output_path.display()),
            ],
            env: BTreeMap::new(),
        };

        let result = builder.build(&drv).await.unwrap();
        assert!(result.is_success());

        // Check that the reference was recorded via MockStore's internal state
        let paths = store.paths.lock().unwrap();
        assert!(paths.contains_key(&output_str));
        let info = &paths[&output_str];
        assert!(info.references.contains(&input_path));
    }

    // ── build_closure tests ──────────────────────────────────────

    /// Make a BuildClosure from a list of (drv_path, derivation) pairs.
    fn depends_on(mut drv: Derivation, input: &str) -> Derivation {
        drv.input_derivations.insert(input.to_string(), vec!["out".to_string()]);
        drv
    }

    fn make_closure(drvs: Vec<(&str, Derivation)>) -> BuildClosure {
        BuildClosure {
            derivations: drvs
                .into_iter()
                .map(|(p, d)| (p.to_string(), d))
                .collect(),
        }
    }

    /// MockStore that also implements add_to_store for substitution tests.
    struct SubMockStore {
        paths: Mutex<BTreeMap<String, PathInfo>>,
    }

    impl SubMockStore {
        fn new() -> Self {
            Self {
                paths: Mutex::new(BTreeMap::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Store for SubMockStore {
        async fn query_path_info(
            &self,
            path: &StorePath,
        ) -> sui_store::traits::StoreResult<Option<PathInfo>> {
            let abs = path.to_absolute_path();
            Ok(self.paths.lock().unwrap().get(&abs).cloned())
        }

        async fn is_valid_path(
            &self,
            path: &StorePath,
        ) -> sui_store::traits::StoreResult<bool> {
            let abs = path.to_absolute_path();
            Ok(self.paths.lock().unwrap().contains_key(&abs))
        }

        async fn query_all_valid_paths(
            &self,
        ) -> sui_store::traits::StoreResult<Vec<StorePath>> {
            Ok(Vec::new())
        }

        async fn register_path(
            &self,
            info: &PathInfo,
        ) -> sui_store::traits::StoreResult<()> {
            self.paths
                .lock()
                .unwrap()
                .insert(info.path.clone(), info.clone());
            Ok(())
        }

        async fn add_nar_at(
            &self,
            info: &PathInfo,
            _nar_data: &[u8],
        ) -> sui_store::traits::StoreResult<()> {
            self.register_path(info).await
        }

        async fn add_to_store(
            &self,
            name: &str,
            _nar_data: &[u8],
            references: &[String],
        ) -> sui_store::traits::StoreResult<PathInfo> {
            let path = format!("/nix/store/mock-{name}");
            let info = PathInfo {
                path: path.clone(),
                nar_hash: "sha256:mock".to_string(),
                nar_size: 100,
                references: references.to_vec(),
                ..PathInfo::default()
            };
            self.paths.lock().unwrap().insert(path, info.clone());
            Ok(info)
        }
    }

    #[tokio::test]
    async fn a_stale_unregistered_output_is_cleared_before_building() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("debris"), "left by an interrupted build").unwrap();

        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store, Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());
        let drv = make_drv(
            "/bin/sh",
            &["-c", &format!("/bin/mkdir {} && echo ok > {}/file", out.display(), out.display())],
            out.to_str().unwrap(),
        );

        let result = builder.build(&drv).await.unwrap();
        assert!(result.is_success(), "{}", result.log);
        assert!(!out.join("debris").exists());
    }

    async fn overlapping(jobs: usize) -> bool {
        let tmp = tempfile::tempdir().unwrap();
        let builder = LocalBuilder::new(Arc::new(MockStore::new()), Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string())
            .with_max_jobs(jobs);
        let dir = tmp.path().display().to_string();
        let script = |me: &str, other: &str, out: &str| {
            format!("echo > {dir}/{me}.started; /bin/sleep 1; if [ -e {dir}/{other}.started ] && [ ! -e {dir}/{other}.done ]; then echo > {dir}/overlap; fi; echo > {dir}/{me}.done; echo hi > {out}")
        };
        let (out_a, out_b) = (tmp.path().join("a"), tmp.path().join("b"));
        let (a, b) = (script("a", "b", out_a.to_str().unwrap()), script("b", "a", out_b.to_str().unwrap()));
        let (drv_a, drv_b) = ("/nix/store/0123456789abcdfghijklmnpqrsvwxa-a.drv", "/nix/store/0123456789abcdfghijklmnpqrsvwxb-b.drv");
        let out_root = tmp.path().join("root");
        let mut root = make_drv("/bin/sh", &["-c", &format!("echo hi > {}", out_root.display())], out_root.to_str().unwrap());
        root.input_derivations.insert(drv_a.to_string(), vec!["out".to_string()]);
        root.input_derivations.insert(drv_b.to_string(), vec!["out".to_string()]);
        let closure = make_closure(vec![
            (drv_a, make_drv("/bin/sh", &["-c", &a], out_a.to_str().unwrap())),
            (drv_b, make_drv("/bin/sh", &["-c", &b], out_b.to_str().unwrap())),
            ("/nix/store/0123456789abcdfghijklmnpqrsvwxc-root.drv", root),
        ]);
        let result = builder.build_closure(&closure, None).await.unwrap();
        assert!(result.success, "{}", result.log);
        assert!(out_a.exists() && out_b.exists(), "{}", result.log);
        tmp.path().join("overlap").exists()
    }

    #[tokio::test]
    async fn independent_builds_run_together_up_to_max_jobs_and_one_at_a_time_at_one() {
        assert!(overlapping(2).await);
        assert!(!overlapping(1).await);
    }

    #[test]
    fn max_jobs_reads_like_cppnix() {
        assert_eq!(parse_max_jobs(None), 1);
        assert_eq!(parse_max_jobs(Some("6")), 6);
        assert!(parse_max_jobs(Some("auto")) >= 1);
    }

    #[tokio::test]
    async fn a_closure_build_reports_cppnix_build_activities_log_lines_and_phases() {
        use sui_compat::activity::{ActivityEvent, Field, ResultType};
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink: SharedSink = events.clone();
        let builder = LocalBuilder::new(Arc::new(MockStore::new()), Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string())
            .with_sink(sink);
        let script = format!(
            "exec 1>&2; echo one; echo '@nix {{ \"action\": \"setPhase\", \"phase\": \"buildPhase\" }}'; printf 'two'; echo hi > {}",
            out.display()
        );
        let drv_path = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-t.drv";
        let closure = make_closure(vec![(drv_path, make_drv("/bin/sh", &["-c", &script], out.to_str().unwrap()))]);
        let result = builder.build_closure(&closure, None).await.unwrap();
        assert!(result.success, "{}", result.log);
        let events = events.lock().unwrap().clone();
        let start = |kind: ActivityType| {
            events
                .iter()
                .find_map(|e| match e {
                    ActivityEvent::Start { id, activity, text, fields, parent, level } if *activity == kind => {
                        Some((*id, text.clone(), fields.clone(), *parent, *level))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no {kind:?} in {events:?}"))
        };
        let builds = start(ActivityType::Builds).0;
        let (build, text, fields, parent, level) = start(ActivityType::Build);
        assert_eq!(text, format!("building '{drv_path}'"));
        assert_eq!(fields, vec![Field::from(drv_path), Field::from(""), Field::Int(1), Field::Int(1)]);
        assert_eq!((parent, level), (0, Verbosity::Info));
        let on_build: Vec<(ResultType, Vec<Field>)> = events
            .iter()
            .filter_map(|e| match e {
                ActivityEvent::Result { id, result, fields } if *id == build => Some((*result, fields.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            on_build,
            vec![
                (ResultType::BuildLogLine, vec![Field::from("one")]),
                (ResultType::SetPhase, vec![Field::from("buildPhase")]),
                (ResultType::BuildLogLine, vec![Field::from("two")]),
            ]
        );
        let at = |e: &ActivityEvent| events.iter().position(|x| x == e).unwrap();
        let progress = |v: [u64; 4]| ActivityEvent::Result {
            id: builds,
            result: ResultType::Progress,
            fields: v.iter().map(|n| Field::Int(*n)).collect(),
        };
        assert!(at(&progress([0, 1, 1, 0])) < at(&progress([1, 1, 0, 0])));
        assert!(at(&progress([1, 1, 0, 0])) < at(&ActivityEvent::Stop { id: build }));
        assert_eq!(events.last(), Some(&ActivityEvent::Stop { id: start(ActivityType::Realise).0 }));
    }

    #[tokio::test]
    async fn a_failed_build_leaves_no_partial_output() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store, Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());
        let drv = make_drv(
            "/bin/sh",
            &["-c", &format!("/bin/mkdir {} && exit 1", out.display())],
            out.to_str().unwrap(),
        );

        let result = builder.build(&drv).await.unwrap();
        assert!(!result.is_success());
        assert!(!out.exists());
    }

    #[tokio::test]
    async fn an_input_wanted_for_out_ignores_its_absent_dev_output() {
        let tmp = tempfile::tempdir().unwrap();
        let dep_out = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-dep-1.0";
        let dep_dev = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2-dep-1.0-dev";
        let target_out = tmp.path().join("target");

        let store = Arc::new(MockStore::new().with_path(PathInfo::new(dep_out, "sha256:a")));
        let builder = LocalBuilder::new(store.clone(), Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());

        let mut dep = make_drv("/bin/sh", &["-c", "exit 1"], dep_out);
        dep.outputs.insert(
            "dev".to_string(),
            DerivationOutput { path: dep_dev.to_string(), hash_algo: String::new(), hash: String::new() },
        );
        let target = depends_on(
            make_drv("/bin/sh", &["-c", &format!("echo t > {}", target_out.display())], target_out.to_str().unwrap()),
            "dep.drv",
        );
        let closure = make_closure(vec![("dep.drv", dep), ("target.drv", target)]);

        let result = builder.build_closure(&closure, None).await.unwrap();
        assert!(result.is_success(), "{}", result.log);
        assert!(result.log.contains("1 built"));
        assert_eq!(store.paths.lock().unwrap()[target_out.to_str().unwrap()].deriver.as_deref(), Some("target.drv"));
    }

    #[tokio::test]
    async fn a_dry_plan_names_what_would_build_and_runs_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dep_out = tmp.path().join("dep");
        let target_out = tmp.path().join("target");
        let present = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-present-1.0";
        let store = MockStore::new().with_path(PathInfo::new(present, "sha256:a"));

        let closure = make_closure(vec![
            ("present.drv", make_drv("/bin/sh", &["-c", "exit 1"], present)),
            ("dep.drv", make_drv("/bin/sh", &["-c", &format!("echo d > {}", dep_out.display())], dep_out.to_str().unwrap())),
            (
                "target.drv",
                depends_on(
                    depends_on(
                        make_drv("/bin/sh", &["-c", &format!("echo t > {}", target_out.display())], target_out.to_str().unwrap()),
                        "dep.drv",
                    ),
                    "present.drv",
                ),
            ),
        ]);

        let plan = dry_plan(&store, &closure, None).await.unwrap();
        assert_eq!(plan.to_build, vec!["dep.drv".to_string(), "target.drv".to_string()]);
        assert!(plan.to_fetch.is_empty());
        assert!(!dep_out.exists());
        assert!(!target_out.exists());
    }

    #[tokio::test]
    async fn the_builder_sees_its_build_dir_with_every_symlink_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let out = tmp.path().join("out");
        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store, Box::new(NoSandbox))
            .with_build_dir(link.to_str().unwrap().to_string());
        let drv = make_drv("/bin/sh", &["-c", &format!("echo $NIX_BUILD_TOP > {}", out.display())], out.to_str().unwrap());

        let result = builder.build(&drv).await.unwrap();
        assert!(result.is_success(), "{}", result.log);
        let top = std::fs::read_to_string(&out).unwrap();
        let canon_real = std::fs::canonicalize(&real).unwrap();
        assert!(top.trim().starts_with(canon_real.to_str().unwrap()), "NIX_BUILD_TOP={top}");
    }

    /// Byte parity with cppnix for every Rust crate with a build script
    /// (cid 2026-10-26: rust_serde_core-1.0.229-lib differed from two cppnix
    /// rebuilds, and serde_json's rmeta then named another serde_core SVH).
    ///
    /// The rlib records the path of `include!(concat!(env!("OUT_DIR"), …))`.
    /// buildRustCrate passes `--remap-path-prefix=$NIX_BUILD_TOP=/` and
    /// `-Z remap-cwd-prefix=.`; OUT_DIR is `$(pwd)/target/build/…` (the
    /// shell's logical cwd) and rustc's cwd remap uses the physical cwd
    /// (getcwd). cppnix's build dir has no symlink in it, so all three are one
    /// string and the file is recorded as `./target/build/serde_core.out/private.rs`.
    /// cid's bytes record `/serde_core-1.0.229/target/build/serde_core.out/private.rs`:
    /// the cwd remap missed because the logical and physical cwd differed (a
    /// build dir under the `/var` symlink, before b84c70e). Measured on ryn
    /// 2026-10-26: sui's `--check` rebuild NAR-hashes to cppnix's own
    /// rebuild (sha256:7fe97707…), and both differ from cid's (d6812faa…).
    #[tokio::test]
    async fn the_builder_cwd_is_one_string_logically_physically_and_as_nix_build_top() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let out = tmp.path().join("out");
        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store, Box::new(NoSandbox)).with_build_dir(link.to_str().unwrap().to_string());
        let script = format!(
            "cd \"$NIX_BUILD_TOP\" && /bin/mkdir src && cd src && printf '%s\\n%s\\n%s\\n' \"$NIX_BUILD_TOP/src\" \"$(pwd)\" \"$(pwd -P)\" > {}",
            out.display()
        );
        let drv = make_drv("/bin/sh", &["-c", &script], out.to_str().unwrap());
        let result = builder.build(&drv).await.unwrap();
        assert!(result.is_success(), "{}", result.log);
        let seen = std::fs::read_to_string(&out).unwrap();
        let lines: Vec<&str> = seen.lines().collect();
        assert_eq!(lines.len(), 3, "{seen}");
        assert!(
            lines[0] == lines[1] && lines[1] == lines[2],
            "NIX_BUILD_TOP-relative, logical and physical cwd differ, so rustc's two remaps disagree: {lines:?}"
        );
    }

    #[tokio::test]
    async fn build_closure_present_target_never_builds_its_inputs() {
        let out1 = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-dep-1.0";
        let out2 = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1-target-1.0";

        let store = Arc::new(MockStore::new().with_path(PathInfo::new(out2, "sha256:b")));
        let builder = LocalBuilder::new(store, Box::new(NoSandbox));

        let closure = make_closure(vec![
            ("dep.drv", make_drv("/bin/sh", &["-c", "exit 1"], out1)),
            ("target.drv", depends_on(make_drv("/bin/sh", &["-c", "true"], out2), "dep.drv")),
        ]);

        let result = builder.build_closure(&closure, None).await.unwrap();
        assert!(result.is_success());
        assert!(result.log.contains("1 skipped"));
        assert!(result.log.contains("0 substituted"));
        assert!(result.log.contains("0 built"));
    }

    #[tokio::test]
    async fn build_closure_no_substitutor_builds_locally() {
        let tmp = tempfile::tempdir().unwrap();
        let out1 = tmp.path().join("out1");
        let out2 = tmp.path().join("out2");
        let out1_str = out1.to_str().unwrap().to_string();
        let out2_str = out2.to_str().unwrap().to_string();

        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store, Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());

        let closure = make_closure(vec![
            (
                "dep.drv",
                make_drv(
                    "/bin/sh",
                    &["-c", &format!("echo dep > {}", out1.display())],
                    &out1_str,
                ),
            ),
            (
                "target.drv",
                depends_on(
                    make_drv(
                        "/bin/sh",
                        &["-c", &format!("echo target > {}", out2.display())],
                        &out2_str,
                    ),
                    "dep.drv",
                ),
            ),
        ]);

        let result = builder.build_closure(&closure, None).await.unwrap();
        assert!(result.is_success());
        assert!(result.log.contains("0 skipped"));
        assert!(result.log.contains("0 substituted"));
        assert!(result.log.contains("2 built"));
    }

    #[tokio::test]
    async fn build_closure_failure_stops_processing() {
        let tmp = tempfile::tempdir().unwrap();
        let out1 = tmp.path().join("fail-out");
        let out2 = tmp.path().join("never-reached");

        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store, Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());

        let closure = make_closure(vec![
            (
                "fail.drv",
                make_drv("/bin/sh", &["-c", "exit 1"], out1.to_str().unwrap()),
            ),
            (
                "target.drv",
                depends_on(
                    make_drv(
                        "/bin/sh",
                        &["-c", &format!("echo ok > {}", out2.display())],
                        out2.to_str().unwrap(),
                    ),
                    "fail.drv",
                ),
            ),
        ]);

        let result = builder.build_closure(&closure, None).await.unwrap();
        assert!(!result.is_success());
        assert!(result.log.contains("FAILED: fail.drv"));
        // The second derivation should not have been attempted
        assert!(!out2.exists());
    }

    #[tokio::test]
    async fn build_closure_mixed_skip_and_build() {
        let tmp = tempfile::tempdir().unwrap();
        let out1 = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-existing";
        let out2 = tmp.path().join("new-out");
        let out2_str = out2.to_str().unwrap().to_string();

        let store = Arc::new(MockStore::new()
            .with_path(PathInfo::new(out1, "sha256:a")));
        let builder = LocalBuilder::new(store, Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());

        let closure = make_closure(vec![
            ("existing.drv", make_drv("/bin/sh", &["-c", "true"], out1)),
            (
                "new.drv",
                depends_on(
                    make_drv(
                        "/bin/sh",
                        &["-c", &format!("echo new > {}", out2.display())],
                        &out2_str,
                    ),
                    "existing.drv",
                ),
            ),
        ]);

        let result = builder.build_closure(&closure, None).await.unwrap();
        assert!(result.is_success());
        assert!(result.log.contains("1 skipped"));
        assert!(result.log.contains("1 built"));
    }

    #[tokio::test]
    async fn build_closure_with_substitutor_substitutes() {
        // Set up a mock HTTP client + binary cache + substitutor
        use sui_store::http::{HttpClient as HttpClientTrait, HttpError as HErr, HttpResponse as HResp};

        let out_path = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1";
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";

        // Create NAR bytes and narinfo
        let nar_bytes = {
            use sui_compat::nar::{NarNode, NarWriter};
            let node = NarNode::Regular {
                executable: false,
                contents: b"hello".to_vec(),
            };
            let mut buf = Vec::new();
            NarWriter::write(&mut buf, &node).unwrap();
            buf
        };

        // ── ★ THE NARINFO MUST SURVIVE TWO REAL GATES ───────────────────
        // Substitution refuses a path that is not both TRUSTED and
        // BYTE-INTACT (`substitute.rs` steps 2 and 5). This fixture predates
        // both: it advertised `NarHash: sha256:bbbb` over bytes that hash to
        // something else, and `Sig: key:sig` signed by nobody. Correctly
        // refused, sui fell back to BUILDING — and the build fails, because
        // the fixture's builder is `/bin/sh -c true`, which creates no
        // output. So a security guard doing its job presented as "the
        // substitutor is broken".
        //
        // Fixed by making the fixture honest rather than by weakening the
        // gate: the NarHash is computed from the very bytes the mock serves,
        // and the cache is built with `require_signatures(false)` — the
        // FIRST acceptance branch, which is a real operator posture (a
        // trusted local cache), not a bypass. The byte-integrity gate still
        // runs, and still has to pass.
        // `NixHash`'s Display is exactly `algo:hex`, which is the form
        // `verify_nar_hash` parses — the project's own primitive rather than
        // a hand-rolled encoding that could drift from it.
        let real_nar_hash = sui_compat::hash::NixHash::new(
            sui_compat::hash::HashAlgorithm::Sha256,
            Sha256::digest(&nar_bytes).to_vec(),
        )
        .to_string();
        let narinfo_text = format!(
            "StorePath: {out_path}\n\
             URL: nar/{hash}.nar.none\n\
             Compression: none\n\
             FileHash: {real_nar_hash}\n\
             FileSize: {}\n\
             NarHash: {real_nar_hash}\n\
             NarSize: {}\n\
             References: \n",
            nar_bytes.len(),
            nar_bytes.len()
        );

        // Build a mock HTTP client
        struct ClosureTestHttpClient {
            narinfo_url: String,
            narinfo_body: String,
            nar_url: String,
            nar_data: Vec<u8>,
        }

        #[async_trait::async_trait]
        impl HttpClientTrait for ClosureTestHttpClient {
            async fn get(&self, url: &str, _h: &[(&str, &str)]) -> Result<HResp, HErr> {
                if url == self.narinfo_url {
                    Ok(HResp { status: 200, body: self.narinfo_body.clone() })
                } else {
                    Ok(HResp { status: 404, body: "not found".to_string() })
                }
            }
            async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, HErr> {
                if url == self.nar_url {
                    Ok(self.nar_data.clone())
                } else {
                    Err(HErr::Request(format!("not found: {url}")))
                }
            }
        }

        let http_client = ClosureTestHttpClient {
            narinfo_url: format!("https://test.cache/{hash}.narinfo"),
            narinfo_body: narinfo_text,
            nar_url: format!("https://test.cache/nar/{hash}.nar.none"),
            nar_data: nar_bytes,
        };

        let cache = Arc::new(
            sui_store::BinaryCacheStore::builder("https://test.cache")
                .http_client(Box::new(http_client))
                .require_signatures(false)
                .build(),
        );

        // Use SubMockStore for the substitutor's local store (supports add_to_store)
        let sub_store: Arc<dyn Store> = Arc::new(SubMockStore::new());
        let substitutor = Substitutor::new(sub_store.clone(), vec![cache]);

        // Use the same store for the builder
        let builder = LocalBuilder::new(sub_store, Box::new(NoSandbox));

        let closure = make_closure(vec![
            ("hello.drv", make_drv("/bin/sh", &["-c", "true"], out_path)),
        ]);

        let result = builder.build_closure(&closure, Some(&substitutor)).await.unwrap();
        // Assertions carry their evidence: a bare `assert!(is_success())`
        // reports only that the build was not successful, which is the one
        // thing already known. The log names WHICH derivation failed and why.
        assert!(
            result.is_success(),
            "closure build should succeed via substitution; log:\n{}",
            result.log
        );
        assert!(
            result.log.contains("1 substituted"),
            "expected the path to be substituted, not built; log:\n{}",
            result.log
        );
        assert!(
            result.log.contains("0 built"),
            "expected nothing to be built locally; log:\n{}",
            result.log
        );
    }

    #[tokio::test]
    async fn build_closure_substitutor_not_found_falls_back_to_build() {
        use sui_store::http::{HttpClient as HttpClientTrait, HttpError as HErr, HttpResponse as HResp};

        let tmp = tempfile::tempdir().unwrap();
        let out_path = tmp.path().join("built-out");
        let out_str = out_path.to_str().unwrap().to_string();

        // Empty cache — nothing in it
        struct EmptyHttpClient;

        #[async_trait::async_trait]
        impl HttpClientTrait for EmptyHttpClient {
            async fn get(&self, _url: &str, _h: &[(&str, &str)]) -> Result<HResp, HErr> {
                Ok(HResp { status: 404, body: "not found".to_string() })
            }
            async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, HErr> {
                Err(HErr::Request(format!("not found: {url}")))
            }
        }

        let cache = Arc::new(
            sui_store::BinaryCacheStore::builder("https://empty.cache")
                .http_client(Box::new(EmptyHttpClient))
                .build(),
        );

        let store: Arc<dyn Store> = Arc::new(MockStore::new());
        let substitutor = Substitutor::new(store.clone(), vec![cache]);
        let builder = LocalBuilder::new(store, Box::new(NoSandbox))
            .with_build_dir(tmp.path().join("build").to_str().unwrap().to_string());

        let closure = make_closure(vec![
            (
                "build-me.drv",
                make_drv(
                    "/bin/sh",
                    &["-c", &format!("echo built > {}", out_path.display())],
                    &out_str,
                ),
            ),
        ]);

        let result = builder.build_closure(&closure, Some(&substitutor)).await.unwrap();
        assert!(result.is_success());
        assert!(result.log.contains("0 substituted"));
        assert!(result.log.contains("1 built"));
    }

    #[tokio::test]
    async fn build_closure_single_element() {
        // BuildClosure::compute never returns empty, so target() is safe.
        // Verify that a single-element closure works correctly.
        let out = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-single";
        let store = Arc::new(MockStore::new().with_path(PathInfo::new(out, "sha256:a")));
        let builder = LocalBuilder::new(store, Box::new(NoSandbox));
        let closure = make_closure(vec![
            ("single.drv", make_drv("/bin/sh", &["-c", "true"], out)),
        ]);

        let result = builder.build_closure(&closure, None).await.unwrap();
        assert!(result.is_success());
        assert!(result.log.contains("1 skipped"));
    }
}

#[derive(Default)]
struct ClosurePlan {
    to_build: std::collections::HashSet<String>,
    to_fetch: std::collections::BTreeMap<String, (u64, u64)>,
    present: u32,
    substituted: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlanMode {
    Realize,
    Query,
}

/// What realizing a closure would do, decided without writing the store.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DryPlan {
    /// Derivations that would be built here, sorted.
    pub to_build: Vec<String>,
    /// Store paths that would be fetched from a cache, sorted.
    pub to_fetch: Vec<String>,
    /// Compressed bytes the fetches download.
    pub download_size: u64,
    /// Bytes the fetches unpack to.
    pub nar_size: u64,
}

/// The plan [`LocalBuilder::build_closure`] would follow, without substituting or building.
///
/// # Errors
///
/// When the closure has no target derivation.
pub async fn dry_plan(
    store: &dyn Store,
    closure: &BuildClosure,
    substitutor: Option<&Substitutor>,
) -> Result<DryPlan, BuildError> {
    let plan = walk_plan(store, closure, substitutor, PlanMode::Query, None).await?;
    let mut to_build: Vec<String> = plan.to_build.into_iter().collect();
    to_build.sort();
    let (download_size, nar_size) =
        plan.to_fetch.values().fold((0, 0), |(d, n), (dd, nn)| (d + dd, n + nn));
    Ok(DryPlan { to_build, to_fetch: plan.to_fetch.into_keys().collect(), download_size, nar_size })
}

fn topo_sort_outputs(outputs: Vec<(String, PathInfo)>) -> Result<Vec<(String, PathInfo)>, String> {
    fn visit(
        i: usize,
        parent: Option<usize>,
        outputs: &[(String, PathInfo)],
        state: &mut [u8],
        order: &mut Vec<usize>,
    ) -> Result<(), String> {
        match state[i] {
            2 => return Ok(()),
            1 => {
                let from = parent.map_or(String::new(), |p| outputs[p].1.path.clone());
                return Err(format!("cycle detected in the references of '{}' from '{from}'", outputs[i].1.path));
            }
            _ => {}
        }
        state[i] = 1;
        let me = &outputs[i].1;
        for r in &me.references {
            if *r == me.path {
                continue;
            }
            if let Some(j) = outputs.iter().position(|(_, o)| o.path == *r) {
                visit(j, Some(i), outputs, state, order)?;
            }
        }
        state[i] = 2;
        order.push(i);
        Ok(())
    }
    let mut state = vec![0u8; outputs.len()];
    let mut order = Vec::with_capacity(outputs.len());
    for i in 0..outputs.len() {
        visit(i, None, &outputs, &mut state, &mut order)?;
    }
    let mut slots: Vec<Option<(String, PathInfo)>> = outputs.into_iter().map(Some).collect();
    Ok(order.into_iter().filter_map(|i| slots[i].take()).collect())
}

async fn output_paths_present(store: &dyn Store, paths: &[String]) -> bool {
    for path in paths {
        let Ok(sp) = StorePath::from_absolute_path(path) else {
            return false;
        };
        if !store.is_valid_path(&sp).await.unwrap_or(false) {
            return false;
        }
    }
    true
}

async fn substitute_paths(substitutor: &Substitutor, paths: &[String], worker: Option<&Worker>) -> bool {
    for path in paths {
        let Ok(sp) = StorePath::from_absolute_path(path) else {
            return false;
        };
        let substituted = match worker {
            Some(worker) => substitutor.substitute_observed(&sp, worker).await,
            None => substitutor.substitute(&sp).await,
        };
        match substituted {
            Ok(SubstituteResult::Substituted { .. } | SubstituteResult::AlreadyPresent) => {}
            _ => return false,
        }
    }
    true
}

fn max_jobs() -> usize {
    let configured = sui_compat::nix_conf::NixConfig::load().ok().and_then(|c| c.get("max-jobs").map(str::to_string));
    parse_max_jobs(std::env::var("SUI_MAX_JOBS").ok().as_deref().or(configured.as_deref()))
}

fn parse_max_jobs(value: Option<&str>) -> usize {
    let auto = || std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    match value.map(str::trim) {
        Some("auto") => auto(),
        Some(n) => n.parse::<usize>().ok().filter(|n| *n > 0).unwrap_or(1),
        None => 1,
    }
}

fn http_connections() -> usize {
    std::env::var("SUI_HTTP_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(25)
}

async fn query_fetches(
    substitutor: &Substitutor,
    paths: &[String],
    fetch: &mut std::collections::BTreeMap<String, (u64, u64)>,
) -> bool {
    use futures::stream::{self, StreamExt};
    let mut found = Vec::new();
    let mut wave: Vec<String> = paths.to_vec();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    while !wave.is_empty() {
        let mut batch = Vec::new();
        for path in wave.drain(..) {
            if fetch.contains_key(&path) || !seen.insert(path.clone()) {
                continue;
            }
            let Ok(sp) = StorePath::from_absolute_path(&path) else {
                return false;
            };
            batch.push((path, sp));
        }
        let answers: Vec<(String, Result<Availability, _>)> = stream::iter(batch)
            .map(|(path, sp)| async move {
                let answer = substitutor.query(&sp).await;
                (path, answer)
            })
            .buffer_unordered(http_connections())
            .collect()
            .await;
        for (path, answer) in answers {
            match answer {
                Ok(Availability::Present) => {}
                Ok(Availability::Substitutable { download_size, nar_size, references, .. }) => {
                    wave.extend(references.into_iter().filter(|r| r != &path));
                    found.push((path, (download_size, nar_size)));
                }
                Ok(Availability::Missing) | Err(_) => {
                    if paths.contains(&path) {
                        return false;
                    }
                }
            }
        }
    }
    fetch.extend(found);
    true
}

enum Decision {
    Present,
    Fetched(std::collections::BTreeMap<String, (u64, u64)>),
    Build,
}

async fn decide(
    store: &dyn Store,
    substitutor: Option<&Substitutor>,
    mode: PlanMode,
    paths: &[String],
    worker: Option<&Worker>,
) -> Decision {
    if output_paths_present(store, paths).await {
        return Decision::Present;
    }
    if let Some(sub) = substitutor {
        let mut fetched = std::collections::BTreeMap::new();
        let ok = match mode {
            PlanMode::Realize => substitute_paths(sub, paths, worker).await,
            PlanMode::Query => query_fetches(sub, paths, &mut fetched).await,
        };
        if ok {
            return Decision::Fetched(fetched);
        }
    }
    Decision::Build
}

async fn walk_plan(
    store: &dyn Store,
    closure: &BuildClosure,
    substitutor: Option<&Substitutor>,
    mode: PlanMode,
    worker: Option<&Worker>,
) -> Result<ClosurePlan, BuildError> {
    use futures::stream::{self, StreamExt};
    let index: std::collections::HashMap<&str, &Derivation> =
        closure.derivations.iter().map(|(p, d)| (p.as_str(), d)).collect();
    let (target, target_drv) = closure.try_target()?;
    let mut plan = ClosurePlan::default();
    let mut handled: std::collections::HashMap<String, std::collections::BTreeSet<String>> =
        std::collections::HashMap::new();
    let mut wave: Vec<(String, Vec<String>)> =
        vec![(target.clone(), target_drv.outputs.keys().cloned().collect())];
    let width = match mode {
        PlanMode::Realize => 1,
        PlanMode::Query => http_connections(),
    };
    while !wave.is_empty() {
        let mut work: Vec<(String, Vec<String>)> = Vec::new();
        for (drv_path, wanted) in wave.drain(..) {
            let Some(drv) = index.get(drv_path.as_str()) else {
                continue;
            };
            let seen = handled.entry(drv_path.clone()).or_default();
            let fresh: Vec<String> = wanted.into_iter().filter(|o| seen.insert(o.clone())).collect();
            if fresh.is_empty() {
                continue;
            }
            let paths: Vec<String> = fresh
                .iter()
                .filter_map(|o| drv.outputs.get(o).map(|out| out.path.clone()))
                .collect();
            work.push((drv_path, paths));
        }
        let decided: Vec<(String, Decision)> = stream::iter(work)
            .map(|(drv_path, paths)| async move {
                let decision = decide(store, substitutor, mode, &paths, worker).await;
                (drv_path, decision)
            })
            .buffer_unordered(width)
            .collect()
            .await;
        for (drv_path, decision) in decided {
            match decision {
                Decision::Present => plan.present += 1,
                Decision::Fetched(fetched) => {
                    plan.substituted += 1;
                    plan.to_fetch.extend(fetched);
                }
                Decision::Build => {
                    if plan.to_build.insert(drv_path.clone()) {
                        for (input, outputs) in &index[drv_path.as_str()].input_derivations {
                            wave.push((input.clone(), outputs.clone()));
                        }
                    }
                }
            }
        }
    }
    Ok(plan)
}
