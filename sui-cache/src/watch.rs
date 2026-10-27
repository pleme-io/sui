//! L2 GC-survival: decide which store paths the warm cache must capture.
//!
//! ── WHAT THIS CLOSES ──────────────────────────────────────────────────────
//! `theory/ATATAME.md` calls L2 *"the single highest-value gap in this
//! doctrine"*. `attic watch-store` used to provide it; attic was retired
//! 2026-07-31, and until something captures a newly-realized store path
//! before `nix-collect-garbage` reaches it, the warm store is a
//! within-session memo rather than a durable win.
//!
//! ── A RECONCILER, NOT AN EVENT STREAM ─────────────────────────────────────
//! attic watched inotify events. This diffs *state* instead, and that is a
//! deliberate upgrade rather than an implementation shortcut:
//!
//!   * it converges after a restart, a missed event, or a crash — an event
//!     stream loses whatever happened while it was down;
//!   * it captures paths that arrived by SUBSTITUTION, which no build hook
//!     ever sees. rio's post-build hook only pushes what rio BUILDS, so a
//!     substituted path has never been covered by anything;
//!   * "what is missing" is computed from the cache itself, so a push that
//!     failed is simply still missing next pass — retry needs no bookkeeping.
//!
//! ── WHY A BASELINE, AND WHY IT IS THE DEFAULT ─────────────────────────────
//! A pure reconciler would mirror the ENTIRE store on first run. Measured on
//! rio 2026-08-08: 60,025 store paths against 6,929 cached, so the first pass
//! would try to capture ~53,000 paths and grow a 12 GiB cache toward the size
//! of the whole store. That is not what the doctrine asks for — L2 is
//! *survival of newly-realized paths*, not a full mirror.
//!
//! So the watcher records a BASELINE at startup and captures only what
//! appears after it. `--initial-reconcile` starts from an empty baseline for
//! operators who do want the backfill, and `max_per_pass` bounds either mode
//! so a large build cannot turn one tick into an unbounded upload.

use std::collections::{BTreeSet, HashMap, HashSet};

use sui_compat::hash::{HashAlgorithm, NixHash};
use sui_compat::store_path_scan::hash_part;

/// What a store path's entry says about it: its NAR hash and its
/// references, normalized so a local-store record and a published narinfo
/// compare equal exactly when they agree (hex vs nix32, absolute paths vs
/// basenames do not matter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathDigest {
    nar_sha256: Option<Vec<u8>>,
    references: BTreeSet<String>,
}

impl PathDigest {
    /// A digest from a NAR hash (`sha256:<hex|nix32>` or SRI) and references
    /// (absolute paths or basenames).
    #[must_use]
    pub fn new<'a>(nar_hash: &str, references: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            nar_sha256: NixHash::parse_any(HashAlgorithm::Sha256, nar_hash).ok().map(|h| h.digest),
            references: references.into_iter().filter_map(hash_part).map(str::to_string).collect(),
        }
    }

    /// The digest a published narinfo records; `None` when it does not parse.
    #[must_use]
    pub fn of_narinfo(text: &str) -> Option<Self> {
        let info = sui_compat::narinfo::NarInfo::parse(text).ok()?;
        Some(Self::new(&info.nar_hash, info.references.iter().map(String::as_str)))
    }
}

/// What this cache publishes for each path, read once per entry and then
/// kept current by the watcher's own captures.
#[derive(Debug, Default)]
pub struct Published {
    digests: HashMap<String, PathDigest>,
}

impl Published {
    /// Bring the map in line with `cached`: read the narinfo of each newly
    /// cached hash, forget hashes no longer cached. An unreadable entry is
    /// left out, so it is retried next pass.
    pub async fn refresh(&mut self, storage: &dyn crate::StorageBackend, cached: &HashSet<String>) {
        self.digests.retain(|h, _| cached.contains(h));
        for h in cached {
            if self.digests.contains_key(h) {
                continue;
            }
            if let Ok(Some(text)) = storage.get_narinfo(h).await
                && let Some(d) = PathDigest::of_narinfo(&text)
            {
                self.digests.insert(h.clone(), d);
            }
        }
    }

    /// Record what a capture just published.
    pub fn record(&mut self, hash: &str, digest: PathDigest) {
        self.digests.insert(hash.to_string(), digest);
    }

    /// The published digests.
    #[must_use]
    pub fn digests(&self) -> &HashMap<String, PathDigest> {
        &self.digests
    }
}

