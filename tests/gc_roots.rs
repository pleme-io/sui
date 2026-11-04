//! GC-root honesty, against a real CppNix daemon that serves a private temporary
//! store: what sui builds survives a collection that runs while sui still needs
//! it, and is released when it no longer does. Nothing here reaches the machine's
//! own `/nix/store` or daemon.
//!
//! Each test is two processes: sui roots a path, then CppNix's
//! `nix-store --delete` (a targeted collection that honours every root) tries
//! to remove it. It must refuse while the root is held and succeed once it is
//! released, which also proves the refusal was caused by sui's root.
//!
//! Skipped, and says so, where CppNix's tools are absent. With `CI` set the
//! absence is a failure, so CI cannot skip this file.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const REAL_STORE: &str = "/nix/store";
const REAL_STATE: &str = "/nix/var/nix";
const NIX_CONF: &str = "substituters =\nbuild-users-group =\nsandbox = false\nkeep-outputs = false\nkeep-derivations = false\n";
const STARTUP_BOUND: Duration = Duration::from_secs(30);
const RELEASE_BOUND: Duration = Duration::from_secs(10);

struct Oracle {
    daemon: PathBuf,
    store: PathBuf,
    instantiate: PathBuf,
}

fn oracle(test: &str) -> Option<Oracle> {
    let locate = sui_compat::cppnix::locate;
    match (locate("nix-daemon"), locate("nix-store"), locate("nix-instantiate")) {
        (Some(daemon), Some(store), Some(instantiate)) => Some(Oracle { daemon, store, instantiate }),
        _ => {
            assert!(std::env::var_os("CI").is_none(), "{test}: CI must run this test, and CppNix's tools are absent");
            eprintln!("SKIP {test}: nix-daemon, nix-store or nix-instantiate is absent, so this test proved nothing");
            None
        }
    }
}

struct Layout {
    root: PathBuf,
}

impl Layout {
    fn store(&self) -> PathBuf {
        self.root.join("nix/store")
    }

    fn state(&self) -> PathBuf {
        self.root.join("nix/var/nix")
    }

    fn socket(&self) -> PathBuf {
        self.root.join("s")
    }

    fn remote(&self) -> String {
        format!("unix://{}", self.socket().display())
    }

    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env_remove("NIX_CONFIG")
            .env("HOME", self.root.join("home"))
            .env("NIX_STORE_DIR", self.store())
            .env("NIX_STATE_DIR", self.state())
            .env("NIX_LOG_DIR", self.root.join("nix/var/log/nix"))
            .env("NIX_CONF_DIR", self.root.join("etc"))
            .env("NIX_USER_CONF_FILES", self.root.join("no-user.conf"))
            .env("NIX_DAEMON_SOCKET_PATH", self.socket())
            .env("NIX_REMOTE", self.remote());
        command
    }
}

struct PrivateStore {
    daemon: Child,
    layout: Layout,
    nix_store: PathBuf,
    nix_instantiate: PathBuf,
    nonce: String,
    _root: tempfile::TempDir,
}

impl PrivateStore {
    fn start(oracle: Oracle) -> Self {
        let tmp = std::fs::canonicalize("/tmp").unwrap();
        let root = tempfile::Builder::new().prefix("sui-c8").tempdir_in(tmp).unwrap();
        let layout = Layout { root: root.path().to_path_buf() };
        for dir in ["nix/store", "nix/var/nix", "nix/var/log/nix", "etc", "home"] {
            std::fs::create_dir_all(layout.root.join(dir)).unwrap();
        }
        std::fs::write(layout.root.join("etc/nix.conf"), NIX_CONF).unwrap();
        let log = std::fs::File::create(layout.root.join("daemon.log")).unwrap();
        let mut daemon = layout
            .command(&oracle.daemon)
            .env("NIX_REMOTE", "local")
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
        Self { daemon, layout, nix_store: oracle.store, nix_instantiate: oracle.instantiate, nonce, _root: root }
    }

    fn sui(&self) -> Command {
        self.layout.command(Path::new(env!("CARGO_BIN_EXE_sui")))
    }

