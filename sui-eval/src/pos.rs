//! Source positions for `builtins.unsafeGetAttrPos` (and `__curPos`).
//!
//! CppNix records, for every attribute-set binding, the source position of
//! its KEY — file + line + column. `builtins.unsafeGetAttrPos name set`
//! returns `{ file; line; column; }` for the key `name` in `set` (or `null`
//! when the key/position is unknown). nixpkgs `lib/types.nix`'s `attrTag`
//! computes each tag's `declarations` from `[ pos.file ]`, so a stub that
//! returns `null` makes every `attrTag` sub-option's `declarations` empty —
//! the `options.json` dock-declarations byte-parity divergence (the six
//! `system.defaults.dock.persistent-{apps,others}.*` fields).
//!
//! This module gives each attrset built from a LITERAL (`eval_attrset`) an
//! optional [`AttrPositions`] table — its static keys' byte offsets keyed by
//! interned `Symbol`, plus the `source_id` of the parse tree it came from —
//! and a per-`source_id` [`SourceInfo`] registry (file path + full text) so
//! an offset resolves to a 1-based line/column.
//!
//! The reported `.file` is passed through [`crate::path::dematerialize`] so a
//! fetched flake input's cache-dir path is lifted to its
//! `/nix/store/<h>-source` store path — the reverse of the read-time
//! `materialize` redirect. That is what makes nix-darwin's `doc/manual`
//! `hasPrefix <nix-darwin>.outPath decl` rewrite fire (`decl` must carry the
//! store prefix), producing the `<nix-darwin/…>` declaration entries.
//!
//! Everything here only produces a REPORTED value on an explicit
//! `unsafeGetAttrPos` call; no value the evaluator observes elsewhere is
//! mutated — the byte-parity invariant.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use rustc_hash::FxHashMap;
use sui_intern::Symbol;

/// The static keys of one attrset literal, keyed by interned `Symbol`, with
/// each key's byte offset into the source text of `file`.
///
/// Attached (as `Option<Rc<AttrPositions>>`) only to attrsets built by
/// `eval_attrset` from a literal with static (`Ident`/`Str`) keys — `None`
/// for the vast majority of attrsets (merges, overlays, builtin-built,
/// dynamic-key). Shared behind `Rc` so cloning an attrset is a refcount
/// bump, never a map copy.
///
/// `file` is the file the literal was built in — captured at
/// `eval_attrset`-force time from the evaluator's eval-file stack (which a
/// thunk restores to its captured file when it forces), NOT the current
/// parse tree. A lazily-forced attrset literal from `dock.nix` therefore
/// records `dock.nix`, not whatever file happened to be top-of-stack.
#[derive(Debug, Default)]
pub struct AttrPositions {
    /// File the literal was built in (store-path-prefixed for imported
    /// inputs), or `None` for a `<string>`-eval'd literal.
    pub file: Option<PathBuf>,
    /// Key symbol → byte offset of the key token in the source text.
    pub keys: FxHashMap<Symbol, u32>,
}

impl AttrPositions {
    /// Start an empty table for a literal built in `file`.
    #[must_use]
    pub fn new(file: Option<PathBuf>) -> Self {
        Self {
            file,
            keys: FxHashMap::default(),
        }
    }

    /// Record a static key's byte offset.
    pub fn insert(&mut self, key: Symbol, offset: u32) {
        self.keys.insert(key, offset);
    }

