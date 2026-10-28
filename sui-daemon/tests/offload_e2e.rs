use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sui_build::offload::{Connector, Guard, Offload, OffloadPolicy, OffloadSwitch, HostCapabilities};
use sui_build::{BuildClosure, LocalBuilder};
use sui_compat::activity::SharedSink;
use sui_compat::derivation::{Derivation, DerivationOutput};
use sui_compat::hash::{HashAlgorithm, NixHash};
use sui_compat::machines::Machine;
use sui_compat::store_path::StorePath;
use sui_daemon::{Connection, DrvRealizer, TrustLevel};
use sui_store::ssh_store::RemoteConn;
use sui_store::traits::{PathInfo, Store, StoreResult};
use sui_store::worker_client::{ClientOptions, WorkerConn, WorkerError};

const DRV: &str = "/nix/store/1111111111111111111111111111111a-hello.drv";
const OUT: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-hello";

struct DirStore {
    dir: PathBuf,
    infos: Mutex<HashMap<String, PathInfo>>,
}

impl DirStore {
    fn new(dir: PathBuf) -> Self {
        Self { dir, infos: Mutex::new(HashMap::new()) }
    }

    fn real(&self, path: &str) -> PathBuf {
        self.dir.join(path.rsplit('/').next().unwrap())
    }

    fn put(&self, path: &str, contents: &[u8], references: Vec<String>) {
        std::fs::write(self.real(path), contents).unwrap();
        let mut nar = Vec::new();
        sui_compat::nar::NarWriter::write_path(&mut nar, &self.real(path)).unwrap();
        self.infos.lock().unwrap().insert(
            path.to_string(),
            PathInfo {
                path: path.to_string(),
                nar_hash: format!("sha256:{}", NixHash::of(HashAlgorithm::Sha256, &nar).to_nix32()),
                nar_size: nar.len() as i64,
                references,
                deriver: None,
                signatures: vec![],
                registration_time: 1,
                content_address: None,
            },
        );
    }
}

#[async_trait::async_trait]
impl Store for DirStore {
    async fn query_path_info(&self, path: &StorePath) -> StoreResult<Option<PathInfo>> {
        Ok(self.infos.lock().unwrap().get(&path.to_absolute_path()).cloned())
    }

    async fn is_valid_path(&self, path: &StorePath) -> StoreResult<bool> {
        Ok(self.infos.lock().unwrap().contains_key(&path.to_absolute_path()))
    }

    async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
        Ok(self.infos.lock().unwrap().keys().filter_map(|p| StorePath::from_absolute_path(p).ok()).collect())
    }

    async fn add_nar_at(&self, info: &PathInfo, nar: &[u8]) -> StoreResult<()> {
        sui_compat::nar::unpack_nar(nar, &self.real(&info.path)).map_err(|e| sui_store::StoreError::Internal(e.to_string()))?;
        self.infos.lock().unwrap().insert(info.path.clone(), info.clone());
        Ok(())
    }
}

struct WritesTheOutput(Arc<DirStore>);

impl DrvRealizer for WritesTheOutput {
    fn realize<'a>(
        &'a self,
        _drv_path: &'a str,
        _sink: SharedSink,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async move { Err("the remote never had the .drv; it must build what the client sent".to_string()) })
    }

    fn build_basic<'a>(
        &'a self,
        drv: &'a Derivation,
        _drv_path: &'a str,
        _sink: SharedSink,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async move {
            assert_eq!(drv.builder, "/bin/sh");
            self.0.put(OUT, b"built remotely\n", vec![]);
            Ok(vec![OUT.to_string()])
        })
    }
}

struct InProcessDaemon {
    remote: Arc<DirStore>,
    runtime: tokio::runtime::Handle,
    opened: Arc<Mutex<u32>>,
}

impl Connector for InProcessDaemon {
    fn open(&self, _machine: &Machine, _policy: &OffloadPolicy) -> Result<(RemoteConn, Guard), WorkerError> {
        *self.opened.lock().unwrap() += 1;
        let (client, server) = std::os::unix::net::UnixStream::pair()?;
        server.set_nonblocking(true)?;
        let remote = self.remote.clone();
        let dir = remote.dir.clone();
        let handle = self.runtime.spawn(async move {
            let (reader, writer) = tokio::net::UnixStream::from_std(server).unwrap().into_split();
            let realizer = Arc::new(WritesTheOutput(remote.clone()));
            let mut conn = Connection::new(remote, reader, writer, TrustLevel::Trusted)
                .with_dirs(&dir, &dir)
                .with_realizer(realizer);
            conn.handshake().await.unwrap();
            let _ = conn.run().await;
        });
        let reader: Box<dyn std::io::Read + Send> = Box::new(client.try_clone()?);
        let writer: Box<dyn std::io::Write + Send> = Box::new(client);
        let options = ClientOptions { max_jobs: 1, max_silent_time: 0, build_cores: 0 };
        let conn = WorkerConn::over_transport((reader, writer), options)?;
        Ok((RemoteConn::Worker(conn), Box::new(handle)))
    }
}

