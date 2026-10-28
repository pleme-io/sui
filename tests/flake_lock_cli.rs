use std::path::Path;

use assert_cmd::Command;

fn flake(dir: &Path, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("flake.nix"), body).unwrap();
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (a, b, root) = (base.join("a"), base.join("b"), base.join("root"));
    flake(&a, "{ outputs = _: { v = 1; }; }");
    flake(&b, "{ outputs = _: { v = 2; }; }");
    flake(
        &root,
        &format!(
            r#"{{ inputs.x.url = "path:{}"; outputs = {{ self, x }}: {{ v = x.v; src = "${{x}}"; }}; }}"#,
            a.display()
        ),
    );
    (tmp, a, b, root)
}

fn sui(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::cargo_bin("sui").unwrap().current_dir(dir).args(args).output().unwrap()
}

#[test]
fn flake_lock_creates_the_lock_file_a_flake_does_not_have() {
    let (_tmp, a, _b, root) = fixture();
    let out = sui(&root, &["flake", "lock"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(out.stdout.is_empty());
    assert!(stderr.contains(&format!("warning: creating lock file \"{}\": \n• Added input 'x':", root.join("flake.lock").display())), "{stderr}");
    let lock: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(root.join("flake.lock")).unwrap()).unwrap();
    assert_eq!(lock["nodes"]["x"]["locked"]["path"], a.display().to_string());
    assert_eq!(
        lock["nodes"]["x"]["locked"]["narHash"],
        sui_compat::source::nar_hash_source_tree(&a, "source").unwrap().nar_hash_sri
    );
    let again = sui(&root, &["flake", "lock"]);
    assert!(again.status.success() && again.stderr.is_empty(), "{}", String::from_utf8_lossy(&again.stderr));
}
