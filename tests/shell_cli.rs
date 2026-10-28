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
fn shell_puts_each_installable_and_its_propagated_packages_first_on_path_and_runs_the_command() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (a, b, dep) = (root.join("a"), root.join("b"), root.join("dep"));
    for d in [&a, &b, &dep] {
        std::fs::create_dir_all(d.join("bin")).unwrap();
    }
    std::fs::create_dir_all(a.join("nix-support")).unwrap();
    std::fs::write(a.join("nix-support/propagated-user-env-packages"), format!("{}\n", dep.display())).unwrap();
    std::fs::write(
        root.join("flake.nix"),
        format!(
            r#"{{ outputs = _: {{ packages.{sys}.a = {{ type = "derivation"; name = "a"; outPath = "{a}"; }}; packages.{sys}.b = {{ type = "derivation"; name = "b"; outPath = "{b}"; }}; }}; }}"#,
            sys = system(),
            a = a.display(),
            b = b.display()
        ),
    )
    .unwrap();
    for sub in [&["shell"][..], &["env", "shell"][..]] {
        let run = Command::cargo_bin("sui")
            .unwrap()
            .current_dir(&root)
            .args(sub)
            .args([".#a", ".#b", "-c", "/bin/sh", "-c", "echo $PATH"])
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(
            String::from_utf8_lossy(&run.stdout).trim(),
            format!("{}/bin:{}/bin:{}/bin:/usr/bin:/bin", a.display(), b.display(), dep.display())
        );
    }
}
