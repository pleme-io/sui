//! `sui cache audit`: run admission over every narinfo a cache already holds.
//!
//! Admission guards every write from now on; it cannot see what was stored
//! before it existed. Receipt #23 (cid, 2026-10-25) found 909 narinfos with
//! `References:` empty and moved them aside by hand to `/var/cache/sui-poisoned`.
//! This is that sweep as a verb: each stored entry is admitted against the NAR
//! it advertises, with the cache itself as the universe, and the result is
//! counted by refusal kind.
//!
//! With `evict`, a refused entry is **moved aside, never deleted**: its
//! narinfo text is kept as a blob under [`QUARANTINE_PREFIX`] (not a path any
//! client fetches) and its narinfo record is removed, so the cache stops
//! serving it. Its NAR stays where it is: another entry may advertise it, and
//! a kept blob is a leak, never an outage. One entry's fault never stops the
//! sweep.

use std::collections::BTreeMap;

use sui_castore::{AdmissionError, CacheUniverse, StorageBackend};

/// Where an evicted narinfo's text is kept: `quarantine/<hash>.narinfo`.
pub const QUARANTINE_PREFIX: &str = "quarantine/";

/// The outcome of an audit sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheAudit {
    /// Entries examined.
    pub total: usize,
    /// Entries admission accepts.
    pub admitted: usize,
    /// Entries refused, by refusal kind.
    pub refused: BTreeMap<&'static str, usize>,
    /// Refused entries moved to quarantine (with `evict`).
    pub evicted: usize,
    /// Entries that could not be judged (a read failed): nothing is known.
    pub unreadable: usize,
    /// The refused hashes, with their reason.
    pub findings: Vec<(String, String)>,
}

impl CacheAudit {
    /// Refused entries, all kinds.
    #[must_use]
    pub fn refused_total(&self) -> usize {
        self.refused.values().sum()
    }
}

/// Audit every narinfo in `storage`; with `evict`, move refused ones aside.
///
/// # Errors
///
/// Only when the listing itself fails; per-entry failures are counted.
pub async fn audit_cache(storage: &dyn StorageBackend, evict: bool) -> Result<CacheAudit, crate::CacheError> {
    let hashes = storage.list_narinfos().await?;
    let universe = CacheUniverse(storage);
    let mut audit = CacheAudit { total: hashes.len(), ..CacheAudit::default() };
    for hash in hashes {
        let Ok(Some(text)) = storage.get_narinfo(&hash).await else {
            audit.unreadable += 1;
            continue;
        };
        match sui_castore::admit_stored(storage, &hash, &text, &universe).await {
            Ok(_) => audit.admitted += 1,
            Err(AdmissionError::Store(_)) => audit.unreadable += 1,
            Err(AdmissionError::Refused(r)) => {
                *audit.refused.entry(r.kind()).or_default() += 1;
                tracing::warn!(target: "sui_cache::audit", hash = %hash, refusal = r.kind(), reason = %r, "cache audit: entry refused");
                if evict && quarantine(storage, &hash, &text).await.is_ok() {
                    audit.evicted += 1;
                }
                audit.findings.push((hash, r.to_string()));
            }
        }
    }
    tracing::info!(
        target: "sui_cache::audit",
        total = audit.total,
        admitted = audit.admitted,
        refused = audit.refused_total(),
        evicted = audit.evicted,
        unreadable = audit.unreadable,
        evict,
        "cache audit finished",
    );
    Ok(audit)
}

