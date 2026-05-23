//! Typed analysis over an observed store.
//!
//! Consumes a [`StoreInventory`] + optional [`RefIndex`] and
//! emits typed [`Finding`]s the operator can act on:
//!
//! - **Duplicate** — N entries share a NAR digest (waste).
//! - **Orphan** — no referrers in the indexed set.
//! - **HighFanout** — drv with > N input edges.
//! - **VersionShadow** — older version superseded by newer.
//!
//! Plus typed [`UpgradePath`]s mined from the version-shadow
//! relation — for every `pkg-1.0` shadowed by `pkg-1.1`, emit
//! a recommended upgrade.

use std::collections::{BTreeMap, BTreeSet};

use crate::nar;
use crate::store_inventory::{RefIndex, StoreInventory};

/// One typed finding the analyzer surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// Multiple store paths with the same NAR content hash.
    /// `hash` is hex sha256; `paths` are the duplicates.
    Duplicate {
        hash: String,
        paths: Vec<std::path::PathBuf>,
        bytes_each: u64,
    },
    /// Path with zero referrers in the indexed set.
    /// Candidate for GC (subject to root-set semantics).
    Orphan {
        path: std::path::PathBuf,
        size: u64,
    },
    /// Drv with input-fan-out beyond the threshold.
    HighFanout {
        path: std::path::PathBuf,
        fanout: usize,
    },
    /// Older version superseded by a newer one of the same name.
    VersionShadow {
        older: std::path::PathBuf,
        newer: std::path::PathBuf,
        name_root: String,
        older_version: String,
        newer_version: String,
    },
}

/// Aggregated counts.
#[derive(Debug, Default, Clone)]
pub struct FindingsHistogram {
    pub duplicates: usize,
    pub orphans: usize,
    pub high_fanout: usize,
    pub version_shadows: usize,
}

/// Configuration knobs for [`analyze`].
#[derive(Debug, Clone)]
pub struct AnalyzeConfig {
    /// Compute NAR sha256 for every entry to find duplicates.
    /// Expensive on large stores — operator-driven opt-in.
    pub detect_duplicates: bool,
    /// Flag entries with no referrers in the inventory.  Requires
    /// `idx` to be supplied to `analyze`.
    pub detect_orphans: bool,
    /// Flag drvs with input fan-out above this threshold.  0 disables.
    pub high_fanout_threshold: usize,
    /// Detect older/newer version shadow pairs.
    pub detect_version_shadows: bool,
}

impl Default for AnalyzeConfig {
    fn default() -> Self {
        Self {
            detect_duplicates: true,
            detect_orphans: true,
            high_fanout_threshold: 8,
            detect_version_shadows: true,
        }
    }
}

/// Walk an inventory + (optional) ref index and emit findings.
pub fn analyze(
    inv: &StoreInventory,
    idx: Option<&RefIndex>,
    config: &AnalyzeConfig,
) -> Vec<Finding> {
    let mut findings: Vec<Finding> = Vec::new();

    if config.detect_duplicates {
        findings.extend(detect_duplicates(inv));
    }
    if config.detect_orphans {
        if let Some(idx) = idx {
            findings.extend(detect_orphans(inv, idx));
        }
    }
    if config.high_fanout_threshold > 0 {
        if let Some(idx) = idx {
            findings.extend(detect_high_fanout(inv, idx, config.high_fanout_threshold));
        }
    }
    if config.detect_version_shadows {
        findings.extend(detect_version_shadows(inv));
    }

    findings
}

/// Convenience: bucket findings by kind.
#[must_use]
pub fn histogram(findings: &[Finding]) -> FindingsHistogram {
    let mut h = FindingsHistogram::default();
    for f in findings {
        match f {
            Finding::Duplicate { .. }     => h.duplicates += 1,
            Finding::Orphan { .. }        => h.orphans += 1,
            Finding::HighFanout { .. }    => h.high_fanout += 1,
            Finding::VersionShadow { .. } => h.version_shadows += 1,
        }
    }
    h
}

fn detect_duplicates(inv: &StoreInventory) -> Vec<Finding> {
    use sha2::Digest;
    let mut by_hash: BTreeMap<String, Vec<std::path::PathBuf>> = BTreeMap::new();
    let mut size_of: BTreeMap<String, u64> = BTreeMap::new();
    for entry in inv.entries.values() {
        let Ok(bytes) = nar::encode(&entry.path) else { continue };
        let digest = sha2::Sha256::digest(&bytes);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        size_of.entry(hex.clone()).or_insert(entry.size);
        by_hash.entry(hex).or_default().push(entry.path.clone());
    }
    by_hash.into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|(hash, paths)| Finding::Duplicate {
            bytes_each: *size_of.get(&hash).unwrap_or(&0),
            hash,
            paths,
        })
        .collect()
}

fn detect_orphans(inv: &StoreInventory, idx: &RefIndex) -> Vec<Finding> {
    inv.entries.values()
        .filter(|e| idx.referrers_of(&e.path).is_empty())
        .map(|e| Finding::Orphan {
            path: e.path.clone(),
            size: e.size,
        })
        .collect()
}

fn detect_high_fanout(
    inv: &StoreInventory,
    idx: &RefIndex,
    threshold: usize,
) -> Vec<Finding> {
    inv.entries.values()
        .filter(|e| idx.refs_from(&e.path).len() >= threshold)
        .map(|e| Finding::HighFanout {
            path: e.path.clone(),
            fanout: idx.refs_from(&e.path).len(),
        })
        .collect()
}

