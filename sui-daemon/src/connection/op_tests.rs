use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

use sui_compat::content_address::{make_fixed_output_path_from_ca, render_content_address, ContentAddressMethod};
use sui_compat::hash::{HashAlgorithm, NixHash};
use sui_compat::nar::{NarNode, NarWriter};
use sui_compat::store_path::{StorePath, DEFAULT_STORE_DIR};
use sui_compat::wire::{self, StderrMsg, WorkerOp, PROTOCOL_VERSION};
use sui_store::traits::{PathInfo, Store, StoreError, StoreResult, VerifyResult};

use super::{Connection, ConnectionError};
use crate::trust::TrustLevel;
use sui_compat::activity::{
    Activity, ActivityEvent, ActivitySink, ActivityType, BuilderLog, ResultType, SharedSink, Verbosity, Worker,
};

const A_DRV: &str = "/nix/store/iyb4kvbj4ldvky1ia33wv05fsk49xcil-a.drv";
const A_OUT: &str = "/nix/store/19k94y693b5mi3rbb0f3y9c4zq1jc3z0-a";
const B_DRV: &str = "/nix/store/khbdq1zqmpdpppz3k8wai0p2v878c173-b.drv";
const B_DEV: &str = "/nix/store/h4ayh8c8zf9hvnwbaf1dr990ghfy71cw-b-dev";
const B_OUT: &str = "/nix/store/49dc9j6aviq2v2xjswzwzbhvwcc6693b-b";
const C_DRV: &str = "/nix/store/yah3dwzyj0wsrscgsdhvlikdhxgk4h43-c.drv";
const C_OUT: &str = "/nix/store/wpdbmhnn9ypjjk44pk8m8i5dq16ndqaz-c";
const A_TEXT: &str = r#"Derive([("out","/nix/store/19k94y693b5mi3rbb0f3y9c4zq1jc3z0-a","sha256","0000000000000000000000000000000000000000000000000000000000000000")],[],[],"aarch64-darwin","/bin/sh",[],[("builder","/bin/sh"),("name","a"),("out","/nix/store/19k94y693b5mi3rbb0f3y9c4zq1jc3z0-a"),("outputHash","0000000000000000000000000000000000000000000000000000000000000000"),("outputHashAlgo","sha256"),("outputHashMode","flat"),("system","aarch64-darwin")])"#;
const B_TEXT: &str = r#"Derive([("dev","/nix/store/h4ayh8c8zf9hvnwbaf1dr990ghfy71cw-b-dev","",""),("out","/nix/store/49dc9j6aviq2v2xjswzwzbhvwcc6693b-b","","")],[("/nix/store/iyb4kvbj4ldvky1ia33wv05fsk49xcil-a.drv",["out"])],[],"aarch64-darwin","/bin/sh",["-c","/nix/store/19k94y693b5mi3rbb0f3y9c4zq1jc3z0-a"],[("builder","/bin/sh"),("dev","/nix/store/h4ayh8c8zf9hvnwbaf1dr990ghfy71cw-b-dev"),("name","b"),("out","/nix/store/49dc9j6aviq2v2xjswzwzbhvwcc6693b-b"),("outputs","out dev"),("system","aarch64-darwin")])"#;
const C_TEXT: &str = r#"Derive([("out","/nix/store/wpdbmhnn9ypjjk44pk8m8i5dq16ndqaz-c","","")],[("/nix/store/khbdq1zqmpdpppz3k8wai0p2v878c173-b.drv",["dev","out"])],[],"aarch64-darwin","/bin/sh",["/nix/store/h4ayh8c8zf9hvnwbaf1dr990ghfy71cw-b-dev","/nix/store/49dc9j6aviq2v2xjswzwzbhvwcc6693b-b"],[("builder","/bin/sh"),("name","c"),("out","/nix/store/wpdbmhnn9ypjjk44pk8m8i5dq16ndqaz-c"),("system","aarch64-darwin")])"#;

#[derive(Default)]
struct TestStore {
    paths: Mutex<BTreeMap<String, PathInfo>>,
    outputs: BTreeMap<String, Vec<(String, String)>>,
    nars: Mutex<BTreeMap<String, Vec<u8>>>,
    corrupt: bool,
}

impl TestStore {
    fn with(self, path: &str) -> Self {
        self.add(path);
        self
    }

    fn add(&self, path: &str) {
        self.paths.lock().unwrap().insert(path.to_string(), PathInfo {
            nar_hash: format!("sha256:{}", NixHash::of(HashAlgorithm::Sha256, path.as_bytes()).to_hex()),
            nar_size: 8,
            ..PathInfo::new(path, "")
        });
    }

    fn with_drvs(mut self) -> Self {
        for (drv, outs) in [
            (A_DRV, vec![("out", A_OUT)]),
            (B_DRV, vec![("dev", B_DEV), ("out", B_OUT)]),
            (C_DRV, vec![("out", C_OUT)]),
        ] {
            self = self.with(drv);
            self.outputs.insert(drv.to_string(), outs.into_iter().map(|(n, p)| (n.to_string(), p.to_string())).collect());
        }
        self
    }

    fn has(&self, path: &str) -> bool {
        self.paths.lock().unwrap().contains_key(path)
    }
}

#[async_trait::async_trait]
impl Store for TestStore {
    async fn gc_liveness(&self, extra_roots: &[String]) -> StoreResult<(Vec<String>, std::collections::HashSet<String>)> {
        let paths = self.paths.lock().unwrap();
        let mut reachable = std::collections::HashSet::new();
        let mut todo: Vec<String> = extra_roots.to_vec();
        while let Some(p) = todo.pop() {
            if reachable.insert(p.rsplit('/').next().unwrap_or_default().to_string()) {
                todo.extend(paths.get(&p).map(|i| i.references.clone()).unwrap_or_default());
            }
        }
        Ok((paths.keys().cloned().collect(), reachable))
    }

    async fn gc_delete(&self, targets: &[String], ignore_liveness: bool, extra_roots: &[String]) -> StoreResult<(Vec<String>, u64)> {
        let (_, reachable) = self.gc_liveness(extra_roots).await?;
        for t in targets {
            if !ignore_liveness && reachable.contains(t.rsplit('/').next().unwrap_or_default()) {
                return Err(sui_store::traits::StoreError::StillAlive(t.clone()));
            }
        }
        let mut paths = self.paths.lock().unwrap();
        Ok((targets.iter().filter(|t| paths.remove(*t).is_some()).cloned().collect(), targets.len() as u64))
    }

    async fn query_path_info(&self, path: &StorePath) -> StoreResult<Option<PathInfo>> {
        Ok(self.paths.lock().unwrap().get(&path.to_absolute_path()).cloned())
    }

    async fn is_valid_path(&self, path: &StorePath) -> StoreResult<bool> {
        Ok(self.has(&path.to_absolute_path()))
    }

