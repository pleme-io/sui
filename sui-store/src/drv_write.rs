//! Instantiating a derivation: putting its `.drv` into the store.
//!
//! CppNix's `derivationStrict` ends in `writeDerivation`, which adds the `.drv`
//! to the store as a text object (through the daemon on a multi-user install)
//! unless the evaluator runs read-only (`--read-only` / `--readonly-mode`), in
//! which case the path is computed and nothing is written. This module is that
//! step for sui, with the same two modes and nothing in between:
//!
//! * [`DrvWriteMode::Instantiate`] (the default) — the `.drv` reaches the store,
//!   or the evaluation fails with a [`DrvWriteError`] naming the path and the
//!   cause.
//! * [`DrvWriteMode::ReadOnly`] — nothing is written. The caller selects it
//!   explicitly ([`set_process_mode`], `sui eval --read-only`).
//!
//! There is no third outcome. sui used to park an unwritable `.drv` in
//! `$TMPDIR/sui-drv-cache`, log at debug and report success, so `sui eval`
//! printed drvPaths that existed nowhere a consumer looks.
//!
//! # Where the `.drv` goes
//!
//! 1. `SUI_STORE_DIR=<dir>` (tests, alternate stores): a file under `<dir>`.
//! 2. `NIX_REMOTE=daemon` / `unix://<socket>`: the daemon.
//! 3. Otherwise, a reachable daemon socket: the daemon, over the process's
//!    long-lived connection ([`crate::daemon_session`]), so the daemon's
//!    temporary GC root on the path lives as long as this process. The daemon
//!    also computes the path itself; a path that differs from sui's is a parity
//!    defect and fails the write ([`DrvWriteError::PathMismatch`]).
//! 4. Otherwise, a writable `/nix/store` (single-user install): a file in the
//!    store. The file is not registered in the store database; that was sui's
//!    behaviour before this module and is unchanged.
//! 5. Otherwise: [`DrvWriteError::NoWritePath`].
//!
//! Step 3 prefers the daemon even where the store is writable (root on a
//! multi-user machine), where CppNix would write locally. Both leave a valid
//! store path; the daemon route also registers it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use sui_compat::derivation::Derivation;

use crate::daemon_realize::{DEFAULT_DAEMON_SOCKET, WritableStore};
use crate::worker_client::WorkerError;

/// Whether evaluation instantiates derivations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrvWriteMode {
    /// Write every `.drv` to the store (CppNix's default).
    Instantiate,
    /// Compute paths only (CppNix's `--read-only`). Selected explicitly.
    ReadOnly,
}

static PROCESS_MODE: AtomicU8 = AtomicU8::new(0);

/// Select the mode for every evaluation in this process.
pub fn set_process_mode(mode: DrvWriteMode) {
    PROCESS_MODE.store(u8::from(mode == DrvWriteMode::ReadOnly), Ordering::SeqCst);
}

/// The mode selected for this process.
#[must_use]
pub fn process_mode() -> DrvWriteMode {
    if PROCESS_MODE.load(Ordering::SeqCst) == 1 {
        DrvWriteMode::ReadOnly
    } else {
        DrvWriteMode::Instantiate
    }
}

/// Where a `.drv` is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrvDestination {
    /// A directory standing in for `/nix/store` (`SUI_STORE_DIR`).
    Dir(PathBuf),
    /// The nix daemon at this socket.
    Daemon(PathBuf),
    /// A `/nix/store` this process can write.
    DirectStore(PathBuf),
}

/// What [`write_drv`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrvWritten {
    /// Added through the daemon (or already added by this process).
    Daemon,
    /// Written as a file (or already present) at this path.
    File(PathBuf),
    /// Read-only mode: nothing was written.
    NotWritten,
}