/// Mine version-shadow relations from an inventory.  Groups
/// entries by `<name-without-version>`; within each group,
/// detects pairs where one version is older than another via
/// the [`split_name_version`] heuristic.
fn detect_version_shadows(inv: &StoreInventory) -> Vec<Finding> {
    // Bucket entries by name-root.
    let mut by_root: BTreeMap<String, Vec<(&str, &std::path::Path, u64)>> = BTreeMap::new();
    for entry in inv.entries.values() {
        if let Some((name_root, version)) = split_name_version(&entry.parsed.name) {
            by_root.entry(name_root.to_string())
                .or_default()
                .push((version, entry.path.as_path(), entry.size));
        }
    }

    let mut findings: Vec<Finding> = Vec::new();
    for (name_root, mut versions) in by_root {
        if versions.len() < 2 { continue; }
        // Sort by version (string-wise — coarse, good enough for
        // the common semver-like case).
        versions.sort_by(|a, b| version_cmp(a.0, b.0));
        // Pairs of (older, newer).
        for window in versions.windows(2) {
            let (older_v, older_p, _) = window[0];
            let (newer_v, newer_p, _) = window[1];
            findings.push(Finding::VersionShadow {
                older: older_p.to_path_buf(),
                newer: newer_p.to_path_buf(),
                name_root: name_root.clone(),
                older_version: older_v.to_string(),
                newer_version: newer_v.to_string(),
            });
        }
    }
    findings
}

/// Heuristic split of `name-1.2.3` into `("name", "1.2.3")`.
/// Walks backwards from the end; the first `-` followed by a
/// digit-leading suffix is the version separator.
fn split_name_version(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            let name = &s[..i];
            let version = &s[i + 1..];
            if !name.is_empty() && !version.is_empty() {
                return Some((name, version));
            }
        }
        i += 1;
    }
    None
}

/// Coarse version comparator — splits on `.` / `-` and compares
/// numeric components numerically, alphas lexically.  Good
/// enough for typical semver-like nix names.
fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let split = |s: &str| -> Vec<String> {
        s.split(|c: char| c == '.' || c == '-' || c == '+')
            .map(String::from)
            .collect()
    };
    let av = split(a);
    let bv = split(b);
    let max = av.len().max(bv.len());
    for i in 0..max {
        let ai = av.get(i).map(String::as_str).unwrap_or("");
        let bi = bv.get(i).map(String::as_str).unwrap_or("");
        let ord = match (ai.parse::<u64>(), bi.parse::<u64>()) {
            (Ok(x), Ok(y))   => x.cmp(&y),
            (Ok(_), Err(_))  => std::cmp::Ordering::Greater,
            (Err(_), Ok(_))  => std::cmp::Ordering::Less,
            (Err(_), Err(_)) => ai.cmp(bi),
        };
        if ord != std::cmp::Ordering::Equal { return ord; }
    }
    std::cmp::Ordering::Equal
}

// ── UpgradePath miner ──────────────────────────────────────────────

/// Recommended typed upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradePath {
    pub from: std::path::PathBuf,
    pub to: std::path::PathBuf,
    pub name_root: String,
    pub from_version: String,
    pub to_version: String,
    /// Number of paths that currently reference `from` — the
    /// blast radius of applying this upgrade.
    pub referrers_count: usize,
}

/// Extract `UpgradePath`s from `findings` plus a `RefIndex`.
/// Every `VersionShadow` becomes an upgrade recommendation;
/// the referrer count comes from the index.
#[must_use]
pub fn mine_upgrade_paths(
    findings: &[Finding],
    idx: &RefIndex,
) -> Vec<UpgradePath> {
    findings.iter().filter_map(|f| match f {
        Finding::VersionShadow { older, newer, name_root, older_version, newer_version } => {
            Some(UpgradePath {
                from: older.clone(),
                to: newer.clone(),
                name_root: name_root.clone(),
                from_version: older_version.clone(),
                to_version: newer_version.clone(),
                referrers_count: idx.referrers_of(older).len(),
            })
        }
        _ => None,
    }).collect()
}

/// Sort upgrade paths by descending blast radius (referrers).
pub fn sort_upgrade_paths(paths: &mut [UpgradePath]) {
    paths.sort_by_key(|p| std::cmp::Reverse(p.referrers_count));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_name_version_handles_typical_shapes() {
        assert_eq!(split_name_version("hello-2.12"), Some(("hello", "2.12")));
        assert_eq!(split_name_version("rust_litrs-1.0.0-lib"), Some(("rust_litrs", "1.0.0-lib")));
        assert_eq!(split_name_version("nodash"), None);
        assert_eq!(split_name_version("dashed-but-no-version"), None);
    }

    #[test]
    fn version_cmp_orders_typical_versions() {
        use std::cmp::Ordering;
        assert_eq!(version_cmp("1.0", "1.1"), Ordering::Less);
        assert_eq!(version_cmp("1.10", "1.9"), Ordering::Greater);
        assert_eq!(version_cmp("1.0.0", "1.0.0"), Ordering::Equal);
        assert_eq!(version_cmp("2.0", "1.99"), Ordering::Greater);
    }
}
