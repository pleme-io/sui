//! Pure-Rust git helpers using `gix` (gitoxide).
//!
//! Replaces all `Command::new("git")` process spawning with in-process
//! library calls. Every public function in this module corresponds to a
//! git CLI operation that was previously shelled out to.

use std::path::Path;

/// Clone a remote repository into `dest`.
///
/// Parameters:
/// - `url`: remote URL (HTTPS or file://)
/// - `dest`: target directory (must not exist or be empty)
/// - `branch`: optional branch/ref name to checkout after clone
/// - `shallow`: when true, fetches only the latest commit (like `--depth 1`)
/// - `submodules`: when true, recursively initializes submodules
pub fn clone(
    url: &str,
    dest: &Path,
    branch: Option<&str>,
    shallow: bool,
    submodules: bool,
) -> Result<gix::Repository, String> {
    // Use git CLI for clone operations. gix's edition-2024 fork panics
    // on background threads during fetch for certain refspec patterns.
    // git CLI is reliable and clone only happens on cache miss.
    let mut args = vec!["clone".to_string()];
    if shallow {
        args.extend(["--depth".into(), "1".into()]);
    }
    if let Some(br) = branch {
        args.extend(["--branch".into(), br.to_string()]);
    }
    args.push(url.to_string());
    args.push(dest.to_string_lossy().into_owned());

    let status = std::process::Command::new("git")
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("git clone {url}: {e}"))?;

    if !status.success() {
        let _ = std::fs::remove_dir_all(dest);
        if shallow {
            // Retry without shallow (some transports don't support it)
            let mut retry_args = vec!["clone".to_string()];
            if let Some(br) = branch {
                retry_args.extend(["--branch".into(), br.to_string()]);
            }
            retry_args.push(url.to_string());
            retry_args.push(dest.to_string_lossy().into_owned());
            let retry = std::process::Command::new("git")
                .args(&retry_args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map_err(|e| format!("git clone retry: {e}"))?;
            if !retry.success() {
                return Err(format!("git clone {url} failed"));
            }
        } else {
            return Err(format!("git clone {url} failed"));
        }
    }

    let repo = gix::open(dest)
        .map_err(|e| format!("open cloned repo: {e}"))?;

    if submodules {
        init_submodules_recursive(&repo)?;
    }

    Ok(repo)
}

/// Recursively initialize and update all submodules.
fn init_submodules_recursive(repo: &gix::Repository) -> Result<(), String> {
    let modules = match repo.submodules() {
        Ok(Some(mods)) => mods,
        Ok(None) => return Ok(()),
        Err(e) => return Err(format!("list submodules: {e}")),
    };

    let workdir = repo
        .workdir()
        .ok_or("repo has no worktree")?;

    for sub in modules {
        let name = sub.name().to_string();
        let sub_url = match sub.url() {
            Ok(url) => url.to_bstring().to_string(),
            Err(e) => return Err(format!("submodule {name} url: {e}")),
        };
        let sub_path = sub.path().map_err(|e| format!("submodule {name} path: {e}"))?;
        let dest = workdir.join(sub_path.to_string());

        if !dest.exists() {
            clone(&sub_url, &dest, None, false, true)
                .map_err(|e| format!("clone submodule {name}: {e}"))?;
        }
    }
    Ok(())
}

/// Checkout a specific revision (commit SHA) in an already-cloned repo.
///
/// This detaches HEAD at the given commit and resets the working tree.
pub fn checkout_rev(repo_path: &Path, rev: &str) -> Result<(), String> {
    let repo = gix::open(repo_path)
        .map_err(|e| format!("open {}: {e}", repo_path.display()))?;

    let oid = gix::ObjectId::from_hex(rev.as_bytes())
        .map_err(|e| format!("invalid rev {rev}: {e}"))?;

    let commit = repo
        .find_object(oid)
        .map_err(|e| format!("rev {rev} not found: {e}"))?
        .into_commit();

    let tree = commit
        .tree()
        .map_err(|e| format!("tree for {rev}: {e}"))?;

    // Detach HEAD at the commit by writing HEAD directly
    let head_path = repo.git_dir().join("HEAD");
    std::fs::write(&head_path, format!("{oid}\n"))
        .map_err(|e| format!("write HEAD: {e}"))?;

    let workdir = repo
        .workdir()
        .ok_or("repo has no worktree")?;

    // Remove existing working tree files (except .git)
    for entry in std::fs::read_dir(workdir).map_err(|e| format!("read workdir: {e}"))? {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }

    // Write tree contents to the working directory
    write_tree_to_workdir(&repo, &tree, workdir)?;

    Ok(())
}

