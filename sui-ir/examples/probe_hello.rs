//! PROBE (throwaway): how far does eval_ir get on real nixpkgs hello.drvPath?
//!
//! Sets NIX_PATH to the pinned nixpkgs store source, writes the target
//! expression to a temp .nix file, and drives it through the FULL file-eval
//! path (`sui_ir::eval_ir_file` → `base_env()` + set_eval_file + import +
//! derivation). Prints the FIRST typed failure (IrEvalError) verbatim, or the
//! rendered drvPath on success.

use std::path::PathBuf;

fn render(v: &sui_ir::eval_ir::IrValue) -> String {
    use sui_ir::eval_ir::IrValue::*;
    match v {
        Str(s, _) => (**s).clone(),
        Path(p) => (**p).clone(),
        other => format!("{other:?}"),
    }
}

fn main() {
    // NIX_PATH must be set in the LAUNCH env (edition 2024 makes set_var
    // unsafe); the resolver reads it lazily via std::env::var.
    let nixpkgs = std::env::var("NIX_PATH").unwrap_or_default();

    let expr = std::env::var("PROBE_EXPR").unwrap_or_else(|_| {
        "(import <nixpkgs> { system = \"aarch64-darwin\"; }).hello.drvPath".to_string()
    });

    // Write to a temp file so the eval-file stack is set (relative imports
    // inside nixpkgs resolve against the imported file, not this one, but the
    // top file must still be on the stack).
    let dir = std::env::temp_dir();
    let path: PathBuf = dir.join(format!("sui_probe_hello_{}.nix", std::process::id()));
    std::fs::write(&path, &expr).expect("write temp expr");

    eprintln!("[probe] NIX_PATH=nixpkgs={nixpkgs}");
    eprintln!("[probe] expr: {expr}");

    // ── the walker (oracle) ──────────────────────────────────────────────
    {
        use sui_eval::Evaluator;
        let t0 = std::time::Instant::now();
        let w = sui_eval::TreeWalkEvaluator;
        let wres = w.eval_file(&path);
        let dt = t0.elapsed();
        eprintln!("[probe] walker eval took {dt:?}");
        match wres {
            Ok(v) => match v.as_string() {
                Ok(s) => println!("WALKER OK drvPath = {s}"),
                Err(_) => println!("WALKER OK (non-string) = {v:?}"),
            },
            Err(e) => {
                let m = e.to_string();
                let head: String = m.chars().take(160).collect();
                println!("WALKER ERR {head}");
            }
        }
    }

    // ── eval_ir ──────────────────────────────────────────────────────────
    let t0 = std::time::Instant::now();
    let result = sui_ir::eval_ir_file(&path);
    let dt = t0.elapsed();
    eprintln!("[probe] eval_ir took {dt:?}");

    match result {
        Ok(v) => match v.force() {
            Ok(fv) => println!("IR OK drvPath = {}", render(&fv)),
            Err(e) => println!("IR FORCE-ERR {e:?}  ::  {e}"),
        },
        Err(e) => {
            let m = e.to_string();
            let head: String = m.chars().take(160).collect();
            println!("IR EVAL-ERR {head}");
        }
    }

    let _ = std::fs::remove_file(&path);
}
