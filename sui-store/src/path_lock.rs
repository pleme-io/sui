use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::traits::{StoreError, StoreResult};

#[derive(Debug)]
pub struct PathLock {
    file: File,
    lock_path: PathBuf,
}

impl PathLock {
    pub fn acquire(path: &Path) -> std::io::Result<Self> {
        let mut name = path.as_os_str().to_owned();
        name.push(".lock");
        let lock_path = PathBuf::from(name);
        loop {
            let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&lock_path)?;
            file.lock()?;
            if file.metadata()?.len() == 0 {
                return Ok(Self { file, lock_path });
            }
        }
    }

    pub async fn acquire_async(path: PathBuf) -> StoreResult<Self> {
        tokio::task::spawn_blocking(move || Self::acquire(&path))
            .await
            .map_err(|e| StoreError::Internal(format!("path lock task: {e}")))?
            .map_err(StoreError::Io)
    }
}

impl Drop for PathLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
        let _ = self.file.write_all(b"d");
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn holders_of_one_path_never_overlap_and_no_lock_file_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("abc-x");
        let inside = Arc::new(AtomicUsize::new(0));
        let overlaps = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let (target, inside, overlaps) = (target.clone(), inside.clone(), overlaps.clone());
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let _guard = PathLock::acquire(&target).unwrap();
                        if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                            overlaps.fetch_add(1, Ordering::SeqCst);
                        }
                        std::thread::yield_now();
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(overlaps.load(Ordering::SeqCst), 0);
        assert!(!dir.path().join("abc-x.lock").exists());
    }
}
