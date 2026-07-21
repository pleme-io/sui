//! Parity gate for the [`sui_ir::path`] MIRRORS against their originals in
//! `sui_eval::path` (importable here — sui-eval is a dev-dependency; the
//! library itself cannot link it, which is why the mirrors exist). Drift
//! between mirror and original is a red test, not a silent divergence.

use std::path::Path;

// ── canon_abs ─────────────────────────────────────────────────────────────

const CANON_CASES: &[&str] = &[
    "/",
    "/.",
    "/..",
    "/../..",
    "/a/../..",
    "/foo/./bar",
    "/foo/../bar",
    "/foo//bar",
    "/nix/store/",
    "/nix/store",
    "/a/b/c/./../../d",
    "/trailing/",
    "//double//everywhere//",
    "/.hidden/./x",
    "/a/.../b", // `...` is a regular component
    "relative/unchanged",
    "./also/unchanged",
    "~/home/unchanged",
    "",
];

#[test]
fn canon_abs_matches_walker() {
    for case in CANON_CASES {
        assert_eq!(
            sui_ir::path::canon_abs(case),
            sui_eval::path::canon_abs(case),
            "canon_abs mirror diverged on {case:?}"
        );
    }
}

// ── normalize ─────────────────────────────────────────────────────────────

const NORMALIZE_CASES: &[&str] = &[
    "/a/./b",
    "/a/x/../b",
    "/a/../../b",
    "a/./b",
    "a/../b",
    "./a/b",
    "../a/b",
    "/",
    ".",
    "..",
    "/x/y/z/../../w",
    "~/h/./i",
];

#[test]
fn normalize_matches_walker() {
    for case in NORMALIZE_CASES {
        assert_eq!(
            sui_ir::path::normalize(Path::new(case)),
            sui_eval::path::normalize(Path::new(case)),
            "normalize mirror diverged on {case:?}"
        );
    }
}

// ── resolve_relative ──────────────────────────────────────────────────────

#[test]
fn resolve_relative_matches_walker() {
    let cases = [
        ("/base", "sub/file.nix"),
        ("/base/sub", "../file.nix"),
        ("/base", "./x/./y.nix"),
        ("/a/b/c", "../../z.nix"),
        ("/", "x.nix"),
    ];
    for (base, rel) in cases {
        assert_eq!(
            sui_ir::path::resolve_relative(Path::new(base), rel),
            sui_eval::path::resolve_relative(Path::new(base), rel),
            "resolve_relative mirror diverged on ({base:?}, {rel:?})"
        );
    }
}

// ── resolve_import (directory rule against a real temp tree) ──────────────

#[test]
fn resolve_import_matches_walker() {
    let tmp = std::env::temp_dir().join("sui-ir-slice3-path-parity");
    let dir = tmp.join("adir");
    std::fs::create_dir_all(&dir).expect("temp tree");
    std::fs::write(dir.join("default.nix"), "1").expect("default.nix");
    std::fs::write(tmp.join("afile.nix"), "1").expect("afile.nix");

    let tmp_s = tmp.display().to_string();
    let file_abs = {
        let mut s = tmp_s.clone();
        s.push_str("/afile.nix");
        s
    };
    let dir_abs = {
        let mut s = tmp_s.clone();
        s.push_str("/adir");
        s
    };
    let cases: Vec<(Option<&Path>, String)> = vec![
        // absolute file / absolute directory (→ default.nix appended)
        (None, file_abs),
        (None, dir_abs),
        // relative against a base
        (Some(tmp.as_path()), "afile.nix".to_string()),
        (Some(tmp.as_path()), "adir".to_string()),
        (Some(tmp.as_path()), "./adir".to_string()),
        (Some(dir.as_path()), "../afile.nix".to_string()),
        // nonexistent target (no directory probe hit)
        (Some(tmp.as_path()), "missing.nix".to_string()),
        // relative with no base — both error
        (None, "lib.nix".to_string()),
    ];
    for (base, raw) in cases {
        let mirror = sui_ir::path::resolve_import(base, &raw);
        let original = sui_eval::path::resolve_import(base, &raw);
        match (&mirror, &original) {
            (Ok(m), Ok(o)) => assert_eq!(m, o, "resolve_import diverged on {raw:?}"),
            (Err(_), Err(_)) => {}
            _ => panic!(
                "resolve_import Ok/Err split on {raw:?}: mirror={mirror:?} original={original:?}"
            ),
        }
    }
}
