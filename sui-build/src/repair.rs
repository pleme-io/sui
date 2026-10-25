//! Re-derive the references of paths this machine built, from their bytes.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use sui_compat::store_path::STORE_PATH_HASH_LEN;
use sui_store::traits::{Store, StoreResult};
use sui_store::LocalStore;

/// What a reference repair found for one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceRepair {
    /// The repaired path.
    pub path: String,
    /// Valid paths its bytes name that were not recorded as references.
    pub added: Vec<String>,
    /// Store paths its bytes name that are not valid here: its closure is broken.
    pub absent: Vec<String>,
}

/// Compares each signed path registered since `since` with its cache narinfo; substitutes and records the missing references.
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
        let hash = hash_of(&path);
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
        repairs.push(ReferenceRepair { path, added, absent });
    }
    Ok(repairs)
}

/// Rescans every unsigned local build against all valid paths; records the missing references.
///
/// # Errors
///
/// When the store cannot be read or written.
pub async fn repair_references(store: &LocalStore, dry_run: bool) -> StoreResult<Vec<ReferenceRepair>> {
    let valid: Vec<String> = store.query_all_valid_paths().await?.iter().map(|p| p.to_absolute_path()).collect();
    let by_hash: std::collections::HashMap<String, String> =
        valid.iter().map(|p| (hash_of(p), p.clone())).collect();
    let mut repairs = Vec::new();
    for path in store.unsigned_local_builds().await? {
        let named = named_store_paths(Path::new(&path));
        let (present, absent): (Vec<String>, Vec<String>) =
            named.into_iter().partition(|p| by_hash.contains_key(&hash_of(p)));
        let refs: Vec<String> = present
            .iter()
            .map(|p| by_hash[&hash_of(p)].clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let added = store.add_missing_references(&path, &refs, dry_run).await?;
        if !added.is_empty() || !absent.is_empty() {
            repairs.push(ReferenceRepair { path, added, absent });
        }
    }
    Ok(repairs)
}

fn hash_of(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or_default();
    base.get(..STORE_PATH_HASH_LEN).unwrap_or(base).to_string()
}

/// Every `/nix/store/<hash>-<name>` written in the bytes or symlink targets under `root`.
#[must_use]
pub fn named_store_paths(root: &Path) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut seen_hashes = HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        let bytes = if meta.file_type().is_symlink() {
            std::fs::read_link(&p).map(|t| t.to_string_lossy().into_owned().into_bytes()).unwrap_or_default()
        } else if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&p) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
            continue;
        } else {
            std::fs::read(&p).unwrap_or_default()
        };
        collect_store_paths(&bytes, &mut found, &mut seen_hashes);
    }
    found
}

fn collect_store_paths(bytes: &[u8], found: &mut BTreeSet<String>, seen: &mut HashSet<String>) {
    const PREFIX: &[u8] = b"/nix/store/";
    const BASE32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
    let mut i = 0;
    while let Some(off) = bytes[i..].windows(PREFIX.len()).position(|w| w == PREFIX) {
        let start = i + off + PREFIX.len();
        let end = start + STORE_PATH_HASH_LEN;
        i = start;
        if end >= bytes.len() || bytes[end] != b'-' || !bytes[start..end].iter().all(|b| BASE32.contains(b)) {
            continue;
        }
        let hash = String::from_utf8_lossy(&bytes[start..end]).into_owned();
        if !seen.insert(hash.clone()) {
            continue;
        }
        let name_end = bytes[end + 1..]
            .iter()
            .position(|b| !(b.is_ascii_alphanumeric() || b"+-._?=".contains(b)))
            .map_or(bytes.len(), |n| end + 1 + n);
        found.insert(format!("/nix/store/{hash}-{}", String::from_utf8_lossy(&bytes[end + 1..name_end])));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_paths_named_in_bytes_and_symlinks_are_found_once() {
        let tmp = tempfile::tempdir().unwrap();
        let a = "/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello-2.12.1";
        let b = "/nix/store/0000000000000000000000000000000a-nghttp2-1.68.1-lib";
        std::fs::write(tmp.path().join("f"), format!("x {a}/bin/hello y {a}/share\0{b}/lib/libnghttp2.so")).unwrap();
        std::os::unix::fs::symlink(format!("{b}/include"), tmp.path().join("l")).unwrap();
        std::fs::write(tmp.path().join("g"), "/nix/store/not-a-hash and /nix/store/").unwrap();

        let found: Vec<String> = named_store_paths(tmp.path()).into_iter().collect();
        assert_eq!(found, vec![b.to_string(), a.to_string()]);
    }
}