    async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
        Ok(self.paths.lock().unwrap().keys().filter_map(|p| StorePath::from_absolute_path(p).ok()).collect())
    }

    async fn add_to_store_from_dump(
        &self,
        name: &str,
        dump: &[u8],
        method: &ContentAddressMethod,
        algo: HashAlgorithm,
        references: &[String],
        _repair: bool,
    ) -> StoreResult<PathInfo> {
        let hash = NixHash::of(algo, dump);
        let path = make_fixed_output_path_from_ca(DEFAULT_STORE_DIR, method, &hash, references, name)
            .map_err(|e| StoreError::Internal(e.to_string()))?;
        let info = PathInfo {
            path: path.clone(),
            nar_hash: "sha256:00".into(),
            nar_size: dump.len() as i64,
            references: references.to_vec(),
            registration_time: 7,
            content_address: Some(render_content_address(method, &hash)),
            ..PathInfo::default()
        };
        self.paths.lock().unwrap().insert(path, info.clone());
        Ok(info)
    }

    async fn add_nar_at(&self, info: &PathInfo, nar: &[u8]) -> StoreResult<()> {
        self.nars.lock().unwrap().insert(info.path.clone(), nar.to_vec());
        self.paths.lock().unwrap().insert(info.path.clone(), info.clone());
        Ok(())
    }

    async fn add_signatures(&self, path: &StorePath, signatures: &[String]) -> StoreResult<()> {
        let mut paths = self.paths.lock().unwrap();
        let info = paths
            .get_mut(&path.to_absolute_path())
            .ok_or_else(|| StoreError::PathNotFound(format!("path '{}' is not valid", path.to_absolute_path())))?;
        info.signatures.extend(signatures.iter().cloned());
        Ok(())
    }

    async fn query_derivation_output_map(&self, drv: &StorePath) -> StoreResult<Option<Vec<(String, String)>>> {
        let abs = drv.to_absolute_path();
        if !self.has(&abs) {
            return Ok(None);
        }
        Ok(Some(self.outputs.get(&abs).cloned().unwrap_or_default()))
    }

    async fn query_valid_derivers(&self, path: &StorePath) -> StoreResult<Vec<String>> {
        let abs = path.to_absolute_path();
        Ok(self.outputs.iter().filter(|(_, o)| o.iter().any(|(_, p)| *p == abs)).map(|(d, _)| d.clone()).collect())
    }

    async fn query_path_from_hash_part(&self, hash_part: &str) -> StoreResult<Option<String>> {
        let prefix = format!("{DEFAULT_STORE_DIR}/{hash_part}-");
        Ok(self.paths.lock().unwrap().keys().find(|p| p.starts_with(&prefix)).cloned())
    }

    async fn verify_store(&self) -> StoreResult<VerifyResult> {
        let corrupt = if self.corrupt {
            vec![sui_store::traits::CorruptPath { path: A_OUT.into(), expected_hash: "x".into(), actual_hash: "y".into() }]
        } else {
            Vec::new()
        };
        Ok(VerifyResult { corrupt, ..VerifyResult::default() })
    }

    async fn optimise_store(&self, _dry_run: bool) -> StoreResult<sui_store::traits::OptimiseResult> {
        Ok(sui_store::traits::OptimiseResult::default())
    }
}

struct Session {
    state: Arc<tempfile::TempDir>,
    store: Arc<TestStore>,
    trust: TrustLevel,
    version: u64,
    dirs: Option<(std::path::PathBuf, std::path::PathBuf)>,
    caches: Vec<Arc<sui_store::BinaryCacheStore>>,
    drvs: Option<Arc<dyn super::DrvRealizer>>,
}

impl Session {
    fn new(store: TestStore) -> Self {
        let state = Arc::new(tempfile::tempdir().unwrap());
        let dirs = Some((std::path::PathBuf::from("/nix/store"), state.path().to_path_buf()));
        Self { state, store: Arc::new(store), trust: TrustLevel::Trusted, version: PROTOCOL_VERSION, dirs, caches: Vec::new(), drvs: None }
    }

    fn realizer(mut self, drvs: Arc<dyn super::DrvRealizer>) -> Self {
        self.drvs = Some(drvs);
        self
    }

    fn untrusted(mut self) -> Self {
        self.trust = TrustLevel::NotTrusted;
        self
    }

    fn minor(mut self, minor: u64) -> Self {
        self.version = (1 << 8) | minor;
        self
    }

    fn dirs(mut self, store: &std::path::Path, state: &std::path::Path) -> Self {
        self.dirs = Some((store.to_path_buf(), state.to_path_buf()));
        self
    }

    async fn run(&self, input: Vec<u8>) -> Reply {
        let mut conn = Connection::new(self.store.clone(), Cursor::new(input), Vec::<u8>::new(), self.trust)
            .with_caches(self.caches.clone());
        if let Some((store, state)) = &self.dirs {
            conn = conn.with_dirs(store, state);
        }
        if let Some(drvs) = &self.drvs {
            conn = conn.with_realizer(drvs.clone());
        }
        conn.client_version = self.version;
        conn.run().await.unwrap();
        Reply(Cursor::new(conn.writer))
    }
}

struct Reply(Cursor<Vec<u8>>);

impl Reply {
    fn u64(&mut self) -> u64 {
        wire::read_u64(&mut self.0).unwrap()
    }

    fn string(&mut self) -> String {
        wire::read_string(&mut self.0).unwrap()
    }

    fn strings(&mut self) -> Vec<String> {
        wire::read_string_list(&mut self.0).unwrap()
    }

    fn last(&mut self) -> &mut Self {
        assert_eq!(self.u64(), StderrMsg::Last as u64, "expected STDERR_LAST");
        self
    }

    fn error(&mut self) -> String {
        assert_eq!(self.u64(), StderrMsg::Error as u64, "expected STDERR_ERROR");
        assert_eq!(self.string(), "Error");
        assert_eq!(self.u64(), 0);
        assert_eq!(self.string(), "Error");
        let msg = self.string();
        assert_eq!(self.u64(), 0);
        assert_eq!(self.u64(), 0);
        msg
    }

    fn end(&mut self) {
        assert!(wire::read_u64(&mut self.0).is_err(), "unexpected trailing reply bytes");
    }
}

fn op(op: WorkerOp) -> Vec<u8> {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, op as u64).unwrap();
    buf
}

fn s(buf: &mut Vec<u8>, v: &str) {
    wire::write_string(buf, v).unwrap();
}

fn n(buf: &mut Vec<u8>, v: u64) {
    wire::write_u64(buf, v).unwrap();
}

fn list(buf: &mut Vec<u8>, v: &[&str]) {
    wire::write_string_list(buf, v).unwrap();
}

fn framed(buf: &mut Vec<u8>, data: &[u8]) {
    for chunk in data.chunks(5) {
        n(buf, chunk.len() as u64);
        buf.extend_from_slice(chunk);
    }
    n(buf, 0);
}

fn is_valid(buf: &mut Vec<u8>, path: &str) {
    buf.extend(op(WorkerOp::IsValidPath));
    s(buf, path);
}

fn nar_of(node: &NarNode) -> Vec<u8> {
    let mut nar = Vec::new();
    NarWriter::write(&mut nar, node).unwrap();
    nar
}

fn file_nar(contents: &[u8]) -> Vec<u8> {
    nar_of(&NarNode::Regular { executable: false, contents: contents.to_vec() })
}

fn drv_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, text) in [(A_DRV, A_TEXT), (B_DRV, B_TEXT), (C_DRV, C_TEXT)] {
        std::fs::write(dir.path().join(path.rsplit('/').next().unwrap()), text).unwrap();
    }
    dir
}

fn add_to_store(method: &str, name: &str, refs: &[&str], dump: &[u8]) -> Vec<u8> {
    let mut input = op(WorkerOp::AddToStore);
    s(&mut input, name);
    s(&mut input, method);
    list(&mut input, refs);
    n(&mut input, 0);
    framed(&mut input, dump);
    input
}

#[tokio::test]
async fn add_to_store_ingests_text_flat_and_recursive_under_every_algorithm() {
    let session = Session::new(TestStore::default().with(A_OUT));
    for (method, ca_method, algo, dump) in [
        ("text:sha256", ContentAddressMethod::Text, HashAlgorithm::Sha256, b"y".to_vec()),
        ("fixed:sha256", ContentAddressMethod::Flat, HashAlgorithm::Sha256, b"y".to_vec()),
        ("fixed:sha1", ContentAddressMethod::Flat, HashAlgorithm::Sha1, b"y".to_vec()),
        ("fixed:md5", ContentAddressMethod::Flat, HashAlgorithm::Md5, b"y".to_vec()),
        ("fixed:r:sha256", ContentAddressMethod::Recursive, HashAlgorithm::Sha256, file_nar(b"y")),
        ("fixed:r:sha512", ContentAddressMethod::Recursive, HashAlgorithm::Sha512, file_nar(b"y")),
    ] {
        let refs: &[&str] = if method == "text:sha256" { &[A_OUT] } else { &[] };
        let refs_owned: Vec<String> = refs.iter().map(|r| (*r).to_string()).collect();
        let hash = NixHash::of(algo, &dump);
        let expected = make_fixed_output_path_from_ca(DEFAULT_STORE_DIR, &ca_method, &hash, &refs_owned, "x").unwrap();
        let mut r = session.run(add_to_store(method, "x", refs, &dump)).await;
        r.last();
        assert_eq!(r.string(), expected, "{method}");
        assert_eq!(r.string(), "");
        assert_eq!(r.string(), "sha256:00");
        assert_eq!(r.strings(), refs_owned);
        assert_eq!(r.u64(), 7);
        assert_eq!(r.u64(), dump.len() as u64);
        assert_eq!(r.u64(), 0);
        assert!(r.strings().is_empty());
        assert_eq!(r.string(), render_content_address(&ca_method, &hash));
        r.end();
    }
    assert_eq!(
        sui_compat::content_address::compute_text_store_path("x", b"y", &[]).unwrap().to_absolute_path(),
        make_fixed_output_path_from_ca(
            DEFAULT_STORE_DIR,
            &ContentAddressMethod::Text,
            &NixHash::of(HashAlgorithm::Sha256, b"y"),
            &[],
            "x"
        )
        .unwrap()
    );
}

