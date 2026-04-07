//! Pure-Rust git helpers using `git2` (libgit2 bindings).
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
) -> Result<git2::Repository, String> {
    let do_clone = |use_shallow: bool| -> Result<git2::Repository, git2::Error> {
        let mut builder = git2::build::RepoBuilder::new();

        if let Some(br) = branch {
            builder.branch(br);
        }

        let mut fetch_opts = git2::FetchOptions::new();
        if use_shallow {
            fetch_opts.depth(1);
        }
        builder.fetch_options(fetch_opts);

        builder.clone(url, dest)
    };

    // Try shallow first if requested; fall back to full clone if the
    // transport does not support shallow fetches (e.g. local file://).
    let repo = if shallow {
        match do_clone(true) {
            Ok(r) => r,
            Err(_) => {
                // Clean up partial clone before retrying.
                let _ = std::fs::remove_dir_all(dest);
                do_clone(false)
                    .map_err(|e| format!("git clone {url} -> {}: {e}", dest.display()))?
            }
        }
    } else {
        do_clone(false)
            .map_err(|e| format!("git clone {url} -> {}: {e}", dest.display()))?
    };

    if submodules {
        init_submodules_recursive(&repo)?;
    }

    Ok(repo)
}

/// Recursively initialize and update all submodules.
fn init_submodules_recursive(repo: &git2::Repository) -> Result<(), String> {
    for mut sub in repo
        .submodules()
        .map_err(|e| format!("list submodules: {e}"))?
    {
        sub.init(false)
            .map_err(|e| format!("init submodule {}: {e}", sub.name().unwrap_or("?")))?;
        sub.update(true, None)
            .map_err(|e| format!("update submodule {}: {e}", sub.name().unwrap_or("?")))?;
    }
    Ok(())
}

