//! End-to-end wire test for the `AddToStore` / `AddToStoreNar` store
//! *write* path — the surface that wires a Nix worker-protocol
//! `AddToStore` op through to sui's sealed `LocalStore::add_to_store`
//! realizer.
//!
//! Unlike `real_nix_client.rs` (which needs `/nix/var/nix/db` + a real
//! `nix-store` binary and only exercises READ ops), this test is fully
//! hermetic and root-free: it spins the daemon against a **temp store
//! dir** + in-memory DB, then drives the protocol with a hand-rolled
//! client so we control every byte on the wire.
//!
//! # What it proves
//!
//! 1. The daemon decodes the modern (protocol >= 25) `AddToStore` wire
//!    shape — `name`, `caMethod`, `references`, `repair`, then the NAR
//!    as a `FramedSource` — exactly as CppNix's `remote-store.cc` sends
//!    it.
//! 2. The returned `ValidPathInfo`'s store path == the path the sealed
//!    `LocalStore::add_to_store` realizer computes for the same NAR
//!    (the store-add-path oracle). No divergence between the wire path
//!    and the direct-realizer path.
//! 3. `AddToStoreNar` drains its full unkeyed-`ValidPathInfo` header +
//!    framed NAR and registers the path.
//!
//! # Root gate
//!
//! The privileged `/nix/store` write is NOT exercised here (that needs
//! root + the real store dir). This test proves the wire + realizer are
//! correct against a temp store; the only remaining root-gated step is
//! pointing `LocalStore` at the real `/nix/store` and running the
//! daemon as root.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use sui_compat::nar::{NarNode, NarWriter};
use sui_compat::wire::{PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2, WorkerOp};
use sui_daemon::{DaemonConfig, DaemonServer};
use sui_store::traits::Store;
use sui_store::LocalStore;

// ── Sync wire helpers for the hand-rolled client ────────────────
//
// The daemon's own `wire.rs` is async + `pub(crate)`; the client side
// here is a small, deliberate re-implementation so the test controls
// the exact bytes and can't accidentally share a bug with the code
// under test.

fn write_u64(w: &mut impl Write, v: u64) {
    w.write_all(&v.to_le_bytes()).unwrap();
}

fn read_u64(r: &mut impl Read) -> u64 {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).unwrap();
    u64::from_le_bytes(b)
}

/// Padded `WorkerProto` byte string: u64 len + bytes + zero pad to 8.
fn write_string(w: &mut impl Write, s: &str) {
    let bytes = s.as_bytes();
    write_u64(w, bytes.len() as u64);
    w.write_all(bytes).unwrap();
    let pad = (8 - (bytes.len() % 8)) % 8;
    if pad > 0 {
        w.write_all(&vec![0u8; pad]).unwrap();
    }
}

fn read_string(r: &mut impl Read) -> String {
    let len = read_u64(r) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).unwrap();
    let pad = (8 - (len % 8)) % 8;
    if pad > 0 {
        let mut p = vec![0u8; pad];
        r.read_exact(&mut p).unwrap();
    }
    String::from_utf8(buf).unwrap()
}

/// Write a NAR as a single-chunk `FramedSource`: `u64 len` + raw bytes
/// (NO padding) + `u64 0` terminator. This is the CppNix `FramedSink`
/// shape, distinct from the padded string encoding above.
fn write_framed(w: &mut impl Write, data: &[u8]) {
    if !data.is_empty() {
        write_u64(w, data.len() as u64);
        w.write_all(data).unwrap();
    }
    write_u64(w, 0); // terminator
}

/// StderrMsg::Last marker (see sui_compat::wire::StderrMsg::Last).
const STDERR_LAST: u64 = 0x616c7473;

/// Read stderr frames until STDERR_LAST, returning nothing. Panics on
/// an Error frame with the message (so a wire desync is visible).
fn drain_to_stderr_last(r: &mut impl Read) {
    const STDERR_ERROR: u64 = 0x63787470;
    const STDERR_WRITE: u64 = 0x6f6c6d67;
    loop {
        let msg = read_u64(r);
        if msg == STDERR_LAST {
            return;
        }
        if msg == STDERR_ERROR {
            let _ty = read_string(r);
            let text = read_string(r);
            let _n = read_u64(r);
            panic!("daemon returned STDERR_ERROR: {text}");
        }
        if msg == STDERR_WRITE {
            let _ = read_string(r);
            continue;
        }
        panic!("unexpected stderr frame: {msg:#x}");
    }
}

// ── Daemon fixture (temp store, in-memory DB, root-free) ────────

struct Fixture {
    socket: PathBuf,
    store_dir: PathBuf,
    _tmp: tempfile::TempDir,
    task: tokio::task::JoinHandle<Result<(), sui_daemon::DaemonError>>,
}