#[tokio::test]
async fn an_ingestion_error_is_an_error_frame_and_the_connection_continues() {
    let session = Session::new(TestStore::default().with(A_OUT));
    let mut input = add_to_store("fixed:git:sha1", "x", &[], b"y");
    input.extend(add_to_store("fixed:sha256", "x", &[A_OUT], b"y"));
    is_valid(&mut input, A_OUT);
    let mut r = session.run(input).await;
    assert!(r.error().contains("git-hashing"));
    assert!(r.error().contains("not allowed to refer to other store paths"));
    r.last();
    assert_eq!(r.u64(), 1);
    r.end();
}

#[tokio::test]
async fn add_to_store_before_1_25_reads_the_legacy_fields_and_a_raw_nar() {
    let session = Session::new(TestStore::default()).minor(24);
    let dir_nar = nar_of(&NarNode::Directory {
        entries: vec![
            sui_compat::nar::NarEntry { name: "a".into(), node: NarNode::Regular { executable: true, contents: b"aa".to_vec() } },
            sui_compat::nar::NarEntry {
                name: "d".into(),
                node: NarNode::Directory {
                    entries: vec![sui_compat::nar::NarEntry { name: "l".into(), node: NarNode::Symlink { target: "../a".into() } }],
                },
            },
            sui_compat::nar::NarEntry { name: "e".into(), node: NarNode::Directory { entries: vec![] } },
        ],
    });
    let mut input = op(WorkerOp::AddToStore);
    s(&mut input, "tree");
    n(&mut input, 1);
    n(&mut input, 1);
    s(&mut input, "sha256");
    input.extend_from_slice(&dir_nar);
    input.extend(op(WorkerOp::AddToStore));
    s(&mut input, "flat");
    n(&mut input, 1);
    n(&mut input, 0);
    s(&mut input, "sha1");
    input.extend_from_slice(&file_nar(b"z"));
    let mut r = session.run(input).await;
    r.last();
    assert_eq!(r.string(), sui_compat::store_path::make_source_path(&NixHash::of(HashAlgorithm::Sha256, &dir_nar).to_hex(), &[], false, "tree"));
    r.last();
    assert_eq!(
        r.string(),
        sui_compat::store_path::compute_fixed_output_hash("sha1", &NixHash::of(HashAlgorithm::Sha1, b"z").to_hex(), false, "flat")
    );
    r.end();
}

#[tokio::test]
async fn add_text_to_store_lands_at_the_text_path_of_its_references() {
    let session = Session::new(TestStore::default().with(A_OUT));
    let mut input = op(WorkerOp::AddTextToStore);
    s(&mut input, "t");
    s(&mut input, "ref");
    list(&mut input, &[A_OUT]);
    let mut r = session.run(input).await;
    r.last();
    assert_eq!(
        r.string(),
        sui_compat::content_address::compute_text_store_path("t", b"ref", &[A_OUT.to_string()]).unwrap().to_absolute_path()
    );
    r.end();
}

fn add_to_store_nar(path: &str, nar: &[u8], sigs: &[&str], ca: &str, dont_check_sigs: bool) -> Vec<u8> {
    let mut input = op(WorkerOp::AddToStoreNar);
    s(&mut input, path);
    s(&mut input, "");
    s(&mut input, &NixHash::of(HashAlgorithm::Sha256, nar).to_hex());
    list(&mut input, &[]);
    n(&mut input, 0);
    n(&mut input, nar.len() as u64);
    n(&mut input, 0);
    list(&mut input, sigs);
    s(&mut input, ca);
    n(&mut input, 0);
    n(&mut input, u64::from(dont_check_sigs));
    framed(&mut input, nar);
    input
}

#[tokio::test]
async fn add_to_store_nar_checks_signatures_unless_a_trusted_client_waives_them() {
    let nar = file_nar(b"n");
    let path = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-n";
    let untrusted = Session::new(TestStore::default()).untrusted();
    let mut r = untrusted.run(add_to_store_nar(path, &nar, &[], "", true)).await;
    assert!(r.error().contains("lacks a signature by a trusted key"));
    r.end();
    assert!(!untrusted.store.has(path));

    let key = sui_compat::signature::SecretKey::generate("test-1");
    let fingerprint = sui_compat::signature::compute_fingerprint(
        path,
        &format!("sha256:{}", NixHash::of(HashAlgorithm::Sha256, &nar).to_hex()),
        nar.len() as u64,
        &[],
    );
    let sig = key.sign_fingerprint(&fingerprint);
    let store = Arc::new(TestStore::default());
    let mut policy = super::SignaturePolicy::default();
    policy.trusted_keys.push(key.to_public_string());
    let state = tempfile::tempdir().unwrap();
    let mut conn = Connection::new(store.clone(), Cursor::new(add_to_store_nar(path, &nar, &[&sig], "", false)), Vec::<u8>::new(), TrustLevel::NotTrusted)
        .with_signature_policy(policy)
        .with_dirs("/nix/store", state.path());
    conn.client_version = PROTOCOL_VERSION;
    conn.run().await.unwrap();
    let mut r = Reply(Cursor::new(conn.writer));
    r.last();
    r.end();
    assert!(store.has(path));

    let ca_nar = file_nar(b"c");
    let ca = render_content_address(&ContentAddressMethod::Flat, &NixHash::of(HashAlgorithm::Sha256, b"c"));
    let ca_path = make_fixed_output_path_from_ca(DEFAULT_STORE_DIR, &ContentAddressMethod::Flat, &NixHash::of(HashAlgorithm::Sha256, b"c"), &[], "c").unwrap();
    let mut r = untrusted.run(add_to_store_nar(&ca_path, &ca_nar, &[], &ca, false)).await;
    r.last();
    r.end();
    assert_eq!(untrusted.store.nars.lock().unwrap()[&ca_path], ca_nar);

    let trusted = Session::new(TestStore::default());
    let mut r = trusted.run(add_to_store_nar(path, &nar, &[], "", true)).await;
    r.last();
    r.end();
    assert_eq!(trusted.store.nars.lock().unwrap()[path], nar);
}

#[tokio::test]
async fn add_multiple_to_store_registers_every_path_in_the_framed_stream() {
    let session = Session::new(TestStore::default());
    let mut stream = Vec::new();
    n(&mut stream, 2);
    let entries = [("/nix/store/0123456789abcdfghijklmnpqrsvwxyz-p", file_nar(b"p")), ("/nix/store/1123456789abcdfghijklmnpqrsvwxyz-q", file_nar(b"qq"))];
    for (path, nar) in &entries {
        s(&mut stream, path);
        s(&mut stream, "");
        s(&mut stream, &NixHash::of(HashAlgorithm::Sha256, nar).to_hex());
        list(&mut stream, &[]);
        n(&mut stream, 0);
        n(&mut stream, nar.len() as u64);
        n(&mut stream, 0);
        list(&mut stream, &[]);
        s(&mut stream, "");
        stream.extend_from_slice(nar);
    }
    let mut input = op(WorkerOp::AddMultipleToStore);
    n(&mut input, 0);
    n(&mut input, 1);
    framed(&mut input, &stream);
    let mut r = session.run(input).await;
    r.last();
    r.end();
    for (path, nar) in &entries {
        assert_eq!(&session.store.nars.lock().unwrap()[*path], nar);
    }
}

#[tokio::test]
async fn nar_from_path_streams_the_raw_nar_after_stderr_last() {
    let dir = tempfile::tempdir().unwrap();
    let base = "0123456789abcdfghijklmnpqrsvwxyz-tree";
    std::fs::create_dir(dir.path().join(base)).unwrap();
    std::fs::write(dir.path().join(base).join("f"), vec![7u8; 200_000]).unwrap();
    let session = Session::new(TestStore::default()).dirs(dir.path(), dir.path());
    let mut input = op(WorkerOp::NarFromPath);
    s(&mut input, &format!("/nix/store/{base}"));
    is_valid(&mut input, A_OUT);
    let mut r = session.run(input).await;
    r.last();
    let mut expected = Vec::new();
    NarWriter::write_path(&mut expected, &dir.path().join(base)).unwrap();
    let start = r.0.position() as usize;
    assert_eq!(&r.0.get_ref()[start..start + expected.len()], expected.as_slice());
    r.0.set_position((start + expected.len()) as u64);
    r.last();
    assert_eq!(r.u64(), 0);
    r.end();
}

