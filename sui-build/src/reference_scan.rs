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
pub fn scan_references(data: &[u8], known_hashes: &[&str]) -> Vec<String> {
    let valid: Vec<&str> = known_hashes
        .iter()
        .filter(|h| h.len() == STORE_PATH_HASH_LEN)
        .copied()
        .collect();

    if valid.is_empty() || data.is_empty() {
        return Vec::new();
    }

    let ac = AhoCorasick::new(&valid).expect("valid patterns");
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
pub fn scan_file(path: &Path, known_hashes: &[&str]) -> std::io::Result<Vec<String>> {
    scan_file_with(&RealFileSystem, path, known_hashes)
}

/// Scan a file using a custom filesystem implementation.
pub fn scan_file_with(fs: &dyn FileSystem, path: &Path, known_hashes: &[&str]) -> std::io::Result<Vec<String>> {
    let data = fs.read_file(path)?;
    Ok(scan_references(&data, known_hashes))
}

/// Scan a directory tree for store path references (uses real filesystem).
pub fn scan_directory(dir: &Path, known_hashes: &[&str]) -> std::io::Result<Vec<String>> {
    scan_directory_with(&RealFileSystem, dir, known_hashes)
}

/// Scan a directory tree using a custom filesystem implementation.
pub fn scan_directory_with(
    fs: &dyn FileSystem,
    dir: &Path,
    known_hashes: &[&str],
) -> std::io::Result<Vec<String>> {
    let mut all_refs = Vec::new();

    if fs.is_file(dir) {
        return scan_file_with(fs, dir, known_hashes);
    }

    for path in fs.walk_dir(dir)? {
        if fs.is_file(&path) {
            let refs = scan_file_with(fs, &path, known_hashes)?;
            for r in refs {
                if !all_refs.contains(&r) {
                    all_refs.push(r);
                }
            }
        } else if fs.is_symlink(&path) {
            if let Ok(target) = fs.read_link(&path) {
                let target_str = target.to_string_lossy();
                let refs = scan_references(target_str.as_bytes(), known_hashes);
                for r in refs {
                    if !all_refs.contains(&r) {
                        all_refs.push(r);
                    }
                }
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
        files: std::collections::HashMap<PathBuf, Vec<u8>>,
    }

    impl MockFs {
        fn new() -> Self { Self { files: std::collections::HashMap::new() } }
        fn with_file(mut self, path: &str, data: &[u8]) -> Self {
            self.files.insert(PathBuf::from(path), data.to_vec());
            self
        }
    }

    impl FileSystem for MockFs {
        fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            self.files.get(path).cloned().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }
        fn walk_dir(&self, dir: &Path) -> std::io::Result<Vec<PathBuf>> {
            let prefix = dir.to_string_lossy();
            Ok(self.files.keys().filter(|p| p.to_string_lossy().starts_with(prefix.as_ref())).cloned().collect())
        }
        fn read_link(&self, _: &Path) -> std::io::Result<PathBuf> {
            Err(std::io::Error::new(std::io::ErrorKind::Other, "mock: not a symlink"))
        }
        fn is_file(&self, path: &Path) -> bool { self.files.contains_key(path) }
        fn is_symlink(&self, _: &Path) -> bool { false }
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
}