/// Checkout a specific revision (commit SHA) in an already-cloned repo.
///
/// This detaches HEAD at the given commit and resets the working tree.
pub fn checkout_rev(repo_path: &Path, rev: &str) -> Result<(), String> {
    let repo = git2::Repository::open(repo_path)
        .map_err(|e| format!("open {}: {e}", repo_path.display()))?;

    let oid = git2::Oid::from_str(rev)
        .map_err(|e| format!("invalid rev {rev}: {e}"))?;

    let commit = repo
        .find_commit(oid)
        .map_err(|e| format!("rev {rev} not found: {e}"))?;

    // Detach HEAD at the commit
    repo.set_head_detached(oid)
        .map_err(|e| format!("detach HEAD at {rev}: {e}"))?;

    // Reset working directory to match the commit tree
    repo.checkout_tree(
        commit.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .map_err(|e| format!("checkout tree at {rev}: {e}"))?;

    Ok(())
}

/// Get the full commit hash of HEAD (equivalent to `git rev-parse HEAD`).
pub fn head_rev(repo_path: &Path) -> Result<String, String> {
    let repo = git2::Repository::open(repo_path)
        .map_err(|e| format!("open {}: {e}", repo_path.display()))?;
    let head = repo
        .head()
        .map_err(|e| format!("head: {e}"))?;
    let oid = head
        .peel_to_commit()
        .map_err(|e| format!("peel to commit: {e}"))?
        .id();
    Ok(oid.to_string())
}

/// Count the number of commits reachable from HEAD
/// (equivalent to `git rev-list --count HEAD`).
pub fn rev_count(repo_path: &Path) -> Result<i64, String> {
    let repo = git2::Repository::open(repo_path)
        .map_err(|e| format!("open {}: {e}", repo_path.display()))?;
    let head = repo
        .head()
        .map_err(|e| format!("head: {e}"))?;
    let oid = head
        .peel_to_commit()
        .map_err(|e| format!("peel to commit: {e}"))?
        .id();

    let mut revwalk = repo
        .revwalk()
        .map_err(|e| format!("revwalk: {e}"))?;
    revwalk
        .push(oid)
        .map_err(|e| format!("revwalk push: {e}"))?;

    let count = revwalk.count();
    Ok(count as i64)
}

/// Get the committer timestamp of HEAD in seconds since epoch
/// (equivalent to `git log -1 --format=%ct`).
pub fn head_timestamp(repo_path: &Path) -> Result<i64, String> {
    let repo = git2::Repository::open(repo_path)
        .map_err(|e| format!("open {}: {e}", repo_path.display()))?;
    let head = repo
        .head()
        .map_err(|e| format!("head: {e}"))?;
    let commit = head
        .peel_to_commit()
        .map_err(|e| format!("peel to commit: {e}"))?;
    Ok(commit.time().seconds())
}

/// List remote refs and find the commit SHA for a given ref name
/// (equivalent to `git ls-remote <url> <ref>`).
///
/// Searches for the ref in `refs/heads/<ref_name>`, `refs/tags/<ref_name>`,
/// and as a direct match.
pub fn ls_remote(url: &str, ref_name: &str) -> Result<String, String> {
    let mut remote = git2::Remote::create_detached(url)
        .map_err(|e| format!("create remote for {url}: {e}"))?;

    // Connect to the remote (read-only)
    let conn = remote
        .connect_auth(git2::Direction::Fetch, None, None)
        .map_err(|e| format!("connect to {url}: {e}"))?;

    let refs = conn
        .list()
        .map_err(|e| format!("list refs from {url}: {e}"))?;

    // Search patterns in priority order
    let candidates = [
        format!("refs/heads/{ref_name}"),
        format!("refs/tags/{ref_name}"),
        ref_name.to_string(),
    ];

    for pattern in &candidates {
        for head in refs {
            if head.name() == pattern {
                return Ok(head.oid().to_string());
            }
        }
    }

    Err(format!("ref {ref_name} not found in remote {url}"))
}

/// Initialize a new bare/non-bare git repository (for test helpers).
/// Equivalent to `git init -b <branch>`.
pub fn init_repo(path: &Path, initial_branch: &str) -> Result<git2::Repository, String> {
    let repo = git2::Repository::init(path)
        .map_err(|e| format!("init {}: {e}", path.display()))?;

    // Create the initial branch by setting HEAD
    // (git2::Repository::init creates HEAD -> refs/heads/master by default,
    //  so we rename it)
    if initial_branch != "master" {
        repo.set_head(&format!("refs/heads/{initial_branch}"))
            .map_err(|e| format!("set HEAD to {initial_branch}: {e}"))?;
    }

    Ok(repo)
}

/// Create an initial commit in the given repo.
/// Adds all files in the working directory and commits them.
pub fn commit_all(
    repo: &git2::Repository,
    message: &str,
    name: &str,
    email: &str,
) -> Result<git2::Oid, String> {
    let sig = git2::Signature::now(name, email)
        .map_err(|e| format!("create signature: {e}"))?;

    let mut index = repo
        .index()
        .map_err(|e| format!("get index: {e}"))?;

    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .map_err(|e| format!("add all: {e}"))?;

    index
        .write()
        .map_err(|e| format!("write index: {e}"))?;

    let tree_oid = index
        .write_tree()
        .map_err(|e| format!("write tree: {e}"))?;

    let tree = repo
        .find_tree(tree_oid)
        .map_err(|e| format!("find tree: {e}"))?;

    // Check if there is a parent commit
    let parent_commit = repo.head().ok().and_then(|h| h.peel_to_commit().ok());

    let parents: Vec<&git2::Commit<'_>> = parent_commit.iter().collect();

    let oid = repo
        .commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
        .map_err(|e| format!("commit: {e}"))?;

    Ok(oid)
}

/// Set a config key in the repo's local config.
pub fn set_config(repo: &git2::Repository, key: &str, value: &str) -> Result<(), String> {
    let mut config = repo
        .config()
        .map_err(|e| format!("get config: {e}"))?;
    config
        .set_str(key, value)
        .map_err(|e| format!("set {key}={value}: {e}"))?;
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

        assert!(!oid.is_zero());
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
