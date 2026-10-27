//! `sui store audit`: rebuild a sample of valid paths and compare each with
//! its record.
//!
//! A store records a NAR hash and references for every path; nothing checks
//! that record against what the path's derivation produces. On cid
//! (2026-10-26) sui's builder produced serde_core bytes that two cppnix
//! rebuilds on ryn did not, and the mixed closure failed rustc E0463. This
//! samples valid paths with a readable deriver, rebuilds each derivation to
//! scratch (`--check`), and counts agreement, byte divergence and reference
//! divergence. Every path not sampled is counted under its reason.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sui_compat::derivation::Derivation;
use sui_store::traits::{Store, StoreResult};
use sui_store::LocalStore;

use crate::local_builder::{Divergence, OutputCheck};
use crate::traits::BuildError;

/// What can rebuild a derivation and compare it: [`crate::LocalBuilder`].
#[async_trait]
pub trait RebuildChecker: Send + Sync {
    /// Rebuild `drv` (at store path `deriver`) and compare its outputs.
    async fn check(&self, drv: &Derivation, deriver: &str) -> Result<Vec<OutputCheck>, BuildError>;
}

#[async_trait]
impl RebuildChecker for crate::LocalBuilder {
    async fn check(&self, drv: &Derivation, deriver: &str) -> Result<Vec<OutputCheck>, BuildError> {
        crate::LocalBuilder::check(self, drv, deriver).await
    }
}

/// The outcome of a rebuild audit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RebuildAudit {
    /// Derivations rebuilt.
    pub checked: usize,
    /// Outputs whose rebuild agrees with the record.
    pub agree: usize,
    /// Outputs whose rebuilt bytes differ.
    pub bytes: usize,
    /// Outputs whose bytes agree but whose recorded references do not.
    pub references: usize,
    /// Derivations whose rebuild failed, with the error.
    pub failed: Vec<(String, String)>,
    /// Candidate paths not sampled, by reason.
    pub skipped: BTreeMap<&'static str, usize>,
    /// The divergent outputs.
    pub divergent: Vec<OutputCheck>,
}

