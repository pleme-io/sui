use std::path::PathBuf;

use serde_yaml_ng::Value;

const APP_POOL: &str = "pleme-public-build";

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
fn the_image_push_runs_where_the_posture_app_token_is_minted() {
    let w = workflow("image-release.yml");
    assert_eq!(text(&w, &["jobs", "image-amd64", "with", "runner"]), APP_POOL);
}

#[test]
fn the_image_push_skips_every_bot_pushed_bump_commit() {
    let w = workflow("image-release.yml");
    let guard = text(&w, &["jobs", "image-amd64", "if"]);
    assert!(guard.contains("endsWith(github.actor, '[bot]')"), "{guard}");
}

#[test]
fn the_release_tag_is_pushed_with_the_posture_app_token() {
    let w = workflow("auto-release.yml");
    assert_eq!(text(&w, &["jobs", "release", "with", "runner"]), APP_POOL);
}

#[test]
fn a_release_tag_ships_the_sui_binary() {
    let w = workflow("release-binaries.yml");
    let tags = at(&w, &["on", "push", "tags"])
        .as_sequence()
        .expect("on.push.tags is a list");
    assert!(tags.iter().any(|t| t.as_str() == Some("v*")), "{tags:?}");
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
