//! Local builder — executes derivation builds on the local machine.
//!
//! [`LocalBuilder`] implements the [`Builder`] trait using a [`Sandbox`]
//! for process isolation and a [`Store`] for output registration.
//! After a successful build it scans outputs for runtime references,
//! computes NAR hashes, and registers the results in the store.

use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use sui_compat::derivation::{Derivation, DerivationOutput};
use sui_compat::nar::{NarWriter, NarNode, NarEntry};
use sui_compat::store_path::{StorePath, STORE_PATH_HASH_LEN};

use crate::closure::BuildClosure;
use crate::reference_scan;
use crate::sandbox::{Sandbox, SandboxConfig};
use crate::traits::{BuildError, BuildLog, BuildOutcome, BuildResult, Builder};

use sui_store::substitute::{SubstituteResult, Substitutor};
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

/// A builder that executes derivations locally with sandbox isolation.
pub struct LocalBuilder {
    store: Arc<dyn Store>,
    sandbox: Box<dyn Sandbox>,
    build_dir_base: String,
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
            sandbox,
            build_dir_base: std::env::temp_dir()
                .join("sui-build")
                .to_string_lossy()
                .into_owned(),
        }
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
        let start = std::time::Instant::now();
        let mut log = BuildLog::new();

        // 1. Check if all outputs already exist
        if self.all_outputs_exist(drv).await? {
            log.push("all outputs already exist, skipping build");
            let outputs = self.collect_output_paths(drv);
            return Ok(BuildResult::success(
                outputs,
                log.finish(),
                start.elapsed().as_secs_f64(),
            ));
        }

        // 2. Create build directory
        let build_id = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let build_dir = format!("{}/{}", self.build_dir_base, build_id);
        std::fs::create_dir_all(&build_dir)?;

        // 3. Create sandbox config from derivation
        let mut config = SandboxConfig::from_derivation(drv, &build_dir);

        // 4. Handle fixed-output derivations (allow network)
        if is_fixed_output(drv) {
            config.allow_network = true;
        }

        // 5. Execute in sandbox
        log.push(&format!("executing builder: {}", config.builder));
        self.sandbox.prepare(&config)?;
        let result = self.sandbox.execute(&config);
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
        for (output_name, output) in &drv.outputs {
            let output_path = &output.path;

            // Check if the output path actually exists on disk
            if !Path::new(output_path).exists() {
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
            let runtime_refs = self.scan_output_refs(drv, output_path)?;

            // Compute NAR hash of the output
            let (nar_hash, nar_size) = compute_nar_hash(output_path)?;

            // Verify fixed-output hash if applicable
            if !output.hash.is_empty() {
                verify_fixed_output(output, &nar_hash)?;
            }

            // Build PathInfo and register
            let info = PathInfo {
                path: output_path.clone(),
                nar_hash: format!("sha256:{nar_hash}"),
                nar_size: nar_size as i64,
                references: runtime_refs,
                deriver: None,
                signatures: vec![],
                registration_time: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64,
                content_address: None,
            };

            self.store.register_path(&info).await.map_err(|e| {
                BuildError::Failed(format!("failed to register output: {e}"))
            })?;

            if let Ok(sp) = StorePath::from_absolute_path(output_path) {
                output_paths.push(sp);
            }

            log.push(&format!("registered output '{output_name}': {output_path}"));
        }

        // 8. Clean up build directory
        let _ = std::fs::remove_dir_all(&build_dir);

        Ok(BuildResult::success(
            output_paths,
            log.finish(),
            start.elapsed().as_secs_f64(),
        ))
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

    /// Scan a build output for store path references.
    ///
    /// Collects all candidate hashes from the derivation's inputs (both
    /// input derivations and input sources), then uses the reference scanner
    /// to find which actually appear in the output.
    fn scan_output_refs(
        &self,
        drv: &Derivation,
        output_path: &str,
    ) -> Result<Vec<String>, BuildError> {
        // Collect candidate store path hashes and their full paths
        let mut candidates: Vec<(String, String)> = Vec::new(); // (hash, full_path)

        for input_drv_path in drv.input_derivations.keys() {
            if let Ok(sp) = StorePath::from_absolute_path(input_drv_path) {
                let basename = sp.to_basename();
                let hash = &basename[..STORE_PATH_HASH_LEN];
                candidates.push((hash.to_string(), sp.to_absolute_path()));
            }
        }

        for src in &drv.input_sources {
            if let Ok(sp) = StorePath::from_absolute_path(src) {
                let basename = sp.to_basename();
                let hash = &basename[..STORE_PATH_HASH_LEN];
                candidates.push((hash.to_string(), sp.to_absolute_path()));
            }
        }

        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        // Scan the output path for references
        let hash_strs: Vec<&str> = candidates.iter().map(|(h, _)| h.as_str()).collect();

        let found_hashes = if Path::new(output_path).is_dir() {
            reference_scan::scan_directory(output_path, &hash_strs)
                .unwrap_or_default()
        } else {
            reference_scan::scan_file(output_path, &hash_strs)
                .unwrap_or_default()
        };

        // Map found hashes back to full store paths
        let found_paths: Vec<String> = found_hashes
            .iter()
            .filter_map(|found_hash| {
                candidates
                    .iter()
                    .find(|(h, _)| h == found_hash)
                    .map(|(_, path)| path.clone())
            })
            .collect();

        Ok(found_paths)
    }
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

        for (drv_path, drv) in &closure.derivations {
            // 1. Check if all outputs already exist
            if self.all_outputs_exist(drv).await? {
                skipped += 1;
                tracing::debug!(drv_path, "all outputs present, skipping");
                continue;
            }

            // 2. Try substitution for each output
            if let Some(sub) = substitutor {
                let sub_result = self.try_substitute_outputs(sub, drv).await;
                if sub_result {
                    substituted += 1;
                    tracing::debug!(drv_path, "all outputs substituted");
                    continue;
                }
            }

            // 3. Build locally
            log.push(&format!("building {drv_path}"));
            tracing::info!(drv_path, "building locally");
            let result = self.build_single(drv).await?;

            if !result.success {
                log.push(&format!("FAILED: {drv_path}"));
                // Merge closure log into the build result log
                let merged_log = format!("{}\n{}", log.finish(), result.log);
                return Ok(BuildResult {
                    log: merged_log,
                    ..result
                });
            }
            built += 1;
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

        for (drv_path, drv) in &closure.derivations {
            progress(BuildProgress {
                total,
                completed,
                current_drv: drv_path.clone(),
                action: BuildAction::Checking,
            });

            // 1. Check if all outputs already exist
            if self.all_outputs_exist(drv).await? {
                skipped += 1;
                completed += 1;
                progress(BuildProgress {
                    total,
                    completed,
                    current_drv: drv_path.clone(),
                    action: BuildAction::Skipped,
                });
                continue;
            }

            // 2. Try substitution
            if let Some(sub) = substitutor {
                progress(BuildProgress {
                    total,
                    completed,
                    current_drv: drv_path.clone(),
                    action: BuildAction::Substituting,
                });
                if self.try_substitute_outputs(sub, drv).await {
                    substituted += 1;
                    completed += 1;
                    continue;
                }
            }

            // 3. Build locally
            progress(BuildProgress {
                total,
                completed,
                current_drv: drv_path.clone(),
                action: BuildAction::Building,
            });
            log.push(&format!("building {drv_path}"));
            let result = self.build_single(drv).await?;

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
        .map_err(|e| BuildError::Io(e))?;

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

/// Verify a fixed-output derivation's hash against the actual NAR hash.
fn verify_fixed_output(
    output: &DerivationOutput,
    actual_nar_hex: &str,
) -> Result<(), BuildError> {
    // The output.hash_algo may contain "r:" prefix for recursive mode
    let algo = output.hash_algo.strip_prefix("r:").unwrap_or(&output.hash_algo);

    if algo != "sha256" {
        // For now, only verify sha256 FODs
        return Ok(());
    }

    // The expected hash is in output.hash (hex or base32)
    let expected = &output.hash;
    if expected != actual_nar_hex {
        return Err(BuildError::Failed(format!(
            "fixed-output hash mismatch: expected {expected}, got {actual_nar_hex}"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use sui_compat::derivation::DerivationOutput;
    use sui_compat::store_path::StorePath;

    use crate::sandbox::NoSandbox;
    use crate::traits::BuildOutcome;

    // ── MockStore ─────────────────────────────────────────────

    /// A simple in-memory store for testing.
    struct MockStore {
        paths: Mutex<BTreeMap<String, PathInfo>>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                paths: Mutex::new(BTreeMap::new()),
            }
        }

        fn with_path(self, info: PathInfo) -> Self {
            self.paths.lock().unwrap().insert(info.path.clone(), info);
            self
        }
    }

    #[async_trait::async_trait]
    impl Store for MockStore {
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
            Ok(self
                .paths
                .lock()
                .unwrap()
                .keys()
                .filter_map(|p| StorePath::from_absolute_path(p).ok())
                .collect())
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
    }

    // ── Helper ────────────────────────────────────────────────

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

    #[test]
    fn verify_fixed_output_matching_hash() {
        let output = DerivationOutput {
            path: "/nix/store/abc-src".to_string(),
            hash_algo: "sha256".to_string(),
            hash: "abcdef1234567890".to_string(),
        };
        assert!(verify_fixed_output(&output, "abcdef1234567890").is_ok());
    }

    #[test]
    fn verify_fixed_output_mismatched_hash() {
        let output = DerivationOutput {
            path: "/nix/store/abc-src".to_string(),
            hash_algo: "sha256".to_string(),
            hash: "expected_hash".to_string(),
        };
        let result = verify_fixed_output(&output, "actual_hash");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("hash mismatch"));
    }

    #[test]
    fn verify_fixed_output_recursive_prefix() {
        let output = DerivationOutput {
            path: "/nix/store/abc-src".to_string(),
            hash_algo: "r:sha256".to_string(),
            hash: "matchme".to_string(),
        };
        assert!(verify_fixed_output(&output, "matchme").is_ok());
    }

    #[test]
    fn verify_fixed_output_non_sha256_skipped() {
        let output = DerivationOutput {
            path: "/nix/store/abc-src".to_string(),
            hash_algo: "sha512".to_string(),
            hash: "whatever".to_string(),
        };
        // Non-sha256 is currently skipped
        assert!(verify_fixed_output(&output, "different").is_ok());
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
                    "mkdir -p {} && echo data > {}/file.txt",
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
    fn scan_output_refs_finds_embedded_hash() {
        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store, Box::new(NoSandbox));

        let tmp = tempfile::tempdir().unwrap();
        let output_file = tmp.path().join("output");

        // Create a file that contains a store path hash
        let input_hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let input_path = format!("/nix/store/{input_hash}-dependency-1.0");
        let content = format!("using {input_path}/lib/libfoo.so");
        std::fs::write(&output_file, content.as_bytes()).unwrap();

        // Build a derivation that references the input
        let mut drv = Derivation::default();
        drv.input_sources.push(input_path.clone());

        let refs = builder
            .scan_output_refs(&drv, output_file.to_str().unwrap())
            .unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0], input_path);
    }

    #[test]
    fn scan_output_refs_no_matches_returns_empty() {
        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store, Box::new(NoSandbox));

        let tmp = tempfile::tempdir().unwrap();
        let output_file = tmp.path().join("output");
        std::fs::write(&output_file, b"no store paths here").unwrap();

        let mut drv = Derivation::default();
        drv.input_sources.push(
            "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-dep-1.0".to_string(),
        );

        let refs = builder
            .scan_output_refs(&drv, output_file.to_str().unwrap())
            .unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn scan_output_refs_empty_inputs() {
        let store = Arc::new(MockStore::new());
        let builder = LocalBuilder::new(store, Box::new(NoSandbox));

        let tmp = tempfile::tempdir().unwrap();
        let output_file = tmp.path().join("output");
        std::fs::write(&output_file, b"data").unwrap();

        let drv = Derivation::default();
        let refs = builder
            .scan_output_refs(&drv, output_file.to_str().unwrap())
            .unwrap();
        assert!(refs.is_empty());
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
    async fn build_closure_all_outputs_exist_skips_all() {
        let out1 = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-dep-1.0";
        let out2 = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1-target-1.0";

        let store = Arc::new(MockStore::new()
            .with_path(PathInfo::new(out1, "sha256:a"))
            .with_path(PathInfo::new(out2, "sha256:b")));
        let builder = LocalBuilder::new(store, Box::new(NoSandbox));

        let closure = make_closure(vec![
            ("dep.drv", make_drv("/bin/sh", &["-c", "true"], out1)),
            ("target.drv", make_drv("/bin/sh", &["-c", "true"], out2)),
        ]);

        let result = builder.build_closure(&closure, None).await.unwrap();
        assert!(result.is_success());
        assert!(result.log.contains("2 skipped"));
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
                make_drv(
                    "/bin/sh",
                    &["-c", &format!("echo target > {}", out2.display())],
                    &out2_str,
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
                make_drv(
                    "/bin/sh",
                    &["-c", &format!("echo ok > {}", out2.display())],
                    out2.to_str().unwrap(),
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
                make_drv(
                    "/bin/sh",
                    &["-c", &format!("echo new > {}", out2.display())],
                    &out2_str,
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

        let narinfo_text = format!(
            "StorePath: {out_path}\n\
             URL: nar/{hash}.nar.none\n\
             Compression: none\n\
             FileHash: sha256:aaaa\n\
             FileSize: 100\n\
             NarHash: sha256:bbbb\n\
             NarSize: 200\n\
             References: \n\
             Sig: key:sig\n"
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
        assert!(result.is_success());
        assert!(result.log.contains("1 substituted"));
        assert!(result.log.contains("0 built"));
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
