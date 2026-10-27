//! Re-sign every narinfo already in a cache.
//!
//! ── WHY THIS EXISTS ───────────────────────────────────────────────────────
//! A cache's stored `Sig:` lines are only as good as the fingerprint the
//! signer computed when the entry was INGESTED. When that computation is
//! corrected — or when the signing key rotates — every entry written before
//! the change carries a signature that will never verify, and nothing in the
//! normal serving path repairs it.
//!
//! `server::sign_narinfo_text` deliberately will not: it skips any narinfo
//! that already carries a signature under our key name, so it does not
//! double-sign on re-ingest. That guard is correct for ingest and is exactly
//! what makes a BAD signature permanent — the stale signature is by our key,
//! so it reads as "already signed" forever.
//!
//! Measured 2026-08-08 on the fleet origin (rio): 6,668 narinfos signed over a
//! hex fingerprint while Nix fingerprints in Nix-base32, so every consumer
//! discarded every path with *"not signed by any of the keys in
//! trusted-public-keys"*. Only paths that happened to be REBUILT and re-pushed
//! after the fix were repaired; the rest needed this.
//!
//! ── WHY REWRITE THE narinfo AND NOT RE-PUSH ───────────────────────────────
//! A re-push re-uploads the NAR — 12 GiB on that origin — to change one line
//! of text, and it can only cover paths whose store path still exists locally.
//! Re-signing rewrites the narinfo alone, uploads nothing, and it works for
//! entries whose original store path is long gone.
//!
//! ── WHY THE DEFAULT ADMITS FIRST ─────────────────────────────────────────
//! A signature is a vouch. Re-signing an entry that omits references (the cid
//! `rust-stable-with-components` receipt, 2026-10-26) or whose hashes do not
//! match its NAR makes every consumer trust the poison anew. So the default
//! sweep runs admission over each entry and re-signs only what passes; a
//! refused entry is counted and left as it was (`sui cache audit --evict`
//! moves it aside). The O(entries) text-only sweep is kept, configured off:
//! [`ResignMode::SignaturesOnly`].

use crate::signing::CacheSigner;
use sui_castore::storage::StorageBackend;
use sui_compat::narinfo::NarInfo;

/// How a re-sign sweep treats each entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResignMode {
    /// Admit each entry against its NAR first; re-sign only what passes.
    /// O(bytes).
    #[default]
    Admit,
    /// Rewrite `Sig:` lines without reading the NAR. O(entries); it vouches
    /// for whatever is stored.
    SignaturesOnly,
}

/// Outcome of a re-sign sweep.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResignReport {
    /// Entries examined.
    pub total: usize,
    /// Entries whose `Sig:` under our key changed and were written back.
    pub resigned: usize,
    /// Entries already carrying the correct signature — a re-run resigns
    /// nothing, which is what makes this safe to schedule.
    pub unchanged: usize,
    /// Entries admission refused: left unsigned by this sweep.
    pub refused: usize,
    /// Entries that could not be read or parsed. Reported, never fatal: one
    /// corrupt entry must not abort the sweep for the other 6,667.
    pub failed: usize,
}

/// Our signature over `info`, with every other key's kept; `None` when the
/// entry already carries exactly that signature.
fn resigned(info: &NarInfo, signer: &CacheSigner) -> Option<Vec<String>> {
    let key_prefix = format!("{}:", signer.key_name());
    let ours: Vec<&String> = info.signatures.iter().filter(|s| s.starts_with(&key_prefix)).collect();
    let unsigned = NarInfo { signatures: Vec::new(), ..info.clone() };
    let sig = signer.sign_narinfo(&unsigned);
    if ours.len() == 1 && *ours[0] == sig {
        return None;
    }
    let mut signatures: Vec<String> = info.signatures.iter().filter(|s| !s.starts_with(&key_prefix)).cloned().collect();
    signatures.push(sig);
    Some(signatures)
}

