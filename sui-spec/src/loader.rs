//! Generic spec loader — thin wrapper around `tatara_lisp::compile_typed`.
//!
//! Parses a Lisp source string into a typed [`TataraDomain`] value,
//! returning either a single value (`load_one`) or all values in
//! document order (`load_all`).  Exists mostly so that call sites
//! don't have to import `tatara_lisp` directly.

use tatara_lisp::TataraDomain;

use crate::SpecError;

/// Compile the entire `src` and return every top-level form of type `T`.
///
/// # Errors
///
/// Returns an error if the source fails to read, macroexpand, or
/// compile under `T`'s schema.
pub fn load_all<T: TataraDomain>(src: &str) -> Result<Vec<T>, SpecError> {
    Ok(tatara_lisp::compile_typed::<T>(src)?)
}

/// Convenience: compile `src` and return the single top-level form
/// of type `T`, erroring if there are zero forms.  Extra forms are
/// discarded (callers who care should use [`load_all`]).
///
/// # Errors
///
/// Returns an error if compilation fails or produces no forms.
pub fn load_one<T: TataraDomain>(src: &str) -> Result<T, SpecError> {
    let mut forms = load_all::<T>(src)?;
    forms.pop().ok_or_else(|| SpecError::Load(
        format!("no `({} ...)` form found in source", T::KEYWORD),
    ))
}
