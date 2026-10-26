use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

#[derive(Default)]
struct Registry {
    file: Option<(PathBuf, File)>,
    by_connection: BTreeMap<u64, BTreeSet<String>>,
}

fn registries() -> MutexGuard<'static, BTreeMap<PathBuf, Registry>> {
    static REGISTRIES: OnceLock<Mutex<BTreeMap<PathBuf, Registry>>> = OnceLock::new();
    REGISTRIES.get_or_init(Mutex::default).lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) fn next_connection() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A connection's temp roots, released when the connection is dropped.
pub(super) struct ConnectionRoots(u64);

impl ConnectionRoots {
    pub(super) fn new() -> Self {
        Self(next_connection())
    }

    pub(super) fn id(&self) -> u64 {
        self.0
    }
}

impl Drop for ConnectionRoots {
    fn drop(&mut self) {
        release(self.0);
    }
}

impl Registry {
    fn open(&mut self, state_dir: &Path) -> std::io::Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        let dir = state_dir.join("temproots");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(std::process::id().to_string());
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path)?;
        file.lock()?;
        self.file = Some((path, file));
        Ok(())
    }

    fn rewrite(&mut self) -> std::io::Result<()> {
        let roots: BTreeSet<&String> = self.by_connection.values().flatten().collect();
        let Some((_, file)) = self.file.as_mut() else { return Ok(()) };
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        for root in roots {
            file.write_all(root.as_bytes())?;
            file.write_all(&[0])?;
        }
        file.flush()
    }
}

/// cppnix's `addTempRoot`, held per connection: the path stays a root, in
/// `<state>/temproots/<pid>` under this process's lock, until the connection
/// that added it closes.
pub(super) fn add(state_dir: &Path, connection: u64, path: &str) -> std::io::Result<()> {
    let mut all = registries();
    let reg = all.entry(state_dir.to_path_buf()).or_default();
    reg.open(state_dir)?;
    if reg.by_connection.entry(connection).or_default().insert(path.to_string()) {
        reg.rewrite()?;
    }
    Ok(())
}

pub(super) fn release(connection: u64) {
    for reg in registries().values_mut() {
        if reg.by_connection.remove(&connection).is_some() {
            let _ = reg.rewrite();
        }
    }
}

pub(super) fn all(state_dir: &Path) -> Vec<String> {
    let all = registries();
    let Some(reg) = all.get(state_dir) else { return Vec::new() };
    let roots: BTreeSet<&String> = reg.by_connection.values().flatten().collect();
    roots.into_iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connections_temp_roots_live_in_a_locked_file_until_it_closes() {
        let state = tempfile::tempdir().unwrap();
        let (a, b) = (next_connection(), next_connection());
        let dropped = ConnectionRoots::new();
        add(state.path(), dropped.id(), "/nix/store/cccccccccccccccccccccccccccccccc-c").unwrap();
        drop(dropped);
        add(state.path(), a, "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a").unwrap();
        add(state.path(), b, "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b").unwrap();
        add(state.path(), b, "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a").unwrap();
        let seen: Vec<String> =
            sui_store::find_temp_roots(state.path(), "/nix/store").into_iter().map(|(_, p)| p).collect();
        assert_eq!(seen, vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a", "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b"]);
        release(b);
        let seen: Vec<String> =
            sui_store::find_temp_roots(state.path(), "/nix/store").into_iter().map(|(_, p)| p).collect();
        assert_eq!(seen, vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a"]);
        assert_eq!(all(state.path()), vec!["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a".to_string()]);
        release(a);
        assert!(sui_store::find_temp_roots(state.path(), "/nix/store").is_empty());
    }
}
