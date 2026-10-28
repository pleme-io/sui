use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use sui_compat::store_path::StorePath;
use sui_compat::wire::{self, StderrMsg, WorkerOp, PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2};
use sui_daemon::{DaemonConfig, DaemonServer};
use sui_store::traits::{PathInfo, Store, StoreResult};

const KNOWN: &str = "/nix/store/00bgd045z0d4icpbc2yyz4gx48ak44la-hello-2.12";

struct OnePath;

#[async_trait::async_trait]
impl Store for OnePath {
    async fn query_path_info(&self, path: &StorePath) -> StoreResult<Option<PathInfo>> {
        Ok((path.to_absolute_path() == KNOWN).then(|| PathInfo {
            nar_hash: "sha256:1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s".into(),
            nar_size: 120,
            ..PathInfo::new(KNOWN, "")
        }))
    }

    async fn is_valid_path(&self, path: &StorePath) -> StoreResult<bool> {
        Ok(path.to_absolute_path() == KNOWN)
    }

    async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
        Ok(vec![StorePath::from_absolute_path(KNOWN).unwrap()])
    }
}

struct Daemon {
    socket: PathBuf,
    dir: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn daemon<S: Store + 'static>(store: S) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let server = DaemonServer::new(DaemonConfig::with_socket_path(&socket), store);
    let task = tokio::spawn(async move {
        let _ = server.run().await;
    });
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(socket.exists(), "daemon socket never appeared");
    Daemon { socket, dir, task }
}

fn legacy(dir: &Path, name: &str) -> PathBuf {
    let link = dir.join(name);
    if !link.exists() {
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_sui"), &link).unwrap();
    }
    link
}

#[tokio::test(flavor = "multi_thread")]
async fn nix_daemon_stdio_speaks_the_worker_protocol_on_stdout_and_nothing_else() {
    let d = daemon(OnePath).await;
    let program = legacy(d.dir.path(), "nix-daemon");
    let store = format!("unix://{}", d.socket.display());
    let mut child = Command::new(&program)
        .args(["--stdio", "--store", &store])
        .env_remove("SUI_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let result = tokio::task::spawn_blocking(move || {
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        wire::write_u64(&mut stdin, WORKER_MAGIC_1).unwrap();
        stdin.flush().unwrap();
        assert_eq!(wire::read_u64(&mut stdout).unwrap(), WORKER_MAGIC_2);
        assert_eq!(wire::read_u64(&mut stdout).unwrap(), PROTOCOL_VERSION);
        wire::write_u64(&mut stdin, PROTOCOL_VERSION).unwrap();
        wire::write_u64(&mut stdin, 0).unwrap();
        wire::write_u64(&mut stdin, 0).unwrap();
        stdin.flush().unwrap();
        assert!(wire::read_string(&mut stdout).unwrap().starts_with("sui-daemon"));
        assert_eq!(wire::read_u64(&mut stdout).unwrap(), 1, "the daemon's own user is trusted");
        assert_eq!(wire::read_u64(&mut stdout).unwrap(), StderrMsg::Last as u64);
        for (path, valid) in [(KNOWN, true), ("/nix/store/11bgd045z0d4icpbc2yyz4gx48ak44la-absent", false)] {
            wire::write_u64(&mut stdin, WorkerOp::IsValidPath as u64).unwrap();
            wire::write_string(&mut stdin, path).unwrap();
            stdin.flush().unwrap();
            assert_eq!(wire::read_u64(&mut stdout).unwrap(), StderrMsg::Last as u64);
            assert_eq!(wire::read_bool(&mut stdout).unwrap(), valid, "{path}");
        }
        drop(stdin);
        let mut rest = Vec::new();
        stdout.read_to_end(&mut rest).unwrap();
        let status = child.wait().unwrap();
        let mut stderr = String::new();
        child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
        (status, rest, stderr)
    })
    .await
    .unwrap();
    let (status, rest, stderr) = result;
    assert!(rest.is_empty(), "stray bytes on stdout after the session: {rest:?}");
    assert!(status.success(), "exit {status:?}, stderr: {stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stdio_serve_mode_has_a_typed_off_switch() {
    let d = daemon(OnePath).await;
    let cfg = d.dir.path().join("sui.yaml");
    std::fs::write(&cfg, "serve:\n  stdio: false\n").unwrap();
    let out = Command::new(legacy(d.dir.path(), "nix-daemon"))
        .args(["--stdio", "--store", &format!("unix://{}", d.socket.display())])
        .env("SUI_CONFIG", &cfg)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("serve.stdio"), "{stderr}");
}

fn cppnix() -> Option<PathBuf> {
    let nix = sui_compat::cppnix::locate("nix")?;
    Path::new("/nix/var/nix/db/db.sqlite").exists().then_some(nix)
}

