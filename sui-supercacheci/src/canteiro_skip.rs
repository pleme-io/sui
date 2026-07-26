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
//! The only shipped [`CacheProbe`] is [`UnrealizedProbe`], which returns
//! `false` for every node — because a `CiNode`'s arbitrary-shell action is NOT
//! yet a Nix derivation with a content-addressed store-path output (B-Root2's
//! deep gate: realizing a node via sui-store's realize/substitute path). With no
//! realized output, nothing is cached, so [`partition`] SKIPS NOTHING and the
//! executor behaves exactly as today. The mechanism is therefore **sound by
//! construction**: a skip is *unrepresentable* until a real `CacheProbe` — one
//! that computes a node's realized output store-path and queries
//! `sui_castore::StorageBackend::get_narinfo` — is wired, which is the named
//! B-Root2 gate. Wiring [`partition`] into the executor (`run_in_process` /
//! `run_distributed` publishing only `partition.run`) is the small follow-on
//! ROOT-4 step; it is sound the moment the real probe exists, and a no-op before.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::canteiro::{affected_set, CiNode, CiRun, DecomposeError};

/// The content-address of a node's realized OUTPUT — the store-path hash the NAR
/// cache keys by, DISTINCT from a node's input `ContentAddr`. Populated by the
/// derivation-realize (B-Root2's gate); carried as a typed newtype so a real
/// [`CacheProbe`] maps `node → OutputAddr → StorageBackend::get_narinfo`. Unused
/// by the M0 [`UnrealizedProbe`] (which needs no output identity to return
/// `false`), so no unbuilt realize is faked here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputAddr(pub String);

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
    use crate::canteiro::{ActionRef, EnvClass};
    use std::collections::HashSet;

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
}
