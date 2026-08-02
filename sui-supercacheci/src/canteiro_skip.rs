//! canteiro sound affected-skip — the SOUND skip predicate + the cache-probe
//! seam (`theory/CANTEIRO.md` §7.1-B, the cascade root B-Root2).
//!
//! `affected_set` (in [`crate::canteiro`]) is computed + tested, but its SOUND
//! consumption — actually SKIPPING an unaffected node — was NOT wired, because
//! skipping an ancestor is only sound if that ancestor's realized output can be
//! **served from the cache to its descendants**. This module encodes exactly
//! that soundness rule and NOTHING that violates it.
//!
//! ## The soundness rule (the whole point)
//!
//! A node is skipped **iff** it is (a) NOT affected by the diff AND (b) its
//! output is already in the cache. Every other case runs:
//! - affected → **Run** (its inputs changed);
//! - unaffected but **not cached** → **Run** (we cannot skip a node whose output
//!   a descendant would then be unable to obtain — this is the case a naive
//!   affected-only prune gets WRONG, shipping a broken descendant).
//!
//! ## Tier-honest: this makes ZERO unsound skips (never round up)
//!
//! Two [`CacheProbe`]s ship, and NEITHER produces an unsound skip against a
//! real CI node today:
//!
//! - [`UnrealizedProbe`] returns `false` for every node unconditionally — the
//!   M0 floor that makes [`partition`] skip NOTHING.
//! - [`StoreBackedProbe`] is the REAL cache axis (B-Root2's PROBE HALF): it
//!   queries a `sui_castore::StorageBackend` and reports a node cached IFF the
//!   node carries a realized [`OutputAddr`](crate::canteiro::OutputAddr) AND
//!   the backend genuinely holds BOTH the narinfo for that store-path hash AND
//!   the NAR blob it references (genuine servability). It is sound and
//!   mock-tested against the [`StorageBackend`] trait.
//!
//! The remaining deep gate — the reason a skip still cannot FIRE against real
//! nodes — is that a `CiNode`'s arbitrary-shell action is NOT yet a Nix
//! derivation with a content-addressed store-path output, so NO production path
//! populates [`CiNode::output_addr`](crate::canteiro::CiNode). With no realized
//! output identity, [`StoreBackedProbe`] returns `false` for every real node
//! (exactly like [`UnrealizedProbe`]), so [`partition`] still SKIPS NOTHING and
//! the executor behaves exactly as today — **sound by construction**. Wiring
//! [`partition`] into the executor (`run_in_process` / `run_distributed`
//! publishing only `partition.run`) is the small follow-on ROOT-4 step; the
//! `CiNode → realized Nix derivation → output store-path → NAR in the cache`
//! morphism (via sui-store's realize/substitute path) is the deeper B-Root2
//! gate that populates `output_addr` and lets a real skip finally fire.

use std::sync::Arc;

use async_trait::async_trait;
use sui_castore::StorageBackend;

use crate::canteiro::{affected_set, CiNode, CiRun, DecomposeError};

// The realized-OUTPUT content-address a probe keys against — `OutputAddr` — is a
// shared value type and now lives in `canteiro-types` (carried as
// `CiNode::output_addr`), re-exported through `crate::canteiro`. It moved out of
// this module so the node itself can carry its realized output identity for a
// real [`CacheProbe`] to resolve.

/// Asks whether a node's realized output is already cached (servable to its
/// descendants) — the **soundness gate** for a skip. The destination impl
/// computes the node's output store-path from its realized derivation and
/// queries `sui_castore::StorageBackend::get_narinfo`; the M0 [`UnrealizedProbe`]
/// returns `false` for every node.
#[async_trait]
pub trait CacheProbe: Send + Sync {
    async fn is_output_cached(&self, node: &CiNode) -> bool;
}

/// The M0 probe: nothing is realized yet, so nothing is cached — `false` for
/// every node. This is what makes [`partition`] skip NOTHING (sound by
/// construction) until B-Root2's real derivation-realize + probe land.
pub struct UnrealizedProbe;

#[async_trait]
impl CacheProbe for UnrealizedProbe {
    async fn is_output_cached(&self, _node: &CiNode) -> bool {
        false
    }
}