#[tokio::test]
async fn export_then_import_round_trips_through_the_tunnel() {
    let dir = tempfile::tempdir().unwrap();
    let base = "0123456789abcdfghijklmnpqrsvwxyz-e";
    std::fs::write(dir.path().join(base), b"exported").unwrap();
    let path = format!("/nix/store/{base}");
    let nar = file_nar(b"exported");
    let store = TestStore::default().with(A_OUT);
    store.paths.lock().unwrap().insert(path.clone(), PathInfo {
        nar_hash: format!("sha256:{}", NixHash::of(HashAlgorithm::Sha256, &nar).to_hex()),
        nar_size: nar.len() as i64,
        references: vec![A_OUT.to_string()],
        ..PathInfo::new(&path, "")
    });
    let session = Session::new(store).dirs(dir.path(), dir.path());
    let mut input = op(WorkerOp::ExportPath);
    s(&mut input, &path);
    n(&mut input, 0);
    let mut r = session.run(input).await;
    let mut exported = Vec::new();
    loop {
        match r.u64() {
            m if m == StderrMsg::Data as u64 => exported.extend(wire::read_bytes(&mut r.0).unwrap()),
            m => {
                assert_eq!(m, StderrMsg::Last as u64);
                break;
            }
        }
    }
    assert_eq!(r.u64(), 1);
    r.end();
    assert_eq!(&exported[..nar.len()], nar.as_slice());

    let importer = Session::new(TestStore::default().with(A_OUT));
    let mut stream = Vec::new();
    n(&mut stream, 1);
    stream.extend_from_slice(&exported);
    n(&mut stream, 0);
    let mut input = op(WorkerOp::ImportPaths);
    wire::write_bytes(&mut input, &stream).unwrap();
    let mut r = importer.run(input).await;
    assert_eq!(r.u64(), StderrMsg::Read as u64);
    assert_eq!(r.u64(), 65536);
    r.last();
    assert_eq!(r.strings(), vec![path.clone()]);
    r.end();
    assert_eq!(importer.store.nars.lock().unwrap()[&path], nar);
    assert_eq!(importer.store.paths.lock().unwrap()[&path].references, vec![A_OUT.to_string()]);
}

fn built(drv: &str, outputs: &str) -> String {
    format!("{drv}!{outputs}")
}

#[tokio::test]
async fn build_paths_acks_valid_targets_and_reports_a_missing_one_without_closing() {
    let dir = drv_dir();
    let session = Session::new(TestStore::default().with_drvs().with(C_OUT)).dirs(dir.path(), dir.path());
    let mut input = op(WorkerOp::BuildPaths);
    list(&mut input, &[A_DRV, &built(C_DRV, "out")]);
    n(&mut input, 0);
    input.extend(op(WorkerOp::BuildPaths));
    list(&mut input, &[B_OUT]);
    n(&mut input, 0);
    input.extend(op(WorkerOp::BuildPaths));
    list(&mut input, &[A_OUT]);
    n(&mut input, 1);
    let mut r = session.clone_untrusted().run(input.clone()).await;
    r.last();
    assert_eq!(r.u64(), 1);
    assert!(r.error().contains("there is no substituter that can build it"));
    assert!(r.error().contains("repairing is not allowed"));
    r.end();
}

impl Session {
    fn clone_untrusted(&self) -> Self {
        Self { state: self.state.clone(), store: self.store.clone(), trust: TrustLevel::NotTrusted, version: self.version, dirs: self.dirs.clone(), caches: self.caches.clone(), drvs: self.drvs.clone() }
    }
}

#[tokio::test]
async fn build_paths_with_results_reports_already_valid_outputs_with_their_realisations() {
    let dir = drv_dir();
    let session = Session::new(TestStore::default().with_drvs().with(B_DEV).with(B_OUT)).dirs(dir.path(), dir.path());
    let mut input = op(WorkerOp::BuildPathsWithResults);
    list(&mut input, &[&built(B_DRV, "*"), A_OUT, &built(B_DRV, "nope")]);
    n(&mut input, 0);
    let mut r = session.run(input).await;
    r.last();
    assert_eq!(r.u64(), 3);
    assert_eq!(r.string(), built(B_DRV, "*"));
    assert_eq!(r.u64(), 2);
    assert_eq!(r.string(), "");
    assert_eq!(r.u64(), 0);
    assert_eq!(r.u64(), 0);
    r.u64();
    r.u64();
    assert_eq!(r.u64(), 0);
    assert_eq!(r.u64(), 0);
    assert_eq!(r.u64(), 2);
    let b = sui_compat::derivation::Derivation::parse(B_TEXT.as_bytes()).unwrap();
    let mut load = |_: &str| sui_compat::derivation::Derivation::parse(A_TEXT.as_bytes());
    let hashes = sui_compat::derivation::output_hashes_modulo(&b, true, &mut load, &mut std::collections::HashMap::new()).unwrap();
    for (name, path) in [("dev", B_DEV), ("out", B_OUT)] {
        let id = format!("sha256:{}!{name}", hashes[name]);
        assert_eq!(r.string(), id);
        let realisation = sui_compat::realisation::Realisation::from_json(&r.string()).unwrap();
        assert_eq!(realisation.id.to_string(), id);
        assert_eq!(realisation.out_path, path.rsplit('/').next().unwrap());
    }
    assert_eq!(r.string(), A_OUT);
    assert_eq!(r.u64(), 14);
    assert!(r.string().contains("no substituter"));
    for _ in 0..6 {
        r.u64();
    }
    assert_eq!(r.u64(), 0);
    assert_eq!(r.string(), built(B_DRV, "nope"));
    assert_eq!(r.u64(), 9);
    assert!(r.string().contains("does not have wanted outputs 'nope'"));
    for _ in 0..7 {
        r.u64();
    }
    r.end();
}

fn build_derivation_request(drv: &str, text: &str) -> Vec<u8> {
    let d = sui_compat::derivation::Derivation::parse(text.as_bytes()).unwrap();
    let mut input = op(WorkerOp::BuildDerivation);
    s(&mut input, drv);
    n(&mut input, d.outputs.len() as u64);
    for (name, o) in &d.outputs {
        s(&mut input, name);
        s(&mut input, &o.path);
        s(&mut input, &o.hash_algo);
        s(&mut input, &o.hash);
    }
    let srcs: Vec<&str> = d.input_sources.iter().map(String::as_str).collect();
    list(&mut input, &srcs);
    s(&mut input, &d.system);
    s(&mut input, &d.builder);
    let args: Vec<&str> = d.args.iter().map(String::as_str).collect();
    list(&mut input, &args);
    n(&mut input, d.env.len() as u64);
    for (k, v) in &d.env {
        s(&mut input, k);
        s(&mut input, v);
    }
    n(&mut input, 0);
    input
}

#[tokio::test]
async fn build_derivation_refuses_untrusted_input_addressed_and_builds_a_stored_one() {
    let dir = drv_dir();
    let session = Session::new(TestStore::default().with_drvs().with(C_OUT)).dirs(dir.path(), dir.path());
    let mut r = session.clone_untrusted().run(build_derivation_request(C_DRV, C_TEXT)).await;
    assert!(r.error().contains("not privileged to build input-addressed derivations"));
    r.end();
    let mut r = session.run(build_derivation_request(C_DRV, C_TEXT)).await;
    r.last();
    assert_eq!(r.u64(), 2);
    assert_eq!(r.string(), "");
    for _ in 0..6 {
        r.u64();
    }
    assert_eq!(r.u64(), 1);
    r.string();
    r.string();
    r.end();
    let mut r = session.realizer(Arc::new(StoredOnly)).run(build_derivation_request(C_DRV, B_TEXT)).await;
    r.last();
    assert_eq!(r.u64(), 9);
    let e = r.string();
    assert!(e.contains("as sent by the client"), "{e}");
}

struct StoredOnly;

impl super::DrvRealizer for StoredOnly {
    fn realize<'a>(
        &'a self,
        drv_path: &'a str,
        _sink: SharedSink,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async move { Err(format!("realize of '{drv_path}' was not expected")) })
    }
}

