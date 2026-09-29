//! Permanent GC roots: CppNix's `addPermRoot`, for `./result` and friends.
//!
//! `nix build` leaves `./result` pointing at its output and registers that link
//! as an **indirect** root: `<state>/gcroots/auto/<nix32(sha1(link))>` points at
//! the link, the collector follows it, and deleting `./result` releases the
//! output. Without the registration the output is garbage the moment the
//! building process exits.
//!
//! [`add_perm_root`] follows CppNix step for step: refuse a link inside the
//! store, temp-root the path first, refuse to clobber anything that is not
//! already a link into the store, replace the link atomically, then register it
//! with the daemon (`AddIndirectRoot`) or, on a store this process owns,
//! directly under `gcroots/auto`.

use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use crate::drv_write::DrvDestination;
use crate::worker_client::WorkerError;

/// Why a permanent root could not be created.
#[derive(Debug, thiserror::Error)]
pub enum GcRootError {
    /// CppNix's refusal: a root inside the store would root itself.
    #[error("creating a garbage collector root ({0}) in the Nix store is forbidden")]
    InStore(PathBuf),
    /// Something other than a link into the store is in the way.
    #[error("cannot create symlink '{0}'; already exists")]
    Exists(PathBuf),
    /// Filesystem failure creating a link.
    #[error("cannot create GC root {link}: {cause}")]
    Io {
        link: PathBuf,
        #[source]
        cause: std::io::Error,
    },
    /// The daemon refused the temp or indirect root.
    #[error("cannot register GC root {link} with the nix daemon: {cause}")]
    Daemon {
        link: PathBuf,
        #[source]
        cause: WorkerError,
    },
    /// No store to register with.
    #[error("cannot register GC root {0}: {1}")]
    NoStore(PathBuf, String),
}

/// The `gcroots/auto` entry name CppNix uses for an indirect root at `link`.
#[must_use]
pub fn auto_root_name(link: &str) -> String {
    sui_compat::store_path::nix_base32_encode(&Sha1::digest(link.as_bytes()))
}

/// Make `gc_root` a symlink to `store_path` and register it as an indirect GC
/// root. Returns the absolute link path.
///
/// # Errors
/// Any [`GcRootError`]; the link is never left unregistered on success.
pub fn add_perm_root(store_path: &str, gc_root: &Path) -> Result<PathBuf, GcRootError> {
    let link = if gc_root.is_absolute() {
        gc_root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|cause| GcRootError::Io { link: gc_root.to_path_buf(), cause })?
            .join(gc_root)
    };
    let dest = crate::drv_write::detect_destination(store_path)
        .map_err(|e| GcRootError::NoStore(link.clone(), e.to_string()))?;
    let store_dir = match &dest {
        DrvDestination::Dir(d) => d.to_string_lossy().into_owned(),
        _ => "/nix/store".to_string(),
    };
    if link.starts_with(&store_dir) {
        return Err(GcRootError::InStore(link));
    }
    if let DrvDestination::Daemon(socket) = &dest {
        crate::daemon_session::add_temp_root(socket, store_path)
            .map_err(|cause| GcRootError::Daemon { link: link.clone(), cause })?;
    }
    if let Ok(meta) = std::fs::symlink_metadata(&link) {
        let points_into_store = meta.file_type().is_symlink()
            && std::fs::read_link(&link).is_ok_and(|t| t.starts_with(&store_dir));
        if !points_into_store {
            return Err(GcRootError::Exists(link));
        }
    }
    replace_symlink(&link, Path::new(store_path))?;
    let link_str = link.to_string_lossy().into_owned();
    match &dest {
        DrvDestination::Daemon(socket) => crate::daemon_session::add_indirect_root(socket, &link_str)
            .map_err(|cause| GcRootError::Daemon { link: link.clone(), cause })?,
        DrvDestination::Dir(d) | DrvDestination::DirectStore(d) => {
            let state = crate::local::state_dir_for_store(&d.to_string_lossy());
            let auto = Path::new(&state).join("gcroots/auto");
            std::fs::create_dir_all(&auto)
                .map_err(|cause| GcRootError::Io { link: auto.clone(), cause })?;
            replace_symlink(&auto.join(auto_root_name(&link_str)), &link)?;
        }
    }
    Ok(link)
}

/// `makeSymlink`: create at a temporary name, then rename over `link`.
fn replace_symlink(link: &Path, target: &Path) -> Result<(), GcRootError> {
    let io = |cause| GcRootError::Io { link: link.to_path_buf(), cause };
    let parent = link.parent().unwrap_or(Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        link.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).map_err(io)?;
    std::fs::rename(&tmp, link).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_root_name_is_nix32_of_sha1() {
        // sha1("/tmp/result") in nix32, as CppNix's LocalStore::addIndirectRoot
        // computes it; 32 chars for a 20-byte digest.
        let name = auto_root_name("/tmp/result");
        assert_eq!(name.len(), 32);
        assert_eq!(name, sui_compat::store_path::nix_base32_encode(&Sha1::digest(b"/tmp/result")));
    }

    #[test]
    fn a_link_into_a_local_store_is_registered_under_gcroots_auto() {
        let _g = crate::drv_write::tests::STORE_DIR_ENV.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("nix/store");
        std::fs::create_dir_all(&store).unwrap();
        let out = store.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-thing");
        std::fs::write(&out, b"x").unwrap();
        unsafe { std::env::set_var("SUI_STORE_DIR", &store) };
        let link = root.path().join("work/result");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        let made = add_perm_root(&out.to_string_lossy(), &link);
        unsafe { std::env::remove_var("SUI_STORE_DIR") };
        let made = made.unwrap();
        assert_eq!(std::fs::read_link(&made).unwrap(), out);
        let auto = root.path().join("nix/var/nix/gcroots/auto").join(auto_root_name(&made.to_string_lossy()));
        assert_eq!(std::fs::read_link(auto).unwrap(), made);
        // The existing find_gc_roots walk now sees the output as live.
        assert!(crate::local::find_gc_roots(&store.to_string_lossy()).contains(&out.to_string_lossy().into_owned()));
    }

    #[test]
    fn a_non_link_in_the_way_is_refused_and_a_store_link_is_refused() {
        let _g = crate::drv_write::tests::STORE_DIR_ENV.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("nix/store");
        std::fs::create_dir_all(&store).unwrap();
        let out = store.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-thing");
        let file = root.path().join("result");
        std::fs::write(&file, b"precious").unwrap();
        unsafe { std::env::set_var("SUI_STORE_DIR", &store) };
        let clobber = add_perm_root(&out.to_string_lossy(), &file);
        let in_store = add_perm_root(&out.to_string_lossy(), &store.join("result"));
        unsafe { std::env::remove_var("SUI_STORE_DIR") };
        assert!(matches!(clobber, Err(GcRootError::Exists(_))));
        assert_eq!(std::fs::read(&file).unwrap(), b"precious");
        assert!(matches!(in_store, Err(GcRootError::InStore(_))));
    }
}