fn hello() -> Derivation {
    Derivation {
        outputs: BTreeMap::from([(
            "out".to_string(),
            DerivationOutput { path: OUT.to_string(), hash_algo: String::new(), hash: String::new() },
        )]),
        input_derivations: BTreeMap::new(),
        input_sources: vec![],
        system: "x86_64-linux".to_string(),
        builder: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), "echo hi > $out".to_string()],
        env: BTreeMap::from([("out".to_string(), OUT.to_string())]),
    }
}

fn policy(local_dir: &std::path::Path) -> OffloadPolicy {
    let none = |_: &std::path::Path| -> std::io::Result<String> { Err(std::io::ErrorKind::NotFound.into()) };
    let machines = sui_compat::machines::parse_config(&BTreeSet::new(), "ssh-ng://builder x86_64-linux - 1", &none).unwrap();
    let host = HostCapabilities { system: "aarch64-darwin".into(), extra_platforms: BTreeSet::new(), system_features: BTreeSet::new(), max_jobs: 1 };
    OffloadPolicy {
        switch: OffloadSwitch::On,
        machines,
        real_store_dir: local_dir.to_path_buf(),
        ..OffloadPolicy::off(host)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_foreign_derivation_builds_on_the_remote_and_its_output_comes_back_registered() {
    let local_dir = tempfile::tempdir().unwrap();
    let remote_dir = tempfile::tempdir().unwrap();
    let local = Arc::new(DirStore::new(local_dir.path().to_path_buf()));
    let remote = Arc::new(DirStore::new(remote_dir.path().to_path_buf()));
    let drv = hello();
    local.put(DRV, drv.serialize().as_bytes(), vec![]);
    let opened = Arc::new(Mutex::new(0));
    let connector = InProcessDaemon { remote: remote.clone(), runtime: tokio::runtime::Handle::current(), opened: opened.clone() };
    let builder = LocalBuilder::new(local.clone(), Box::new(sui_build::sandbox::NoSandbox))
        .with_offload(Offload::with_connector(policy(local_dir.path()), Arc::new(connector)));
    let closure = BuildClosure { derivations: vec![(DRV.to_string(), drv)] };
    let result = builder.build_closure(&closure, None).await.unwrap();
    assert!(result.success, "{}", result.log);
    assert_eq!(*opened.lock().unwrap(), 1);
    assert!(!remote.infos.lock().unwrap().contains_key(DRV), "a trusted builder gets the derivation in BuildDerivation, not as a store path");
    let info = local.infos.lock().unwrap().get(OUT).cloned().expect("the output is registered locally");
    assert_eq!(info, remote.infos.lock().unwrap().get(OUT).cloned().map(|mut i| { i.registration_time = info.registration_time; i }).unwrap());
    assert_eq!(std::fs::read(local.real(OUT)).unwrap(), b"built remotely\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_builder_is_disabled_and_the_build_falls_back_to_the_next_machine() {
    struct Refuses;
    impl Connector for Refuses {
        fn open(&self, _m: &Machine, _p: &OffloadPolicy) -> Result<(RemoteConn, Guard), WorkerError> {
            Err(WorkerError::Protocol("failed to start SSH connection to 'down'".into()))
        }
    }
    let local_dir = tempfile::tempdir().unwrap();
    let local = Arc::new(DirStore::new(local_dir.path().to_path_buf()));
    let drv = hello();
    local.put(DRV, drv.serialize().as_bytes(), vec![]);
    let builder = LocalBuilder::new(local.clone(), Box::new(sui_build::sandbox::NoSandbox))
        .with_offload(Offload::with_connector(policy(local_dir.path()), Arc::new(Refuses)));
    let closure = BuildClosure { derivations: vec![(DRV.to_string(), drv)] };
    let start = std::time::Instant::now();
    let result = builder.build_closure(&closure, None).await;
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    let log = match result {
        Ok(r) => r.log,
        Err(e) => e.to_string(),
    };
    assert!(log.contains("cannot build on 'ssh-ng://builder'"), "{log}");
}
