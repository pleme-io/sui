use assert_cmd::Command;

#[test]
fn log_prints_the_stored_build_log_of_a_derivation_and_names_a_missing_one() {
    let tmp = tempfile::tempdir().unwrap();
    let drv = "/nix/store/0123456789abcdfghijklmnpqrsvwxa-ok.drv";
    sui_build::build_log::write(tmp.path(), drv, b"ok - test 1\nnot ok - test 2\n").unwrap();
    let run = Command::cargo_bin("sui").unwrap().env("NIX_LOG_DIR", tmp.path()).args(["log", drv]).output().unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(run.stdout, b"ok - test 1\nnot ok - test 2\n");
    let missing = "/nix/store/0123456789abcdfghijklmnpqrsvwxb-none.drv";
    let run = Command::cargo_bin("sui").unwrap().env("NIX_LOG_DIR", tmp.path()).args(["log", missing]).output().unwrap();
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains(&format!("build log of '{missing}' is not available")));
}
