//! Import cache infrastructure.
//!
//! CppNix caches `import` results so that `import ./lib.nix` evaluated
//! from different call sites returns the same thunk/value. This is
//! critical for nixpkgs performance — without it, ~500 unique files
//! times 50+ overlay applications produce 25,000+ redundant parse-
//! and-evaluate cycles, easily blowing the eval depth limit.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::value::Value;

thread_local! {
    /// Cache of imported file values, keyed by canonical absolute path.
    ///
    /// The cache persists for the entire evaluation session (including
    /// recursive `evaluate_flake` calls for flake inputs) so that shared
    /// dependencies like nixpkgs are evaluated only once.
    pub(crate) static IMPORT_CACHE: RefCell<HashMap<std::path::PathBuf, Value>> = RefCell::new(HashMap::new());
}

/// Clear the import cache.
///
/// Call at the start of a fresh top-level evaluation when you need to
/// guarantee that no stale values survive from a previous session.
/// During normal flake evaluation this should **not** be called — the
/// cache intentionally spans recursive `evaluate_flake` calls.
pub fn clear_import_cache() {
    IMPORT_CACHE.with(|c| c.borrow_mut().clear());
}
