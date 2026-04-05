//! Reference scanner — detects store path references in build outputs.
//!
//! After a build completes, all output files are scanned for byte patterns
//! matching the 32-character hash portion of store paths. This determines
//! the runtime closure (which paths the output actually references).

use aho_corasick::AhoCorasick;
use sui_compat::store_path::STORE_PATH_HASH_LEN;

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
    let mut seen = std::collections::HashSet::new();

    for mat in ac.find_iter(data) {
        let idx = mat.pattern().as_usize();
        if seen.insert(idx) {
            found.push(valid[idx].to_string());
        }
    }

    found
}

/// Scan a file for store path references.
pub fn scan_file(path: &std::path::Path, known_hashes: &[&str]) -> std::io::Result<Vec<String>> {
    let data = std::fs::read(path)?;
    Ok(scan_references(&data, known_hashes))
}

/// Scan an entire directory tree for store path references.
pub fn scan_directory(
    dir: &std::path::Path,
    known_hashes: &[&str],
) -> std::io::Result<Vec<String>> {
    let mut all_refs = Vec::new();

    if dir.is_file() {
        return scan_file(dir, known_hashes);
    }

    for entry in walkdir(dir)? {
        let path = entry;
        if path.is_file() {
            let refs = scan_file(&path, known_hashes)?;
            for r in refs {
                if !all_refs.contains(&r) {
                    all_refs.push(r);
                }
            }
        } else if path.is_symlink() {
            // Check symlink target for store path references
            if let Ok(target) = std::fs::read_link(&path) {
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
        // Store path hash embedded in binary data
        let hash = "sn5lbjwwmkbzj7cx0hfnlwf4sh16cll6";
        let mut data = vec![0u8; 100];
        data[20..52].copy_from_slice(hash.as_bytes());
        let found = scan_references(&data, &[hash]);
        assert_eq!(found, vec![hash.to_string()]);
    }
}
