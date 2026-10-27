//! Re-derive the references of store paths from their bytes.
//!
//! # The predicate
//!
//! For every valid path that is not a `.drv`: its recorded references must
//! include every store hash its own NAR serialization names (file bytes and
//! symlink targets) that is valid in this store, itself included. That is
//! the whole check, and it does not depend on who registered the path: a
//! cppnix build (`ultimate = 1`), a substituted path (signed) and a path with
//! no deriver are scanned like a sui build. The old detector skipped all
//! three and checked substituted paths against the cache's narinfo instead
//! of their bytes, so a poisoned narinfo (cid 2026-10-26:
//! rust-stable-with-components with no references) could never be found.
//!
//! # What may narrow an addition, and what may not
//!
//! A path's bytes can name a valid path that is not one of its dependencies:
//! a source tree that mentions a store path in a doc (df34d43). cppnix only
//! records a named hash that is one of the build's candidates (its input
//! closure and its own outputs), so when the deriver is readable the
//! candidates narrow what is added, and a named path outside them is
//! reported as unresolved, not added. When no candidate set exists, every
//! missing named path is added: an extra reference keeps a path alive (a
//! leak); a missing one lets GC delete what the path needs (an outage).
//! Candidates never decide whether a path is scanned.
//!
//! # Nothing is skipped silently
//!
//! A path that is not scanned is counted under a reason and printed
//! (`N unscanned: reason`), so a pass that checked nothing cannot read as a
//! clean store.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use sui_compat::store_path_scan::{hash_part, NarDigester};
use sui_store::traits::{Store, StoreResult};
use sui_store::LocalStore;

/// What a reference repair found for one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceRepair {
    /// The repaired path.
    pub path: String,
    /// Valid paths its bytes name that were not recorded as references.
    pub added: Vec<String>,
    /// Candidates its bytes name that are not valid here: its closure is broken.
    pub absent: Vec<String>,
    /// Valid paths its bytes name that its build's candidates exclude, so they
    /// were not added (a mention, not a dependency, by cppnix's rule).
    pub unresolved: Vec<String>,
}

/// The outcome of a reference audit over a whole store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReferenceAudit {
    /// Paths whose references were short.
    pub repairs: Vec<ReferenceRepair>,
    /// Paths whose NAR serialization was scanned.
    pub scanned: usize,
    /// Paths not scanned, by reason.
    pub unscanned: BTreeMap<&'static str, usize>,
}

impl ReferenceAudit {
    /// One line per skip reason, `N unscanned: reason`.
    #[must_use]
    pub fn unscanned_lines(&self) -> Vec<String> {
        self.unscanned.iter().map(|(reason, n)| format!("{n} unscanned: {reason}")).collect()
    }
}

