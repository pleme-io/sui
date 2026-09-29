//! Locating CppNix, the differential oracle.
//!
//! The oracle binary is found by absolute path, never by whatever a shell alias
//! or a narrowed `PATH` happens to resolve: `SUI_CPPNIX_BIN_DIR` when set, then
//! the two places a nix install puts its tools, then `PATH`.

use std::path::PathBuf;

const INSTALL_DIRS: &[&str] = &[
    "/nix/var/nix/profiles/default/bin",
    "/run/current-system/sw/bin",
];

/// The absolute path of CppNix's `tool` (`nix`, `nix-store`, …), or `None`.
#[must_use]
pub fn locate(tool: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SUI_CPPNIX_BIN_DIR") {
        let p = PathBuf::from(dir).join(tool);
        return p.exists().then_some(p);
    }
    INSTALL_DIRS
        .iter()
        .map(|d| PathBuf::from(d).join(tool))
        .find(|p| p.exists())
        .or_else(|| {
            std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths).map(|d| d.join(tool)).find(|p| p.exists())
            })
        })
}