struct BuildsSent {
    store: Arc<TestStore>,
    got: Mutex<Vec<(String, sui_compat::derivation::Derivation)>>,
}

impl super::DrvRealizer for BuildsSent {
    fn realize<'a>(
        &'a self,
        drv_path: &'a str,
        _sink: SharedSink,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async move { Err(format!("realize of a stored '{drv_path}' was not expected")) })
    }

    fn build_basic<'a>(
        &'a self,
        drv: &'a sui_compat::derivation::Derivation,
        drv_path: &'a str,
        _sink: SharedSink,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async move {
            self.got.lock().unwrap().push((drv_path.to_string(), drv.clone()));
            let outs: Vec<String> = drv.outputs.values().map(|o| o.path.clone()).collect();
            for o in &outs {
                self.store.add(o);
            }
            Ok(outs)
        })
    }
}

#[tokio::test]
async fn build_derivation_builds_a_derivation_the_builder_has_never_seen_as_cppnix_remote_builds_need() {
    let store = Arc::new(TestStore::default().with(A_OUT));
    let realizer = Arc::new(BuildsSent { store: store.clone(), got: Mutex::new(Vec::new()) });
    let session = Session { store: store.clone(), ..Session::new(TestStore::default()) }.realizer(realizer.clone());
    let mut r = session.clone_untrusted().run(build_derivation_request(C_DRV, C_TEXT)).await;
    assert!(r.error().contains("not privileged to build input-addressed derivations"));
    r.end();
    assert!(realizer.got.lock().unwrap().is_empty());

    let mut r = session.run(build_derivation_request(C_DRV, C_TEXT)).await;
    r.last();
    assert_eq!(r.u64(), 0, "Built");
    assert_eq!(r.string(), "");
    for _ in 0..6 {
        r.u64();
    }
    assert_eq!(r.u64(), 1);
    let id = r.string();
    let realisation = sui_compat::realisation::Realisation::from_json(&r.string()).unwrap();
    assert!(id.ends_with("!out"), "{id}");
    assert_eq!(realisation.out_path, C_OUT.rsplit('/').next().unwrap());
    r.end();
    assert!(store.has(C_OUT));
    let got = realizer.got.lock().unwrap();
    assert_eq!(got.len(), 1);
    let (path, drv) = &got[0];
    let sent = sui_compat::derivation::Derivation::parse(C_TEXT.as_bytes()).unwrap();
    assert_eq!(path, C_DRV);
    assert!(drv.input_derivations.is_empty());
    assert_eq!((&drv.outputs, &drv.input_sources, &drv.system, &drv.builder, &drv.args, &drv.env),
               (&sent.outputs, &sent.input_sources, &sent.system, &sent.builder, &sent.args, &sent.env));
}

#[tokio::test]
async fn ensure_path_acks_a_valid_path_and_fails_an_unsubstitutable_one() {
    let session = Session::new(TestStore::default().with(A_OUT));
    let mut input = op(WorkerOp::EnsurePath);
    s(&mut input, A_OUT);
    input.extend(op(WorkerOp::EnsurePath));
    s(&mut input, B_OUT);
    let mut r = session.run(input).await;
    r.last();
    assert_eq!(r.u64(), 1);
    assert_eq!(r.error(), format!("path '{B_OUT}' does not exist and cannot be created"));
    r.end();
}

#[tokio::test]
async fn query_missing_walks_the_derivation_graph_like_cppnix() {
    let dir = drv_dir();
    let session = Session::new(TestStore::default().with_drvs().with(A_OUT)).dirs(dir.path(), dir.path());
    let mut input = op(WorkerOp::QueryMissing);
    list(&mut input, &[&built(C_DRV, "out"), A_OUT, B_DEV, "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-x.drv!out"]);
    let mut r = session.run(input).await;
    r.last();
    assert_eq!(r.strings(), vec![B_DRV.to_string(), C_DRV.to_string()]);
    assert!(r.strings().is_empty());
    assert_eq!(r.strings(), vec!["/nix/store/0123456789abcdfghijklmnpqrsvwxyz-x.drv".to_string(), B_DEV.to_string()]);
    assert_eq!(r.u64(), 0);
    assert_eq!(r.u64(), 0);
    r.end();
}

#[tokio::test]
async fn realisations_are_never_registered_and_never_found() {
    let session = Session::new(TestStore::default());
    let id = format!("sha256:{}!out", "0".repeat(64));
    let mut input = op(WorkerOp::RegisterDrvOutput);
    s(&mut input, &sui_compat::realisation::Realisation {
        id: sui_compat::realisation::DrvOutput::parse(&id).unwrap(),
        out_path: "0123456789abcdfghijklmnpqrsvwxyz-x".into(),
        signatures: Default::default(),
        dependent_realisations: Default::default(),
    }
    .to_json());
    input.extend(op(WorkerOp::QueryRealisation));
    s(&mut input, &id);
    let mut r = session.run(input).await;
    assert!(r.error().contains("ca-derivations"));
    r.last();
    assert_eq!(r.u64(), 0);
    r.end();
}

#[tokio::test]
async fn derivation_queries_answer_from_the_output_map() {
    let dir = drv_dir();
    let session = Session::new(TestStore::default().with_drvs()).dirs(dir.path(), dir.path());
    let mut input = Vec::new();
    for o in [WorkerOp::QueryDerivationOutputMap, WorkerOp::QueryDerivationOutputs, WorkerOp::QueryDerivationOutputNames] {
        input.extend(op(o));
        s(&mut input, B_DRV);
    }
    input.extend(op(WorkerOp::QueryValidDerivers));
    s(&mut input, B_DEV);
    input.extend(op(WorkerOp::QueryDerivationOutputMap));
    s(&mut input, "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-x.drv");
    let mut r = session.run(input).await;
    r.last();
    assert_eq!(r.u64(), 2);
    assert_eq!((r.string(), r.string()), ("dev".to_string(), B_DEV.to_string()));
    assert_eq!((r.string(), r.string()), ("out".to_string(), B_OUT.to_string()));
    r.last();
    assert_eq!(r.strings(), vec![B_OUT.to_string(), B_DEV.to_string()]);
    r.last();
    assert_eq!(r.strings(), vec!["dev".to_string(), "out".to_string()]);
    r.last();
    assert_eq!(r.strings(), vec![B_DRV.to_string()]);
    assert!(r.error().contains("is not valid"));
    r.end();
}

#[tokio::test]
async fn path_queries_answer_hash_part_hash_references_and_deriver() {
    let store = TestStore::default().with(A_OUT);
    store.paths.lock().unwrap().get_mut(A_OUT).unwrap().deriver = Some(A_DRV.rsplit('/').next().unwrap().to_string());
    let session = Session::new(store);
    let mut input = op(WorkerOp::QueryPathFromHashPart);
    s(&mut input, "19k94y693b5mi3rbb0f3y9c4zq1jc3z0");
    input.extend(op(WorkerOp::QueryPathFromHashPart));
    s(&mut input, "29k94y693b5mi3rbb0f3y9c4zq1jc3z0");
    input.extend(op(WorkerOp::QueryPathHash));
    s(&mut input, A_OUT);
    input.extend(op(WorkerOp::QueryDeriver));
    s(&mut input, A_OUT);
    input.extend(op(WorkerOp::QueryReferences));
    s(&mut input, B_OUT);
    input.extend(op(WorkerOp::QueryReferrers));
    s(&mut input, A_OUT);
    let mut r = session.run(input).await;
    r.last();
    assert_eq!(r.string(), A_OUT);
    r.last();
    assert_eq!(r.string(), "");
    r.last();
    assert_eq!(r.string(), NixHash::of(HashAlgorithm::Sha256, A_OUT.as_bytes()).to_hex());
    r.last();
    assert_eq!(r.string(), A_DRV);
    assert_eq!(r.error(), format!("path '{B_OUT}' is not valid"));
    r.last();
    assert!(r.strings().is_empty());
    r.end();
}

