use assert_cmd::Command;

fn nix_instantiate(dir: &std::path::Path) -> std::path::PathBuf {
    let link = dir.join("nix-instantiate");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_sui"), &link).unwrap();
    link
}

#[test]
fn nix_instantiate_parse_prints_the_parsed_expression_and_fails_on_a_bad_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let bin = nix_instantiate(&dir);
    std::fs::write(dir.join("f.nix"), "{ a.b = 1; c = ./x; d = x: x - 1; }").unwrap();
    let run = Command::new(&bin).current_dir(&dir).args(["--parse", "f.nix"]).output().unwrap();
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(
        String::from_utf8_lossy(&run.stdout),
        format!("{{ a = {{ b = 1; }}; c = {}/x; d = (x: (__sub x 1)); }}\n", dir.display())
    );
    let run = Command::new(&bin).current_dir(&dir).args(["--parse", "-E", "let x = ;"]).output().unwrap();
    assert_eq!(run.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&run.stderr).starts_with("error: syntax error"));
    let run = Command::new(&bin).current_dir(&dir).args(["--parse", "-E", "a == b"]).output().unwrap();
    assert_eq!(run.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&run.stderr).starts_with("error: undefined variable 'a'"));
}