/// Why a `.drv` could not be put into the store.
#[derive(Debug, thiserror::Error)]
pub enum DrvWriteError {
    /// Writing the file failed.
    #[error("cannot write derivation {drv_path} to {file}: {cause}")]
    File {
        drv_path: String,
        file: PathBuf,
        #[source]
        cause: std::io::Error,
    },
    /// The daemon did not accept the `.drv`.
    #[error("cannot add derivation {drv_path} through the nix daemon at {socket}: {cause}")]
    Daemon {
        drv_path: String,
        socket: PathBuf,
        #[source]
        cause: WorkerError,
    },
    /// The daemon placed the `.drv` at a different path than sui computed.
    #[error(
        "derivation path mismatch: sui computed {computed}, the nix daemon stored the same \
         bytes at {daemon} (a sui/nix parity defect in the .drv text or its references)"
    )]
    PathMismatch { computed: String, daemon: String },
    /// `NIX_REMOTE` names a store sui cannot write to.
    #[error("cannot write derivation {drv_path}: NIX_REMOTE={remote} is not a store sui can write to")]
    UnsupportedRemote { drv_path: String, remote: String },
    /// No write path exists and read-only mode was not selected.
    #[error(
        "cannot write derivation {drv_path}: /nix/store is not writable and no nix daemon \
         socket is reachable at {socket}. Run the daemon, or evaluate with --read-only to \
         compute paths without instantiating"
    )]
    NoWritePath { drv_path: String, socket: PathBuf },
}

/// The references CppNix gives a `.drv` text object: its input derivations and
/// input sources, sorted and deduplicated.
#[must_use]
pub fn drv_references(drv: &Derivation) -> Vec<String> {
    let mut refs: Vec<String> = drv.input_derivations.keys().cloned().collect();
    refs.extend(drv.input_sources.iter().cloned());
    refs.sort();
    refs.dedup();
    refs
}

/// Instantiate `drv` at `drv_path` under the process mode and the detected
/// destination.
///
/// # Errors
/// Any [`DrvWriteError`]; in [`DrvWriteMode::Instantiate`] there is no way to
/// return `Ok` without the `.drv` in the store.
pub fn write_drv(drv_path: &str, drv: &Derivation) -> Result<DrvWritten, DrvWriteError> {
    if process_mode() == DrvWriteMode::ReadOnly {
        return Ok(DrvWritten::NotWritten);
    }
    let dest = detect_destination(drv_path)?;
    write_drv_to(&dest, drv_path, drv)
}

/// Resolve where a `.drv` goes (see the module docs for the order).
///
/// # Errors
/// [`DrvWriteError::UnsupportedRemote`] or [`DrvWriteError::NoWritePath`].
pub fn detect_destination(drv_path: &str) -> Result<DrvDestination, DrvWriteError> {
    if let Ok(dir) = std::env::var("SUI_STORE_DIR")
        && dir != "/nix/store"
    {
        return Ok(DrvDestination::Dir(PathBuf::from(dir)));
    }
    match std::env::var("NIX_REMOTE").as_deref() {
        Ok(v) if v.starts_with("unix://") => {
            return Ok(DrvDestination::Daemon(PathBuf::from(v.trim_start_matches("unix://"))));
        }
        Ok("daemon") => return Ok(DrvDestination::Daemon(PathBuf::from(DEFAULT_DAEMON_SOCKET))),
        Ok("" | "auto") | Err(_) => {}
        Ok(other) => {
            return Err(DrvWriteError::UnsupportedRemote {
                drv_path: drv_path.to_string(),
                remote: other.to_string(),
            });
        }
    }
    static AUTO: OnceLock<DrvDestination> = OnceLock::new();
    if let Some(d) = AUTO.get() {
        return Ok(d.clone());
    }
    let socket = PathBuf::from(DEFAULT_DAEMON_SOCKET);
    let dest = if socket.exists() {
        DrvDestination::Daemon(socket)
    } else if let Some(w) = WritableStore::probe(Path::new("/nix/store")) {
        DrvDestination::DirectStore(w.store_dir().to_path_buf())
    } else {
        return Err(DrvWriteError::NoWritePath { drv_path: drv_path.to_string(), socket });
    };
    Ok(AUTO.get_or_init(|| dest).clone())
}