#[tokio::test]
async fn query_path_info_writes_the_unkeyed_info_with_its_content_address() {
    let store = TestStore::default();
    store.paths.lock().unwrap().insert(A_OUT.to_string(), PathInfo {
        nar_hash: format!("sha256:{}", "ab".repeat(32)),
        nar_size: 9,
        references: vec![B_OUT.into(), A_OUT.into()],
        signatures: vec!["k:2".into(), "k:1".into()],
        registration_time: 5,
        content_address: Some("fixed:sha256:abc".into()),
        ..PathInfo::new(A_OUT, "")
    });
    let session = Session::new(store);
    let mut input = op(WorkerOp::QueryPathInfo);
    s(&mut input, A_OUT);
    input.extend(op(WorkerOp::QueryPathInfo));
    s(&mut input, B_OUT);
    input.extend(op(WorkerOp::QueryPathInfo));
    s(&mut input, "/not/a/store/path");
    let mut r = session.run(input).await;
    r.last();
    assert_eq!(r.u64(), 1);
    assert_eq!(r.string(), "");
    assert_eq!(r.string(), "ab".repeat(32));
    assert_eq!(r.strings(), vec![A_OUT.to_string(), B_OUT.to_string()]);
    assert_eq!(r.u64(), 5);
    assert_eq!(r.u64(), 9);
    assert_eq!(r.u64(), 0);
    assert_eq!(r.strings(), vec!["k:1".to_string(), "k:2".to_string()]);
    assert_eq!(r.string(), "fixed:sha256:abc");
    r.last();
    assert_eq!(r.u64(), 0);
    assert_eq!(r.error(), "path '/not/a/store/path' is not in the Nix store");
    r.end();
}

#[tokio::test]
async fn substitutable_queries_are_empty_without_substituters() {
    let session = Session::new(TestStore::default());
    let mut input = op(WorkerOp::QuerySubstitutablePaths);
    list(&mut input, &[A_OUT]);
    input.extend(op(WorkerOp::QuerySubstitutablePathInfo));
    s(&mut input, A_OUT);
    input.extend(op(WorkerOp::QuerySubstitutablePathInfos));
    n(&mut input, 1);
    s(&mut input, A_OUT);
    s(&mut input, "");
    input.extend(op(WorkerOp::HasSubstitutes));
    s(&mut input, A_OUT);
    let mut r = session.run(input).await;
    r.last();
    assert!(r.strings().is_empty());
    r.last();
    assert_eq!(r.u64(), 0);
    r.last();
    assert_eq!(r.u64(), 0);
    r.last();
    assert_eq!(r.u64(), 0);
    r.end();
}

struct OneNarinfo(String);

#[async_trait::async_trait]
impl sui_store::HttpClient for OneNarinfo {
    async fn get(&self, url: &str, _headers: &[(&str, &str)]) -> Result<sui_store::HttpResponse, sui_store::HttpError> {
        if url.ends_with("/19k94y693b5mi3rbb0f3y9c4zq1jc3z0.narinfo") {
            return Ok(sui_store::HttpResponse { status: 200, body: self.0.clone() });
        }
        Ok(sui_store::HttpResponse { status: 404, body: String::new() })
    }

    async fn get_bytes(&self, _url: &str) -> Result<Vec<u8>, sui_store::HttpError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn substitutable_queries_read_the_configured_caches() {
    let narinfo = format!(
        "StorePath: {A_OUT}\nURL: nar/x.nar\nCompression: none\nFileHash: sha256:{h}\nFileSize: 11\nNarHash: sha256:{h}\nNarSize: 22\nReferences: 49dc9j6aviq2v2xjswzwzbhvwcc6693b-b\nDeriver: iyb4kvbj4ldvky1ia33wv05fsk49xcil-a.drv\n",
        h = "1".repeat(52)
    );
    let cache = sui_store::BinaryCacheStore::builder("https://cache.test")
        .http_client(Box::new(OneNarinfo(narinfo)))
        .require_signatures(false)
        .build();
    let mut session = Session::new(TestStore::default().with(B_OUT));
    session.caches = vec![Arc::new(cache)];
    let mut input = op(WorkerOp::QuerySubstitutablePathInfos);
    n(&mut input, 2);
    for p in [A_OUT, C_OUT] {
        s(&mut input, p);
        s(&mut input, "");
    }
    input.extend(op(WorkerOp::QueryMissing));
    list(&mut input, &[A_OUT]);
    let mut r = session.run(input).await;
    r.last();
    assert_eq!(r.u64(), 1);
    assert_eq!(r.string(), A_OUT);
    assert_eq!(r.string(), A_DRV);
    assert_eq!(r.strings(), vec![B_OUT.to_string()]);
    assert_eq!(r.u64(), 11);
    assert_eq!(r.u64(), 22);
    r.last();
    assert!(r.strings().is_empty());
    assert_eq!(r.strings(), vec![A_OUT.to_string()]);
    assert!(r.strings().is_empty());
    assert_eq!(r.u64(), 11);
    assert_eq!(r.u64(), 22);
    r.end();
}

#[tokio::test]
async fn add_signatures_merges_and_refuses_an_invalid_path() {
    let session = Session::new(TestStore::default().with(A_OUT));
    let mut input = op(WorkerOp::AddSignatures);
    s(&mut input, A_OUT);
    list(&mut input, &["k:1"]);
    input.extend(op(WorkerOp::AddSignatures));
    s(&mut input, B_OUT);
    list(&mut input, &["k:1"]);
    let mut r = session.run(input).await;
    r.last();
    assert_eq!(r.u64(), 1);
    assert!(r.error().contains("is not valid"));
    r.end();
    assert_eq!(session.store.paths.lock().unwrap()[A_OUT].signatures, vec!["k:1"]);
}

#[tokio::test]
async fn roots_are_registered_and_found_as_cppnix_lays_them_out() {
    let state = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let result = home.path().join("result");
    let session = Session::new(TestStore::default().with(A_OUT).with(B_OUT)).dirs(state.path(), state.path());
    let mut input = op(WorkerOp::AddPermRoot);
    s(&mut input, A_OUT);
    s(&mut input, result.to_str().unwrap());
    input.extend(op(WorkerOp::SyncWithGC));
    input.extend(op(WorkerOp::AddTempRoot));
    s(&mut input, A_OUT);
    input.extend(op(WorkerOp::FindRoots));
    let mut r = session.run(input).await;
    r.last();
    assert_eq!(r.string(), result.to_str().unwrap());
    r.last();
    assert_eq!(r.u64(), 1);
    r.last();
    assert_eq!(r.u64(), 1);
    r.last();
    assert_eq!(r.u64(), 2);
    assert_eq!(r.string(), result.to_str().unwrap());
    assert_eq!(r.string(), A_OUT);
    assert_eq!(r.string(), format!("{{temp:{}}}", std::process::id()));
    assert_eq!(r.string(), A_OUT);
    r.end();
    assert_eq!(std::fs::read_link(&result).unwrap().to_str().unwrap(), A_OUT);
    let digest = NixHash::of(HashAlgorithm::Sha1, result.to_str().unwrap().as_bytes()).to_nix32();
    assert_eq!(std::fs::read_link(state.path().join("gcroots/auto").join(digest)).unwrap(), result);

    let mut input = op(WorkerOp::AddPermRoot);
    s(&mut input, A_OUT);
    s(&mut input, result.to_str().unwrap());
    let mut r = session.clone_untrusted().run(input).await;
    assert!(r.error().contains("not privileged to create perm roots"));
    r.end();
}

#[tokio::test]
async fn optimise_and_verify_answer() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(A_OUT.rsplit('/').next().unwrap()), b"").unwrap();
    let session = Session::new(TestStore::default().with(A_OUT)).dirs(dir.path(), dir.path());
    let mut input = op(WorkerOp::OptimiseStore);
    for (contents, repair) in [(0, 0), (1, 0), (0, 1)] {
        input.extend(op(WorkerOp::VerifyStore));
        n(&mut input, contents);
        n(&mut input, repair);
    }
    let mut r = session.run(input).await;
    r.last();
    assert_eq!(r.u64(), 1);
    r.last();
    assert_eq!(r.u64(), 0);
    r.last();
    assert_eq!(r.u64(), 0);
    assert!(r.error().contains("does not repair"));
    r.end();

    let corrupt = Session::new(TestStore { corrupt: true, ..TestStore::default().with(A_OUT).with(B_OUT) }).dirs(dir.path(), dir.path());
    let mut input = op(WorkerOp::VerifyStore);
    n(&mut input, 1);
    n(&mut input, 0);
    input.extend(op(WorkerOp::VerifyStore));
    n(&mut input, 0);
    n(&mut input, 0);
    let mut r = corrupt.run(input).await;
    r.last();
    assert_eq!(r.u64(), 1);
    r.last();
    assert_eq!(r.u64(), 1);
    r.end();
}