fn ssh_ng(program: &Path, socket: &Path) -> String {
    format!(
        "ssh-ng://localhost?remote-program={}&remote-store=unix://{}&max-connections=1",
        program.display(),
        socket.display()
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cppnix_client_uses_sui_as_an_ssh_ng_store() {
    let Some(nix) = cppnix() else {
        eprintln!("skip: no cppnix oracle or no local nix db");
        return;
    };
    let real = sui_store::LocalStore::open("/nix/var/nix/db/db.sqlite").await.unwrap();
    let d = daemon(real).await;
    let program = legacy(d.dir.path(), "nix-daemon");
    let url = ssh_ng(&program, &d.socket);
    let run = |args: &[&str]| {
        let out = Command::new(&nix).args(args).env("NIX_CONFIG", "experimental-features = nix-command").output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "nix {args:?}: {text}");
        (String::from_utf8_lossy(&out.stdout).into_owned(), text)
    };
    let (_, info) = run(&["store", "info", "--store", &url]);
    assert!(info.contains("Version: sui-daemon"), "{info}");
    let leaf = d.dir.path().join("sui-stdio-leaf");
    std::fs::write(&leaf, b"sui remote store over stdio\n").unwrap();
    let (added, _) = run(&["store", "add-file", &leaf.display().to_string()]);
    let path = added.trim().to_string();
    let (local, _) = run(&["path-info", "--json", &path]);
    let (remote, _) = run(&["path-info", "--json", "--store", &url, &path]);
    let field = |json: &str, key: &str| -> serde_json::Value {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let entry = v.as_object().and_then(|o| o.values().next().cloned()).or_else(|| v.as_array().and_then(|a| a.first().cloned())).unwrap();
        entry[key].clone()
    };
    for key in ["narHash", "narSize", "references"] {
        assert_eq!(field(&local, key), field(&remote, key), "{key}");
    }
    let root = std::fs::canonicalize(d.dir.path()).unwrap().join("copy-root");
    run(&["copy", "--no-check-sigs", "--from", &url, "--to", &format!("local?root={}", root.display()), &path]);
    let (copied, _) = run(&["path-info", "--json", "--store", &format!("local?root={}", root.display()), &path]);
    assert_eq!(field(&local, "narHash"), field(&copied, "narHash"));
}

#[tokio::test(flavor = "multi_thread")]
async fn nix_store_serve_reaches_the_daemon_for_a_user_without_the_local_store() {
    let d = daemon(OnePath).await;
    let program = legacy(d.dir.path(), "nix-store");
    let store = format!("unix://{}", d.socket.display());
    let mut child = Command::new(&program)
        .args(["--serve", "--store", &store])
        .env_remove("SUI_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (status, stderr) = tokio::task::spawn_blocking(move || {
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        wire::write_u64(&mut stdin, wire::SERVE_MAGIC_1).unwrap();
        wire::write_u64(&mut stdin, wire::SERVE_PROTOCOL_VERSION).unwrap();
        stdin.flush().unwrap();
        assert_eq!(wire::read_u64(&mut stdout).unwrap(), wire::SERVE_MAGIC_2);
        assert_eq!(wire::read_u64(&mut stdout).unwrap(), wire::SERVE_PROTOCOL_VERSION);
        wire::write_u64(&mut stdin, wire::ServeCommand::QueryValidPaths as u64).unwrap();
        wire::write_u64(&mut stdin, 0).unwrap();
        wire::write_u64(&mut stdin, 0).unwrap();
        wire::write_string_list(&mut stdin, &[KNOWN, "/nix/store/11bgd045z0d4icpbc2yyz4gx48ak44la-absent"]).unwrap();
        stdin.flush().unwrap();
        assert_eq!(wire::read_string_list(&mut stdout).unwrap(), vec![KNOWN.to_string()]);
        wire::write_u64(&mut stdin, wire::ServeCommand::QueryPathInfos as u64).unwrap();
        wire::write_string_list(&mut stdin, &[KNOWN]).unwrap();
        stdin.flush().unwrap();
        assert_eq!(wire::read_string(&mut stdout).unwrap(), KNOWN);
        assert_eq!(wire::read_string(&mut stdout).unwrap(), "");
        assert!(wire::read_string_list(&mut stdout).unwrap().is_empty());
        assert_eq!(wire::read_u64(&mut stdout).unwrap(), 120);
        assert_eq!(wire::read_u64(&mut stdout).unwrap(), 120);
        assert_eq!(wire::read_string(&mut stdout).unwrap(), "sha256:1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s");
        drop(stdin);
        let mut rest = Vec::new();
        stdout.read_to_end(&mut rest).unwrap();
        let status = child.wait().unwrap();
        let mut stderr = String::new();
        child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
        (status, stderr)
    })
    .await
    .unwrap();
    assert!(status.success(), "exit {status:?}: {stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cppnix_client_uses_sui_as_an_ssh_store() {
    let Some(nix) = cppnix() else {
        eprintln!("skip: no cppnix oracle or no local nix db");
        return;
    };
    let real = sui_store::LocalStore::open("/nix/var/nix/db/db.sqlite").await.unwrap();
    let d = daemon(real).await;
    let program = legacy(d.dir.path(), "nix-store");
    let url = format!(
        "ssh://localhost?remote-program={}&remote-store=unix://{}",
        program.display(),
        d.socket.display()
    );
    let run = |args: &[&str]| {
        let out = Command::new(&nix).args(args).env("NIX_CONFIG", "experimental-features = nix-command").output().unwrap();
        assert!(out.status.success(), "nix {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let leaf = d.dir.path().join("sui-serve-leaf");
    std::fs::write(&leaf, b"sui legacy serve protocol\n").unwrap();
    let path = run(&["store", "add-file", &leaf.display().to_string()]).trim().to_string();
    let hash = |json: &str| -> serde_json::Value {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        v.as_object().and_then(|o| o.values().next().cloned()).unwrap()["narHash"].clone()
    };
    let local = run(&["path-info", "--json", &path]);
    assert_eq!(hash(&local), hash(&run(&["path-info", "--json", "--store", &url, &path])));
    let root = std::fs::canonicalize(d.dir.path()).unwrap().join("copy-root");
    let to = format!("local?root={}", root.display());
    run(&["copy", "--no-check-sigs", "--from", &url, "--to", &to, &path]);
    assert_eq!(hash(&local), hash(&run(&["path-info", "--json", "--store", &to, &path])));
}
