//! Dockerfile build **equivalence checker** — Phase 4 of
//! `supa-charge-akeyless-ci`'s correctness half.
//!
//! Given two already-built image references (or two `docker save`
//! tarball paths — see [`filesystem::union_filesystem`]), this crate
//! answers "are these the same image, modulo the volatile bits every
//! build legitimately differs on (timestamps, per-run `/etc/hostname`,
//! …)?" via two independent checks, both driven through the exact same
//! [`sui_dockerfile_wrapper::command::CommandRunner`] seam the wrapper
//! itself uses (reused, not reinvented):
//!
//! 1. **Layer shape** — `docker history --no-trunc` layer count +
//!    instruction order ([`history::fetch_history`]).
//! 2. **Final filesystem content** — `docker save` + tar inspection,
//!    building the union filesystem each image's layers describe and
//!    diffing path-by-path, content-hash-by-content-hash, skipping any
//!    path on the caller's [`VolatileAllowlist`] ([`filesystem`]).
//!
//! The typed [`EquivalenceReport`] is the sole output — never a free
//! string diff.

pub mod filesystem;
pub mod history;

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sui_dockerfile_wrapper::command::{CommandRunError, CommandRunner};

/// One concrete way two images were found to differ. Never a `String`
/// blob — every variant names exactly what/where.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum TypedDifference {
    /// The two images have a different number of `docker history`
    /// layers.
    LayerCountMismatch { image_a_layers: usize, image_b_layers: usize },
    /// At a given layer index (0 = newest, matching `docker history`
    /// order), the two images' `CreatedBy` instruction text differs.
    InstructionMismatch { layer_index: usize, image_a_instruction: String, image_b_instruction: String },
    /// A path exists in image A's final filesystem but not image B's.
    FileOnlyInImageA { path: String },
    /// A path exists in image B's final filesystem but not image A's.
    FileOnlyInImageB { path: String },
    /// A path exists in both images but its content hash differs.
    FileContentMismatch { path: String, hash_a: String, hash_b: String },
}

/// The typed outcome of comparing two images — the sole output surface
/// of this crate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EquivalenceReport {
    /// `true` iff `differences` is empty.
    pub equivalent: bool,
    /// `true` iff the two images have the same `docker history` layer
    /// count (independent of whether the instructions themselves
    /// match — see [`TypedDifference::InstructionMismatch`] for that).
    pub layer_count_match: bool,
    pub differences: Vec<TypedDifference>,
}

/// A typed allowlist of paths that are expected to differ between two
/// otherwise-equivalent builds (per-build timestamps encoded into a
/// file's content, host-identity files docker itself writes, …) — an
/// exact-path set plus a prefix set, checked before any path is ever
/// reported as a [`TypedDifference`].
#[derive(Debug, Clone, Default)]
pub struct VolatileAllowlist {
    exact_paths: BTreeSet<String>,
    prefixes: Vec<String>,
}

