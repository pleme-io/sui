use std::path::PathBuf;

use serde_yaml_ng::Value;

fn workflow(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../.github/workflows")
        .join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_yaml_ng::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn at<'a>(v: &'a Value, keys: &[&str]) -> &'a Value {
    keys.iter().fold(v, |v, k| &v[*k])
}

fn text<'a>(v: &'a Value, keys: &[&str]) -> &'a str {
    at(v, keys)
        .as_str()
        .unwrap_or_else(|| panic!("{keys:?} is not a string"))
}

#[test]
fn the_image_push_stays_on_a_hosted_runner() {
    let w = workflow("image-release.yml");
    assert!(at(&w, &["jobs", "image-amd64", "with", "runner"]).is_null());
}

#[test]
fn the_image_ships_through_nix_and_writes_to_no_registry() {
    let w = workflow("image-release.yml");
    assert_eq!(
        at(&w, &["jobs", "image-amd64", "with", "publish"]).as_bool(),
        Some(false)
    );
}

#[test]
fn the_image_push_hands_its_secrets_to_the_publish_app_mint() {
    let w = workflow("image-release.yml");
    assert_eq!(text(&w, &["jobs", "image-amd64", "secrets"]), "inherit");
    assert!(
        text(&w, &["jobs", "image-amd64", "uses"])
            .starts_with("pleme-io/substrate/.github/workflows/image-push.yml@")
    );
}

#[test]
fn the_image_push_skips_every_bot_pushed_bump_commit() {
    let w = workflow("image-release.yml");
    let guard = text(&w, &["jobs", "image-amd64", "if"]);
    assert!(guard.contains("endsWith(github.actor, '[bot]')"), "{guard}");
}

#[test]
fn the_release_stays_on_a_hosted_runner() {
    let w = workflow("auto-release.yml");
    assert!(at(&w, &["jobs", "release", "with", "runner"]).is_null());
}

#[test]
fn a_new_tag_dispatches_the_binary_release() {
    let w = workflow("auto-release.yml");
    let job = at(&w, &["jobs", "binaries"]);
    assert_eq!(text(job, &["needs"]), "release");
    assert_eq!(
        text(job, &["if"]),
        "needs.release.outputs.bumped == 'true'"
    );
    assert_eq!(text(job, &["permissions", "actions"]), "write");
    let steps = at(job, &["steps"]).as_sequence().expect("steps is a list");
    assert_eq!(steps.len(), 1);
    let run = text(&steps[0], &["run"]);
    assert!(
        run.contains("gh workflow run release-binaries.yml") && run.contains("--ref \"v$VERSION\""),
        "{run}"
    );
    assert_eq!(
        text(&steps[0], &["env", "VERSION"]),
        "${{ needs.release.outputs.new-version }}"
    );
}

#[test]
fn a_release_tag_ships_the_sui_binary() {
    let w = workflow("release-binaries.yml");
    let tags = at(&w, &["on", "push", "tags"])
        .as_sequence()
        .expect("on.push.tags is a list");
    assert!(tags.iter().any(|t| t.as_str() == Some("v*")), "{tags:?}");
    assert!(
        at(&w, &["on"]).as_mapping().is_some_and(|m| m.contains_key("workflow_dispatch")),
        "release-binaries must accept workflow_dispatch, the one trigger GITHUB_TOKEN may fire"
    );
    let uses = text(&w, &["jobs", "binaries", "uses"]);
    assert!(
        uses.starts_with("pleme-io/substrate/.github/workflows/rust-binary-release.yml@"),
        "{uses}"
    );
    assert_eq!(text(&w, &["jobs", "binaries", "with", "binary-name"]), "sui");
    assert_eq!(
        text(&w, &["jobs", "binaries", "with", "package"]),
        "pleme-io-sui"
    );
}
