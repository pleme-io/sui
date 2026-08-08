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
//! Re-signing reads and rewrites the narinfo alone: O(entries), not O(bytes),
//! and it works for entries whose original store path is long gone.

use crate::signing::CacheSigner;
use sui_castore::storage::StorageBackend;
use sui_compat::narinfo::NarInfo;

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
    /// Entries that could not be read or parsed. Reported, never fatal: one
    /// corrupt entry must not abort the sweep for the other 6,667.
    pub failed: usize,
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
/// Per-entry read/parse failures increment `failed` and the sweep continues.
pub async fn resign_all(
    storage: &dyn StorageBackend,
    signer: &CacheSigner,
) -> Result<ResignReport, crate::CacheError> {
    let hashes = storage
        .list_narinfos()
        .await
        .map_err(|e| crate::CacheError::NarInfo(e.to_string()))?;

    let key_prefix = format!("{}:", signer.key_name());
    let mut report = ResignReport {
        total: hashes.len(),
        ..Default::default()
    };

    for hash in hashes {
        let Ok(Some(content)) = storage.get_narinfo(&hash).await else {
            report.failed += 1;
            continue;
        };
        let Ok(mut info) = NarInfo::parse(&content) else {
            report.failed += 1;
            continue;
        };

        let before: Vec<String> = info
            .signatures
            .iter()
            .filter(|s| s.starts_with(&key_prefix))
            .cloned()
            .collect();

        // Drop OUR signature(s), keep everyone else's, then re-sign. This is
        // the one behavioural difference from the ingest path, and the whole
        // point of the command.
        info.signatures.retain(|s| !s.starts_with(&key_prefix));
        let sig = signer.sign_narinfo(&info);

        if before.len() == 1 && before[0] == sig {
            report.unchanged += 1;
            continue;
        }

        info.signatures.push(sig);
        if storage
            .put_narinfo_record(&hash, &info.serialize())
            .await
            .is_err()
        {
            report.failed += 1;
        } else {
            report.resigned += 1;
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sui_castore::storage::LocalStorage;

    const SECRET: &str =
        "test-key-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";

    fn narinfo_with_sig(sig: Option<&str>) -> String {
        // FileHash and FileSize are REQUIRED by NarInfo::parse — omitting
        // them makes every entry a parse failure, which surfaces as
        // `failed`, not `resigned`, and reads exactly like "the sweep found
        // nothing".
        let mut s = String::from(
            "StorePath: /nix/store/00000000000000000000000000000000-x\n\
             URL: nar/x.nar\n\
             Compression: none\n\
             FileHash: sha256:0000000000000000000000000000000000000000000000000000000000000000\n\
             FileSize: 1\n\
             NarHash: sha256:0000000000000000000000000000000000000000000000000000000000000000\n\
             NarSize: 1\n\
             References: \n",
        );
        if let Some(sig) = sig {
            s.push_str(&format!("Sig: {sig}\n"));
        }
        s
    }

    async fn seed(dir: &std::path::Path, hash: &str, body: &str) -> LocalStorage {
        let st = LocalStorage::new(dir);
        st.put_narinfo_record(hash, body).await.unwrap();
        st
    }

    /// The regression this module exists for: an entry already signed under
    /// OUR key with a WRONG signature must be replaced, not skipped. The
    /// ingest path skips it (see `server::sign_narinfo_text`), which is
    /// precisely why a bad signature is otherwise permanent.
    #[tokio::test]
    async fn replaces_a_stale_signature_under_our_own_key() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "00000000000000000000000000000000";
        let stale = "test-key-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
        let st = seed(dir.path(), hash, &narinfo_with_sig(Some(stale))).await;
        let signer = CacheSigner::from_secret_key_string(SECRET).unwrap();

        let r = resign_all(&st, &signer).await.unwrap();
        assert_eq!(r.resigned, 1, "a stale same-key signature must be replaced");
        assert_eq!(r.unchanged, 0);

        let out = st.get_narinfo(hash).await.unwrap().unwrap();
        assert!(!out.contains(stale), "the stale signature must be gone");
        assert!(out.contains("test-key-1:"), "a fresh one must be present");
    }

    /// Idempotence — a second sweep must resign nothing. This is what makes
    /// the command safe to run on a schedule or twice by accident.
    #[tokio::test]
    async fn second_sweep_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "00000000000000000000000000000000";
        let st = seed(dir.path(), hash, &narinfo_with_sig(None)).await;
        let signer = CacheSigner::from_secret_key_string(SECRET).unwrap();

        assert_eq!(resign_all(&st, &signer).await.unwrap().resigned, 1);
        let second = resign_all(&st, &signer).await.unwrap();
        assert_eq!(second.resigned, 0);
        assert_eq!(second.unchanged, 1);
    }

    /// Another cache's signature is provenance, not noise — preserve it.
    #[tokio::test]
    async fn preserves_signatures_by_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "00000000000000000000000000000000";
        let foreign = "cache.nixos.org-1:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB==";
        let st = seed(dir.path(), hash, &narinfo_with_sig(Some(foreign))).await;
        let signer = CacheSigner::from_secret_key_string(SECRET).unwrap();

        resign_all(&st, &signer).await.unwrap();
        let out = st.get_narinfo(hash).await.unwrap().unwrap();
        assert!(out.contains(foreign), "a foreign signature must survive");
        assert!(out.contains("test-key-1:"), "ours must be added");
    }
}
