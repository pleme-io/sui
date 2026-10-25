use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::value::EvalError;

fn io_error(context: String, message: impl ToString) -> EvalError {
    EvalError::IoError { context, message: message.to_string() }
}

fn git_listing(dir: &Path, args: &[&str]) -> Result<Vec<String>, EvalError> {
    let context = format!("git {} in {}", args.join(" "), dir.display());
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| io_error(context.clone(), e))?;
    if !out.status.success() {
        return Err(io_error(context, String::from_utf8_lossy(&out.stderr).trim()));
    }
    let mut files: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

fn tracked_files(dir: &Path) -> Result<Vec<String>, EvalError> {
    let indexed = git_listing(dir, &["ls-files", "-z", "--cached"])?;
    if !indexed.is_empty() {
        return Ok(indexed);
    }
    Ok(git_listing(dir, &["ls-tree", "-r", "-z", "--name-only", "HEAD"]).unwrap_or_default())
}

pub fn tracked_tree(dir: &Path) -> Result<PathBuf, EvalError> {
    if !dir.join(".git").exists() {
        return Ok(dir.to_path_buf());
    }
    let files = tracked_files(dir)?;
    if files.is_empty() {
        return Ok(dir.to_path_buf());
    }
    let mut kept: Vec<(String, std::fs::Metadata)> = Vec::new();
    for rel in files {
        if let Ok(meta) = std::fs::symlink_metadata(dir.join(&rel))
            && !meta.is_dir()
        {
            kept.push((rel, meta));
        }
    }
    let mut key = Sha256::new();
    for (rel, meta) in &kept {
        key.update(rel.as_bytes());
        key.update([0]);
        key.update(meta.len().to_le_bytes());
        key.update(meta.mtime().to_le_bytes());
        key.update(meta.mtime_nsec().to_le_bytes());
        key.update(meta.mode().to_le_bytes());
    }
    let key: String = key.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let root = std::env::temp_dir().join("sui-git-tree");
    let dest = root.join(&key);
    if dest.exists() {
        return Ok(dest);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let staging = root.join(format!("{key}.tmp-{}-{nanos}", std::process::id()));
    let staged = (|| -> Result<(), EvalError> {
        for (rel, meta) in &kept {
            let from = dir.join(rel);
            let to = staging.join(rel);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| io_error(format!("staging {}", parent.display()), e))?;
            }
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&from)
                    .map_err(|e| io_error(format!("readlink {}", from.display()), e))?;
                std::os::unix::fs::symlink(&target, &to)
                    .map_err(|e| io_error(format!("symlink {}", to.display()), e))?;
            } else {
                std::fs::copy(&from, &to)
                    .map_err(|e| io_error(format!("copy {}", from.display()), e))?;
            }
        }
        std::fs::create_dir_all(&staging)
            .map_err(|e| io_error(format!("staging {}", staging.display()), e))
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    if std::fs::rename(&staging, &dest).is_err() {
        let _ = std::fs::remove_dir_all(&staging);
        if !dest.exists() {
            return Err(io_error(
                format!("git tree {}", dest.display()),
                "could not move the staged tree into place",
            ));
        }
    }
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn a_git_checkout_evaluates_its_tracked_files_only() {
        let repo = tempfile::tempdir().unwrap();
        let d = repo.path();
        git(d, &["init", "-q"]);
        std::fs::write(d.join("flake.nix"), "{}").unwrap();
        std::fs::create_dir_all(d.join("lib")).unwrap();
        std::fs::write(d.join("lib/a.nix"), "1").unwrap();
        git(d, &["add", "flake.nix", "lib/a.nix"]);
        std::fs::write(d.join("lib/a.nix"), "2").unwrap();
        std::fs::write(d.join("untracked.txt"), "x").unwrap();
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::write(d.join("target/junk"), "x").unwrap();

        let tree = tracked_tree(d).unwrap();
        assert!(tree.join("flake.nix").exists());
        assert_eq!(std::fs::read_to_string(tree.join("lib/a.nix")).unwrap(), "2");
        assert!(!tree.join("untracked.txt").exists());
        assert!(!tree.join("target").exists());
        assert!(!tree.join(".git").exists());
    }

    #[test]
    fn a_plain_directory_is_used_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(tracked_tree(dir.path()).unwrap(), dir.path());
    }
}
