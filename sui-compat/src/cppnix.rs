//! Locating CppNix, the differential oracle.
//!
//! The oracle binary is found by absolute path, never by whatever a shell alias
//! or a narrowed `PATH` happens to resolve: `SUI_CPPNIX_BIN_DIR` when set, then
//! the two places a nix install puts its tools, then `PATH`.

use std::path::{Path, PathBuf};

const INSTALL_DIRS: &[&str] = &[
    "/nix/var/nix/profiles/default/bin",
    "/run/current-system/sw/bin",
];

/// The absolute path of CppNix's `tool` (`nix`, `nix-store`, …), or `None`.
/// A candidate that is sui under a nix name is never the oracle; a `cppnix`
/// alias leads to the real install's sibling tools.
#[must_use]
pub fn locate(tool: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SUI_CPPNIX_BIN_DIR") {
        let p = PathBuf::from(dir).join(tool);
        return p.exists().then_some(p);
    }
    let dirs: Vec<PathBuf> = INSTALL_DIRS
        .iter()
        .map(PathBuf::from)
        .chain(std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default())
        .collect();
    locate_in(tool, &dirs)
}

fn is_sui(path: &Path) -> bool {
    std::fs::canonicalize(path).is_ok_and(|real| real.file_name().is_some_and(|n| n == "sui"))
}

#[must_use]
pub fn locate_in(tool: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .map(|d| d.join(tool))
        .find(|p| p.exists() && !is_sui(p))
        .or_else(|| {
            dirs.iter()
                .map(|d| d.join("cppnix"))
                .filter(|alias| alias.exists() && !is_sui(alias))
                .find_map(|alias| {
                    let sibling = std::fs::canonicalize(alias).ok()?.parent()?.join(tool);
                    (sibling.exists() && !is_sui(&sibling)).then_some(sibling)
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn sui_under_a_nix_name_is_never_the_oracle_and_the_cppnix_alias_leads_to_the_real_tools() {
        let root = tempfile::tempdir().unwrap();
        let sui_dir = root.path().join("sui/bin");
        let real = root.path().join("nix-2.31.5/bin");
        let system = root.path().join("system/bin");
        for d in [&sui_dir, &real, &system] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(sui_dir.join("sui"), "").unwrap();
        for t in ["nix", "nix-store"] {
            std::fs::write(real.join(t), "").unwrap();
            symlink(sui_dir.join("sui"), system.join(t)).unwrap();
        }
        let dirs = vec![system.clone()];
        assert_eq!(locate_in("nix", &dirs), None);
        symlink(real.join("nix"), system.join("cppnix")).unwrap();
        let real = std::fs::canonicalize(&real).unwrap();
        assert_eq!(locate_in("nix", &dirs), Some(real.join("nix")));
        assert_eq!(locate_in("nix-store", &dirs), Some(real.join("nix-store")));
        assert_eq!(locate_in("nix-build", &dirs), None);
        let plain = root.path().join("plain/bin");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("nix"), "").unwrap();
        assert_eq!(locate_in("nix", &[plain.clone(), system]), Some(plain.join("nix")));
        let sui_alias = root.path().join("sui-alias/bin");
        std::fs::create_dir_all(&sui_alias).unwrap();
        std::fs::write(sui_dir.join("nix-build"), "").unwrap();
        symlink(sui_dir.join("sui"), sui_alias.join("cppnix")).unwrap();
        assert_eq!(locate_in("nix-build", &[sui_alias]), None);
    }

    #[test]
    fn an_explicit_oracle_dir_wins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("nix"), "").unwrap();
        // SAFETY: the only lib test that reads SUI_CPPNIX_BIN_DIR.
        unsafe { std::env::set_var("SUI_CPPNIX_BIN_DIR", dir.path()) };
        assert_eq!(locate("nix"), Some(dir.path().join("nix")));
        assert_eq!(locate("nix-store"), None);
        unsafe { std::env::remove_var("SUI_CPPNIX_BIN_DIR") };
    }
}