#[tokio::test]
async fn add_build_log_writes_where_nix_log_reads_and_is_trusted_only() {
    let state = tempfile::tempdir().unwrap();
    let nix_state = state.path().join("nix");
    let session = Session::new(TestStore::default()).dirs(state.path(), &nix_state);
    let base = A_DRV.rsplit('/').next().unwrap();
    let mut input = op(WorkerOp::AddBuildLog);
    s(&mut input, base);
    framed(&mut input, b"log line\n");
    let mut r = session.run(input.clone()).await;
    r.last();
    assert_eq!(r.u64(), 1);
    r.end();
    assert_eq!(std::fs::read(state.path().join("log/nix/drvs").join(&base[..2]).join(&base[2..])).unwrap(), b"log line\n");
    let mut r = session.clone_untrusted().run(input).await;
    assert!(r.error().contains("not privileged to add logs"));
    r.end();
}

#[tokio::test]
async fn removed_and_unknown_operations_close_the_connection() {
    for (code, message) in [(24u64, "Removed operation 24"), (25, "Removed operation 25"), (2, "invalid operation 2"), (999, "invalid operation 999")] {
        let mut input = Vec::new();
        n(&mut input, code);
        is_valid(&mut input, A_OUT);
        let mut r = Session::new(TestStore::default()).run(input).await;
        assert_eq!(r.error(), message);
        r.end();
    }
}

#[tokio::test]
async fn a_store_error_is_an_error_frame_not_a_dropped_connection() {
    struct Broken;
    #[async_trait::async_trait]
    impl Store for Broken {
        async fn query_path_info(&self, _: &StorePath) -> StoreResult<Option<PathInfo>> {
            Err(StoreError::Database("down".into()))
        }
        async fn is_valid_path(&self, _: &StorePath) -> StoreResult<bool> {
            Err(StoreError::Database("down".into()))
        }
        async fn query_all_valid_paths(&self) -> StoreResult<Vec<StorePath>> {
            Err(StoreError::Database("down".into()))
        }
    }
    let mut input = Vec::new();
    is_valid(&mut input, A_OUT);
    input.extend(op(WorkerOp::QueryAllValidPaths));
    let mut conn = Connection::new(Arc::new(Broken), Cursor::new(input), Vec::<u8>::new(), TrustLevel::Trusted);
    conn.client_version = PROTOCOL_VERSION;
    conn.run().await.unwrap();
    let mut r = Reply(Cursor::new(conn.writer));
    assert_eq!(r.error(), "database error: down");
    assert_eq!(r.error(), "database error: down");
    r.end();
    assert!(matches!(ConnectionError::Op("x".into()), ConnectionError::Op(_)));
}

struct FakeBuild {
    store: Arc<TestStore>,
    emitted: Arc<Mutex<Vec<ActivityEvent>>>,
}

struct Tee(SharedSink, Arc<Mutex<Vec<ActivityEvent>>>);

impl ActivitySink for Tee {
    fn emit(&self, event: ActivityEvent) {
        self.1.lock().unwrap().push(event.clone());
        self.0.emit(event);
    }
}

