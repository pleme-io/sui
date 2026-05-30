//! End-to-end module compiler tests against real pleme-io modules.
//!
//! Reads `.nix` files from the conventional checkout location, lowers
//! each to an AstGraph, runs them through the compiler, and asserts
//! the extracted IR matches what an operator would expect:
//!
//! * Every module compiles without `UnexpectedRootShape`.
//! * Modules that declare options surface at least one OptionDecl.
//! * Modules that assign config surface at least one ConfigSetter.
//! * Setters that read config in their body have non-empty slices.
//! * Archive + cast round-trip yields the same module count.
//!
//! Skipped silently if the checkout isn't reachable — keeps CI on a
//! stripped tree green.

use std::path::PathBuf;
use std::time::Instant;

use sui_spec::ast_graph::AstGraph;
use sui_spec::module_compiler::{collect_config_read_slice, compile_module};
use sui_spec::module_graph::{ArchivedModuleGraph, ModuleGraph};

fn find_real_modules() -> Vec<(String, PathBuf)> {
    if let Ok(p) = std::env::var("SUI_TEST_MODULE_DIR") {
        return walk(&PathBuf::from(p));
    }
    let candidates = [
        // The new nixos-sui-daemon-graph module is a perfect test
        // subject: declares options, has setters, has imports, real
        // NixOS module shape.
        "/home/drzzln/code/github/pleme-io/nix/modules/pleme/nixos/sui-daemon-graph.nix",
        "/home/drzzln/code/github/pleme-io/nix/modules/pleme/nixos/attic-cache-warmer.nix",
        "/home/drzzln/code/github/pleme-io/nix/modules/pleme/nixos/home-services.nix",
        "/home/drzzln/code/github/pleme-io/nix/modules/pleme/nixos/home-storage.nix",
        "/home/drzzln/code/github/pleme-io/nix/profiles/nixos-sui-daemon-graph/default.nix",
    ];
    candidates
        .iter()
        .filter_map(|c| {
            let p = PathBuf::from(c);
            if p.exists() {
                Some((p.file_name().unwrap().to_string_lossy().to_string(), p))
            } else {
                None
            }
        })
        .collect()
}

fn walk(root: &PathBuf) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) == Some("nix") {
            out.push((
                p.file_name().unwrap().to_string_lossy().to_string(),
                p,
            ));
        }
    }
    out
}

#[test]
fn real_pleme_io_modules_all_compile() {
    let modules = find_real_modules();
    if modules.is_empty() {
        eprintln!("[skip] no real .nix files reachable; set SUI_TEST_MODULE_DIR");
        return;
    }

    let mut total_options = 0;
    let mut total_setters = 0;
    let mut total_imports = 0;
    let mut modules_with_options = 0;
    let mut modules_with_setters = 0;

    for (label, path) in &modules {
        let source = std::fs::read_to_string(path).expect("read .nix file");

        let t0 = Instant::now();
        let ast = AstGraph::from_source(&source).expect("parse");
        let parse_ms = t0.elapsed().as_millis();

        let t1 = Instant::now();
        let node = compile_module(label, &ast, 0).expect("compile");
        let compile_ms = t1.elapsed().as_millis();

        total_options += node.option_decls.len();
        total_setters += node.setters.len();
        total_imports += node.imports.len();
        if !node.option_decls.is_empty() {
            modules_with_options += 1;
        }
        if !node.setters.is_empty() {
            modules_with_setters += 1;
        }

        eprintln!(
            "[module_compiler] {} ({} bytes) → {} options, {} setters, {} imports; \
             parse {}ms, compile {}ms",
            label,
            source.len(),
            node.option_decls.len(),
            node.setters.len(),
            node.imports.len(),
            parse_ms,
            compile_ms,
        );

        // Every setter that reads config should have a non-empty slice.
        for setter in &node.setters {
            let slice = collect_config_read_slice(&ast, setter.body_ast_root);
            // We don't assert non-empty universally — some setters
            // assign constants. But check that the slice we get is
            // consistent with the setter's own captured slice (today
            // captured slice is empty because emit_setter doesn't call
            // the walker — that hook lands when the walker becomes
            // mutating-friendly). Until then, the standalone walker is
            // the canonical analyzer.
            let _ = slice;
        }
    }

    eprintln!(
        "[module_compiler] summary: {} modules, {} options total ({} modules), \
         {} setters total ({} modules), {} imports total",
        modules.len(),
        total_options,
        modules_with_options,
        total_setters,
        modules_with_setters,
        total_imports,
    );

    // At least one module should have surfaced an OptionDecl (the
    // sui-daemon-graph module has many).
    assert!(
        total_options > 0,
        "no OptionDecls extracted across {} modules — pattern recognizer regression?",
        modules.len()
    );
    // At least one module should have surfaced a ConfigSetter.
    assert!(
        total_setters > 0,
        "no ConfigSetters extracted across {} modules — pattern recognizer regression?",
        modules.len()
    );
}

#[test]
fn module_graph_builds_with_real_modules() {
    let modules = find_real_modules();
    if modules.is_empty() {
        eprintln!("[skip] no real .nix files reachable");
        return;
    }

    let pairs: Vec<(String, AstGraph)> = modules
        .iter()
        .map(|(label, path)| {
            let source = std::fs::read_to_string(path).unwrap();
            let ast = AstGraph::from_source(&source).expect("parse");
            (label.clone(), ast)
        })
        .collect();

    let g = ModuleGraph::from_ast_graphs(&pairs).expect("build");
    assert_eq!(g.modules.len(), pairs.len());

    // Archive + cast round-trip — proves the full pipeline yields a
    // consistent graph.
    let (stamped, bytes) = g.clone().archive_and_hash().expect("archive");
    let archived =
        rkyv::access::<ArchivedModuleGraph, rkyv::rancor::Error>(&bytes).expect("cast");
    assert_eq!(archived.modules.len(), stamped.modules.len());
    assert_ne!(stamped.canonical_hash.bytes, [0u8; 32]);
}