/// Instantiate `drv` at `drv_path` into an explicit destination.
///
/// # Errors
/// Any [`DrvWriteError`].
pub fn write_drv_to(
    dest: &DrvDestination,
    drv_path: &str,
    drv: &Derivation,
) -> Result<DrvWritten, DrvWriteError> {
    match dest {
        DrvDestination::Dir(dir) | DrvDestination::DirectStore(dir) => {
            let base = Path::new(drv_path).file_name().unwrap_or_default();
            let file = dir.join(base);
            if file.exists() {
                return Ok(DrvWritten::File(file));
            }
            let file_err = |cause| DrvWriteError::File {
                drv_path: drv_path.to_string(),
                file: file.clone(),
                cause,
            };
            std::fs::create_dir_all(dir).map_err(file_err)?;
            std::fs::write(&file, drv.serialize().as_bytes()).map_err(file_err)?;
            Ok(DrvWritten::File(file))
        }
        DrvDestination::Daemon(socket) => {
            static ADDED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
            let mut added = ADDED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if added.as_ref().is_some_and(|s| s.contains(drv_path)) {
                return Ok(DrvWritten::Daemon);
            }
            let base = Path::new(drv_path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let name = sui_compat::source::strip_store_hash_prefix(base);
            let text = drv.serialize();
            let refs = drv_references(drv);
            let stored = crate::daemon_session::with_session(socket, |conn| {
                conn.add_text_to_store(name, text.as_bytes(), &refs)
            })
            .map_err(|cause| DrvWriteError::Daemon {
                drv_path: drv_path.to_string(),
                socket: socket.clone(),
                cause,
            })?;
            if stored != drv_path {
                return Err(DrvWriteError::PathMismatch {
                    computed: drv_path.to_string(),
                    daemon: stored,
                });
            }
            added.get_or_insert_with(HashSet::new).insert(drv_path.to_string());
            Ok(DrvWritten::Daemon)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn tiny_drv() -> Derivation {
        Derivation {
            outputs: BTreeMap::new(),
            input_derivations: BTreeMap::new(),
            input_sources: vec!["/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-src".into(),
                                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src".into()],
            system: "x86_64-linux".into(),
            builder: "/bin/sh".into(),
            args: vec![],
            env: BTreeMap::new(),
        }
    }

    const DRV: &str = "/nix/store/cccccccccccccccccccccccccccccccc-t.drv";

    #[test]
    fn a_refused_file_write_is_an_error_naming_path_and_cause() {
        let dir = tempfile::tempdir().unwrap();
        if std::fs::metadata(dir.path()).unwrap().uid() == 0 {
            return; // root ignores the permission bits this test relies on
        }
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let err = write_drv_to(&DrvDestination::Dir(dir.path().into()), DRV, &tiny_drv())
            .unwrap_err()
            .to_string();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.contains(DRV), "{err}");
        assert!(err.contains("ermission denied"), "{err}");
    }

    #[test]
    fn a_file_write_lands_the_serialized_drv() {
        let dir = tempfile::tempdir().unwrap();
        let out = write_drv_to(&DrvDestination::Dir(dir.path().into()), DRV, &tiny_drv()).unwrap();
        let file = dir.path().join("cccccccccccccccccccccccccccccccc-t.drv");
        assert_eq!(out, DrvWritten::File(file.clone()));
        assert_eq!(std::fs::read_to_string(file).unwrap(), tiny_drv().serialize());
    }

    #[test]
    fn an_unreachable_daemon_is_an_error_not_a_fallback() {
        let dest = DrvDestination::Daemon(PathBuf::from("/nonexistent/sui-c1-socket"));
        let err = write_drv_to(&dest, DRV, &tiny_drv()).unwrap_err();
        assert!(matches!(err, DrvWriteError::Daemon { .. }), "{err}");
        assert!(err.to_string().contains(DRV));
    }

    #[test]
    fn references_are_inputs_sorted_and_deduplicated() {
        let mut d = tiny_drv();
        d.input_sources.push("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src".into());
        let refs = drv_references(&d);
        assert_eq!(refs, vec![
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src".to_string(),
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-src".to_string(),
        ]);
    }
}