/// Recursively write a tree's contents to a directory.
fn write_tree_to_workdir(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    dest: &Path,
) -> Result<(), String> {
    for entry in tree.iter() {
        let entry = entry.map_err(|e| format!("tree entry: {e}"))?;
        let name = entry.filename().to_string();
        let path = dest.join(&name);

        match entry.mode().kind() {
            gix::objs::tree::EntryKind::Blob | gix::objs::tree::EntryKind::BlobExecutable => {
                let obj = repo
                    .find_object(entry.oid())
                    .map_err(|e| format!("find blob {}: {e}", entry.oid()))?;
                std::fs::write(&path, &obj.data)
                    .map_err(|e| format!("write {}: {e}", path.display()))?;

                #[cfg(unix)]
                if entry.mode().kind() == gix::objs::tree::EntryKind::BlobExecutable {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
                }
            }
            gix::objs::tree::EntryKind::Tree => {
                std::fs::create_dir_all(&path)
                    .map_err(|e| format!("mkdir {}: {e}", path.display()))?;
                let subtree = repo
                    .find_object(entry.oid())
                    .map_err(|e| format!("find tree {}: {e}", entry.oid()))?
                    .into_tree();
                write_tree_to_workdir(repo, &subtree, &path)?;
            }
            _ => {
                // Skip symlinks, submodules, etc.
            }
        }
    }
    Ok(())
}

/// Get the full commit hash of HEAD (equivalent to `git rev-parse HEAD`).
pub fn head_rev(repo_path: &Path) -> Result<String, String> {
    let repo = gix::open(repo_path)
        .map_err(|e| format!("open {}: {e}", repo_path.display()))?;
    let head = repo
        .head_commit()
        .map_err(|e| format!("head commit: {e}"))?;
    Ok(head.id.to_string())
}

/// Count the number of commits reachable from HEAD
/// (equivalent to `git rev-list --count HEAD`).
pub fn rev_count(repo_path: &Path) -> Result<i64, String> {
    let repo = gix::open(repo_path)
        .map_err(|e| format!("open {}: {e}", repo_path.display()))?;
    let head = repo
        .head_commit()
        .map_err(|e| format!("head commit: {e}"))?;

    let mut count: i64 = 0;
    let walk = repo
        .rev_walk([head.id])
        .all()
        .map_err(|e| format!("rev walk: {e}"))?;

    for info in walk {
        let _info = info.map_err(|e| format!("rev walk step: {e}"))?;
        count += 1;
    }

    Ok(count)
}

/// Get the committer timestamp of HEAD in seconds since epoch
/// (equivalent to `git log -1 --format=%ct`).
pub fn head_timestamp(repo_path: &Path) -> Result<i64, String> {
    let repo = gix::open(repo_path)
        .map_err(|e| format!("open {}: {e}", repo_path.display()))?;
    let head = repo
        .head_commit()
        .map_err(|e| format!("head commit: {e}"))?;
    let commit = head
        .decode()
        .map_err(|e| format!("decode commit: {e}"))?;
    let committer = commit
        .committer()
        .map_err(|e| format!("parse committer: {e}"))?;
    Ok(committer.seconds())
}

