use std::io::Read;
use std::path::{Path, PathBuf};

#[must_use]
pub fn default_log_dir() -> PathBuf {
    std::env::var_os("NIX_LOG_DIR").map_or_else(|| PathBuf::from("/nix/var/log/nix"), PathBuf::from)
}

fn log_file(log_dir: &Path, drv_path: &str) -> Option<PathBuf> {
    let base = drv_path.rsplit('/').next()?;
    if base.len() < 3 {
        return None;
    }
    Some(log_dir.join("drvs").join(&base[..2]).join(&base[2..]))
}

pub fn write(log_dir: &Path, drv_path: &str, log: &[u8]) -> std::io::Result<()> {
    let path = log_file(log_dir, drv_path).ok_or_else(|| std::io::Error::other(format!("{drv_path}: not a store path")))?;
    let parent = path.parent().unwrap_or(log_dir);
    std::fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, log)?;
    let _ = std::fs::remove_file(path.with_file_name(format!("{}.bz2", path.file_name().unwrap_or_default().to_string_lossy())));
    std::fs::rename(&tmp, &path)
}

pub fn read(log_dir: &Path, drv_path: &str) -> std::io::Result<Option<Vec<u8>>> {
    let Some(path) = log_file(log_dir, drv_path) else { return Ok(None) };
    match std::fs::read(&path) {
        Ok(bytes) => return Ok(Some(bytes)),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        Err(_) => {}
    }
    let bz2 = path.with_file_name(format!("{}.bz2", path.file_name().unwrap_or_default().to_string_lossy()));
    match std::fs::File::open(&bz2) {
        Ok(file) => {
            let mut out = Vec::new();
            bzip2_rs::DecoderReader::new(file).read_to_end(&mut out)?;
            Ok(Some(out))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRV: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-t.drv";

    #[test]
    fn a_log_lands_where_cppnix_keeps_it_and_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), DRV, b"one\ntwo\n").unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("drvs/01/23456789abcdfghijklmnpqrsvwxyz-t.drv")).unwrap(),
            b"one\ntwo\n"
        );
        assert_eq!(read(tmp.path(), DRV).unwrap().as_deref(), Some(&b"one\ntwo\n"[..]));
        assert_eq!(read(tmp.path(), "/nix/store/zz3456789abcdfghijklmnpqrsvwxyz-u.drv").unwrap(), None);
    }

    #[test]
    fn a_cppnix_bzip2_log_is_decompressed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("drvs/01");
        std::fs::create_dir_all(&dir).unwrap();
        let bz2 = [
            0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x4e, 0xec, 0xe8, 0x36, 0x00, 0x00, 0x02, 0x51,
            0x80, 0x00, 0x10, 0x40, 0x00, 0x06, 0x44, 0x90, 0x80, 0x20, 0x00, 0x31, 0x06, 0x4c, 0x41, 0x01, 0xa7, 0xa9,
            0xa5, 0x80, 0xbb, 0x94, 0x31, 0xf8, 0xbb, 0x92, 0x29, 0xc2, 0x84, 0x82, 0x77, 0x67, 0x41, 0xb0,
        ];
        std::fs::write(dir.join("23456789abcdfghijklmnpqrsvwxyz-t.drv.bz2"), bz2).unwrap();
        assert_eq!(read(tmp.path(), DRV).unwrap().as_deref(), Some(&b"hello world\n"[..]));
    }
}
