//! Differential: sui's nix.conf resolution against CppNix's own.
//!
//! Every scenario is a set of fixture files plus environment. CppNix resolves
//! it with `nix config show access-tokens` (the value it would actually use),
//! sui with `NixConfig::load_from(ConfigSources::from_env())` under the same
//! environment, and the two resolved maps must be equal. A scenario CppNix
//! refuses must be refused by sui too. The tokens are fixtures.
//!
//! Skipped, and says so, where no `nix` binary exists.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use sui_compat::nix_conf::{ConfigSources, NixConfig};

static ENV: Mutex<()> = Mutex::new(());

fn nix_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SUI_ORACLE_NIX") {
        return Some(PathBuf::from(p));
    }
    let candidates = [
        "/nix/var/nix/profiles/default/bin/nix",
        "/run/current-system/sw/bin/nix",
    ];
    if let Some(c) = candidates.iter().map(PathBuf::from).find(|p| p.exists()) {
        return Some(c);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|d| d.join("nix")).find(|p| p.exists())
    })
}

const VARS: &[&str] = &[
    "NIX_CONF_DIR",
    "NIX_USER_CONF_FILES",
    "NIX_CONFIG",
    "XDG_CONFIG_HOME",
    "XDG_CONFIG_DIRS",
    "HOME",
];

/// One scenario: files under a temp root, and env values (with `{root}`
/// substituted). Returns (cppnix, sui), each `Ok(resolved map)` or `Err(())`.
fn run(files: &[(&str, &str)], env: &[(&str, &str)], mode_000: &[&str]) -> Option<(Result<String, String>, Result<String, String>)> {
    let nix = nix_bin()?;
    let root = tempfile::tempdir().unwrap();
    let r = root.path().to_string_lossy().into_owned();
    for (name, text) in files {
        let p = root.path().join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
    }
    for name in mode_000 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.path().join(name), std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.replace("{root}", &r)))
        .collect();

    let mut cmd = Command::new(&nix);
    cmd.env_clear()
        .env("HOME", root.path())
        .args(["config", "show", "access-tokens", "--extra-experimental-features", "nix-command"]);
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run nix");
    let cppnix = if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    };

    let _g = ENV.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let saved: Vec<_> = VARS.iter().map(|k| (*k, std::env::var_os(k))).collect();
    unsafe {
        for k in VARS {
            std::env::remove_var(k);
        }
        std::env::set_var("HOME", root.path());
        for (k, v) in &env {
            std::env::set_var(k, v);
        }
    }
    let sources = ConfigSources::from_env();
    unsafe {
        for (k, v) in saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
    let sui = NixConfig::load_from(&sources)
        .map(|c| c.access_tokens().to_config_value())
        .map_err(|e| e.to_string());

    for name in mode_000 {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(root.path().join(name), std::fs::Permissions::from_mode(0o600));
    }
    Some((cppnix, sui))
}

fn agree(label: &str, files: &[(&str, &str)], env: &[(&str, &str)], mode_000: &[&str]) {
    let Some((cppnix, sui)) = run(files, env, mode_000) else {
        eprintln!("skip nix_conf_oracle/{label}: no nix binary");
        return;
    };
    match (&cppnix, &sui) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "{label}: resolved access-tokens differ"),
        (Err(_), Err(_)) => {}
        _ => panic!("{label}: cppnix={cppnix:?} sui={sui:?}"),
    }
}

const SYS: (&str, &str) = ("NIX_CONF_DIR", "{root}/etc");
const NO_USER: (&str, &str) = ("NIX_USER_CONF_FILES", "{root}/no-user.conf");

#[test]
fn bang_include_of_a_tokens_file() {
    agree(
        "bang-include",
        &[
            ("etc/nix.conf", "substituters = https://cache.nixos.org\n!include secret-tokens\n"),
            ("etc/secret-tokens", "access-tokens = github.com=F1 github.com/org=F2\n"),
        ],
        &[SYS, NO_USER],
        &[],
    );
}

#[test]
fn system_user_and_nix_config_layer_in_cppnix_order() {
    agree(
        "layers",
        &[
            ("etc/nix.conf", "access-tokens = github.com=SYS example.com=E\n"),
            ("u1.conf", "extra-access-tokens = github.com=U1 u1.example=X\n"),
            ("u2.conf", "access-tokens = github.com=U2\n"),
        ],
        &[
            SYS,
            ("NIX_USER_CONF_FILES", "{root}/u1.conf:{root}/u2.conf"),
            ("NIX_CONFIG", "extra-access-tokens = env.example=V github.com=ENV"),
        ],
        &[],
    );
}

#[test]
fn xdg_dirs_resolve_when_user_files_are_unset() {
    agree(
        "xdg",
        &[
            ("etc/nix.conf", ""),
            ("xdg-home/nix/nix.conf", "access-tokens = github.com=HOME\n"),
            ("xdg-dir/nix/nix.conf", "access-tokens = github.com=DIR other.example=O\n"),
        ],
        &[
            SYS,
            ("XDG_CONFIG_HOME", "{root}/xdg-home"),
            ("XDG_CONFIG_DIRS", "{root}/xdg-dir"),
        ],
        &[],
    );
}

#[test]
fn comments_tabs_and_crlf() {
    agree(
        "syntax",
        &[("etc/nix.conf", "# c\n\taccess-tokens\t=  github.com=T   a.example=A # tail\r\n\n")],
        &[SYS, NO_USER],
        &[],
    );
}

#[test]
fn a_missing_include_is_refused_by_both() {
    agree("missing-include", &[("etc/nix.conf", "include not-there\n")], &[SYS, NO_USER], &[]);
}

#[test]
fn a_malformed_line_is_refused_by_both() {
    agree("malformed", &[("etc/nix.conf", "access-tokens=github.com=T\n")], &[SYS, NO_USER], &[]);
}

#[test]
fn an_unreadable_include_is_skipped_by_both() {
    if nix_bin().is_some() && is_root() {
        eprintln!("skip nix_conf_oracle/unreadable: root reads mode-000 files");
        return;
    }
    agree(
        "unreadable",
        &[
            ("etc/nix.conf", "access-tokens = github.com=KEEP\n!include locked\n"),
            ("etc/locked", "access-tokens = github.com=LOCKED\n"),
        ],
        &[SYS, NO_USER],
        &["etc/locked"],
    );
}

fn is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    let d = tempfile::tempdir().unwrap();
    std::fs::metadata(d.path()).map(|m| m.uid() == 0).unwrap_or(false)
}