impl VolatileAllowlist {
    /// The empty allowlist — every path difference is reported.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The default allowlist covering the well-known volatile paths
    /// every Docker build legitimately varies on: hostname/dns/mtab
    /// bind-mounts the daemon injects at container-create time, and
    /// the `.dockerenv` marker file.
    #[must_use]
    pub fn default_docker_volatile() -> Self {
        Self {
            exact_paths: [
                "etc/hostname".to_string(),
                "etc/resolv.conf".to_string(),
                "etc/mtab".to_string(),
                ".dockerenv".to_string(),
            ]
            .into_iter()
            .collect(),
            prefixes: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_exact_path(mut self, path: &str) -> Self {
        self.exact_paths.insert(path.to_string());
        self
    }

    #[must_use]
    pub fn with_prefix(mut self, prefix: &str) -> Self {
        self.prefixes.push(prefix.to_string());
        self
    }

    #[must_use]
    pub fn is_volatile(&self, path: &str) -> bool {
        self.exact_paths.contains(path) || self.prefixes.iter().any(|p| path.starts_with(p.as_str()))
    }
}

/// Errors comparing two images — never a panic.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("failed to spawn docker: {0}")]
    Command(#[from] CommandRunError),
    #[error("`docker {image}` failed: {stderr_tail}")]
    CommandFailed { image: String, stderr_tail: String },
    #[error("tar/IO error: {0}")]
    Tar(#[from] std::io::Error),
    #[error("manifest.json missing or missing layer `{layer_path}` — not a valid `docker save` tarball")]
    ManifestMissing { layer_path: String },
    #[error("manifest.json parse error: {0}")]
    ManifestParse(#[from] serde_json::Error),
}

/// Compare two **already-built, already-present-locally** images by
/// `image_ref` — runs `docker history` + `docker save` against both via
/// `runner`, using `workdir` to stage the two save tarballs.
///
/// # Errors
///
/// Returns [`VerifyError`] if either `docker history`/`docker save`
/// invocation fails to spawn or exits non-zero, or if a save tarball
/// isn't parseable as `docker save` output.
pub fn compare_images<R: CommandRunner>(
    runner: &R,
    image_a: &str,
    image_b: &str,
    allowlist: &VolatileAllowlist,
    workdir: &Path,
) -> Result<EquivalenceReport, VerifyError> {
    let history_a = history::fetch_history(runner, image_a)?;
    let history_b = history::fetch_history(runner, image_b)?;

    let tar_a = workdir.join("image_a.tar");
    let tar_b = workdir.join("image_b.tar");
    filesystem::save_image(runner, image_a, &tar_a)?;
    filesystem::save_image(runner, image_b, &tar_b)?;
    let fs_a = filesystem::union_filesystem(&tar_a)?;
    let fs_b = filesystem::union_filesystem(&tar_b)?;

    Ok(compare_parsed(&history_a, &history_b, &fs_a, &fs_b, allowlist))
}

/// The pure comparison core — separated from [`compare_images`] so
/// unit tests can drive it directly against canned histories +
/// filesystems without any `CommandRunner` at all.
#[must_use]
pub fn compare_parsed(
    history_a: &[String],
    history_b: &[String],
    fs_a: &filesystem::UnionFilesystem,
    fs_b: &filesystem::UnionFilesystem,
    allowlist: &VolatileAllowlist,
) -> EquivalenceReport {
    let mut differences = Vec::new();

    let layer_count_match = history_a.len() == history_b.len();
    if !layer_count_match {
        differences.push(TypedDifference::LayerCountMismatch {
            image_a_layers: history_a.len(),
            image_b_layers: history_b.len(),
        });
    }
    for (layer_index, (a, b)) in history_a.iter().zip(history_b.iter()).enumerate() {
        if a != b {
            differences.push(TypedDifference::InstructionMismatch {
                layer_index,
                image_a_instruction: a.clone(),
                image_b_instruction: b.clone(),
            });
        }
    }

    let all_paths: BTreeSet<&String> = fs_a.keys().chain(fs_b.keys()).collect();
    for path in all_paths {
        if allowlist.is_volatile(path) {
            continue;
        }
        match (fs_a.get(path), fs_b.get(path)) {
            (Some(a), Some(b)) => {
                if a.content_hash != b.content_hash {
                    differences.push(TypedDifference::FileContentMismatch {
                        path: path.clone(),
                        hash_a: a.content_hash.clone(),
                        hash_b: b.content_hash.clone(),
                    });
                }
            }
            (Some(_), None) => differences.push(TypedDifference::FileOnlyInImageA { path: path.clone() }),
            (None, Some(_)) => differences.push(TypedDifference::FileOnlyInImageB { path: path.clone() }),
            (None, None) => unreachable!("path came from the union of fs_a and fs_b keys"),
        }
    }

    EquivalenceReport { equivalent: differences.is_empty(), layer_count_match, differences }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filesystem::FileState;
    use std::collections::BTreeMap;

    fn fs_with(entries: &[(&str, &str)]) -> filesystem::UnionFilesystem {
        entries
            .iter()
            .map(|(p, h)| ((*p).to_string(), FileState { content_hash: (*h).to_string() }))
            .collect::<BTreeMap<_, _>>()
    }

    #[test]
    fn identical_histories_and_filesystems_are_equivalent() {
        let history = vec!["CMD [\"true\"]".to_string(), "FROM debian".to_string()];
        let fs = fs_with(&[("etc/os-release", "aaa")]);
        let report = compare_parsed(&history, &history, &fs, &fs, &VolatileAllowlist::none());
        assert!(report.equivalent);
        assert!(report.layer_count_match);
        assert!(report.differences.is_empty());
    }

    #[test]
    fn layer_count_mismatch_is_reported_but_does_not_short_circuit_fs_diff() {
        let a = vec!["FROM debian".to_string()];
        let b = vec!["FROM debian".to_string(), "RUN extra".to_string()];
        let fs = fs_with(&[]);
        let report = compare_parsed(&a, &b, &fs, &fs, &VolatileAllowlist::none());
        assert!(!report.equivalent);
        assert!(!report.layer_count_match);
        assert!(matches!(
            report.differences[0],
            TypedDifference::LayerCountMismatch { image_a_layers: 1, image_b_layers: 2 }
        ));
    }

    #[test]
    fn allowlisted_volatile_path_is_not_reported() {
        let history = vec!["FROM debian".to_string()];
        let fs_a = fs_with(&[("etc/hostname", "aaa")]);
        let fs_b = fs_with(&[("etc/hostname", "bbb")]);
        let report = compare_parsed(&history, &history, &fs_a, &fs_b, &VolatileAllowlist::default_docker_volatile());
        assert!(report.equivalent, "etc/hostname must be allowlisted by default");
    }

    #[test]
    fn non_allowlisted_content_mismatch_is_reported() {
        let history = vec!["FROM debian".to_string()];
        let fs_a = fs_with(&[("opt/app/bin", "aaa")]);
        let fs_b = fs_with(&[("opt/app/bin", "bbb")]);
        let report = compare_parsed(&history, &history, &fs_a, &fs_b, &VolatileAllowlist::none());
        assert!(!report.equivalent);
        assert_eq!(report.differences.len(), 1);
        assert!(matches!(&report.differences[0], TypedDifference::FileContentMismatch { path, .. } if path == "opt/app/bin"));
    }

    #[test]
    fn file_only_in_one_image_is_reported() {
        let history = vec!["FROM debian".to_string()];
        let fs_a = fs_with(&[("opt/app/only-in-a", "aaa")]);
        let fs_b = fs_with(&[]);
        let report = compare_parsed(&history, &history, &fs_a, &fs_b, &VolatileAllowlist::none());
        assert!(!report.equivalent);
        assert!(matches!(&report.differences[0], TypedDifference::FileOnlyInImageA { path } if path == "opt/app/only-in-a"));
    }

    #[test]
    fn equivalence_report_json_roundtrip() {
        let report = EquivalenceReport {
            equivalent: false,
            layer_count_match: true,
            differences: vec![TypedDifference::FileContentMismatch {
                path: "opt/app/bin".to_string(),
                hash_a: "aaa".to_string(),
                hash_b: "bbb".to_string(),
            }],
        };
        let json = serde_json::to_string_pretty(&report).unwrap();
        let parsed: EquivalenceReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }
}
