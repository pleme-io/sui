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

// ── NIX_PATH search-path resolution (mirror + eval parity) ────────────────
//
// Lives in this SEPARATE test binary (its own process) so the `NIX_PATH`
// mutation cannot race a concurrent `getenv` in another test file — every
// `IrEnv::with_pure_builtins()` / `sui_eval::eval` reads NIX_PATH at env
// construction (for `builtins.nixPath`), so this must not run alongside them.
// The other tests in THIS file do path-string + filesystem work only (never
// read the environment), so a single env-mutating test here is safe.

#[test]
fn search_path_resolution_matches_walker() {
    use std::rc::Rc;

    let tmp = std::env::temp_dir().join("sui-ir-slice4-sp-parity");
    std::fs::create_dir_all(&tmp).expect("temp tree");
    std::fs::write(tmp.join("thing.nix"), "1").expect("thing.nix");
    let tmp_s = tmp.display().to_string();
    let nix_path = format!("sp={tmp_s}");

    // SAFETY: no other test in this binary reads the environment, so this
    // set_var cannot race a concurrent getenv. Restored at the end.
    unsafe {
        std::env::set_var("NIX_PATH", &nix_path);
    }

    // (1) parse_nix_path + resolve_search_path FUNCTION parity. `nix/…` is
    // deliberately excluded — the pure-subset mirror ships no embedded
    // corepkgs (a documented divergence), so it is not a parity case.
    assert_eq!(
        sui_ir::path::parse_nix_path(&nix_path),
        sui_eval::builtins::parse_nix_path(&nix_path),
        "parse_nix_path mirror diverged"
    );
    for name in ["sp/thing.nix", "sp/nope.nix", "absent", "sp"] {
        assert_eq!(
            sui_ir::path::resolve_search_path(name),
            sui_eval::builtins::resolve_search_path(name),
            "resolve_search_path mirror diverged on {name:?}"
        );
    }

    // (2) EVAL-level parity: `<sp/thing.nix>` resolves to the SAME `Path`
    // value on both engines; a miss throws on both.
    let ir_eval =
        |src: &str| -> Result<sui_ir::eval_ir::IrValue, sui_ir::eval_ir::IrEvalError> {
            let prog = Rc::new(sui_ir::lower_file(src).expect("lowers"));
            let env = sui_ir::eval_ir::IrEnv::with_pure_builtins();
            sui_ir::eval_ir::eval_ir(&prog, prog.root, &env).and_then(|v| v.force())
        };

    let hit = "<sp/thing.nix>";
    let ir_hit = ir_eval(hit).expect("ir resolves the hit");
    let walker_hit = sui_eval::eval(hit).expect("walker resolves the hit");
    let ir_path = match &ir_hit {
        sui_ir::eval_ir::IrValue::Path(p) => (**p).clone(),
        other => panic!("ir: expected Path, got {other:?}"),
    };
    // The walker renders a `Path` value as its raw string (Value::Display).
    assert_eq!(
        ir_path,
        format!("{walker_hit}"),
        "search-path eval Path diverged from the walker"
    );
    assert_eq!(
        Some(ir_path),
        sui_ir::path::resolve_search_path("sp/thing.nix"),
        "eval Path must equal the resolver's answer"
    );

    for miss in ["<sp/nope.nix>", "<totally-absent-prefix>"] {
        assert!(
            matches!(ir_eval(miss), Err(sui_ir::eval_ir::IrEvalError::Throw(_))),
            "ir: a NIX_PATH miss must be a catchable Throw for {miss:?}"
        );
        assert!(
            sui_eval::eval(miss).is_err(),
            "walker: a NIX_PATH miss must error for {miss:?}"
        );
    }

    // SAFETY: single test owns NIX_PATH for its whole duration.
    unsafe {
        std::env::remove_var("NIX_PATH");
    }
}