/// Keep `text` under the quarantine prefix, then stop serving the entry.
async fn quarantine(storage: &dyn StorageBackend, hash: &str, text: &str) -> Result<(), crate::CacheError> {
    storage.put_nar(&format!("{QUARANTINE_PREFIX}{hash}.narinfo"), text.as_bytes()).await?;
    storage.delete_narinfo_record(hash).await?;
    if let Some(url) = sui_castore::advertised_nar_url(text) {
        storage.nar_ref_index().forget(&url, hash).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalStorage;
    use sui_castore::storage::admission::fixture;
    use sui_castore::AdmittedNarInfo;

    const STD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
    const TOOL: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2";
    const LIAR: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa3";

    async fn seed(st: &LocalStorage, e: &fixture::FixtureEntry, nar: &[u8]) {
        st.put_nar(&e.url, nar).await.unwrap();
        st.put_narinfo(&AdmittedNarInfo::unchecked_fixture(&e.key, &e.text)).await.unwrap();
    }

    /// Receipt #23 as a verb: poisoned entries stored before admission are
    /// found, counted by kind, moved aside with `evict` (text kept, never
    /// deleted, NAR left in place), and the good sibling keeps serving.
    #[tokio::test]
    async fn the_audit_finds_moves_aside_and_never_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let st = LocalStorage::new(dir.path());
        let std_e = fixture::entry(STD, "rust-std", b"std", &[]);
        seed(&st, &std_e, &std_e.nar_file).await;
        let link = format!("/nix/store/{STD}-rust-std/lib");
        let tool = fixture::entry(TOOL, "rust-stable", link.as_bytes(), &[]);
        seed(&st, &tool, &tool.nar_file).await;
        let liar = fixture::entry(LIAR, "x", b"declared", &[]);
        let old_url = format!("nar/{LIAR}.nar");
        let liar_text = liar.text.replace(&liar.url, &old_url);
        st.put_nar(&old_url, b"other builder's bytes").await.unwrap();
        st.put_narinfo(&AdmittedNarInfo::unchecked_fixture(LIAR, &liar_text)).await.unwrap();

        let dry = audit_cache(&st, false).await.unwrap();
        assert_eq!((dry.total, dry.admitted, dry.refused_total(), dry.evicted), (3, 1, 2, 0), "{dry:?}");
        assert_eq!(dry.refused.get("omitted-references"), Some(&1));
        assert_eq!(dry.refused.get("file-size-mismatch").or(dry.refused.get("file-hash-mismatch")), Some(&1), "{dry:?}");
        assert!(st.get_narinfo(TOOL).await.unwrap().is_some(), "a dry audit moves nothing");

        let wet = audit_cache(&st, true).await.unwrap();
        assert_eq!(wet.evicted, 2);
        assert!(st.get_narinfo(TOOL).await.unwrap().is_none(), "no longer served");
        assert!(st.get_narinfo(STD).await.unwrap().is_some(), "the good sibling is untouched");
        let kept = st.get_nar(&format!("{QUARANTINE_PREFIX}{TOOL}.narinfo")).await.unwrap().unwrap();
        assert_eq!(kept, tool.text.as_bytes(), "the text is kept, not deleted");
        assert!(st.get_nar(&tool.url).await.unwrap().is_some(), "its NAR stays");
        let again = audit_cache(&st, true).await.unwrap();
        assert_eq!((again.total, again.refused_total()), (1, 0), "a swept cache audits clean");
    }

    /// A read failure is counted as unreadable (nothing is known), never as
    /// admitted or refused, and nothing is evicted for it.
    #[tokio::test]
    async fn an_unreadable_entry_is_neither_admitted_nor_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let st = crate::test_backend::Flaky::new(dir.path(), Some(TOOL), false);
        for key in [STD, TOOL] {
            let e = fixture::entry(key, "x", key.as_bytes(), &[]);
            st.put_nar(&e.url, &e.nar_file).await.unwrap();
            st.put_narinfo(&AdmittedNarInfo::unchecked_fixture(key, &e.text)).await.unwrap();
        }
        let a = audit_cache(&st, true).await.unwrap();
        assert_eq!((a.total, a.admitted, a.unreadable, a.evicted), (2, 1, 1, 0), "{a:?}");

        let dir = tempfile::tempdir().unwrap();
        let st = crate::test_backend::Flaky::new(dir.path(), None, true);
        let e = fixture::entry(STD, "x", b"x", &[]);
        st.inner.put_nar(&e.url, &e.nar_file).await.unwrap();
        st.put_narinfo(&AdmittedNarInfo::unchecked_fixture(STD, &e.text)).await.unwrap();
        let a = audit_cache(&st, true).await.unwrap();
        assert_eq!((a.admitted, a.unreadable, a.refused_total(), a.evicted), (0, 1, 0, 0), "{a:?}");
        assert!(st.inner.get_narinfo(STD).await.unwrap().is_some(), "a NAR read failure evicts nothing");
    }
}