/// Cached hashes whose published entry no longer agrees with the local
/// store's record of the path.
///
/// A path's record can change after it was published: `sui store
/// repair-references` adds the references an old scanner missed, a
/// re-registration corrects a NAR hash. Without this the cache keeps serving
/// the old entry forever, because `cached` alone says "done" (cid 2026-10-26:
/// rust-stable-with-components stayed published with no references after
/// its local record was repaired).
#[must_use]
pub fn stale_cached(
    published: &HashMap<String, PathDigest>,
    local: &HashMap<String, PathDigest>,
) -> HashSet<String> {
    published
        .iter()
        .filter(|(h, p)| local.get(*h).is_some_and(|l| l != *p))
        .map(|(h, _)| h.clone())
        .collect()
}

/// What a single capture pass should do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapturePlan {
    /// Store-path hashes to push this pass, bounded by `max_per_pass`.
    pub to_capture: Vec<String>,
    /// Candidates that did not fit this pass. They are NOT lost — the next
    /// pass recomputes from live state and picks them up. Reported so a
    /// persistently non-zero value tells the operator the interval is too
    /// long or the bound too small for this machine's build rate.
    pub deferred: usize,
}

/// Decide what to capture, from state alone.
///
/// `valid` are the hashes currently valid in the nix store, `cached` the
/// hashes the cache already holds, `baseline` the hashes that existed when
/// the watcher started (empty for a full backfill).
///
/// A candidate is a path that is valid, not cached, and not in the baseline,
/// or a cached path in `stale` (see [`stale_cached`]): a stale entry is
/// re-captured whatever the baseline says, because the cache already serves
/// it and serves it wrong. Order is deterministic (sorted) so a bounded pass is reproducible rather
/// than dependent on hash-map iteration order — an operator re-running a pass
/// against unchanged state gets the same answer.
#[must_use]
pub fn plan_capture(
    valid: &[String],
    cached: &HashSet<String>,
    stale: &HashSet<String>,
    baseline: &HashSet<String>,
    max_per_pass: usize,
) -> CapturePlan {
    let mut candidates: Vec<String> = valid
        .iter()
        .filter(|h| stale.contains(*h) || (!cached.contains(*h) && !baseline.contains(*h)))
        .cloned()
        .collect();
    candidates.sort_unstable();
    candidates.dedup();

    let deferred = candidates.len().saturating_sub(max_per_pass);
    candidates.truncate(max_per_pass);
    CapturePlan {
        to_capture: candidates,
        deferred,
    }
}

