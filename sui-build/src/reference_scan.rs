//! Reference scanner — detects store path references in build outputs.
//!
//! After a build completes, all output files are scanned for byte patterns
//! matching the 32-character hash portion of store paths. This determines
//! the runtime closure (which paths the output actually references).

use aho_corasick::AhoCorasick;
use std::path::{Path, PathBuf};
use sui_compat::store_path::STORE_PATH_HASH_LEN;

/// Filesystem abstraction for testable reference scanning.
///
/// The default implementation uses `std::fs`. Tests can provide
/// an in-memory filesystem.
pub trait FileSystem: Send + Sync {
    /// Read a file's contents.
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>>;
    /// List all files recursively in a directory.
    fn walk_dir(&self, path: &Path) -> std::io::Result<Vec<PathBuf>>;
    /// Read a symlink target.
    fn read_link(&self, path: &Path) -> std::io::Result<PathBuf>;
    /// Check if a path is a file.
    fn is_file(&self, path: &Path) -> bool;
    /// Check if a path is a symlink.
    fn is_symlink(&self, path: &Path) -> bool;
}

/// Default filesystem using `std::fs`.
pub struct RealFileSystem;

impl FileSystem for RealFileSystem {
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn walk_dir(&self, path: &Path) -> std::io::Result<Vec<PathBuf>> {
        walkdir(path)
    }

    fn read_link(&self, path: &Path) -> std::io::Result<PathBuf> {
        std::fs::read_link(path)
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn is_symlink(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink())
    }
}

/// Scan a byte buffer for Nix store path hash references.
///
/// Uses Aho-Corasick automaton for O(n + m) multi-pattern matching —
/// scans the input once for all patterns simultaneously.
///
/// Hashes whose length is not exactly [`STORE_PATH_HASH_LEN`] (32) are
/// silently filtered out before scanning.
///
/// # Examples
///
/// ```
/// use sui_build::reference_scan::scan_references;
///
/// let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
/// let data = format!("/nix/store/{hash}-hello/bin/hello");
/// let found = scan_references(data.as_bytes(), &[hash]);
/// assert_eq!(found, vec![hash.to_string()]);
/// ```
///
/// Empty inputs return an empty vector:
///
/// ```
/// use sui_build::reference_scan::scan_references;
///
/// assert!(scan_references(b"", &["hash"]).is_empty());
/// assert!(scan_references(b"some data", &[]).is_empty());
/// ```
#[must_use]
pub fn scan_references(data: &[u8], known_hashes: &[&str]) -> Vec<String> {
    let valid: Vec<&str> = known_hashes
        .iter()
        .filter(|h| h.len() == STORE_PATH_HASH_LEN)
        .copied()
        .collect();

    if valid.is_empty() || data.is_empty() {
        return Vec::new();
    }

    let Ok(ac) = AhoCorasick::new(&valid) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for mat in ac.find_iter(data) {
        let idx = mat.pattern().as_usize();
        if seen.insert(idx) {
            found.push(valid[idx].to_string());
        }
    }

    found
}

/// Scan a file for store path references (uses real filesystem).
pub fn scan_file(path: impl AsRef<Path>, known_hashes: &[&str]) -> std::io::Result<Vec<String>> {
    scan_file_with(&RealFileSystem, path.as_ref(), known_hashes)
}

/// Scan a file using a custom filesystem implementation.
pub fn scan_file_with(fs: &dyn FileSystem, path: &Path, known_hashes: &[&str]) -> std::io::Result<Vec<String>> {
    let data = fs.read_file(path)?;
    Ok(scan_references(&data, known_hashes))
}

/// Scan a directory tree for store path references (uses real filesystem).
pub fn scan_directory(dir: impl AsRef<Path>, known_hashes: &[&str]) -> std::io::Result<Vec<String>> {
    scan_directory_with(&RealFileSystem, dir.as_ref(), known_hashes)
}