impl super::DrvRealizer for FakeBuild {
    fn realize<'a>(
        &'a self,
        drv_path: &'a str,
        sink: SharedSink,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async move {
            let sink: SharedSink = Arc::new(Tee(sink, self.emitted.clone()));
            let worker = Worker::start(&sink);
            worker.expect_builds(1);
            let build = Activity::start(
                &sink,
                Verbosity::Info,
                ActivityType::Build,
                format!("building '{drv_path}'"),
                vec![drv_path.into(), "".into(), 1.into(), 1.into()],
                0,
            );
            worker.build_started();
            let mut log = BuilderLog::new(&build, false);
            log.feed(b"unpacking sources\n@nix {\"action\":\"setPhase\",\"phase\":\"buildPhase\"}\n50%\r100%\n");
            log.finish();
            drop(log);
            sink.emit(ActivityEvent::Log { level: Verbosity::Error, text: "a daemon message".into() });
            self.store.add(A_OUT);
            worker.build_finished(true);
            drop(build);
            drop(worker);
            Ok(vec![A_OUT.to_string()])
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_build_streams_its_activities_to_the_client_as_the_same_typed_events() {
    let dir = drv_dir();
    let store = Arc::new(TestStore::default().with_drvs());
    let emitted = Arc::new(Mutex::new(Vec::new()));
    let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
    server.set_nonblocking(true).unwrap();
    let (reader, writer) = tokio::net::UnixStream::from_std(server).unwrap().into_split();
    let realizer = Arc::new(FakeBuild { store: store.clone(), emitted: emitted.clone() });
    let daemon = tokio::spawn(async move {
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted)
            .with_dirs(dir.path(), dir.path())
            .with_realizer(realizer);
        conn.handshake().await.unwrap();
        conn.run().await.unwrap();
    });
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink: SharedSink = received.clone();
    tokio::task::spawn_blocking(move || {
        let options = sui_store::worker_client::ClientOptions { max_jobs: 1, max_silent_time: 0, build_cores: 0 };
        let mut conn = sui_store::worker_client::WorkerConn::over(client, options).unwrap().with_sink(sink);
        conn.build_paths(&[A_DRV]).unwrap();
        assert!(conn.is_valid_path(A_OUT).unwrap());
    })
    .await
    .unwrap();
    daemon.await.unwrap();
    let emitted = emitted.lock().unwrap().clone();
    let received = received.lock().unwrap().clone();
    assert!(emitted.len() > 10, "{emitted:?}");
    assert_eq!(received, emitted);
    let build = emitted
        .iter()
        .find_map(|e| match e {
            ActivityEvent::Start { id, activity: ActivityType::Build, .. } => Some(*id),
            _ => None,
        })
        .unwrap();
    let on_build: Vec<&ActivityEvent> =
        received.iter().filter(|e| matches!(e, ActivityEvent::Result { id, .. } if *id == build)).collect();
    assert_eq!(
        on_build,
        vec![
            &ActivityEvent::Result { id: build, result: ResultType::BuildLogLine, fields: vec!["unpacking sources".into()] },
            &ActivityEvent::Result { id: build, result: ResultType::SetPhase, fields: vec!["buildPhase".into()] },
            &ActivityEvent::Result { id: build, result: ResultType::BuildLogLine, fields: vec!["100%".into()] },
        ]
    );
}

struct WaitsForTheClient {
    store: Arc<TestStore>,
    seen: Arc<std::sync::atomic::AtomicBool>,
}

impl super::DrvRealizer for WaitsForTheClient {
    fn realize<'a>(
        &'a self,
        _drv_path: &'a str,
        sink: SharedSink,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async move {
            sink.emit(ActivityEvent::Log { level: Verbosity::Error, text: "ping".into() });
            for _ in 0..200 {
                if self.seen.load(std::sync::atomic::Ordering::SeqCst) {
                    self.store.add(A_OUT);
                    return Ok(vec![A_OUT.to_string()]);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err("the client never saw the event while the build ran".to_string())
        })
    }
}

struct SetsFlag(Arc<std::sync::atomic::AtomicBool>);

impl ActivitySink for SetsFlag {
    fn emit(&self, _event: ActivityEvent) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_client_receives_a_build_event_while_the_build_still_runs() {
    let dir = drv_dir();
    let store = Arc::new(TestStore::default().with_drvs());
    let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
    server.set_nonblocking(true).unwrap();
    let (reader, writer) = tokio::net::UnixStream::from_std(server).unwrap().into_split();
    let realizer = Arc::new(WaitsForTheClient { store: store.clone(), seen: seen.clone() });
    let daemon = tokio::spawn(async move {
        let mut conn = Connection::new(store, reader, writer, TrustLevel::Trusted)
            .with_dirs(dir.path(), dir.path())
            .with_realizer(realizer);
        conn.handshake().await.unwrap();
        conn.run().await.unwrap();
    });
    let built = tokio::task::spawn_blocking(move || {
        let options = sui_store::worker_client::ClientOptions { max_jobs: 1, max_silent_time: 0, build_cores: 0 };
        let mut conn = sui_store::worker_client::WorkerConn::over(client, options)
            .unwrap()
            .with_sink(Arc::new(SetsFlag(seen)));
        conn.build_paths(&[A_DRV])
    })
    .await
    .unwrap();
    daemon.await.unwrap();
    built.unwrap();
}

fn gc_op(input: &mut Vec<u8>, action: u64, paths: &[&str], max_freed: u64) {
    input.extend(op(WorkerOp::CollectGarbage));
    n(input, action);
    list(input, paths);
    n(input, 0);
    n(input, max_freed);
    for _ in 0..3 {
        n(input, 0);
    }
}

fn gc_reply(r: &mut Reply) -> (Vec<String>, u64) {
    r.last();
    let count = r.u64();
    let paths = (0..count).map(|_| r.string()).collect();
    let freed = r.u64();
    assert_eq!(r.u64(), 0);
    (paths, freed)
}

#[tokio::test]
async fn a_temp_root_keeps_a_path_alive_until_its_connection_closes_and_every_gc_action_answers() {
    let state = tempfile::tempdir().unwrap();
    let session = Session::new(TestStore::default().with(A_OUT).with(B_OUT)).dirs(state.path(), state.path());
    let mut input = op(WorkerOp::AddTempRoot);
    s(&mut input, A_OUT);
    gc_op(&mut input, 0, &[], 0);
    gc_op(&mut input, 1, &[], 0);
    gc_op(&mut input, 3, &[A_OUT], 0);
    gc_op(&mut input, 2, &[], 0);
    gc_op(&mut input, 7, &[], 0);
    let mut r = session.run(input).await;
    r.last();
    assert_eq!(r.u64(), 1);
    assert_eq!(gc_reply(&mut r), (vec![A_OUT.to_string()], 0));
    assert_eq!(gc_reply(&mut r), (vec![B_OUT.to_string()], 0));
    assert!(r.error().contains("still alive"));
    assert_eq!(gc_reply(&mut r).0, vec![B_OUT.to_string()]);
    assert!(r.error().contains("unknown garbage collector action 7"));
    r.end();

    let mut input = Vec::new();
    gc_op(&mut input, 3, &[A_OUT], 0);
    let mut r = session.run(input).await;
    assert_eq!(gc_reply(&mut r).0, vec![A_OUT.to_string()], "the closed connection's temp root was released");
    r.end();

    let mut input = Vec::new();
    gc_op(&mut input, 2, &[], 0);
    input.extend(op(WorkerOp::CollectGarbage));
    n(&mut input, 1);
    list(&mut input, &[]);
    n(&mut input, 1);
    n(&mut input, 0);
    for _ in 0..3 {
        n(&mut input, 0);
    }
    let mut r = session.run(input).await;
    assert_eq!(gc_reply(&mut r), (Vec::new(), 0));
    assert!(r.error().contains("not allowed to ignore liveness"));
    r.end();
}

#[tokio::test]
async fn delete_dead_stops_once_max_freed_is_reached() {
    let state = tempfile::tempdir().unwrap();
    let session = Session::new(TestStore::default().with(A_OUT).with(B_OUT)).dirs(state.path(), state.path());
    let mut input = Vec::new();
    gc_op(&mut input, 2, &[], 1);
    gc_op(&mut input, 2, &[], 5);
    let mut r = session.run(input).await;
    let (first, freed) = gc_reply(&mut r);
    assert_eq!((first.len(), freed), (1, 1));
    let (second, freed) = gc_reply(&mut r);
    assert_eq!((second.len(), freed), (1, 1));
    assert_ne!(first, second);
    r.end();
}

async fn serve_session(store: Arc<TestStore>, drvs: Option<Arc<dyn super::DrvRealizer>>, write: bool, commands: Vec<u8>) -> (Reply, Result<(), ConnectionError>) {
    let mut input = Vec::new();
    n(&mut input, wire::SERVE_MAGIC_1);
    n(&mut input, wire::SERVE_PROTOCOL_VERSION);
    input.extend(commands);
    let state = tempfile::tempdir().unwrap();
    let mut conn = Connection::new(store, Cursor::new(input), Vec::<u8>::new(), TrustLevel::Trusted).with_dirs("/nix/store", state.path());
    if let Some(d) = drvs {
        conn = conn.with_realizer(d);
    }
    let result = conn.serve(write).await;
    let mut r = Reply(Cursor::new(conn.writer));
    assert_eq!(r.u64(), wire::SERVE_MAGIC_2);
    assert_eq!(r.u64(), wire::SERVE_PROTOCOL_VERSION);
    (r, result)
}

fn serve_cmd(cmd: wire::ServeCommand) -> Vec<u8> {
    let mut buf = Vec::new();
    n(&mut buf, cmd as u64);
    buf
}

fn build_options(buf: &mut Vec<u8>) {
    for v in [0, 0, 0, 0, 0, 0] {
        n(buf, v);
    }
}

#[tokio::test]
async fn nix_store_serve_answers_queries_as_cppnix_legacy_ssh_clients_read_them() {
    let store = Arc::new(TestStore::default().with(A_OUT));
    let mut cmds = serve_cmd(wire::ServeCommand::QueryValidPaths);
    n(&mut cmds, 0);
    n(&mut cmds, 0);
    list(&mut cmds, &[A_OUT, B_OUT]);
    cmds.extend(serve_cmd(wire::ServeCommand::QueryPathInfos));
    list(&mut cmds, &[A_OUT, B_OUT]);
    cmds.extend(serve_cmd(wire::ServeCommand::QueryClosure));
    n(&mut cmds, 0);
    list(&mut cmds, &[A_OUT]);
    let (mut r, result) = serve_session(store, None, false, cmds).await;
    result.unwrap();
    assert_eq!(r.strings(), vec![A_OUT.to_string()]);
    assert_eq!(r.string(), A_OUT);
    assert_eq!(r.string(), "");
    assert!(r.strings().is_empty());
    assert_eq!(r.u64(), 8);
    assert_eq!(r.u64(), 8);
    let hash = NixHash::of(HashAlgorithm::Sha256, A_OUT.as_bytes());
    assert_eq!(r.string(), format!("sha256:{}", hash.to_nix32()));
    assert_eq!(r.string(), "");
    assert!(r.strings().is_empty());
    assert_eq!(r.string(), "");
    assert_eq!(r.strings(), vec![A_OUT.to_string()]);
    r.end();
}

#[tokio::test]
async fn nix_store_serve_refuses_writes_without_write_and_builds_a_sent_derivation_with_it() {
    let store = Arc::new(TestStore::default().with(A_OUT));
    let mut cmd = serve_cmd(wire::ServeCommand::BuildDerivation);
    let request = build_derivation_request(C_DRV, C_TEXT);
    cmd.extend_from_slice(&request[8..request.len() - 8]);
    build_options(&mut cmd);

    let (mut r, result) = serve_session(store.clone(), None, false, cmd.clone()).await;
    assert!(result.unwrap_err().to_string().contains("building paths is not allowed"));
    r.end();

    let realizer = Arc::new(BuildsSent { store: store.clone(), got: Mutex::new(Vec::new()) });
    let (mut r, result) = serve_session(store.clone(), Some(realizer.clone()), true, cmd).await;
    result.unwrap();
    assert_eq!(r.u64(), 0, "Built");
    assert_eq!(r.string(), "");
    assert_eq!(r.u64(), 1);
    r.u64();
    r.u64();
    r.u64();
    assert_eq!(r.u64(), 1);
    assert!(r.string().ends_with("!out"));
    r.string();
    r.end();
    assert!(store.has(C_OUT));
    assert_eq!(realizer.got.lock().unwrap()[0].0, C_DRV);
}

#[tokio::test]
async fn nix_store_serve_imports_a_nar_with_add_to_store_nar() {
    let store = Arc::new(TestStore::default());
    let nar = file_nar(b"served");
    let path = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-served";
    let mut cmd = serve_cmd(wire::ServeCommand::AddToStoreNar);
    s(&mut cmd, path);
    s(&mut cmd, "");
    s(&mut cmd, &format!("sha256:{}", NixHash::of(HashAlgorithm::Sha256, &nar).to_nix32()));
    list(&mut cmd, &[]);
    n(&mut cmd, 0);
    n(&mut cmd, nar.len() as u64);
    n(&mut cmd, 0);
    list(&mut cmd, &[]);
    s(&mut cmd, "");
    cmd.extend_from_slice(&nar);
    let (mut r, result) = serve_session(store.clone(), None, true, cmd).await;
    result.unwrap();
    assert_eq!(r.u64(), 1);
    r.end();
    assert_eq!(store.nars.lock().unwrap()[path], nar);
}
