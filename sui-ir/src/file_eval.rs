//! L3 slice 3 — file-capable evaluation: the eval-file stack, the
//! **lower-once program cache**, and `import`.
//!
//! # The lower-once destination in miniature
//!
//! `PROGRAM_CACHE` is a thread-local map `canonical path → Rc<Program>`:
//! every file is parsed + lowered **exactly once** per thread, however many
//! times it is imported or re-evaluated — the SPEED.md L3 "lower once per
//! source content, evaluate forever after" shape, keyed here by canonical
//! path (content-hash keying arrives when the cache goes cross-process).
//! [`program_cache_len`] exposes the cache size so tests can *prove*
//! lower-once instead of assuming it.
//!
//! # Mirrored walker semantics
//!
//! * The eval-file stack mirrors `sui-eval`'s `EVAL_FILE_STACK` — pushed on
//!   file entry (`eval_ir_file`, `import`), on lambda application (the
//!   closure env's file) and on thunk force (the captured env's file), so
//!   relative path literals resolve against the file that *defined* them.
//! * `import` mirrors the walker's builtin: `resolve_import` (directory →
//!   `default.nix`), a canonical-path-keyed **value cache** (so
//!   `import ./a.nix == import ./a.nix` holds by identity, like the
//!   walker's `IMPORT_CACHE`), fallback-to-raw-path on resolve failure, and
//!   a forced top-level result.
//! * On top of the mirror, `import` adds **typed circular-import
//!   detection** ([`IrEvalError::ImportCycle`]): the walker (like CppNix)
//!   just recurses until the stack dies — a class this engine makes a typed
//!   error instead. IR-side improvement, exercised by an IR-only test
//!   (running the walker on a cycle would abort the process).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use rustc_hash::FxHashMap;

use crate::eval_ir::{eval_ir, IrEvalError, IrValue};
use crate::ir::Program;
use crate::lower_file;