/// Audit every valid path's recorded references against its own bytes, and
/// record the missing ones (unless `dry_run`).
///
/// `root` is where the store's files are (`/nix/store`); a path
/// `/nix/store/<base>` is read at `root/<base>`.
///
/// # Errors
///
/// When the store cannot be read or written.
pub async fn audit_references(store: &LocalStore, root: &Path, dry_run: bool) -> StoreResult<ReferenceAudit> {
    let rows = store.path_digests().await?;
    let valid: HashMap<String, String> =
        rows.iter().filter_map(|(p, _, _)| hash_part(p).map(|h| (h.to_string(), p.clone()))).collect();
    let mut audit = ReferenceAudit::default();
    for (path, _, recorded) in &rows {
        if path.ends_with(".drv") {
            *audit.unscanned.entry("a derivation (its references are its inputs, not scanned)").or_default() += 1;
            continue;
        }
        let Some(base) = path.rsplit('/').next() else { continue };
        let digest = match NarDigester::of_path(&root.join(base)) {
            Ok(d) => d,
            Err(_) => {
                *audit.unscanned.entry("unreadable on disk").or_default() += 1;
                continue;
            }
        };
        audit.scanned += 1;
        let recorded: BTreeSet<&str> = recorded.iter().filter_map(|r| hash_part(r)).collect();
        let missing: BTreeMap<&str, &String> = digest
            .named
            .iter()
            .filter(|h| !recorded.contains(h.as_str()))
            .filter_map(|h| valid.get(h).map(|p| (h.as_str(), p)))
            .collect();
        if missing.is_empty() {
            continue;
        }
        let candidates = match deriver_of(store, root, path).await? {
            Some(drv) => Some(crate::local_builder::reference_candidates(store, &drv, None).await),
            None => None,
        };
        let (to_add, unresolved) = narrow(&missing, candidates.as_deref());
        let absent = candidates
            .iter()
            .flatten()
            .filter(|c| hash_part(c).is_some_and(|h| digest.named.contains(h) && !valid.contains_key(h)))
            .cloned()
            .collect();
        let added = store.add_missing_references(path, &to_add, dry_run).await?;
        audit.repairs.push(ReferenceRepair { path: path.clone(), added, absent, unresolved });
    }
    for line in audit.unscanned_lines() {
        tracing::info!(target: "sui::repair_references", "{line}");
    }
    tracing::info!(
        target: "sui::repair_references",
        scanned = audit.scanned,
        repaired = audit.repairs.iter().filter(|r| !r.added.is_empty()).count(),
        added = audit.repairs.iter().map(|r| r.added.len()).sum::<usize>(),
        unresolved = audit.repairs.iter().map(|r| r.unresolved.len()).sum::<usize>(),
        unscanned = audit.unscanned.values().sum::<usize>(),
        dry_run,
        "reference audit finished",
    );
    Ok(audit)
}

/// The builder's rule applied to bytes already on disk: with a candidate set,
/// a missing named path is added only when it is a candidate; without one,
/// every missing named path is added. Returns `(added, unresolved)`.
fn narrow(missing: &BTreeMap<&str, &String>, candidates: Option<&[String]>) -> (Vec<String>, Vec<String>) {
    let Some(candidates) = candidates else {
        return (missing.values().map(|p| (*p).clone()).collect(), Vec::new());
    };
    let allowed: BTreeSet<&str> = candidates.iter().filter_map(|c| hash_part(c)).collect();
    let (add, rest): (Vec<_>, Vec<_>) = missing.iter().partition(|(h, _)| allowed.contains(**h));
    (add.into_iter().map(|(_, p)| (*p).clone()).collect(), rest.into_iter().map(|(_, p)| (*p).clone()).collect())
}

/// Compares each signed path registered since `since` with its cache narinfo; substitutes and records the missing references.
///
/// This adds what an upstream cache lists; it is not the detector (that is
/// [`audit_references`], which reads the bytes).
///
/// # Errors
///
/// When the store cannot be read or written.
pub async fn repair_substituted_references(
    store: &LocalStore,
    substitutor: &sui_store::Substitutor,
    caches: &[std::sync::Arc<sui_store::BinaryCacheStore>],
    since: i64,
    dry_run: bool,
) -> StoreResult<Vec<ReferenceRepair>> {
    let store_dir = sui_compat::store_path::DEFAULT_STORE_DIR;
    let mut repairs = Vec::new();
    for (path, recorded) in store.signed_since(since).await? {
        let hash = hash_part(&path).unwrap_or_default().to_string();
        let mut listed = None;
        for cache in caches {
            if let Ok(Some(info)) = cache.fetch_narinfo(&hash).await {
                listed = Some(info.references);
                break;
            }
        }
        let Some(listed) = listed else { continue };
        let missing: Vec<String> = listed
            .iter()
            .map(|r| if r.starts_with('/') { r.clone() } else { format!("{store_dir}/{r}") })
            .filter(|r| !recorded.contains(r))
            .collect();
        if missing.is_empty() {
            continue;
        }
        let mut present = Vec::new();
        let mut absent = Vec::new();
        for r in missing {
            let valid = match sui_compat::store_path::StorePath::from_absolute_path(&r) {
                Ok(sp) if store.is_valid_path(&sp).await.unwrap_or(false) => true,
                Ok(sp) if !dry_run => !substitutor.substitute(&sp).await.is_ok_and(|s| s.is_not_found()),
                _ => false,
            };
            if valid { present.push(r) } else { absent.push(r) }
        }
        let added = store.add_missing_references(&path, &present, dry_run).await?;
        repairs.push(ReferenceRepair { path, added, absent, unresolved: Vec::new() });
    }
    Ok(repairs)
}