/// Rebuild up to `sample` derivations of valid paths, chosen by `seed`, and
/// compare every output with its record. `root` is where store files are.
///
/// # Errors
///
/// When the store cannot be read.
pub async fn audit_rebuilds(
    checker: &dyn RebuildChecker,
    store: &LocalStore,
    root: &Path,
    sample: usize,
    seed: u64,
) -> StoreResult<RebuildAudit> {
    let mut audit = RebuildAudit::default();
    let mut derivers: BTreeSet<String> = BTreeSet::new();
    for (path, _, _) in store.path_digests().await? {
        if path.ends_with(".drv") {
            continue;
        }
        let Ok(sp) = sui_compat::store_path::StorePath::from_absolute_path(&path) else { continue };
        let recorded = store.query_path_info(&sp).await?.and_then(|i| i.deriver).filter(|d| !d.is_empty());
        let deriver = match recorded {
            Some(d) => Some(d),
            None => store.query_valid_derivers(&sp).await.unwrap_or_default().into_iter().next(),
        };
        match deriver {
            Some(d) => {
                derivers.insert(d);
            }
            None => *audit.skipped.entry("no deriver recorded").or_default() += 1,
        }
    }
    let mut order: Vec<(Vec<u8>, String)> = derivers
        .into_iter()
        .map(|d| (Sha256::digest(format!("{seed}:{d}").as_bytes()).to_vec(), d))
        .collect();
    order.sort();
    for (_, deriver) in order {
        if audit.checked >= sample {
            *audit.skipped.entry("beyond the sample").or_default() += 1;
            continue;
        }
        let Some(base) = deriver.rsplit('/').next() else { continue };
        let Some(drv) = std::fs::read(root.join(base)).ok().and_then(|b| Derivation::parse(&b).ok()) else {
            *audit.skipped.entry("deriver not readable").or_default() += 1;
            continue;
        };
        if crate::local_builder::is_fixed_output(&drv) {
            *audit.skipped.entry("fixed-output (its hash is checked when fetched)").or_default() += 1;
            continue;
        }
        audit.checked += 1;
        match checker.check(&drv, &deriver).await {
            Ok(checks) => {
                for c in checks {
                    match c.divergence() {
                        None => audit.agree += 1,
                        Some(Divergence::Bytes { .. }) => {
                            audit.bytes += 1;
                            audit.divergent.push(c);
                        }
                        Some(Divergence::References { .. }) => {
                            audit.references += 1;
                            audit.divergent.push(c);
                        }
                    }
                }
            }
            Err(e) => audit.failed.push((deriver.clone(), e.to_string())),
        }
    }
    tracing::info!(
        target: "sui::store_audit",
        checked = audit.checked,
        agree = audit.agree,
        bytes = audit.bytes,
        references = audit.references,
        failed = audit.failed.len(),
        skipped = audit.skipped.values().sum::<usize>(),
        sample,
        seed,
        "store rebuild audit finished",
    );
    Ok(audit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_store::traits::PathInfo;

    /// A checker that answers from a table, and records what it was asked.
    struct Table {
        answers: BTreeMap<String, Result<Vec<OutputCheck>, String>>,
        asked: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl RebuildChecker for Table {
        async fn check(&self, _: &Derivation, deriver: &str) -> Result<Vec<OutputCheck>, BuildError> {
            self.asked.lock().unwrap().push(deriver.to_string());
            match self.answers.get(deriver) {
                Some(Ok(c)) => Ok(c.clone()),
                Some(Err(e)) => Err(BuildError::Failed(e.clone())),
                None => Ok(vec![]),
            }
        }
    }

    fn check(path: &str, recorded: &str, rebuilt: &str, refs: (&[&str], &[&str])) -> OutputCheck {
        OutputCheck {
            output: "out".into(),
            path: path.into(),
            recorded_nar_hash: recorded.into(),
            rebuilt_nar_hash: rebuilt.into(),
            recorded_references: refs.0.iter().map(|s| (*s).to_string()).collect(),
            rebuilt_references: refs.1.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    const H1: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000001";
    const H2: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000002";

    fn drv_text(out: &str, fixed: bool) -> String {
        let hash = if fixed { ("r:sha256", "0000000000000000000000000000000000000000000000000000000000000001") } else { ("", "") };
        format!(
            "Derive([(\"out\",\"{out}\",\"{}\",\"{}\")],[],[],\"aarch64-darwin\",\"/bin/sh\",[],[(\"out\",\"{out}\")])",
            hash.0, hash.1
        )
    }

    #[tokio::test]
    async fn the_audit_counts_agreement_divergence_failure_and_every_skip() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalStore::open_in_memory_with_dir(root.path().to_str().unwrap()).await.unwrap();
        let at = |p: &str| root.path().join(p.rsplit('/').next().unwrap());
        let paths: Vec<(String, String, bool)> = (1..=5)
            .map(|i| {
                (format!("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa{i}-p{i}"), format!("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb{i}-p{i}.drv"), i == 5)
            })
            .collect();
        for (out, drv, fixed) in &paths {
            std::fs::write(at(drv), drv_text(out, *fixed)).unwrap();
            std::fs::write(at(out), "x").unwrap();
            let info = |p: &str, deriver: Option<&str>| PathInfo {
                path: p.to_string(),
                nar_hash: H1.into(),
                nar_size: 1,
                references: vec![],
                deriver: deriver.map(String::from),
                signatures: vec![],
                registration_time: 1,
                content_address: None,
            };
            store.register_path(&info(drv, None)).await.unwrap();
            let recorded = if out.ends_with("-p1") { "" } else { drv.as_str() };
            store.register_path(&info(out, Some(recorded))).await.unwrap();
        }
        let orphan = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa9-orphan";
        std::fs::write(at(orphan), "x").unwrap();
        store
            .register_path(&PathInfo {
                path: orphan.into(),
                nar_hash: H1.into(),
                nar_size: 1,
                references: vec![],
                deriver: None,
                signatures: vec![],
                registration_time: 1,
                content_address: None,
            })
            .await
            .unwrap();
        std::fs::remove_file(at(&paths[3].1)).unwrap();

        let answers = [
            (paths[0].1.clone(), Ok(vec![check(&paths[0].0, H1, H1, (&[], &[]))])),
            (paths[1].1.clone(), Ok(vec![check(&paths[1].0, H1, H2, (&[], &[]))])),
            (paths[2].1.clone(), Err("builder exited 1".to_string())),
        ]
        .into_iter()
        .collect();
        let table = Table { answers, asked: Default::default() };
        let audit = audit_rebuilds(&table, &store, root.path(), 10, 7).await.unwrap();
        assert_eq!((audit.checked, audit.agree, audit.bytes, audit.references), (3, 1, 1, 0), "{audit:?}");
        assert_eq!(audit.failed.len(), 1);
        assert_eq!(audit.skipped.get("no deriver recorded"), Some(&1));
        assert_eq!(audit.skipped.get("deriver not readable"), Some(&1));
        assert_eq!(audit.skipped.get("fixed-output (its hash is checked when fetched)"), Some(&1));
        assert_eq!(audit.divergent.len(), 1);

        let refs = Table {
            answers: [(paths[0].1.clone(), Ok(vec![check(&paths[0].0, H1, H1, (&[], &[orphan]))]))].into_iter().collect(),
            asked: Default::default(),
        };
        let audit = audit_rebuilds(&refs, &store, root.path(), 10, 7).await.unwrap();
        assert_eq!(audit.references, 1);

        let one = Table { answers: BTreeMap::new(), asked: Default::default() };
        let audit = audit_rebuilds(&one, &store, root.path(), 1, 7).await.unwrap();
        assert_eq!(audit.checked, 1);
        assert!(audit.skipped.get("beyond the sample").is_some_and(|n| *n >= 1));
        let again = Table { answers: BTreeMap::new(), asked: Default::default() };
        audit_rebuilds(&again, &store, root.path(), 1, 7).await.unwrap();
        assert_eq!(*one.asked.lock().unwrap(), *again.asked.lock().unwrap(), "a seed picks the same sample");
    }
}