/// A REAL [`CacheProbe`] backed by a `sui_castore` [`StorageBackend`]. A node's
/// output counts as cached **iff** all three hold:
///
/// 1. the node carries a realized [`OutputAddr`](crate::canteiro::OutputAddr)
///    (`output_addr == Some`) — an un-realized node cannot be located in the
///    cache, so it is never skippable;
/// 2. the backend genuinely holds the **narinfo** for that store-path hash
///    (`get_narinfo → Ok(Some)`); and
/// 3. the backend also holds the **NAR blob** that narinfo's `URL:` line
///    references (`get_nar → Ok(Some)`).
///
/// All three are required because a skip is sound only when the node's output is
/// actually SERVABLE to its descendants — a bare narinfo whose NAR is missing is
/// indexed but not fetchable, so it is NOT servable and yields `false`. An
/// absent narinfo, an un-parseable `URL:`, and any `get_narinfo`/`get_nar`
/// transport error all likewise yield `false` (an error is not proof of
/// servability). The probe therefore reports `true` — and thus permits a skip —
/// ONLY for a node whose realized output a real cache can hand to a descendant.
///
/// ## Tier-honest
///
/// This is the PROBE HALF of B-Root2: sound, and mock-testable against the
/// [`StorageBackend`] trait. It does **not** by itself make a skip fire against
/// real CI nodes, because no production path populates
/// [`CiNode::output_addr`](crate::canteiro::CiNode) yet — that is done by the
/// derivation-realize morphism (turning a node's arbitrary-shell action into a
/// Nix derivation with a store-path output + putting its NAR in the cache),
/// which is the remaining deep gate. Wired against today's real (un-realized)
/// nodes it returns `false` for every one, exactly like [`UnrealizedProbe`].
pub struct StoreBackedProbe {
    backend: Arc<dyn StorageBackend>,
}

impl StoreBackedProbe {
    /// Build a probe over a shared `sui_castore` [`StorageBackend`].
    #[must_use]
    pub fn new(backend: Arc<dyn StorageBackend>) -> Self {
        Self { backend }
    }

    /// The relative NAR URL a narinfo references (its `URL:` line value), if
    /// present. A line-based parse of the nix binary-cache narinfo format — no
    /// `format!()` (★★ TYPED EMISSION).
    fn nar_url(narinfo: &str) -> Option<String> {
        narinfo
            .lines()
            .find_map(|line| line.strip_prefix("URL:").map(|rest| rest.trim().to_string()))
    }
}

#[async_trait]
impl CacheProbe for StoreBackedProbe {
    async fn is_output_cached(&self, node: &CiNode) -> bool {
        // (1) un-realized node → cannot be located in the cache → never skip.
        let Some(addr) = node.output_addr.as_ref() else {
            return false;
        };
        // (2) narinfo must be genuinely present — a transport error is not proof.
        let Ok(Some(narinfo)) = self.backend.get_narinfo(&addr.0).await else {
            return false;
        };
        // (3) and the NAR it references must ALSO be present, else the output is
        // indexed but not fetchable → not servable → not soundly skippable.
        let Some(url) = Self::nar_url(&narinfo) else {
            return false;
        };
        matches!(self.backend.get_nar(&url).await, Ok(Some(_)))
    }
}

/// Per-node verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipVerdict {
    Run,
    Skip,
}

/// The SOUND skip decision. Skip IFF unaffected AND the output is cached;
/// otherwise Run. This is a total function over the two booleans — the exact
/// soundness rule, with no third outcome to get wrong.
#[must_use]
pub fn skip_decision(affected: bool, output_cached: bool) -> SkipVerdict {
    if !affected && output_cached {
        SkipVerdict::Skip
    } else {
        SkipVerdict::Run
    }
}

/// The nodes of a run partitioned into those that must run and those soundly
/// skippable. `skip` is always a subset of {unaffected ∧ cached}; with the
/// [`UnrealizedProbe`] it is ALWAYS empty (the safety property).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub run: Vec<String>,
    pub skip: Vec<String>,
}