async fn deriver_of(
    store: &LocalStore,
    root: &Path,
    path: &str,
) -> StoreResult<Option<sui_compat::derivation::Derivation>> {
    let Ok(sp) = sui_compat::store_path::StorePath::from_absolute_path(path) else { return Ok(None) };
    let recorded = store.query_path_info(&sp).await?.and_then(|i| i.deriver).filter(|d| !d.is_empty());
    let derivers = match recorded {
        Some(d) => vec![d],
        None => store.query_valid_derivers(&sp).await.unwrap_or_default(),
    };
    Ok(derivers.iter().find_map(|d| {
        let base = d.rsplit('/').next()?;
        std::fs::read(root.join(base)).ok().and_then(|b| sui_compat::derivation::Derivation::parse(&b).ok())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_store::traits::PathInfo;

    const DEP: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1-dep";
    const DOC: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2-mentioned-in-a-doc";
    const GONE: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa3-gone";

    #[test]
    fn only_a_candidate_is_ever_added_when_candidates_exist() {
        let (dep, doc) = (DEP.to_string(), DOC.to_string());
        let missing: BTreeMap<&str, &String> =
            [(hash_part(DEP).unwrap(), &dep), (hash_part(DOC).unwrap(), &doc)].into_iter().collect();
        let candidates = vec![DEP.to_string(), GONE.to_string()];
        assert_eq!(narrow(&missing, Some(&candidates)), (vec![dep.clone()], vec![doc.clone()]));
        assert_eq!(narrow(&missing, None), (vec![dep, doc], vec![]), "no candidate set: add every named valid path");
    }

    fn info(path: &str, refs: &[&str], sigs: &[&str], deriver: Option<&str>) -> PathInfo {
        PathInfo {
            path: path.to_string(),
            nar_hash: "sha256:aaa".to_string(),
            nar_size: 1,
            references: refs.iter().map(|r| (*r).to_string()).collect(),
            deriver: deriver.map(String::from),
            signatures: sigs.iter().map(|s| (*s).to_string()).collect(),
            registration_time: 1,
            content_address: None,
        }
    }

    /// The cid receipt in a store: a substituted toolchain (signed, no
    /// deriver on disk) registered with no references while its bytes are
    /// symlinks into `rust-std`. The old detector never looked at it
    /// (signed, deriver-less). Now it is scanned, its reference is added,
    /// a `.drv` is counted as unscanned rather than silently passed, and a
    /// path whose file is gone is counted too.
    #[tokio::test]
    async fn every_valid_path_is_scanned_whoever_registered_it() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalStore::open_in_memory_with_dir(root.path().to_str().unwrap()).await.unwrap();
        let tool = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa4-rust-stable";
        let built = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa5-cppnix-built";
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa6-x.drv";
        let lost = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa7-lost";
        let at = |p: &str| root.path().join(p.rsplit('/').next().unwrap());
        std::fs::create_dir_all(at(DEP)).unwrap();
        std::fs::create_dir_all(at(tool)).unwrap();
        std::os::unix::fs::symlink(format!("{DEP}/lib"), at(tool).join("lib")).unwrap();
        std::fs::write(at(built), format!("rpath {DEP}/lib and {built}/share")).unwrap();
        std::fs::write(
            at(drv),
            format!("Derive([(\"out\",\"{DEP}\",\"\",\"\")],[],[],\"aarch64-darwin\",\"/bin/sh\",[],[(\"out\",\"{DEP}\")])"),
        )
        .unwrap();
        store.register_path(&info(DEP, &[], &[], None)).await.unwrap();
        store.register_path(&info(tool, &[], &["cid-sui-1:sig"], None)).await.unwrap();
        store.register_path(&info(built, &[], &[], Some("/nix/store/missing.drv"))).await.unwrap();
        store.register_path(&info(drv, &[], &[], None)).await.unwrap();
        store.register_path(&info(lost, &[], &[], None)).await.unwrap();

        let audit = audit_references(&store, root.path(), false).await.unwrap();
        let added: BTreeMap<String, Vec<String>> =
            audit.repairs.iter().map(|r| (r.path.clone(), r.added.clone())).collect();
        assert_eq!(added.get(tool), Some(&vec![DEP.to_string()]), "the signed, deriver-less path is repaired");
        assert_eq!(
            added.get(built),
            Some(&vec![DEP.to_string(), built.to_string()]),
            "an unsigned path is scanned too, and a self-reference is added"
        );
        assert_eq!(audit.scanned, 3);
        assert_eq!(audit.unscanned.values().sum::<usize>(), 2, "{:?}", audit.unscanned);
        assert!(audit.unscanned_lines().iter().any(|l| l.starts_with("1 unscanned: a derivation")));
        assert!(audit.unscanned_lines().iter().any(|l| l == "1 unscanned: unreadable on disk"));

        let q = store.query_path_info(&sui_compat::store_path::StorePath::from_absolute_path(tool).unwrap()).await;
        assert_eq!(q.unwrap().unwrap().references, vec![DEP.to_string()]);
        let again = audit_references(&store, root.path(), false).await.unwrap();
        assert!(again.repairs.is_empty(), "a repaired store audits clean");
    }

    /// df34d43's rule in a store: with the deriver readable, a valid path the
    /// bytes only mention (not in the build's input closure) is reported as
    /// unresolved, and only the real input is added.
    #[tokio::test]
    async fn the_build_candidates_narrow_what_is_added() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalStore::open_in_memory_with_dir(root.path().to_str().unwrap()).await.unwrap();
        let out = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa8-docs";
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa9-docs.drv";
        let at = |p: &str| root.path().join(p.rsplit('/').next().unwrap());
        std::fs::write(at(DEP), "dep").unwrap();
        std::fs::write(at(DOC), "doc").unwrap();
        std::fs::write(at(out), format!("uses {DEP}/bin and {GONE}/lib; see also {DOC}/README")).unwrap();
        std::fs::write(
            at(drv),
            format!("Derive([(\"out\",\"{out}\",\"\",\"\")],[],[\"{DEP}\",\"{GONE}\"],\"aarch64-darwin\",\"/bin/sh\",[],[(\"out\",\"{out}\")])"),
        )
        .unwrap();
        store.register_path(&info(DEP, &[], &[], None)).await.unwrap();
        store.register_path(&info(DOC, &[], &[], None)).await.unwrap();
        store.register_path(&info(drv, &[], &[], None)).await.unwrap();
        store.register_path(&info(out, &[], &[], Some(""))).await.unwrap();
        let audit = audit_references(&store, root.path(), false).await.unwrap();
        let r = audit.repairs.iter().find(|r| r.path == out).expect("the output is repaired");
        assert_eq!(r.added, vec![DEP.to_string()], "an empty recorded deriver falls back to the store's valid derivers");
        assert_eq!(r.unresolved, vec![DOC.to_string()]);
        assert_eq!(r.absent, vec![GONE.to_string()], "a named candidate that is not valid: the closure is broken");
    }

    /// A dry run reports what it would add and writes nothing.
    #[tokio::test]
    async fn a_dry_run_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalStore::open_in_memory().await.unwrap();
        let tool = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa4-rust-stable";
        let at = |p: &str| root.path().join(p.rsplit('/').next().unwrap());
        std::fs::create_dir_all(at(DEP)).unwrap();
        std::fs::write(at(tool), DEP).unwrap();
        store.register_path(&info(DEP, &[], &[], None)).await.unwrap();
        store.register_path(&info(tool, &[], &[], None)).await.unwrap();
        let audit = audit_references(&store, root.path(), true).await.unwrap();
        assert_eq!(audit.repairs[0].added, vec![DEP.to_string()]);
        let q = store.query_path_info(&sui_compat::store_path::StorePath::from_absolute_path(tool).unwrap()).await;
        assert!(q.unwrap().unwrap().references.is_empty());
    }
}