/// Outcome of a capture pass, for the operator-facing line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WatchReport {
    /// Valid store paths considered.
    pub scanned: usize,
    /// Paths successfully pushed into the warm cache.
    pub captured: usize,
    /// Cached paths re-captured because their local record changed.
    pub recaptured: usize,
    /// Paths that failed to push. Non-fatal — a single bad path must never
    /// stop the watcher, or one unreadable path disables GC-survival for the
    /// whole machine.
    pub failed: usize,
    /// Candidates that did not fit this pass.
    pub deferred: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }
    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    const H: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    const STD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";

    /// The cid receipt: the cache published rust-stable with no references;
    /// its local record was then repaired. The cached entry is stale and is
    /// re-captured, even though it is cached and in the baseline.
    #[test]
    fn a_cached_path_whose_local_record_changed_is_recaptured() {
        let tool = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2".to_string();
        let published: HashMap<String, PathDigest> = [(tool.clone(), PathDigest::new(H, []))].into_iter().collect();
        let std_path = format!("/nix/store/{STD}-rust-std");
        let local: HashMap<String, PathDigest> =
            [(tool.clone(), PathDigest::new(H, [std_path.as_str()]))].into_iter().collect();
        let stale = stale_cached(&published, &local);
        assert_eq!(stale, set(&[&tool]));
        let plan = plan_capture(&v(&[&tool]), &set(&[&tool]), &stale, &set(&[&tool]), 100);
        assert_eq!(plan.to_capture, v(&[&tool]));

        let agreed: HashMap<String, PathDigest> =
            [(tool.clone(), PathDigest::new(H, [format!("{STD}-rust-std").as_str()]))].into_iter().collect();
        assert!(stale_cached(&agreed, &local).is_empty(), "basename vs absolute is the same reference");
        assert!(stale_cached(&published, &HashMap::new()).is_empty(), "not valid locally: nothing to compare");
    }

    /// The published map reads each cached entry once, forgets what left
    /// the cache, and takes the watcher's own captures as the new truth.
    #[tokio::test]
    async fn the_published_map_follows_the_cache() {
        use crate::StorageBackend as _;
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::LocalStorage::new(dir.path());
        let e = sui_castore::storage::admission::fixture::entry(STD, "x", b"x", &[]);
        storage.put_narinfo(&sui_castore::AdmittedNarInfo::unchecked_fixture(STD, &e.text)).await.unwrap();
        let mut p = Published::default();
        p.refresh(&storage, &set(&[STD, "absent"])).await;
        assert_eq!(p.digests().len(), 1, "an unreadable entry is left out");
        assert_eq!(p.digests().get(STD), PathDigest::of_narinfo(&e.text).as_ref());
        p.record(STD, PathDigest::new(H, []));
        p.refresh(&storage, &set(&[STD])).await;
        assert_eq!(p.digests().get(STD), Some(&PathDigest::new(H, [])), "a recorded capture is not re-read");
        p.refresh(&storage, &set(&[])).await;
        assert!(p.digests().is_empty());
    }

    #[test]
    fn a_changed_nar_hash_is_stale_and_hex_vs_nix32_is_not() {
        let hex = "sha256:5df6e0e2761359d30a8275058e299fcc0381534545f55cf43e41983f5d4c9456";
        let digest = NixHash::parse_any(HashAlgorithm::Sha256, hex).unwrap();
        let nix32 = format!("sha256:{}", digest.to_nix32());
        assert_eq!(PathDigest::new(hex, []), PathDigest::new(&nix32, []));
        assert_ne!(PathDigest::new(hex, []), PathDigest::new(H, []));
        let text = format!(
            "StorePath: /nix/store/{STD}-x\nURL: nar/x.nar\nCompression: none\nFileHash: {H}\nFileSize: 1\n\
             NarHash: {nix32}\nNarSize: 1\nReferences: {STD}-x\n"
        );
        assert_eq!(PathDigest::of_narinfo(&text), Some(PathDigest::new(hex, [format!("/nix/store/{STD}-x").as_str()])));
        assert_eq!(PathDigest::of_narinfo("garbage"), None);
    }

    /// The default posture: a path present at startup is NOT captured. This
    /// is what stops the first pass on a real machine from trying to mirror
    /// 53,000 paths.
    #[test]
    fn baseline_paths_are_not_captured() {
        let plan = plan_capture(&v(&["a", "b"]), &set(&[]), &set(&[]), &set(&["a", "b"]), 100);
        assert!(plan.to_capture.is_empty());
        assert_eq!(plan.deferred, 0);
    }

    /// The whole point: something realized after startup gets captured.
    #[test]
    fn a_newly_realized_path_is_captured() {
        let plan = plan_capture(&v(&["a", "b", "new"]), &set(&[]), &set(&[]), &set(&["a", "b"]), 100);
        assert_eq!(plan.to_capture, v(&["new"]));
    }

    /// Already-cached paths are never re-pushed — this is what makes the
    /// watcher cheap to run on a short interval.
    #[test]
    fn cached_paths_are_skipped() {
        let plan = plan_capture(&v(&["a", "new"]), &set(&["new"]), &set(&[]), &set(&["a"]), 100);
        assert!(plan.to_capture.is_empty());
    }

    /// A failed push leaves the path uncached, so the next pass retries it
    /// with no retry bookkeeping at all. Modelled here as: still valid, still
    /// not cached, still not in baseline => still a candidate.
    #[test]
    fn a_failed_capture_is_retried_next_pass() {
        let baseline = set(&["a"]);
        let first = plan_capture(&v(&["a", "new"]), &set(&[]), &set(&[]), &baseline, 100);
        assert_eq!(first.to_capture, v(&["new"]));
        // push failed => cache still empty
        let second = plan_capture(&v(&["a", "new"]), &set(&[]), &set(&[]), &baseline, 100);
        assert_eq!(second.to_capture, v(&["new"]), "must retry, not drop");
    }

    /// A big build must not turn one tick into an unbounded upload, and the
    /// overflow must be REPORTED rather than silently dropped.
    #[test]
    fn max_per_pass_bounds_the_work_and_reports_the_remainder() {
        let plan = plan_capture(&v(&["a", "b", "c", "d", "e"]), &set(&[]), &set(&[]), &set(&[]), 2);
        assert_eq!(plan.to_capture.len(), 2);
        assert_eq!(plan.deferred, 3);
    }

    /// An empty baseline is the `--initial-reconcile` mode: everything
    /// missing becomes a candidate.
    #[test]
    fn empty_baseline_backfills() {
        let plan = plan_capture(&v(&["a", "b"]), &set(&["a"]), &set(&[]), &set(&[]), 100);
        assert_eq!(plan.to_capture, v(&["b"]));
    }

    /// Deterministic order — a bounded pass over unchanged state must pick
    /// the same paths every time, not whatever the hash map yielded.
    #[test]
    fn selection_is_deterministic() {
        let valid = v(&["z", "m", "a", "q"]);
        let a = plan_capture(&valid, &set(&[]), &set(&[]), &set(&[]), 2);
        let b = plan_capture(&valid, &set(&[]), &set(&[]), &set(&[]), 2);
        assert_eq!(a, b);
        assert_eq!(a.to_capture, v(&["a", "m"]));
    }
}