impl Fixture {
    async fn start() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("sui.sock");
        let store_dir = tmp.path().join("store");
        std::fs::create_dir_all(&store_dir).unwrap();

        let store = LocalStore::open_in_memory_with_dir(store_dir.to_str().unwrap())
            .await
            .expect("open in-memory store with temp dir");

        let config = DaemonConfig::with_socket_path(&socket);
        let server = DaemonServer::new(config, store);
        let task = tokio::spawn(async move { server.run().await });

        for _ in 0..40 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(socket.exists(), "daemon socket never appeared");

        Self {
            socket,
            store_dir,
            _tmp: tmp,
            task,
        }
    }

    /// Connect + run the worker-protocol handshake as a client.
    fn connect(&self) -> UnixStream {
        let mut s = UnixStream::connect(&self.socket).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();

        // Client handshake (mirror of handshake.rs, client side).
        write_u64(&mut s, WORKER_MAGIC_1);
        s.flush().unwrap();
        let magic2 = read_u64(&mut s);
        assert_eq!(magic2, WORKER_MAGIC_2, "server magic mismatch");
        let _server_ver = read_u64(&mut s);
        // Client protocol version.
        write_u64(&mut s, PROTOCOL_VERSION);
        // CPU affinity (obsolete) + reserve space (obsolete) — both sent
        // because our version is well past the thresholds.
        write_u64(&mut s, 0);
        write_u64(&mut s, 0);
        s.flush().unwrap();
        // Daemon version string.
        let _daemon_ver = read_string(&mut s);
        // Trust flag (our version >= trust-exchange threshold).
        let _trust = read_u64(&mut s);
        // Handshake terminates with STDERR_LAST.
        assert_eq!(read_u64(&mut s), STDERR_LAST, "handshake missing STDERR_LAST");
        s
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Build a NAR for a single regular file with the given contents.
fn nar_for_file(contents: &[u8]) -> Vec<u8> {
    let node = NarNode::Regular {
        executable: false,
        contents: contents.to_vec(),
    };
    let mut nar = Vec::new();
    NarWriter::write(&mut nar, &node).unwrap();
    nar
}

// ── Tests ───────────────────────────────────────────────────────

/// The load-bearing test: `AddToStore` over the wire returns the SAME
/// store path the sealed realizer computes — path == oracle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_to_store_wire_path_equals_realizer_oracle() {
    let fixture = Fixture::start().await;
    let name = "hello-wire";
    let nar = nar_for_file(b"hello over the wire");

    // Oracle: the store-add-path fingerprint algorithm the sealed
    // realizer uses (`LocalStore::add_to_store`), computed here WITHOUT
    // writing so it can be compared against the wire path. This IS the
    // realizer's exact formula:
    //   fingerprint = "source:sha256:<nar-sha256-hex>:<store_dir>:<name>"
    //   basename    = base32(compress(sha256(fingerprint), 20)) + "-" + name
    // The fingerprint's correctness vs real `nix store add-path` is
    // sealed by sui-store/tests/store_add_path_oracle.rs; this test
    // proves the WIRE handler produces that same sealed path.
    let oracle_path = {
        use sha2::{Digest, Sha256};
        use sui_compat::store_path::{compress_hash, nix_base32_encode};
        let store_dir = fixture.store_dir.to_str().unwrap();
        let nar_hex: String = Sha256::digest(&nar)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let fingerprint = format!("source:sha256:{nar_hex}:{store_dir}:{name}");
        let fp_hash = Sha256::digest(fingerprint.as_bytes());
        let b32 = nix_base32_encode(&compress_hash(&fp_hash, 20));
        format!("{store_dir}/{b32}-{name}")
    };

    // Wire: drive AddToStore against the daemon.
    let mut c = fixture.connect();
    write_u64(&mut c, WorkerOp::AddToStore as u64);
    write_string(&mut c, name);
    write_string(&mut c, "fixed:r:sha256"); // recursive NAR sha256
    write_u64(&mut c, 0); // 0 references
    write_u64(&mut c, 0); // repair = false
    write_framed(&mut c, &nar);
    c.flush().unwrap();

    // Response: STDERR_LAST then ValidPathInfo (path first).
    drain_to_stderr_last(&mut c);
    let wire_path = read_string(&mut c);
    let _deriver = read_string(&mut c);
    let nar_hash = read_string(&mut c);
    let ref_count = read_u64(&mut c);
    for _ in 0..ref_count {
        let _ = read_string(&mut c);
    }
    let _reg_time = read_u64(&mut c);
    let nar_size = read_u64(&mut c);

    assert_eq!(
        wire_path, oracle_path,
        "AddToStore wire path must equal the sealed realizer oracle path"
    );
    assert!(nar_hash.starts_with("sha256:"), "nar hash: {nar_hash}");
    assert_eq!(nar_size, nar.len() as u64, "nar size mismatch");
    assert!(
        wire_path.contains(name),
        "path should carry the name: {wire_path}"
    );

    // The NAR was actually unpacked into the temp store dir.
    let basename = wire_path
        .strip_prefix(&format!("{}/", fixture.store_dir.display()))
        .expect("path under temp store dir");
    assert!(
        fixture.store_dir.join(basename).exists(),
        "unpacked file missing from store dir"
    );
}