/// List remote refs and find the commit SHA for a given ref name
/// (equivalent to `git ls-remote <url> <ref>`).
///
/// Searches for the ref in `refs/heads/<ref_name>`, `refs/tags/<ref_name>`,
/// and as a direct match.
pub fn ls_remote(url: &str, ref_name: &str) -> Result<String, String> {
    // For file:// URLs, open the repo directly and read refs.
    // This avoids the transport layer complexity.
    if let Some(path) = url.strip_prefix("file://") {
        return ls_remote_local(Path::new(path), ref_name);
    }

    // For network URLs, use git CLI (gix's remote connect panics on
    // background threads in our edition-2024 fork).
    let output = std::process::Command::new("git")
        .args(["ls-remote", url])
        .output()
        .map_err(|e| format!("git ls-remote {url}: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "git ls-remote failed for {url}: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse git ls-remote output: "<sha>\t<refname>\n"
    let candidates = [
        format!("refs/heads/{ref_name}"),
        format!("refs/tags/{ref_name}"),
        ref_name.to_string(),
    ];

    for line in stdout.lines() {
        let mut parts = line.split('\t');
        let Some(sha) = parts.next() else { continue };
        let Some(name) = parts.next() else { continue };
        for candidate in &candidates {
            if name == candidate {
                return Ok(sha.to_string());
            }
        }
    }

    Err(format!("ref {ref_name} not found in remote {url}"))
}

/// Extract name and oid from a handshake Ref.
fn ref_to_name_oid(r: &gix::protocol::handshake::Ref) -> (String, Option<String>) {
    match r {
        gix::protocol::handshake::Ref::Direct { full_ref_name, object } => {
            (full_ref_name.to_string(), Some(object.to_string()))
        }
        gix::protocol::handshake::Ref::Symbolic { full_ref_name, object, .. } => {
            (full_ref_name.to_string(), Some(object.to_string()))
        }
        gix::protocol::handshake::Ref::Peeled { full_ref_name, object, .. } => {
            (full_ref_name.to_string(), Some(object.to_string()))
        }
        gix::protocol::handshake::Ref::Unborn { .. } => (String::new(), None),
    }
}

/// List refs from a local repository by opening it directly.
fn ls_remote_local(repo_path: &Path, ref_name: &str) -> Result<String, String> {
    let repo = gix::open(repo_path)
        .map_err(|e| format!("open {}: {e}", repo_path.display()))?;

    // Search patterns in priority order
    let candidates = [
        format!("refs/heads/{ref_name}"),
        format!("refs/tags/{ref_name}"),
        ref_name.to_string(),
    ];

    for pattern in &candidates {
        if let Ok(reference) = repo.find_reference(pattern.as_str()) {
            let id = reference
                .into_fully_peeled_id()
                .map_err(|e| format!("peel ref {pattern}: {e}"))?;
            return Ok(id.to_string());
        }
    }

    // Also check HEAD
    if ref_name == "HEAD" {
        let head = repo
            .head_id()
            .map_err(|e| format!("head id: {e}"))?;
        return Ok(head.to_string());
    }

    Err(format!("ref {ref_name} not found in remote file://{}", repo_path.display()))
}

/// Initialize a new bare/non-bare git repository (for test helpers).
/// Equivalent to `git init -b <branch>`.
pub fn init_repo(path: &Path, initial_branch: &str) -> Result<gix::Repository, String> {
    let repo = gix::init(path)
        .map_err(|e| format!("init {}: {e}", path.display()))?;

    // gix::init creates HEAD -> refs/heads/main by default.
    // If a different branch is requested, update HEAD.
    if initial_branch != "main" {
        let head_path = repo.git_dir().join("HEAD");
        std::fs::write(
            &head_path,
            format!("ref: refs/heads/{initial_branch}\n"),
        )
        .map_err(|e| format!("set HEAD to {initial_branch}: {e}"))?;
    }

    Ok(repo)
}

/// Create an initial commit in the given repo.
/// Adds all files in the working directory and commits them.
pub fn commit_all(
    repo: &gix::Repository,
    message: &str,
    name: &str,
    email: &str,
) -> Result<gix::ObjectId, String> {
    let workdir = repo
        .workdir()
        .ok_or("repo has no worktree")?;

    // Build a tree from the working directory files
    let tree_id = build_tree_from_workdir(repo, workdir)?;

    let time = gix::date::Time::now_local_or_utc();
    let mut time_buf = gix::date::parse::TimeBuf::default();
    let sig = gix::actor::Signature {
        name: name.into(),
        email: email.into(),
        time,
    };
    let sig_ref = sig.to_ref(&mut time_buf);

    // Check if there is a parent commit
    let parent_ids: Vec<gix::ObjectId> = match repo.head_commit() {
        Ok(c) => vec![c.id],
        Err(_) => vec![],
    };

    let commit_id = repo
        .commit_as(
            sig_ref,
            sig_ref,
            "HEAD",
            message,
            tree_id,
            parent_ids.iter().copied(),
        )
        .map_err(|e| format!("commit: {e}"))?;

    Ok(commit_id.detach())
}

/// Build a tree object from all files in the working directory.
fn build_tree_from_workdir(
    repo: &gix::Repository,
    workdir: &Path,
) -> Result<gix::ObjectId, String> {
    let empty_tree = repo.empty_tree();
    let mut editor = repo
        .edit_tree(empty_tree.id)
        .map_err(|e| format!("create tree editor: {e}"))?;

    add_files_to_tree(&mut editor, repo, workdir, workdir)?;

    let tree_id = editor
        .write()
        .map_err(|e| format!("write tree: {e}"))?;

    Ok(tree_id.detach())
}

/// Recursively add files from a directory to a tree editor.
fn add_files_to_tree(
    editor: &mut gix::object::tree::Editor<'_>,
    repo: &gix::Repository,
    base: &Path,
    dir: &Path,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("read dir {}: {e}", dir.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        let path = entry.path();
        let file_name = entry.file_name();
        let relative = path
            .strip_prefix(base)
            .map_err(|e| format!("strip prefix: {e}"))?;

        // Skip .git directory
        if file_name == ".git" {
            continue;
        }

        let metadata = entry
            .metadata()
            .map_err(|e| format!("metadata {}: {e}", path.display()))?;

        if metadata.is_file() {
            let data = std::fs::read(&path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            let blob_id = repo
                .write_blob(&data)
                .map_err(|e| format!("write blob {}: {e}", path.display()))?;

            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 != 0 {
                    gix::objs::tree::EntryKind::BlobExecutable
                } else {
                    gix::objs::tree::EntryKind::Blob
                }
            };
            #[cfg(not(unix))]
            let mode = gix::objs::tree::EntryKind::Blob;

            // Convert path to forward-slash string for gix's ToComponents
            let relative_str = relative.to_string_lossy().replace('\\', "/");
            editor
                .upsert(relative_str.as_str(), mode, blob_id.detach())
                .map_err(|e| format!("upsert {}: {e}", relative.display()))?;
        } else if metadata.is_dir() {
            add_files_to_tree(editor, repo, base, &path)?;
        }
    }

    Ok(())
}

/// Set a config key in the repo's local config.
///
/// Appends to the git config file directly. This is a simple append-based
/// writer that works correctly because git reads the last value for duplicate keys.
pub fn set_config(repo: &gix::Repository, key: &str, value: &str) -> Result<(), String> {
    let config_path = repo.git_dir().join("config");

    // Parse key as section.name or section.subsection.name
    let parts: Vec<&str> = key.splitn(2, '.').collect();
    if parts.len() != 2 {
        return Err(format!("invalid config key: {key}"));
    }

    let section_name = parts[0];
    let remaining = parts[1];
    let (subsection, key_name) = if let Some(dot_pos) = remaining.rfind('.') {
        (Some(&remaining[..dot_pos]), &remaining[dot_pos + 1..])
    } else {
        (None, remaining)
    };

    // Build the INI section header
    let header = if let Some(sub) = subsection {
        format!("[{section_name} \"{sub}\"]")
    } else {
        format!("[{section_name}]")
    };

    // Read existing config or start fresh
    let mut content = std::fs::read_to_string(&config_path).unwrap_or_default();

    // Append section and key
    if !content.ends_with('\n') && !content.is_empty() {
        content.push('\n');
    }
    content.push_str(&format!("{header}\n\t{key_name} = {value}\n"));

    std::fs::write(&config_path, content)
        .map_err(|e| format!("write config: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(suffix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "sui_git_test_{suffix}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn init_and_commit() {
        let dir = temp_dir("init_commit");
        fs::create_dir_all(&dir).unwrap();

        let repo = init_repo(&dir, "main").unwrap();
        set_config(&repo, "user.email", "test@sui.local").unwrap();
        set_config(&repo, "user.name", "sui-test").unwrap();

        fs::write(dir.join("README"), "hello").unwrap();
        let oid = commit_all(&repo, "initial", "sui-test", "test@sui.local").unwrap();

        assert!(!oid.is_null());
        assert_eq!(head_rev(&dir).unwrap().len(), 40);
        assert_eq!(rev_count(&dir).unwrap(), 1);
        assert!(head_timestamp(&dir).unwrap() > 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clone_local_repo() {
        let src = temp_dir("clone_src");
        fs::create_dir_all(&src).unwrap();

        let repo = init_repo(&src, "main").unwrap();
        set_config(&repo, "user.email", "test@sui.local").unwrap();
        set_config(&repo, "user.name", "sui-test").unwrap();
        fs::write(src.join("file.txt"), "content").unwrap();
        commit_all(&repo, "first", "sui-test", "test@sui.local").unwrap();

        let dest = temp_dir("clone_dest");
        let cloned = clone(
            &format!("file://{}", src.display()),
            &dest,
            None,
            false,
            false,
        )
        .unwrap();

        assert!(dest.join("file.txt").exists());
        assert_eq!(
            head_rev(&dest).unwrap(),
            head_rev(&src).unwrap()
        );

        drop(cloned);
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dest);
    }

    #[test]
    fn checkout_rev_works() {
        let dir = temp_dir("checkout");
        fs::create_dir_all(&dir).unwrap();

        let repo = init_repo(&dir, "main").unwrap();
        set_config(&repo, "user.email", "test@sui.local").unwrap();
        set_config(&repo, "user.name", "sui-test").unwrap();
        fs::write(dir.join("a.txt"), "first").unwrap();
        let first_oid = commit_all(&repo, "first", "sui-test", "test@sui.local").unwrap();
        fs::write(dir.join("b.txt"), "second").unwrap();
        commit_all(&repo, "second", "sui-test", "test@sui.local").unwrap();

        // Checkout first commit
        checkout_rev(&dir, &first_oid.to_string()).unwrap();
        assert!(!dir.join("b.txt").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rev_count_multiple_commits() {
        let dir = temp_dir("revcount");
        fs::create_dir_all(&dir).unwrap();

        let repo = init_repo(&dir, "main").unwrap();
        set_config(&repo, "user.email", "test@sui.local").unwrap();
        set_config(&repo, "user.name", "sui-test").unwrap();

        fs::write(dir.join("a"), "1").unwrap();
        commit_all(&repo, "one", "sui-test", "test@sui.local").unwrap();
        fs::write(dir.join("b"), "2").unwrap();
        commit_all(&repo, "two", "sui-test", "test@sui.local").unwrap();
        fs::write(dir.join("c"), "3").unwrap();
        commit_all(&repo, "three", "sui-test", "test@sui.local").unwrap();

        assert_eq!(rev_count(&dir).unwrap(), 3);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ls_remote_local() {
        let src = temp_dir("lsremote");
        fs::create_dir_all(&src).unwrap();

        let repo = init_repo(&src, "main").unwrap();
        set_config(&repo, "user.email", "test@sui.local").unwrap();
        set_config(&repo, "user.name", "sui-test").unwrap();
        fs::write(src.join("f"), "data").unwrap();
        commit_all(&repo, "init", "sui-test", "test@sui.local").unwrap();

        let sha = ls_remote(&format!("file://{}", src.display()), "main").unwrap();
        assert_eq!(sha.len(), 40);
        assert_eq!(sha, head_rev(&src).unwrap());

        let _ = fs::remove_dir_all(&src);
    }
}