/// Partition a run's nodes by the sound skip rule, given the changed files and a
/// [`CacheProbe`]. Reuses the shipped [`affected_set`] for the affected axis
/// (whose DAG validation surfaces as [`DecomposeError`] — propagated, never
/// swallowed); the probe supplies the cache axis. The executor consumes
/// `partition.run` (publishing only those nodes); `partition.skip` are served
/// from cache.
pub async fn partition<P: CacheProbe>(
    run: &CiRun,
    changed_files: &[String],
    probe: &P,
) -> Result<Partition, DecomposeError> {
    let affected = affected_set(run, changed_files)?;
    let mut to_run = Vec::new();
    let mut to_skip = Vec::new();
    for node in &run.nodes {
        let is_affected = affected.contains(&run.job_id(&node.name));
        let cached = probe.is_output_cached(node).await;
        match skip_decision(is_affected, cached) {
            SkipVerdict::Run => to_run.push(node.name.clone()),
            SkipVerdict::Skip => to_skip.push(node.name.clone()),
        }
    }
    Ok(Partition {
        run: to_run,
        skip: to_skip,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canteiro::{ActionRef, CiNodeJob, EnvClass, OutputAddr};
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;
    use sui_castore::{StorageBackend, StoreError};

    fn node(name: &str, deps: &[&str], inputs: &[&str]) -> CiNode {
        CiNode::new(
            name,
            EnvClass::None,
            ActionRef {
                name: name.to_string(),
                command: "true".to_string(),
                args: vec![],
            },
            deps.iter().map(|d| (*d).to_string()).collect(),
        )
        .with_inputs(inputs.iter().map(|i| (*i).to_string()).collect())
    }

    #[test]
    fn skip_decision_is_the_sound_truth_table() {
        // Skip ONLY when unaffected AND cached; every other case runs.
        assert_eq!(skip_decision(true, true), SkipVerdict::Run); // affected → run
        assert_eq!(skip_decision(true, false), SkipVerdict::Run);
        assert_eq!(
            skip_decision(false, false),
            SkipVerdict::Run,
            "unaffected but uncached MUST run — the case a naive prune breaks"
        );
        assert_eq!(skip_decision(false, true), SkipVerdict::Skip);
    }

    /// A probe that reports a fixed set of node names as cached.
    struct CachedNames(HashSet<String>);
    #[async_trait]
    impl CacheProbe for CachedNames {
        async fn is_output_cached(&self, node: &CiNode) -> bool {
            self.0.contains(&node.name)
        }
    }

    #[tokio::test]
    async fn unrealized_probe_skips_nothing_the_safety_property() {
        // build → test; a diff touching build's inputs. Even the unaffected node,
        // and even with an empty diff, is NEVER skipped under UnrealizedProbe —
        // canteiro ships no unsound skip until a real probe exists.
        let run = CiRun {
            workspace: "pleme-io".into(),
            repo: "sui".into(),
            nodes: vec![
                node("build", &[], &["src/"]),
                node("test", &["build"], &["tests/"]),
            ],
        };
        let p = partition(&run, &["docs/README.md".into()], &UnrealizedProbe)
            .await
            .unwrap();
        assert!(
            p.skip.is_empty(),
            "UnrealizedProbe must skip nothing — sound by construction"
        );
        assert_eq!(p.run.len(), 2);
    }

    #[tokio::test]
    async fn skips_only_the_unaffected_and_cached_node() {
        // Diff touches only tests/, so `build` is unaffected; mark build cached.
        // `test` is affected (its own inputs changed) → runs. `build` is
        // unaffected AND cached → soundly skipped (its output serves `test`).
        let run = CiRun {
            workspace: "pleme-io".into(),
            repo: "sui".into(),
            nodes: vec![
                node("build", &[], &["src/"]),
                node("test", &["build"], &["tests/"]),
            ],
        };
        let cached: HashSet<String> = ["build".to_string()].into_iter().collect();
        let p = partition(&run, &["tests/it.rs".into()], &CachedNames(cached))
            .await
            .unwrap();
        assert_eq!(p.skip, vec!["build".to_string()]);
        assert_eq!(p.run, vec!["test".to_string()]);
    }

    #[tokio::test]
    async fn unaffected_but_uncached_still_runs_never_a_broken_descendant() {
        // `build` unaffected but NOT cached → must run (its output isn't
        // available to serve `test`); skipping it would break `test`.
        let run = CiRun {
            workspace: "pleme-io".into(),
            repo: "sui".into(),
            nodes: vec![
                node("build", &[], &["src/"]),
                node("test", &["build"], &["tests/"]),
            ],
        };
        let p = partition(&run, &["tests/it.rs".into()], &CachedNames(HashSet::new()))
            .await
            .unwrap();
        assert!(p.skip.is_empty());
        assert_eq!(p.run, vec!["build".to_string(), "test".to_string()]);
    }

    // --- StoreBackedProbe: the REAL cache axis, mock-tested ---------------

    /// An in-memory mock [`StorageBackend`] — the shipped trait is mockable,
    /// which is exactly what lets the real [`StoreBackedProbe`] be proven
    /// without a live sui-castore. Only the read/write surface the probe
    /// touches carries meaning; the rest satisfy the trait minimally.
    #[derive(Default)]
    struct MemBackend {
        narinfo: Mutex<HashMap<String, String>>,
        nar: Mutex<HashMap<String, Vec<u8>>>,
        nar_refs: sui_cache::MemNarRefIndex,
    }

    #[async_trait]
    impl StorageBackend for MemBackend {
        async fn get_narinfo(&self, hash: &str) -> Result<Option<String>, StoreError> {
            Ok(self.narinfo.lock().unwrap().get(hash).cloned())
        }
        async fn put_narinfo_record(&self, hash: &str, content: &str) -> Result<(), StoreError> {
            self.narinfo
                .lock()
                .unwrap()
                .insert(hash.to_string(), content.to_string());
            Ok(())
        }
        async fn delete_narinfo_record(&self, hash: &str) -> Result<(), StoreError> {
            self.narinfo.lock().unwrap().remove(hash);
            Ok(())
        }
        async fn delete_nar_record(&self, nar_path: &str) -> Result<(), StoreError> {
            self.nar.lock().unwrap().remove(nar_path);
            Ok(())
        }
        fn nar_ref_index(&self) -> &dyn sui_cache::NarRefIndex {
            &self.nar_refs
        }
        async fn get_nar(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
            Ok(self.nar.lock().unwrap().get(path).cloned())
        }
        async fn put_nar(&self, path: &str, data: &[u8]) -> Result<(), StoreError> {
            self.nar
                .lock()
                .unwrap()
                .insert(path.to_string(), data.to_vec());
            Ok(())
        }
        /// An in-memory test double holds whole values by construction. The
        /// declaration is required precisely so a *production* backend cannot
        /// inherit this path by omission.
        fn nar_residency(&self) -> sui_cache::NarResidency {
            sui_cache::NarResidency::WholeValue
        }

        async fn list_narinfos(&self) -> Result<Vec<String>, StoreError> {
            Ok(self.narinfo.lock().unwrap().keys().cloned().collect())
        }
    }

    // Two narinfos with DISTINCT `URL:` lines, so a NAR present for one never
    // accidentally satisfies the other (the servability check must be per-node).
    const NI_BOTH: &str =
        "StorePath: /nix/store/both\nURL: nar/both.nar.xz\nNarHash: sha256:b\nNarSize: 9\nReferences: \n";
    const NI_ORPHAN: &str =
        "StorePath: /nix/store/orphan\nURL: nar/orphan.nar.xz\nNarHash: sha256:o\nNarSize: 9\nReferences: \n";

    #[tokio::test]
    async fn store_backed_probe_true_only_when_narinfo_and_nar_both_servable() {
        // The probe half's exact soundness: `true` demands a realized output
        // AND a narinfo AND the NAR that narinfo references — genuine
        // servability, never mere indexing.
        let backend = Arc::new(MemBackend::default());
        // "h-both": narinfo + its referenced NAR both present → servable.
        backend.put_narinfo("h-both", NI_BOTH).await.unwrap();
        backend.put_nar("nar/both.nar.xz", b"realbytes").await.unwrap();
        // "h-orphan": narinfo present but its NAR absent → NOT servable.
        backend.put_narinfo("h-orphan", NI_ORPHAN).await.unwrap();
        let probe = StoreBackedProbe::new(backend);

        let both = node("both", &[], &["src/"]).with_output_addr(OutputAddr("h-both".into()));
        assert!(
            probe.is_output_cached(&both).await,
            "realized + narinfo + NAR → genuinely servable → cached"
        );

        let orphan = node("orphan", &[], &["src/"]).with_output_addr(OutputAddr("h-orphan".into()));
        assert!(
            !probe.is_output_cached(&orphan).await,
            "narinfo without its NAR is indexed but not fetchable → NOT cached"
        );

        let absent = node("absent", &[], &["src/"]).with_output_addr(OutputAddr("h-absent".into()));
        assert!(
            !probe.is_output_cached(&absent).await,
            "nothing in the backend for this hash → not cached"
        );

        // A real, un-realized node (no output_addr) — the shipped-pipeline case.
        let unrealized = node("unrealized", &[], &["src/"]);
        assert!(unrealized.output_addr.is_none());
        assert!(
            !probe.is_output_cached(&unrealized).await,
            "un-realized node cannot be located in the cache → not cached"
        );
    }

    /// A unique temp path so "did the node's action run?" is observable as
    /// "does this marker file exist?" — built without `format!()`.
    fn unique_marker(tag: &str) -> std::path::PathBuf {
        let mut name = String::from(tag);
        name.push('_');
        name.push_str(&std::process::id().to_string());
        name.push('_');
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        name.push_str(&nanos.to_string());
        std::env::temp_dir().join(name)
    }

    /// Model the ROOT-4 executor contract: run EXACTLY the nodes in
    /// `partition.run`, through the SAME subprocess runner `run_in_process`
    /// drives ([`CiNodeJob::execute`]). A node in `partition.skip` is therefore
    /// never executed — its action never spawns.
    async fn execute_partition_run(run: &CiRun, p: &Partition) {
        for node in &run.nodes {
            if p.run.contains(&node.name) {
                let job = CiNodeJob::for_run(run, node.clone());
                let _ = job.execute().await;
            }
        }
    }

    #[tokio::test]
    async fn store_backed_skip_is_caused_by_servability_and_a_skipped_node_never_runs() {
        // A dep-free node whose action has a real filesystem SIDE EFFECT, so
        // "did it run?" is directly observable. Declared inputs ("src/") let a
        // docs-only diff leave it unaffected (an empty-inputs node is always
        // affected and could never be skipped).
        let marker = unique_marker("canteiro_skip_zero_spawn");
        let _ = std::fs::remove_file(&marker);
        let mut cmd = String::from("printf x > '");
        cmd.push_str(marker.to_string_lossy().as_ref());
        cmd.push('\'');
        let warm = CiNode::new(
            "warm",
            EnvClass::None,
            ActionRef {
                name: "warm".to_string(),
                command: "sh".to_string(),
                args: vec!["-c".to_string(), cmd],
            },
            vec![],
        )
        .with_inputs(vec!["src/".to_string()])
        .with_output_addr(OutputAddr("h-warm".to_string()));
        let run = CiRun {
            workspace: "pleme-io".into(),
            repo: "sui".into(),
            nodes: vec![warm],
        };
        // Diff touches only docs/, not src/ → `warm` is unaffected in both runs.
        let diff = vec!["docs/README.md".to_string()];

        // --- CACHE HIT: the backend genuinely holds warm's servable output ---
        let hit = Arc::new(MemBackend::default());
        hit.put_narinfo("h-warm", NI_BOTH).await.unwrap();
        hit.put_nar("nar/both.nar.xz", b"servable-output").await.unwrap();
        // Servability is REAL: a descendant could fetch both the narinfo and the
        // NAR it references. This is the fact that MAKES the skip sound.
        assert!(matches!(hit.get_narinfo("h-warm").await, Ok(Some(_))));
        assert!(matches!(hit.get_nar("nar/both.nar.xz").await, Ok(Some(_))));

        let p = partition(&run, &diff, &StoreBackedProbe::new(hit.clone()))
            .await
            .unwrap();
        assert_eq!(
            p.skip,
            vec!["warm".to_string()],
            "unaffected AND genuinely servable → soundly skipped"
        );
        assert!(p.run.is_empty());

        // The executor consumes ONLY partition.run. `warm` is skipped, so its
        // side-effecting action never spawns — zero subprocess, zero marker.
        execute_partition_run(&run, &p).await;
        assert!(
            !marker.exists(),
            "a soundly-skipped node's action MUST NOT run — no marker created"
        );

        // --- CACHE MISS (the flip): identical inputs, empty backend ---------
        let miss = Arc::new(MemBackend::default());
        let p2 = partition(&run, &diff, &StoreBackedProbe::new(miss))
            .await
            .unwrap();
        assert_eq!(
            p2.run,
            vec!["warm".to_string()],
            "no servable output → the SAME unaffected node must RUN"
        );
        assert!(p2.skip.is_empty());

        // With nothing to serve, the executor runs `warm` for real → its action
        // spawns and creates the marker. This proves the skip above was CAUSED
        // by genuine cache-servability, not asserted by construction.
        execute_partition_run(&run, &p2).await;
        assert!(
            marker.exists(),
            "with no cache the node genuinely runs and produces its output"
        );

        let _ = std::fs::remove_file(&marker);
    }
}