    /// Whether any key positions were recorded (a set of only dynamic/dotted
    /// keys records nothing).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

thread_local! {
    /// Canonicalized file path → its full source text, registered once per
    /// `eval_with_file` (each file is parsed once and cached by
    /// `IMPORT_CACHE`, so one text per path). Used to resolve a key's byte
    /// offset to a 1-based line/column.
    static SOURCE_TEXTS: RefCell<FxHashMap<PathBuf, Rc<str>>> =
        RefCell::new(FxHashMap::default());
}

/// Register a file's source text so a key offset in that file resolves to a
/// line/column. Called by `eval_with_file`. A `None` file (a `<string>`
/// eval) registers nothing (it has no reportable position).
/// `(file count, total source bytes)` currently retained by `SOURCE_TEXTS`.
///
/// The registry keeps every parsed file's FULL TEXT for the process lifetime so
/// `unsafeGetAttrPos` can resolve an offset to a line/column. That is a real
/// retainer nothing counted, and it is a proxy for a much bigger one: the rowan
/// GREEN TREE parsed from each of those files, held by `IMPORT_CACHE` and by
/// every unforced `Suspended { expr, .. }` thunk. `rnix::ast::Expr` measures
/// 16 B only because it is a handle into that tree.
///
/// Backed by GLOBAL atomics, not by reading `SOURCE_TEXTS` directly: that map is
/// a `thread_local`, and the census's exit dump runs on the periodic-dump
/// THREAD, where it is empty. Reading it there reported `src_files=0` for an
/// evaluation that had parsed thousands of files — a cross-thread read of
/// thread-local state, indistinguishable from "nothing was registered".
pub(crate) static SRC_FILES: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
pub(crate) static SRC_BYTES: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

#[must_use]
pub fn source_text_census() -> (usize, usize) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        SRC_FILES.load(Relaxed).max(0) as usize,
        SRC_BYTES.load(Relaxed).max(0) as usize,
    )
}

pub fn register_source(file: Option<&Path>, text: &str) {
    let Some(file) = file else { return };
    SOURCE_TEXTS.with(|s| {
        let mut s = s.borrow_mut();
        // Only store the text the first time a path is seen (identical on
        // re-parse; avoids re-allocating the `Rc<str>` on cache-cold imports).
        use std::sync::atomic::Ordering::Relaxed;
        if !s.contains_key(file) {
            SRC_FILES.fetch_add(1, Relaxed);
            SRC_BYTES.fetch_add(text.len() as i64, Relaxed);
        }
        s.entry(file.to_path_buf())
            .or_insert_with(|| Rc::from(text));
    });
}

/// Clear the source-text registry. Called between independent top-level
/// evaluations (alongside the ident-cache clear) so a stale path→text entry
/// from a prior pass doesn't persist.
pub fn clear_sources() {
    SOURCE_TEXTS.with(|s| s.borrow_mut().clear());
}

/// Fetch a registered file's source text (an `Rc<str>` clone), if any.
fn text_for(file: &Path) -> Option<Rc<str>> {
    SOURCE_TEXTS.with(|s| s.borrow().get(file).cloned())
}

/// A resolved source position: the file (store-source-lifted) + 1-based
/// line and column — the shape `unsafeGetAttrPos` returns.
pub struct ResolvedPos {
    pub file: String,
    pub line: u64,
    pub column: u64,
}

/// Resolve `(file, offset)` to a [`ResolvedPos`] — the file is lifted from a
/// fetcher-cache path to its `/nix/store/<h>-source` store path via
/// [`crate::path::dematerialize`]. Returns `None` when `file` is `None` (a
/// `<string>` eval, no position) or the file was never parsed (no source
/// text registered — the attrset can't have originated in a real file).
///
/// LINE/COLUMN are resolved against the file's text, 1-based, with BYTE
/// columns.
///
/// This used to return `line = 1, column = byte_offset + 1` unconditionally,
/// documented as CppNix's "observed" behaviour and "verified against `nix
/// eval`" with the fixture below. It was not verified — the cited numbers are
/// this function's OWN output, recorded as if they were the oracle's, and two
/// unit tests pinned them green. Re-measured against nix 2.31.5 on exactly
/// that fixture (`{\n  aaaaa = 1;\n  bbbbb = 2;\n  ccccc = 3;\n}`):
///
/// ```text
///           nix        sui (before)
///   aaaaa   2:3        1:5
///   bbbbb   3:3        1:18
///   ccccc   4:3        1:31
/// ```
///
/// Columns count BYTES, not characters — measured: with a 2-byte `é` earlier
/// on the line, CppNix's column advances by 2. A tab advances by 1; `\r` is
/// not special.
///
/// The recorded OFFSETS were always correct: normalising both engines back to
/// `base_of_line(line) + column - 1` agreed on 30/30 non-null rows (quoted,
/// escaped, unicode and tab-indented keys; keys after comments; cross-file and
/// `toFile` store-path sets). Only this mapping step was missing.
#[must_use]
pub fn resolve(file: Option<&Path>, offset: u32) -> Option<ResolvedPos> {
    // CppNix has no position for a `<string>`-eval'd expression (no file);
    // such an attrset yields `null` from `unsafeGetAttrPos`.
    let file_path = file?;
    // Existence check only: an attrset with a position table originated in a
    // parsed file, so its text is registered. A missing entry means the
    // position can't be trusted → `null` (matches CppNix's unknown-pos).
    let text = text_for(file_path)?;
    let file = crate::path::dematerialize(file_path)
        .to_string_lossy()
        .into_owned();
    let (line, column) = line_col(&text, offset);
    Some(ResolvedPos { file, line, column })
}

