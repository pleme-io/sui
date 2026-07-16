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
pub fn register_source(file: Option<&Path>, text: &str) {
    let Some(file) = file else { return };
    SOURCE_TEXTS.with(|s| {
        let mut s = s.borrow_mut();
        // Only store the text the first time a path is seen (identical on
        // re-parse; avoids re-allocating the `Rc<str>` on cache-cold imports).
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
/// LINE/COLUMN match CppNix's *observed* `unsafeGetAttrPos` behavior exactly
/// (byte-parity is sacred, even for a quirk): for a position obtained this
/// way CppNix reports `line = 1` and `column = byte_offset + 1` — the raw
/// 1-based byte offset into the file, NOT a newline-resolved visual position.
/// Verified against `nix eval` on multi-line files (aaaaa@off4→col5,
/// bbbbb@off17→col18, ccccc@off30→col31, all line 1) via both `import` and
/// `--file`. `options.json` itself drops `declarationPositions` (so only
/// `.file` feeds the dock-declarations root), but matching line/column keeps
/// every `unsafeGetAttrPos`-consuming surface byte-identical.
#[must_use]
pub fn resolve(file: Option<&Path>, offset: u32) -> Option<ResolvedPos> {
    // CppNix has no position for a `<string>`-eval'd expression (no file);
    // such an attrset yields `null` from `unsafeGetAttrPos`.
    let file_path = file?;
    // Existence check only: an attrset with a position table originated in a
    // parsed file, so its text is registered. A missing entry means the
    // position can't be trusted → `null` (matches CppNix's unknown-pos).
    let _ = text_for(file_path)?;
    let file = crate::path::dematerialize(file_path)
        .to_string_lossy()
        .into_owned();
    let (line, column) = line_col(offset);
    Some(ResolvedPos { file, line, column })
}

/// CppNix's observed `unsafeGetAttrPos` line/column: `line = 1`,
/// `column = byte_offset + 1` (the raw 1-based byte offset into the file, not
/// a newline-resolved position). See [`resolve`] for the verification.
fn line_col(offset: u32) -> (u64, u64) {
    (1, u64::from(offset) + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_matches_cppnix_offset_rule() {
        // CppNix's `unsafeGetAttrPos` reports line=1 always and
        // column = byte_offset + 1 (verified: aaaaa@4→col5, bbbbb@17→col18,
        // ccccc@30→col31, all on "line 1", via `nix eval` on a multi-line
        // file through both `import` and `--file`).
        assert_eq!(line_col(0), (1, 1));
        assert_eq!(line_col(4), (1, 5));
        assert_eq!(line_col(17), (1, 18));
        assert_eq!(line_col(30), (1, 31));
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
        // offset 7 is `bcd` — CppNix reports line 1, column offset+1 = 8.
        let p = resolve(Some(&f), 7).unwrap();
        assert_eq!(p.file, "/nix/store/deadbeef-source/foo.nix");
        assert_eq!(p.line, 1);
        assert_eq!(p.column, 8);
    }
}