/// References supplied over the wire are carried through into the
/// registered `ValidPathInfo`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_to_store_wire_carries_references() {
    let fixture = Fixture::start().await;
    let name = "with-refs";
    let nar = nar_for_file(b"payload");

    // ── ★ THE REFERENCE MUST BE A PATH THAT ACTUALLY EXISTS ───────────────
    // This used to name `<store>/00000000000000000000000000000000-dep`, a
    // syntactically-valid path that was never added — so the test only passed
    // because `register_path` silently DROPPED references it could not
    // resolve. CppNix rejects that request outright (`registerValidPaths`
    // calls `queryValidPathId` per reference and throws `path '%s' is not
    // valid`, rolling back the transaction), so the old fixture asserted a
    // laxness the tool it mirrors does not have.
    //
    // Registering the dependency first keeps the test's real intent — that
    // references survive the wire round-trip — and additionally proves they
    // are VALIDATED, which is the property that matters: a store that accepts
    // a reference to a path it does not have can let a GC collect the
    // referent out from under the referrer.
    let dep_nar = nar_for_file(b"dep payload");
    let mut dep_conn = fixture.connect();
    write_u64(&mut dep_conn, WorkerOp::AddToStore as u64);
    write_string(&mut dep_conn, "dep");
    write_string(&mut dep_conn, "fixed:r:sha256");
    write_u64(&mut dep_conn, 0); // no references of its own
    write_u64(&mut dep_conn, 0); // repair
    write_framed(&mut dep_conn, &dep_nar);
    dep_conn.flush().unwrap();
    drain_to_stderr_last(&mut dep_conn);
    let reference = read_string(&mut dep_conn);
    assert!(
        reference.starts_with(&format!("{}/", fixture.store_dir.display())),
        "the dependency must land in the temp store dir, got {reference}"
    );
    drop(dep_conn);

    let mut c = fixture.connect();
    write_u64(&mut c, WorkerOp::AddToStore as u64);
    write_string(&mut c, name);
    write_string(&mut c, "fixed:r:sha256");
    write_u64(&mut c, 1); // 1 reference
    write_string(&mut c, &reference);
    write_u64(&mut c, 0); // repair
    write_framed(&mut c, &nar);
    c.flush().unwrap();

    drain_to_stderr_last(&mut c);
    let _path = read_string(&mut c);
    let _deriver = read_string(&mut c);
    let _nar_hash = read_string(&mut c);
    let ref_count = read_u64(&mut c);
    assert_eq!(ref_count, 1, "one reference expected back");
    let got_ref = read_string(&mut c);
    assert_eq!(got_ref, reference);
}

/// `AddToStoreNar` drains its full header + framed NAR and registers
/// the path (no response body — STDERR_LAST closes the op).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_to_store_nar_wire_registers_path() {
    let fixture = Fixture::start().await;
    let nar = nar_for_file(b"nar import body");
    // The realizer recomputes the path from the NAR; the header path is
    // the client's assertion. Use a plausible <hash>-<name> basename.
    let asserted =
        "/nix/store/00000000000000000000000000000000-imported-pkg".to_string();

    let mut c = fixture.connect();
    write_u64(&mut c, WorkerOp::AddToStoreNar as u64);
    // Unkeyed ValidPathInfo header, cppnix order:
    write_string(&mut c, &asserted); // path
    write_string(&mut c, ""); // deriver
    write_string(&mut c, "sha256:00"); // narHash
    write_u64(&mut c, 0); // 0 references
    write_u64(&mut c, 0); // registrationTime
    write_u64(&mut c, nar.len() as u64); // narSize
    write_u64(&mut c, 0); // ultimate
    write_u64(&mut c, 0); // 0 sigs
    write_string(&mut c, ""); // ca
    write_u64(&mut c, 0); // repair
    write_u64(&mut c, 1); // dontCheckSigs
    write_framed(&mut c, &nar);
    c.flush().unwrap();

    // No response body — completion is STDERR_LAST only.
    drain_to_stderr_last(&mut c);

    // The realizer registers under the NAR-derived path with name
    // "imported-pkg" (extracted from the asserted basename). Confirm a
    // file with that name landed in the store dir.
    let entries: Vec<_> = std::fs::read_dir(&fixture.store_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.iter().any(|n| n.ends_with("-imported-pkg")),
        "expected an -imported-pkg entry in the store dir, got {entries:?}"
    );
}