/// Scan a directory tree using a custom filesystem implementation.
pub fn scan_directory_with(
    fs: &dyn FileSystem,
    dir: &Path,
    known_hashes: &[&str],
) -> std::io::Result<Vec<String>> {
    if fs.is_file(dir) {
        return scan_file_with(fs, dir, known_hashes);
    }

    let mut seen = std::collections::BTreeSet::new();
    let mut all_refs = Vec::new();

    for path in fs.walk_dir(dir)? {
        let refs = if fs.is_file(&path) {
            scan_file_with(fs, &path, known_hashes)?
        } else if fs.is_symlink(&path) {
            let Ok(target) = fs.read_link(&path) else {
                continue;
            };
            scan_references(target.to_string_lossy().as_bytes(), known_hashes)
        } else {
            continue;
        };

        for r in refs {
            if seen.insert(r.clone()) {
                all_refs.push(r);
            }
        }
    }

    Ok(all_refs)
}

/// Simple recursive directory walker.
fn walkdir(dir: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut paths = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                paths.extend(walkdir(&path)?);
            } else {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn scan_finds_known_hash() {
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let data = format!("/nix/store/{hash}-hello-2.12.1/bin/hello");
        let found = scan_references(data.as_bytes(), &[hash]);
        assert_eq!(found, vec![hash.to_string()]);
    }

    #[test]
    fn scan_ignores_unknown_hash() {
        let data = b"/nix/store/abc123-something/bin/foo";
        let found = scan_references(data, &["sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6"]);
        assert!(found.is_empty());
    }

    #[test]
    fn scan_multiple_references() {
        let hash1 = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let hash2 = "3n58xw4373jp0ljirf06d8077j15pc4j";
        let data = format!(
            "/nix/store/{hash1}-hello-2.12.1/lib\0/nix/store/{hash2}-glibc-2.37/lib"
        );
        let found = scan_references(data.as_bytes(), &[hash1, hash2]);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn scan_file_works() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let mut f = std::fs::File::create(&file).unwrap();
        write!(f, "/nix/store/{hash}-hello").unwrap();

        let found = scan_file(&file, &[hash]).unwrap();
        assert_eq!(found, vec![hash.to_string()]);
    }

    #[test]
    fn scan_empty_data() {
        let found = scan_references(b"", &["sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6"]);
        assert!(found.is_empty());
    }

    #[test]
    fn scan_binary_data() {
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let mut data = vec![0u8; 100];
        data[20..52].copy_from_slice(hash.as_bytes());
        let found = scan_references(&data, &[hash]);
        assert_eq!(found, vec![hash.to_string()]);
    }

    // ── MockFs ────────────────────────────────────────────

    struct MockFs {
        files: std::collections::BTreeMap<PathBuf, Vec<u8>>,
    }

    impl MockFs {
        fn new() -> Self {
            Self {
                files: std::collections::BTreeMap::new(),
            }
        }
        fn with_file(mut self, path: &str, data: &[u8]) -> Self {
            self.files.insert(PathBuf::from(path), data.to_vec());
            self
        }
    }

    impl FileSystem for MockFs {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }
        fn walk_dir(&self, dir: &Path) -> std::io::Result<Vec<PathBuf>> {
            let prefix = dir.to_string_lossy();
            Ok(self
                .files
                .keys()
                .filter(|p| p.to_string_lossy().starts_with(prefix.as_ref()))
                .cloned()
                .collect())
        }
        fn read_link(&self, _: &Path) -> std::io::Result<PathBuf> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "mock: not a symlink",
            ))
        }
        fn is_file(&self, path: &Path) -> bool {
            self.files.contains_key(path)
        }
        fn is_symlink(&self, _: &Path) -> bool {
            false
        }
    }

    #[test]
    fn mock_fs_scan_file() {
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let fs = MockFs::new().with_file("/out/bin/hello", format!("/nix/store/{hash}-hello").as_bytes());
        let found = scan_file_with(&fs, Path::new("/out/bin/hello"), &[hash]).unwrap();
        assert_eq!(found, vec![hash.to_string()]);
    }

    #[test]
    fn mock_fs_scan_directory() {
        let h1 = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let h2 = "3n58xw4373jp0ljirf06d8077j15pc4j";
        let fs = MockFs::new()
            .with_file("/out/a", format!("/nix/store/{h1}-hello").as_bytes())
            .with_file("/out/b", format!("/nix/store/{h2}-glibc").as_bytes());
        let found = scan_directory_with(&fs, Path::new("/out"), &[h1, h2]).unwrap();
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn mock_fs_no_matches() {
        let fs = MockFs::new().with_file("/out/x", b"no store paths");
        let found = scan_directory_with(&fs, Path::new("/out"), &["sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6"]).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn mock_fs_missing_file() {
        let fs = MockFs::new();
        assert!(scan_file_with(&fs, Path::new("/nope"), &["x"]).is_err());
    }

    // ── Overlapping hashes ──────────────────────────────────

    #[test]
    fn scan_overlapping_hashes_in_data() {
        // Two hashes that share a prefix — both should be found independently
        let h1 = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let h2 = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll7"; // differs only in last char (valid nix base32)
        let data = format!(
            "/nix/store/{h1}-hello/lib:/nix/store/{h2}-world/lib"
        );
        let found = scan_references(data.as_bytes(), &[h1, h2]);
        assert_eq!(found.len(), 2);
        assert!(found.contains(&h1.to_string()));
        assert!(found.contains(&h2.to_string()));
    }

    // ── Hash at start/end of data ───────────────────────────

    #[test]
    fn scan_hash_at_start_of_data() {
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        // Hash is the very first bytes in the data
        let data = format!("{hash}-hello/bin/hello");
        let found = scan_references(data.as_bytes(), &[hash]);
        assert_eq!(found, vec![hash.to_string()]);
    }

    #[test]
    fn scan_hash_at_end_of_data() {
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        // Hash is the last 32 bytes
        let data = format!("path=/nix/store/{hash}");
        let found = scan_references(data.as_bytes(), &[hash]);
        assert_eq!(found, vec![hash.to_string()]);
    }

    // ── Aho-Corasick deduplication ──────────────────────────

    #[test]
    fn scan_same_hash_twice_returns_once() {
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let data = format!(
            "/nix/store/{hash}-hello/bin/hello\0/nix/store/{hash}-hello/lib/libhello.so"
        );
        let found = scan_references(data.as_bytes(), &[hash]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], hash);
    }

    #[test]
    fn scan_many_duplicates_still_unique() {
        let hash = "3n58xw4373jp0ljirf06d8077j15pc4j";
        let mut data = Vec::new();
        for _ in 0..10 {
            data.extend_from_slice(format!("/nix/store/{hash}-glibc/lib\n").as_bytes());
        }
        let found = scan_references(&data, &[hash]);
        assert_eq!(found.len(), 1);
    }

    // ── Invalid hash length filtered out ────────────────────

    #[test]
    fn scan_ignores_wrong_length_hashes() {
        let good = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6"; // 32 chars
        let short = "sn5lbjww"; // 8 chars — too short
        let long = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6x"; // 33 chars — too long
        let data = format!("/nix/store/{good}-hello");
        let found = scan_references(data.as_bytes(), &[good, short, long]);
        // Only the valid-length hash should match
        assert_eq!(found, vec![good.to_string()]);
    }

    // ── Empty known_hashes returns empty ─────────────────────

    #[test]
    fn scan_empty_known_hashes() {
        let data = b"/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello";
        let found = scan_references(data, &[]);
        assert!(found.is_empty());
    }

    // ── FileSystem trait: object safety ──────────────────────

    #[test]
    fn filesystem_trait_is_object_safe() {
        fn assert_obj_safe(_: &dyn FileSystem) {}
        assert_obj_safe(&RealFileSystem);
    }

    // ── MockFs with symlink support ─────────────────────────

    struct MockFsWithLinks {
        files: std::collections::BTreeMap<PathBuf, Vec<u8>>,
        links: std::collections::BTreeMap<PathBuf, PathBuf>,
    }

    impl MockFsWithLinks {
        fn new() -> Self {
            Self {
                files: std::collections::BTreeMap::new(),
                links: std::collections::BTreeMap::new(),
            }
        }
        fn with_file(mut self, path: &str, data: &[u8]) -> Self {
            self.files.insert(PathBuf::from(path), data.to_vec());
            self
        }
        fn with_link(mut self, path: &str, target: &str) -> Self {
            self.links.insert(PathBuf::from(path), PathBuf::from(target));
            self
        }
    }

    impl FileSystem for MockFsWithLinks {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }
        fn walk_dir(&self, dir: &Path) -> std::io::Result<Vec<PathBuf>> {
            let prefix = dir.to_string_lossy();
            let mut paths: Vec<_> = self
                .files
                .keys()
                .chain(self.links.keys())
                .filter(|p| p.to_string_lossy().starts_with(prefix.as_ref()))
                .cloned()
                .collect();
            paths.sort();
            Ok(paths)
        }
        fn read_link(&self, path: &Path) -> std::io::Result<PathBuf> {
            self.links
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "not a symlink"))
        }
        fn is_file(&self, path: &Path) -> bool {
            self.files.contains_key(path)
        }
        fn is_symlink(&self, path: &Path) -> bool {
            self.links.contains_key(path)
        }
    }

    #[test]
    fn mock_fs_symlink_target_scanned() {
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let target = format!("/nix/store/{hash}-hello/bin/hello");
        let fs = MockFsWithLinks::new().with_link("/out/link", &target);
        let found = scan_directory_with(&fs, Path::new("/out"), &[hash]).unwrap();
        assert_eq!(found, vec![hash.to_string()]);
    }

    #[test]
    fn mock_fs_symlink_no_match() {
        let fs = MockFsWithLinks::new().with_link("/out/link", "/usr/bin/hello");
        let found =
            scan_directory_with(&fs, Path::new("/out"), &["sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6"])
                .unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn mock_fs_files_and_symlinks_combined() {
        let h1 = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let h2 = "3n58xw4373jp0ljirf06d8077j15pc4j";
        let fs = MockFsWithLinks::new()
            .with_file("/out/bin/hello", format!("/nix/store/{h1}-hello").as_bytes())
            .with_link("/out/lib/link", &format!("/nix/store/{h2}-glibc/lib"));

        let found = scan_directory_with(&fs, Path::new("/out"), &[h1, h2]).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.contains(&h1.to_string()));
        assert!(found.contains(&h2.to_string()));
    }

    // ── scan_directory_with on single file path ─────────────

    #[test]
    fn scan_directory_with_single_file() {
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let fs = MockFs::new().with_file("/out/file", format!("/nix/store/{hash}-hello").as_bytes());
        let found = scan_directory_with(&fs, Path::new("/out/file"), &[hash]).unwrap();
        assert_eq!(found, vec![hash.to_string()]);
    }

    // ── scan_directory deduplicates across files ────────────

    #[test]
    fn scan_directory_deduplicates_across_files() {
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let data = format!("/nix/store/{hash}-hello");
        let fs = MockFs::new()
            .with_file("/out/a", data.as_bytes())
            .with_file("/out/b", data.as_bytes());
        let found = scan_directory_with(&fs, Path::new("/out"), &[hash]).unwrap();
        assert_eq!(found.len(), 1);
    }

    // ── Real filesystem tests ───────────────────────────────

    #[test]
    fn scan_directory_real_fs() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";

        let sub = dir.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();

        let file1 = dir.path().join("a.txt");
        let file2 = sub.join("b.bin");

        std::fs::write(&file1, format!("/nix/store/{hash}-hello")).unwrap();
        std::fs::write(&file2, b"no references").unwrap();

        let found = scan_directory(dir.path(), &[hash]).unwrap();
        assert_eq!(found, vec![hash.to_string()]);
    }

    #[test]
    fn scan_directory_real_fs_no_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.txt"), b"nothing here").unwrap();
        let found = scan_directory(dir.path(), &["sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6"]).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn scan_file_nonexistent() {
        let result = scan_file(Path::new("/tmp/sui-nonexistent-12345"), &["abc"]);
        assert!(result.is_err());
    }

    // ── Large data scan ─────────────────────────────────────

    #[test]
    fn scan_large_data_with_multiple_hashes() {
        let h1 = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let h2 = "3n58xw4373jp0ljirf06d8077j15pc4j";
        let h3 = "abcdefghijklmnopqrstuvwxyz012345";

        let mut data = Vec::with_capacity(10_000);
        for _ in 0..100 {
            data.extend_from_slice(b"padding bytes that don't match anything\n");
        }
        data.extend_from_slice(format!("/nix/store/{h1}-a/lib\n").as_bytes());
        for _ in 0..50 {
            data.extend_from_slice(b"more padding\n");
        }
        data.extend_from_slice(format!("/nix/store/{h2}-b/lib\n").as_bytes());

        let found = scan_references(&data, &[h1, h2, h3]);
        assert_eq!(found.len(), 2);
        assert!(found.contains(&h1.to_string()));
        assert!(found.contains(&h2.to_string()));
    }

    // ── Only-NUL-bytes data ─────────────────────────────────

    #[test]
    fn scan_all_null_bytes() {
        let data = vec![0u8; 1024];
        let found = scan_references(&data, &["sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6"]);
        assert!(found.is_empty());
    }

    // ── scan_references: returned order matches first-occurrence in input ──

    #[test]
    fn scan_references_returns_in_first_match_order() {
        let h_a = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let h_b = "3n58xw4373jp0ljirf06d8077j15pc4j";
        let h_c = "abcdefghijklmnopqrstuvwxyz012345";

        // Order in data: B, then A, then C
        let data = format!(
            "/nix/store/{h_b}/lib /nix/store/{h_a}/bin /nix/store/{h_c}/share"
        );
        let found = scan_references(data.as_bytes(), &[h_a, h_b, h_c]);
        assert_eq!(found.len(), 3);
        // Aho-Corasick visits matches in input order, so the result
        // contains B before A before C.
        assert_eq!(found[0], h_b);
        assert_eq!(found[1], h_a);
        assert_eq!(found[2], h_c);
    }

    // ── scan_references: only some patterns match ───────────

    #[test]
    fn scan_references_subset_match() {
        let h_present = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let h_absent_1 = "3n58xw4373jp0ljirf06d8077j15pc4j";
        let h_absent_2 = "abcdefghijklmnopqrstuvwxyz012345";

        let data = format!("/nix/store/{h_present}-pkg/bin");
        let found = scan_references(data.as_bytes(), &[h_present, h_absent_1, h_absent_2]);
        assert_eq!(found, vec![h_present.to_string()]);
    }

    // ── scan_references: all-empty inputs ────────────────────

    #[test]
    fn scan_references_empty_data_and_no_hashes() {
        let found = scan_references(b"", &[]);
        assert!(found.is_empty());
    }

    #[test]
    fn scan_references_only_invalid_length_hashes() {
        let data = b"/nix/store/sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6-hello";
        // None of these are 32 chars
        let found = scan_references(data, &["short", "alsoshort", "definitelywaytoolongforananixhash33"]);
        assert!(found.is_empty());
    }

    // ── scan_references: large pattern set, sparse matches ──

    #[test]
    fn scan_references_many_patterns_sparse_matches() {
        // Generate 50 unique 32-char hashes by encoding the index in
        // the leading characters and padding with a fixed tail.
        // This guarantees no two patterns share a substring near the
        // index region while still being valid 32-char inputs.
        let mut hashes: Vec<String> = Vec::with_capacity(50);
        for i in 0..50_u32 {
            // 4 hex chars of index + 28-char fixed tail = 32 chars
            let h = format!("{i:04x}efghijklmnopqrstuvwxyz012345");
            assert_eq!(h.len(), 32);
            hashes.push(h);
        }
        let h_refs: Vec<&str> = hashes.iter().map(String::as_str).collect();

        let data = format!(
            "/nix/store/{}-a /nix/store/{}-b",
            hashes[7], hashes[33]
        );
        let found = scan_references(data.as_bytes(), &h_refs);
        assert_eq!(found.len(), 2);
        assert!(found.contains(&hashes[7]));
        assert!(found.contains(&hashes[33]));
    }

    // ── scan_file: exact contents are scanned ───────────────

    #[test]
    fn scan_file_with_returns_io_error_on_missing() {
        let fs = MockFs::new();
        let err = scan_file_with(&fs, Path::new("/missing"), &["abc"]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    // ── scan_directory_with: dedup across many files ────────

    #[test]
    fn scan_directory_with_three_files_two_unique_hashes() {
        let h1 = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let h2 = "3n58xw4373jp0ljirf06d8077j15pc4j";
        let fs = MockFs::new()
            .with_file("/out/a", format!("/nix/store/{h1}-a").as_bytes())
            .with_file("/out/b", format!("/nix/store/{h2}-b").as_bytes())
            .with_file("/out/c", format!("/nix/store/{h1}-c").as_bytes());
        let found = scan_directory_with(&fs, Path::new("/out"), &[h1, h2]).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.contains(&h1.to_string()));
        assert!(found.contains(&h2.to_string()));
    }

    // ── RealFileSystem trait method coverage ────────────────

    #[test]
    fn real_filesystem_is_file_existing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"data").unwrap();
        let fs = RealFileSystem;
        assert!(fs.is_file(&file));
        assert!(!fs.is_file(dir.path()));
    }

    #[test]
    fn real_filesystem_is_file_nonexistent() {
        let fs = RealFileSystem;
        assert!(!fs.is_file(Path::new("/tmp/sui-build-no-such-file-123456")));
    }

    #[test]
    fn real_filesystem_is_symlink_false_for_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("regular");
        std::fs::write(&file, b"data").unwrap();
        let fs = RealFileSystem;
        assert!(!fs.is_symlink(&file));
    }

    #[cfg(unix)]
    #[test]
    fn real_filesystem_read_link_works() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        std::fs::write(&target, b"hello").unwrap();
        symlink(&target, &link).unwrap();

        let fs = RealFileSystem;
        assert!(fs.is_symlink(&link));
        let read = fs.read_link(&link).unwrap();
        assert_eq!(read, target);
    }

    #[test]
    fn real_filesystem_read_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.bin");
        std::fs::write(&file, b"\x00\x01\x02\x03").unwrap();
        let fs = RealFileSystem;
        let data = fs.read_file(&file).unwrap();
        assert_eq!(data, vec![0u8, 1, 2, 3]);
    }

    #[test]
    fn real_filesystem_walk_dir_empty() {
        let dir = tempfile::tempdir().unwrap();
        let fs = RealFileSystem;
        let files = fs.walk_dir(dir.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn real_filesystem_walk_dir_nested() {
        let dir = tempfile::tempdir().unwrap();
        let sub_a = dir.path().join("a");
        let sub_b = dir.path().join("b");
        std::fs::create_dir(&sub_a).unwrap();
        std::fs::create_dir(&sub_b).unwrap();
        std::fs::write(sub_a.join("file1"), b"x").unwrap();
        std::fs::write(sub_b.join("file2"), b"y").unwrap();
        std::fs::write(dir.path().join("top"), b"z").unwrap();

        let fs = RealFileSystem;
        let files = fs.walk_dir(dir.path()).unwrap();
        assert_eq!(files.len(), 3);
    }

    // ── scan_directory_with: when dir is a file path ─────────

    #[test]
    fn scan_directory_with_real_file_as_path() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let file = dir.path().join("only.txt");
        std::fs::write(&file, format!("/nix/store/{hash}-x")).unwrap();
        let found = scan_directory(&file, &[hash]).unwrap();
        assert_eq!(found, vec![hash.to_string()]);
    }

    // ── scan_file_with via custom FS: empty file ────────────

    #[test]
    fn scan_file_with_empty_file_returns_empty() {
        let fs = MockFs::new().with_file("/out/empty", b"");
        let found = scan_file_with(&fs, Path::new("/out/empty"), &["sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6"]).unwrap();
        assert!(found.is_empty());
    }

    // ── MockFsWithLinks: directory with no entries ──────────

    #[test]
    fn mock_fs_with_links_empty_dir() {
        let fs = MockFsWithLinks::new();
        let found = scan_directory_with(&fs, Path::new("/empty"), &["sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6"]).unwrap();
        assert!(found.is_empty());
    }

    // ── Hash adjacent to other matching prefixes ────────────

    #[test]
    fn scan_hash_inside_longer_alphanumeric_run() {
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        // Embed hash inside a sea of similar characters.
        let data = format!("xxxxxxxx{hash}yyyyyyyy");
        let found = scan_references(data.as_bytes(), &[hash]);
        assert_eq!(found, vec![hash.to_string()]);
    }
}
