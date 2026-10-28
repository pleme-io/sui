use std::collections::BTreeSet;

use sui_compat::hash::{HashAlgorithm, NixHash};
use sui_store::ssh_store::{invocation, spawn, RemoteConn, SshOptions};
use sui_store::worker_client::{ClientOptions, RemotePathInfo};

const PATH: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-sui-offload-probe";

fn cppnix(tool: &str) -> Option<std::path::PathBuf> {
    let found = sui_compat::cppnix::locate(tool);
    if found.is_none() {
        eprintln!("skipping: no cppnix {tool} (set SUI_CPPNIX_BIN_DIR)");
    }
    found
}

fn open(scheme: &str, program: &std::path::Path, root: &std::path::Path) -> RemoteConn {
    let none = |_: &std::path::Path| -> std::io::Result<String> { Err(std::io::ErrorKind::NotFound.into()) };
    let line = format!("{scheme}://localhost?remote-program={}&remote-store={}", program.display(), root.display());
    let machine = sui_compat::machines::parse_config(&BTreeSet::new(), &line, &none).unwrap().remove(0);
    let opts = SshOptions { nix_sshopts: vec![], connect_timeout: None };
    let inv = invocation(&machine, &opts, root);
    assert!(inv.fake);
    let (pipes, child) = spawn(&inv).unwrap();
    let conn = RemoteConn::over(&machine.store_uri.scheme, pipes, ClientOptions { max_jobs: 1, max_silent_time: 0, build_cores: 0 }, "localhost");
    match conn {
        Ok(c) => {
            std::mem::forget(child);
            c
        }
        Err(e) => {
            std::thread::sleep(std::time::Duration::from_millis(200));
            panic!("{e}: {}", child.stderr_text());
        }
    }
}

fn probe_nar(dir: &std::path::Path) -> Vec<u8> {
    let file = dir.join("probe");
    std::fs::write(&file, b"copied by sui\n").unwrap();
    let mut nar = Vec::new();
    sui_compat::nar::NarWriter::write_path(&mut nar, &file).unwrap();
    nar
}

fn round_trip(scheme: &str, tool: &str) {
    let Some(program) = cppnix(tool) else { return };
    let root = tempfile::tempdir().unwrap();
    let real_root = std::fs::canonicalize(root.path()).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let nar = probe_nar(scratch.path());
    let mut conn = open(scheme, &program, &real_root);
    assert!(conn.query_valid_paths(&[PATH.to_string()], false).unwrap().is_empty());
    let info = RemotePathInfo {
        path: PATH.to_string(),
        deriver: String::new(),
        nar_hash: format!("sha256:{}", NixHash::of(HashAlgorithm::Sha256, &nar).to_nix32()),
        references: vec![],
        registration_time: 0,
        nar_size: nar.len() as u64,
        ultimate: false,
        sigs: vec![],
        ca: String::new(),
    };
    conn.add_paths(1, |_| Ok((info.clone(), nar.clone()))).unwrap();
    assert_eq!(conn.query_valid_paths(&[PATH.to_string()], false).unwrap(), [PATH.to_string()]);
    let got = conn.query_path_info(PATH).unwrap().expect("cppnix lists the copied path");
    assert_eq!(got.nar_size, nar.len() as u64);
    let declared = NixHash::parse_any(HashAlgorithm::Sha256, &got.nar_hash).unwrap();
    assert_eq!(declared, NixHash::of(HashAlgorithm::Sha256, &nar));
    assert_eq!(conn.nar_from_path(PATH).unwrap(), nar);
}

#[test]
fn ssh_ng_copies_a_path_to_a_cppnix_daemon_and_back() {
    round_trip("ssh-ng", "nix-daemon");
}

#[test]
fn ssh_copies_a_path_to_cppnix_nix_store_serve_and_back() {
    round_trip("ssh", "nix-store");
}