/// Map a byte offset to CppNix's 1-based (line, BYTE column).
///
/// Linear scan: `unsafeGetAttrPos` is rare enough that this never showed up in
/// a profile. If it ever does, memoise a per-file line-start table beside
/// `SOURCE_TEXTS` and binary-search it — do NOT go back to a constant.
fn line_col(text: &str, offset: u32) -> (u64, u64) {
    let off = (offset as usize).min(text.len());
    let head = &text.as_bytes()[..off];
    let line = 1 + head.iter().filter(|b| **b == b'\n').count();
    let bol = head.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    (line as u64, (off - bol) as u64 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Re-baselined against `nix eval` 2.31.5 — the previous expectations
    /// (line 1, column offset+1) were this function's own output recorded as
    /// the oracle's, which is why they never went red.
    #[test]
    fn line_col_matches_cppnix() {
        // The fixture the old doc comment cited. ACTUAL nix answers:
        //   aaaaa 2:3   bbbbb 3:3   ccccc 4:3
        let t = "{\n  aaaaa = 1;\n  bbbbb = 2;\n  ccccc = 3;\n}\n";
        assert_eq!(line_col(t, t.find("aaaaa").unwrap() as u32), (2, 3));
        assert_eq!(line_col(t, t.find("bbbbb").unwrap() as u32), (3, 3));
        assert_eq!(line_col(t, t.find("ccccc").unwrap() as u32), (4, 3));
        assert_eq!(line_col(t, 0), (1, 1));
    }

    /// Columns count BYTES, not chars — measured against nix: a 2-byte `é`
    /// earlier on the line advances the reported column by 2.
    #[test]
    fn line_col_columns_are_bytes_not_chars() {
        let t = "{ \"é\" = 1; b = 2; }";
        let b = t.find(" b =").unwrap() as u32 + 1;
        assert_eq!(line_col(t, b), (1, u64::from(b) + 1));
        assert!(t.chars().count() < t.len(), "fixture must be multi-byte");
    }

    /// An out-of-range offset clamps instead of panicking.
    #[test]
    fn line_col_clamps_past_end() {
        assert_eq!(line_col("ab\ncd", 9_999), (2, 3));
    }

    #[test]
    fn resolve_none_for_unregistered_file() {
        clear_sources();
        assert!(resolve(Some(Path::new("/nowhere/x.nix")), 0).is_none());
    }

    #[test]
    fn resolve_none_when_no_file() {
        clear_sources();
        // A `<string>`-eval'd source (no file) has no position.
        register_source(None, "x = 1;");
        assert!(resolve(None, 0).is_none());
    }

    #[test]
    fn resolve_reports_file_and_cppnix_offset_pos() {
        clear_sources();
        let f = PathBuf::from("/nix/store/deadbeef-source/foo.nix");
        register_source(Some(&f), "a = 1;\nbcd = 2;");
        // offset 7 is `bcd`, which is on line 2 at column 1. The old
        // expectation here was line 1 / column 8 — the offset+1 rule, not
        // CppNix's answer.
        let p = resolve(Some(&f), 7).unwrap();
        assert_eq!(p.file, "/nix/store/deadbeef-source/foo.nix");
        assert_eq!(p.line, 2);
        assert_eq!(p.column, 1);
    }
}