thread_local! {
    /// Files currently being evaluated, innermost last (mirror of the
    /// walker's `EVAL_FILE_STACK`).
    static EVAL_FILE_STACK: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    /// canonical path → lowered program (parse + lower ONCE per file).
    static PROGRAM_CACHE: RefCell<FxHashMap<PathBuf, Rc<Program>>> =
        RefCell::new(FxHashMap::default());
    /// canonical path → evaluated file value (the walker's `IMPORT_CACHE`).
    static IMPORT_VALUE_CACHE: RefCell<FxHashMap<PathBuf, IrValue>> =
        RefCell::new(FxHashMap::default());
    /// Files whose evaluation is in progress — the cycle detector.
    static FILES_IN_PROGRESS: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

// ── the eval-file stack ───────────────────────────────────────────────────

/// RAII guard popping the eval-file stack on drop.
pub struct EvalFileGuard;

impl Drop for EvalFileGuard {
    fn drop(&mut self) {
        EVAL_FILE_STACK.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

/// Push a file onto the eval-file stack (mirror of the walker's
/// `push_eval_file`).
pub fn push_eval_file(file: PathBuf) -> EvalFileGuard {
    EVAL_FILE_STACK.with(|s| s.borrow_mut().push(file));
    EvalFileGuard
}

/// Directory of the file currently being evaluated, if any (mirror of the
/// walker's `current_eval_dir`) — the base for relative path literals and
/// relative imports.
#[must_use]
pub fn current_eval_dir() -> Option<PathBuf> {
    EVAL_FILE_STACK.with(|s| {
        s.borrow()
            .last()
            .and_then(|p| p.parent().map(PathBuf::from))
    })
}

// ── the cycle detector ────────────────────────────────────────────────────

struct InProgressGuard;

impl Drop for InProgressGuard {
    fn drop(&mut self) {
        FILES_IN_PROGRESS.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

fn enter_file(canonical: &Path) -> Result<InProgressGuard, IrEvalError> {
    FILES_IN_PROGRESS.with(|s| {
        let mut stack = s.borrow_mut();
        if stack.iter().any(|p| p == canonical) {
            let mut chain: Vec<String> = stack
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            chain.push(canonical.display().to_string());
            return Err(IrEvalError::ImportCycle(chain.join(" -> ")));
        }
        stack.push(canonical.to_path_buf());
        Ok(InProgressGuard)
    })
}

// ── the lower-once program cache ──────────────────────────────────────────

/// Number of lowered programs currently cached (the lower-once proof
/// surface for tests and the A/B harness).
#[must_use]
pub fn program_cache_len() -> usize {
    PROGRAM_CACHE.with(|c| c.borrow().len())
}

/// Clear the import VALUE cache only (programs stay lowered). The warm
/// half of the file A/B: re-evaluation re-runs every file's code but never
/// re-parses or re-lowers.
pub fn clear_import_value_cache() {
    IMPORT_VALUE_CACHE.with(|c| c.borrow_mut().clear());
}

/// Clear every file cache (programs + values). Test isolation + the cold
/// half of the file A/B.
pub fn clear_file_caches() {
    PROGRAM_CACHE.with(|c| c.borrow_mut().clear());
    IMPORT_VALUE_CACHE.with(|c| c.borrow_mut().clear());
}

/// Fetch (or parse + lower once) the program for `canonical`.
fn lowered_program(canonical: &Path) -> Result<Rc<Program>, IrEvalError> {
    if let Some(hit) = PROGRAM_CACHE.with(|c| c.borrow().get(canonical).cloned()) {
        return Ok(hit);
    }
    let source = std::fs::read_to_string(canonical).map_err(|e| IrEvalError::Io {
        context: {
            let mut s = String::from("import ");
            s.push_str(&canonical.display().to_string());
            s
        },
        message: e.to_string(),
    })?;
    let program = Rc::new(
        lower_file(&source).map_err(|e| IrEvalError::Parse(e.to_string()))?,
    );
    PROGRAM_CACHE.with(|c| {
        c.borrow_mut()
            .insert(canonical.to_path_buf(), program.clone())
    });
    Ok(program)
}

// ── file evaluation ───────────────────────────────────────────────────────

/// Evaluate `canonical` in a fresh base env tagged with the file, forcing
/// the top-level result (the walker's `eval_with_file` forces too).
fn eval_file_inner(canonical: &Path) -> Result<IrValue, IrEvalError> {
    let _cycle = enter_file(canonical)?;
    let program = lowered_program(canonical)?;
    let _file = push_eval_file(canonical.to_path_buf());
    let mut env = crate::builtins::base_env();
    env.set_eval_file(Some(Rc::new(canonical.to_path_buf())));
    eval_ir(&program, program.root, &env).and_then(|v| v.force())
}

/// Evaluate a Nix file through the IR engine — the mirror of the walker's
/// `TreeWalkEvaluator::eval_file` (which also does NOT consult the import
/// value cache for the top file; nested `import`s do).
///
/// # Errors
///
/// Typed [`IrEvalError`] — including `Io` for unreadable files, `Parse`
/// for parse/lower failures and `ImportCycle` for circular imports.
pub fn eval_ir_file(path: &Path) -> Result<IrValue, IrEvalError> {
    let canonical = crate::path::normalize(path);
    eval_file_inner(&canonical)
}

/// The `import` builtin body: resolve, consult the value cache, evaluate,
/// cache. Mirrors the walker's `import` (see module docs).
pub(crate) fn import(raw: &str) -> Result<IrValue, IrEvalError> {
    let resolved = crate::path::resolve_import(current_eval_dir().as_deref(), raw)
        .unwrap_or_else(|_| PathBuf::from(raw));
    let canonical = crate::path::normalize(&resolved);
    if let Some(hit) = IMPORT_VALUE_CACHE.with(|c| c.borrow().get(&canonical).cloned()) {
        return Ok(hit);
    }
    let value = eval_file_inner(&canonical)?;
    IMPORT_VALUE_CACHE.with(|c| c.borrow_mut().insert(canonical, value.clone()));
    Ok(value)
}
