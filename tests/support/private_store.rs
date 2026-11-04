#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const REAL_STORE: &str = "/nix/store";
const REAL_STATE: &str = "/nix/var/nix";
const NIX_CONF: &str = "substituters =\nbuild-users-group =\nsandbox = false\nkeep-outputs = false\nkeep-derivations = false\nexperimental-features = nix-command flakes\n";
const STARTUP_BOUND: Duration = Duration::from_secs(30);
const RELEASE_BOUND: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    OwnStoreDir,
    RootedAtRealStoreDir,
}

pub struct Layout {
    pub root: PathBuf,
    pub shape: Shape,
}

impl Layout {
    pub fn store(&self) -> PathBuf {
        self.root.join("nix/store")
    }

    pub fn state(&self) -> PathBuf {
        self.root.join("nix/var/nix")
    }

    pub fn socket(&self) -> PathBuf {
        self.root.join("s")
    }

    pub fn remote(&self) -> String {
        format!("unix://{}", self.socket().display())
    }

    pub fn physical(&self, logical: &str) -> PathBuf {
        match self.shape {
            Shape::OwnStoreDir => PathBuf::from(logical),
            Shape::RootedAtRealStoreDir => self.root.join(logical.trim_start_matches('/')),
        }
    }

    pub fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env_remove("NIX_CONFIG")
            .env("HOME", self.root.join("home"))
            .env("NIX_STATE_DIR", self.state())
            .env("NIX_LOG_DIR", self.root.join("nix/var/log/nix"))
            .env("NIX_CONF_DIR", self.root.join("etc"))
            .env("NIX_USER_CONF_FILES", self.root.join("no-user.conf"))
            .env("NIX_DAEMON_SOCKET_PATH", self.socket())
            .env("NIX_REMOTE", self.remote());
        if self.shape == Shape::OwnStoreDir {
            command.env("NIX_STORE_DIR", self.store());
        }
        command
    }

    fn daemon_store(&self) -> String {
        match self.shape {
            Shape::OwnStoreDir => "local".to_string(),
            Shape::RootedAtRealStoreDir => format!("local?root={}", self.root.display()),
        }
    }
}

pub struct PrivateStore {
    daemon: Child,
    pub layout: Layout,
    nix_store: PathBuf,
    nix_instantiate: PathBuf,
    pub nonce: String,
    _root: tempfile::TempDir,
}

impl PrivateStore {
    pub fn start(test: &str, shape: Shape) -> Option<Self> {
        let locate = sui_compat::cppnix::locate;
        let (Some(daemon), Some(nix_store), Some(nix_instantiate)) =
            (locate("nix-daemon"), locate("nix-store"), locate("nix-instantiate"))
        else {
            assert!(std::env::var_os("CI").is_none(), "{test}: CI must run this test, and CppNix's tools are absent");
            eprintln!("SKIP {test}: nix-daemon, nix-store or nix-instantiate is absent, so this test proved nothing");
            return None;
        };
        let tmp = std::fs::canonicalize("/tmp").unwrap();
        let root = tempfile::Builder::new().prefix("sui-c8").tempdir_in(tmp).unwrap();
        let layout = Layout { root: root.path().to_path_buf(), shape };
        for dir in ["nix/store", "nix/var/nix", "nix/var/log/nix", "etc", "home"] {
            std::fs::create_dir_all(layout.root.join(dir)).unwrap();
        }
        std::fs::write(layout.root.join("etc/nix.conf"), NIX_CONF).unwrap();
        let log = std::fs::File::create(layout.root.join("daemon.log")).unwrap();
        let mut daemon = layout
            .command(&daemon)
            .env("NIX_REMOTE", layout.daemon_store())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + STARTUP_BOUND;
        while !layout.socket().exists() {
            let log = || std::fs::read_to_string(layout.root.join("daemon.log")).unwrap_or_default();
            assert!(daemon.try_wait().unwrap().is_none(), "the private nix-daemon exited: {}", log());
            assert!(Instant::now() < deadline, "the private nix-daemon never bound its socket: {}", log());
            std::thread::sleep(Duration::from_millis(20));
        }
        let nonce = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        );
        Some(Self { daemon, layout, nix_store, nix_instantiate, nonce, _root: root })
    }

    pub fn fresh_drv(&self, tag: &str) -> String {
        let nonce = &self.nonce;
        let expr = format!(
            r#"derivation {{ name = "sui-c8-{tag}-{nonce}"; system = builtins.currentSystem; builder = "/bin/sh"; args = [ "-c" "echo c8 > $out" ]; }}"#
        );
        let out = self.layout.command(&self.nix_instantiate).args(["--expr", &expr]).output().unwrap();
        assert!(out.status.success(), "nix-instantiate: {}", String::from_utf8_lossy(&out.stderr));
        let drv = String::from_utf8(out.stdout).unwrap().trim().to_string();
        self.assert_private(&drv);
        drv
    }

    pub fn output_of(&self, drv: &str) -> String {
        let out = self.layout.command(&self.nix_store).args(["--query", "--outputs", drv]).output().unwrap();
        assert!(out.status.success(), "nix-store --query --outputs: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    pub fn assert_private(&self, path: &str) {
        assert!(self.layout.physical(path).starts_with(self.layout.store()), "{path} is outside the private store");
    }

    pub fn try_delete(&self, path: &str) -> Result<(), String> {
        self.assert_private(path);
        let out = self.layout.command(&self.nix_store).args(["--delete", path]).output().unwrap();
        if out.status.success() && !self.layout.physical(path).exists() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }

    pub fn delete_once_released(&self, path: &str) -> Result<(), String> {
        let deadline = Instant::now() + RELEASE_BOUND;
        loop {
            match self.try_delete(path) {
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
                result => return result,
            }
        }
    }

    pub fn assert_real_store_untouched(&self, marker: &str) {
        let Ok(entries) = std::fs::read_dir(REAL_STORE) else {
            return;
        };
        let leaked: Vec<_> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(marker))
            .collect();
        assert!(leaked.is_empty(), "this test wrote to the real store: {leaked:?}");
    }
}

impl Drop for PrivateStore {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

pub fn assert_bound_to_the_private_store(layout: &Layout) {
    let root = layout.root.display().to_string();
    let command = layout.command(Path::new("/bin/true"));
    let env: std::collections::BTreeMap<String, String> = command
        .get_envs()
        .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
        .collect();
    assert_eq!(env["NIX_STATE_DIR"], format!("{root}/nix/var/nix"));
    assert_eq!(env["NIX_LOG_DIR"], format!("{root}/nix/var/log/nix"));
    assert_eq!(env["NIX_CONF_DIR"], format!("{root}/etc"));
    assert_eq!(env["NIX_DAEMON_SOCKET_PATH"], format!("{root}/s"));
    assert_eq!(env["NIX_REMOTE"], format!("unix://{root}/s"));
    assert!(command.get_envs().any(|(k, v)| k == "NIX_CONFIG" && v.is_none()), "an inherited NIX_CONFIG must not reach the fixture");
    assert!(env.values().all(|v| !v.starts_with(REAL_STORE) && !v.starts_with(REAL_STATE)));
    match layout.shape {
        Shape::OwnStoreDir => assert_eq!(env["NIX_STORE_DIR"], format!("{root}/nix/store")),
        Shape::RootedAtRealStoreDir => {
            assert!(!env.contains_key("NIX_STORE_DIR"));
            assert_eq!(layout.daemon_store(), format!("local?root={root}"));
            assert_eq!(layout.physical("/nix/store/x"), layout.root.join("nix/store/x"));
        }
    }
}