/// Re-sign every narinfo in `storage` under `signer`'s key.
///
/// Signatures by OTHER key names are preserved — a cache may legitimately
/// carry `cache.nixos.org-1` alongside ours, and dropping those would strip
/// upstream provenance. Only our own key's signature is replaced.
///
/// # Errors
///
/// Returns a [`crate::CacheError`] only if the entry listing itself fails.
/// Per-entry failures increment `failed` or `refused` and the sweep continues.
pub async fn resign_all(
    storage: &dyn StorageBackend,
    signer: &CacheSigner,
    mode: ResignMode,
) -> Result<ResignReport, crate::CacheError> {
    let hashes = storage
        .list_narinfos()
        .await
        .map_err(|e| crate::CacheError::NarInfo(e.to_string()))?;

    let mut report = ResignReport { total: hashes.len(), ..Default::default() };
    let universe = sui_castore::CacheUniverse(storage);

    for hash in hashes {
        let Ok(Some(content)) = storage.get_narinfo(&hash).await else {
            report.failed += 1;
            continue;
        };
        match mode {
            ResignMode::Admit => match sui_castore::admit_stored(storage, &hash, &content, &universe).await {
                Ok(admitted) => match resigned(admitted.info(), signer) {
                    None => report.unchanged += 1,
                    Some(sigs) => match storage.put_narinfo(&admitted.with_signatures(sigs)).await {
                        Ok(()) => report.resigned += 1,
                        Err(_) => report.failed += 1,
                    },
                },
                Err(sui_castore::AdmissionError::Refused(r)) => {
                    tracing::warn!(hash = %hash, refusal = r.kind(), reason = %r, "resign: entry refused admission; not re-signed");
                    report.refused += 1;
                }
                Err(sui_castore::AdmissionError::Store(_)) => report.failed += 1,
            },
            ResignMode::SignaturesOnly => {
                let Ok(info) = NarInfo::parse(&content) else {
                    report.failed += 1;
                    continue;
                };
                let Some(sigs) = resigned(&info, signer) else {
                    report.unchanged += 1;
                    continue;
                };
                match sui_castore::resign_stored(storage, &hash, |_| sigs).await {
                    Ok(true) => report.resigned += 1,
                    _ => report.failed += 1,
                }
            }
        }
    }

    tracing::info!(
        target: "sui_cache::resign",
        total = report.total, resigned = report.resigned, unchanged = report.unchanged,
        refused = report.refused, failed = report.failed, mode = ?mode,
        "resign sweep finished",
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_castore::storage::admission::fixture;
    use sui_castore::storage::LocalStorage;
    use sui_castore::AdmittedNarInfo;

    const SECRET: &str =
        "test-key-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA7aie8zrakLWKjqNAqbw1zZTIVdx3iQ6Y6wEihi1naKQ==";
    const HASH: &str = "00000000000000000000000000000000";

    fn entry_with_sig(sig: Option<&str>) -> fixture::FixtureEntry {
        let mut e = fixture::entry(HASH, "x", b"payload", &[]);
        if let Some(sig) = sig {
            e.text.push_str(&format!("Sig: {sig}\n"));
        }
        e
    }

    async fn seed(dir: &std::path::Path, e: &fixture::FixtureEntry) -> LocalStorage {
        let st = LocalStorage::new(dir);
        st.put_nar(&e.url, &e.nar_file).await.unwrap();
        st.put_narinfo(&AdmittedNarInfo::unchecked_fixture(&e.key, &e.text)).await.unwrap();
        st
    }

    /// The regression this module exists for: an entry already signed under
    /// OUR key with a WRONG signature must be replaced, not skipped. The
    /// ingest path skips it (see `server::sign_admitted`), which is
    /// precisely why a bad signature is otherwise permanent.
    #[tokio::test]
    async fn replaces_a_stale_signature_under_our_own_key() {
        let dir = tempfile::tempdir().unwrap();
        let stale = "test-key-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
        let signer = CacheSigner::from_secret_key_string(SECRET).unwrap();
        for mode in [ResignMode::Admit, ResignMode::SignaturesOnly] {
            let st = seed(dir.path(), &entry_with_sig(Some(stale))).await;
            let r = resign_all(&st, &signer, mode).await.unwrap();
            assert_eq!(r.resigned, 1, "{mode:?}: a stale same-key signature must be replaced");
            assert_eq!(r.unchanged, 0);

            let out = st.get_narinfo(HASH).await.unwrap().unwrap();
            assert!(!out.contains(stale), "the stale signature must be gone");
            assert!(out.contains("test-key-1:"), "a fresh one must be present");
        }
    }

    /// Idempotence — a second sweep must resign nothing. This is what makes
    /// the command safe to run on a schedule or twice by accident.
    #[tokio::test]
    async fn second_sweep_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let st = seed(dir.path(), &entry_with_sig(None)).await;
        let signer = CacheSigner::from_secret_key_string(SECRET).unwrap();

        assert_eq!(resign_all(&st, &signer, ResignMode::Admit).await.unwrap().resigned, 1);
        let second = resign_all(&st, &signer, ResignMode::Admit).await.unwrap();
        assert_eq!(second.resigned, 0);
        assert_eq!(second.unchanged, 1);
    }

    /// Another cache's signature is provenance, not noise — preserve it.
    #[tokio::test]
    async fn preserves_signatures_by_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let foreign = "cache.nixos.org-1:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB==";
        let st = seed(dir.path(), &entry_with_sig(Some(foreign))).await;
        let signer = CacheSigner::from_secret_key_string(SECRET).unwrap();

        resign_all(&st, &signer, ResignMode::Admit).await.unwrap();
        let out = st.get_narinfo(HASH).await.unwrap().unwrap();
        assert!(out.contains(foreign), "a foreign signature must survive");
        assert!(out.contains("test-key-1:"), "ours must be added");
    }

    use crate::test_backend::Flaky;

    /// Every per-entry failure is counted, in both modes, and never aborts
    /// the sweep for the other entries.
    #[tokio::test]
    async fn every_failure_is_counted_and_the_sweep_continues() {
        let signer = CacheSigner::from_secret_key_string(SECRET).unwrap();
        let ok = fixture::entry("11111111111111111111111111111111", "ok", b"ok", &[]);
        let bad_read = "22222222222222222222222222222222";
        let flaky = |dir: &std::path::Path, nar_unreadable: bool| Flaky {
            inner: LocalStorage::new(dir),
            unreadable: Some(bad_read.to_string()),
            nar_unreadable,
            writes_fail: std::sync::atomic::AtomicBool::new(false),
        };
        async fn seed_all(st: &Flaky, ok: &fixture::FixtureEntry, bad_read: &str) {
            st.inner.put_nar(&ok.url, &ok.nar_file).await.unwrap();
            st.put_narinfo(&AdmittedNarInfo::unchecked_fixture(&ok.key, &ok.text)).await.unwrap();
            st.put_kv_value(bad_read, "unread").await.unwrap();
            st.put_kv_value("33333333333333333333333333333333", "not a narinfo").await.unwrap();
        }

        let dir = tempfile::tempdir().unwrap();
        let st = flaky(dir.path(), false);
        seed_all(&st, &ok, bad_read).await;
        let r = resign_all(&st, &signer, ResignMode::SignaturesOnly).await.unwrap();
        assert_eq!((r.total, r.resigned, r.failed, r.unchanged), (3, 1, 2, 0), "{r:?}");
        let r = resign_all(&st, &signer, ResignMode::SignaturesOnly).await.unwrap();
        assert_eq!((r.resigned, r.unchanged, r.failed), (0, 1, 2), "{r:?}");

        let dir = tempfile::tempdir().unwrap();
        let st = flaky(dir.path(), false);
        seed_all(&st, &ok, bad_read).await;
        st.writes_fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let r = resign_all(&st, &signer, ResignMode::Admit).await.unwrap();
        assert_eq!((r.resigned, r.failed, r.refused), (0, 2, 1), "write failure + unreadable, garbage refused: {r:?}");
        let r = resign_all(&st, &signer, ResignMode::SignaturesOnly).await.unwrap();
        assert_eq!((r.resigned, r.failed), (0, 3), "{r:?}");

        let dir = tempfile::tempdir().unwrap();
        let st = flaky(dir.path(), true);
        seed_all(&st, &ok, bad_read).await;
        let r = resign_all(&st, &signer, ResignMode::Admit).await.unwrap();
        assert_eq!((r.resigned, r.failed, r.refused), (0, 2, 1), "a NAR read failure is a failure, not a refusal: {r:?}");
    }

    /// A re-sign is a vouch: the default sweep must not vouch for an entry
    /// whose bytes contradict it. The text-only mode still does, by request.
    #[tokio::test]
    async fn the_default_sweep_does_not_vouch_for_a_poisoned_entry() {
        let dir = tempfile::tempdir().unwrap();
        let named = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
        let dep = fixture::entry(named, "rust-std", b"std", &[]);
        let poison = fixture::entry(HASH, "rust-stable", format!("/nix/store/{named}-rust-std/lib").as_bytes(), &[]);
        let st = seed(dir.path(), &poison).await;
        st.put_nar(&dep.url, &dep.nar_file).await.unwrap();
        st.put_narinfo(&AdmittedNarInfo::unchecked_fixture(&dep.key, &dep.text)).await.unwrap();
        let signer = CacheSigner::from_secret_key_string(SECRET).unwrap();

        let r = resign_all(&st, &signer, ResignMode::Admit).await.unwrap();
        assert_eq!((r.refused, r.resigned), (1, 1), "the poisoned entry is refused, its sibling signed");
        assert!(!st.get_narinfo(HASH).await.unwrap().unwrap().contains("Sig:"));

        let r = resign_all(&st, &signer, ResignMode::SignaturesOnly).await.unwrap();
        assert_eq!(r.resigned, 1, "the text-only mode re-signs what is stored");
    }
}
