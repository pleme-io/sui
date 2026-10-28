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
    assert!(sui(&root, &["flake", "lock"]).status.success());
    (tmp, a, b, root)
}

fn sui(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::cargo_bin("sui").unwrap().current_dir(dir).args(args).output().unwrap()
}

fn stdout(out: &std::process::Output) -> String {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn eval_with_override_input_uses_the_override_and_leaves_the_lock_file_alone() {
    let (_tmp, _a, b, root) = fixture();
    let before = std::fs::read_to_string(root.join("flake.lock")).unwrap();
    let b_url = format!("path:{}", b.display());
    let out = sui(&root, &["eval", "--json", ".#v", "--override-input", "x", &b_url]);
    assert_eq!(stdout(&out), "2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&format!("warning: not writing modified lock file of flake 'path:{}':\n• Updated input 'x':", root.display())), "{stderr}");
    assert_eq!(std::fs::read_to_string(root.join("flake.lock")).unwrap(), before);
    assert_eq!(stdout(&sui(&root, &["eval", "--json", ".#v"])), "1");
}

#[test]
fn an_overridden_path_input_is_its_source_store_path_and_a_relative_override_reads_from_cwd() {
    let (_tmp, _a, b, root) = fixture();
    let store = sui_compat::source::nar_hash_source_tree(&b, "source").unwrap().store_path;
    let out = sui(&root, &["eval", "--json", ".#src", "--override-input", "x", "../b"]);
    assert_eq!(stdout(&out), format!("\"{store}\""));
}

#[test]
fn flake_lock_with_override_input_locks_the_override_and_keeps_the_declared_original() {
    let (_tmp, a, b, root) = fixture();
    let b_url = format!("path:{}", b.display());
    assert!(sui(&root, &["flake", "lock", "--override-input", "x", &b_url]).status.success());
    let lock: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(root.join("flake.lock")).unwrap()).unwrap();
    assert_eq!(lock["nodes"]["x"]["locked"]["path"], b.display().to_string());
    assert_eq!(lock["nodes"]["x"]["original"]["path"], a.display().to_string());
}