    fn fresh_drv(&self, tag: &str) -> String {
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

    fn assert_private(&self, path: &str) {
        assert!(Path::new(path).starts_with(self.layout.store()), "{path} is outside the private store");
    }

    fn try_delete(&self, path: &str) -> Result<(), String> {
        self.assert_private(path);
        let out = self.layout.command(&self.nix_store).args(["--delete", path]).output().unwrap();
        if out.status.success() && !Path::new(path).exists() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }

    fn delete_once_released(&self, path: &str) -> Result<(), String> {
        let deadline = Instant::now() + RELEASE_BOUND;
        loop {
            match self.try_delete(path) {
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
                result => return result,
            }
        }
    }

    fn assert_real_store_untouched(&self) {
        let Ok(entries) = std::fs::read_dir(REAL_STORE) else {
            return;
        };
        let leaked: Vec<_> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(&self.nonce))
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

#[test]
fn every_command_of_the_fixture_is_bound_to_the_private_store() {
    let layout = Layout { root: PathBuf::from("/tmp/sui-c8-layout") };
    let command = layout.command(Path::new("/bin/true"));
    let env: std::collections::BTreeMap<String, String> = command
        .get_envs()
        .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
        .collect();
    assert_eq!(env["NIX_STORE_DIR"], "/tmp/sui-c8-layout/nix/store");
    assert_eq!(env["NIX_STATE_DIR"], "/tmp/sui-c8-layout/nix/var/nix");
    assert_eq!(env["NIX_LOG_DIR"], "/tmp/sui-c8-layout/nix/var/log/nix");
    assert_eq!(env["NIX_CONF_DIR"], "/tmp/sui-c8-layout/etc");
    assert_eq!(env["NIX_DAEMON_SOCKET_PATH"], "/tmp/sui-c8-layout/s");
    assert_eq!(env["NIX_REMOTE"], "unix:///tmp/sui-c8-layout/s");
    assert!(command.get_envs().any(|(k, v)| k == "NIX_CONFIG" && v.is_none()), "an inherited NIX_CONFIG must not reach the fixture");
    assert!(env.values().all(|v| !v.starts_with(REAL_STORE) && !v.starts_with(REAL_STATE)));
}

/// `sui build` leaves `result` behind as an indirect root, the way `nix build`
/// does: the output outlives the sui process until the link is removed.
#[test]
fn build_out_link_is_an_indirect_gc_root() {
    let Some(oracle) = oracle("build_out_link_is_an_indirect_gc_root") else {
        return;
    };
    let store = PrivateStore::start(oracle);
    let drv = store.fresh_drv("link");
    let work = tempfile::tempdir().unwrap();
    let link = work.path().join("result");
    let out = store
        .sui()
        .current_dir(work.path())
        .args(["build", "--out-link", &link.to_string_lossy(), &drv])
        .output()
        .unwrap();
    assert!(out.status.success(), "sui build: {}", String::from_utf8_lossy(&out.stderr));
    let built = String::from_utf8(out.stdout).unwrap().trim().to_string();
    store.assert_private(&built);

    assert_eq!(std::fs::read_link(&link).unwrap(), PathBuf::from(&built));
    let auto = store.layout.state().join("gcroots/auto").join(sui_store::gc_roots::auto_root_name(&link.to_string_lossy()));
    assert_eq!(std::fs::read_link(&auto).unwrap(), link, "indirect root registered");

    let refused = store.try_delete(&built);
    assert!(refused.is_err(), "the linked output must survive a collection");
    let why = refused.unwrap_err();
    assert!(
        why.contains("alive") || why.contains("referenced by the GC root"),
        "refused for liveness, got: {why}"
    );

    std::fs::remove_file(&link).unwrap();
    store.delete_once_released(&built).expect("with the link gone the output is collectable");
    store.assert_real_store_untouched();
}

/// A realize roots its output for the life of the process, so a collection
/// between "built" and "linked" cannot take it.
#[test]
fn a_realized_output_stays_rooted_while_the_process_lives() {
    let Some(oracle) = oracle("a_realized_output_stays_rooted_while_the_process_lives") else {
        return;
    };
    let store = PrivateStore::start(oracle);
    let drv = store.fresh_drv("temp");
    sui_store::set_store_url(&store.layout.remote());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let outputs = rt.block_on(sui_orchestrate::realize_drv(&drv)).unwrap();
    let built = outputs.first().expect("one output").clone();
    store.assert_private(&built);
    assert!(Path::new(&built).exists());

    let refused = store.try_delete(&built);
    assert!(refused.is_err(), "a live realize's output must survive a collection");

    sui_store::daemon_session::release(&store.layout.socket());
    store.delete_once_released(&built).expect("released, the output is collectable");
    store.assert_real_store_untouched();
}
