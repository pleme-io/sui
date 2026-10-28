use assert_cmd::Command;

fn system() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => "aarch64-darwin",
        ("x86_64", "macos") => "x86_64-darwin",
        ("aarch64", _) => "aarch64-linux",
        _ => "x86_64-linux",
    }
}

#[test]
fn fmt_runs_the_flake_formatter_with_the_arguments_and_prj_root_from_a_subdirectory() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let out = root.join("formatter-out");
    std::fs::create_dir_all(out.join("bin")).unwrap();
    let script = out.join("bin/treefmt");
    std::fs::write(&script, "#!/bin/sh\necho \"ARGS:$* ROOT:$PRJ_ROOT\"\n").unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::write(
        root.join("flake.nix"),
        format!(
            r#"{{ outputs = _: {{ formatter.{} = {{ type = "derivation"; name = "treefmt-1.0"; outPath = "{}"; }}; }}; }}"#,
            system(),
            out.display()
        ),
    )
    .unwrap();
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let run = Command::cargo_bin("sui").unwrap().current_dir(&sub).args(["fmt", "a.nix", "--", "--check"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), format!("ARGS:a.nix --check ROOT:{}", root.display()));
    assert!(stderr.contains(&format!("path \"{}\" does not contain a 'flake.nix', searching up", sub.display())), "{stderr}");
}

#[test]
fn fmt_without_a_formatter_names_the_missing_attribute() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("flake.nix"), "{ outputs = _: { }; }").unwrap();
    let run = Command::cargo_bin("sui").unwrap().current_dir(tmp.path()).arg("fmt").output().unwrap();
    assert!(!run.status.success());
    assert!(String::from_utf8_lossy(&run.stderr).contains(&format!("does not provide attribute 'formatter.{}'", system())));
}
