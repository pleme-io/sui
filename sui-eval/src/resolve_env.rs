//! ENV-RESOLVE M0 — the tree-walker's *consume* side of the `sui-resolve`
//! parse-time variable-resolution side-table.
//!
//! This module owns three things:
//!
//! 1. The `SUI_RESOLVE=1` env flag (read once via a `OnceLock`, default
//!    off) — mirrors `perf::enabled()`'s one-way latch.
//! 2. A thread-local resolution table keyed by `(source_id, text_offset)` —
//!    the *identical* key shape `value::intern_cached` uses for the ident
//!    symbol cache, so a resolution recorded during `bind_vars` on one
//!    parse tree never collides with an ident at the same offset in a
//!    different (imported) parse tree.
//! 3. The hot-path lookup `resolution_for(offset)` that the eval `Ident`
//!    arm consults, plus `populate`/`clear` lifecycle hooks wired into
//!    `eval_with_file`.
//!
//! # Parity by construction
//!
//! Enabling this flag only changes the tree-walker's *hot Ident arm*: on a
//! `Resolution::Lexical{sym}` it probes the environment's lexical bindings
//! with the precomputed Symbol and returns on a hit — the byte-identical
//! value the unchanged `lookup_fast` returns (which probes the same lexical
//! map, by the same Symbol, first). On ANY miss (blackhole / unrecorded /
//! `Dynamic`) it falls back to today's exact runtime path. See
//! `sui-resolve`'s crate docs.

use std::cell::RefCell;
use std::sync::OnceLock;

use sui_resolve::{Resolution, ResolveTable};

/// One-time read of `SUI_RESOLVE`. `true` iff `SUI_RESOLVE=1`.
static ENABLED: OnceLock<bool> = OnceLock::new();

/// Whether the ENV-RESOLVE M0 fast path is enabled (`SUI_RESOLVE=1`).
///
/// Read once and cached — matches `perf::enabled()`'s one-way latch. When
/// `false`, every consume site takes today's exact unchanged runtime path.
#[must_use]
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var("SUI_RESOLVE").ok().as_deref() == Some("1"))
}

thread_local! {
    /// `(source_id << 32) | text_offset` -> resolution. Mirrors the
    /// `IDENT_CACHE` keying in `value.rs`: a resolution recorded for an
    /// ident at offset `o` in the parse tree with source id `s` is stored
    /// under `(s << 32) | o`. Only `Lexical` entries live here; an absent
    /// key reads back as `Dynamic` (the fail-safe fallback).
    static RESOLVE_TABLE: RefCell<rustc_hash::FxHashMap<u64, Resolution>> =
        RefCell::new(rustc_hash::FxHashMap::default());
}

#[inline]
fn key(source_id: u32, text_offset: u32) -> u64 {
    (u64::from(source_id) << 32) | u64::from(text_offset)
}

/// Merge a freshly-computed [`ResolveTable`] (for the parse tree tagged
/// `source_id`) into the thread-local table. No-op when the flag is off.
///
/// Called once per `eval_with_file` parse, right after `next_source_id()`
/// assigns the tree its id — so imports (which re-enter `eval_with_file`
/// with their own id) contribute their own resolutions without clobbering
/// the outer file's.
pub fn populate(source_id: u32, table: &ResolveTable) {
    if !enabled() {
        return;
    }
    RESOLVE_TABLE.with(|c| {
        let mut map = c.borrow_mut();
        for (offset, res) in table.entries() {
            map.insert(key(source_id, offset), res);
        }
    });
}

/// Resolution recorded for the ident at `(source_id, text_offset)`.
/// Returns [`Resolution::Dynamic`] for any unrecorded key — the fail-safe
/// path (the caller then takes today's exact runtime lookup).
#[must_use]
pub fn resolution_for(source_id: u32, text_offset: u32) -> Resolution {
    RESOLVE_TABLE.with(|c| {
        c.borrow()
            .get(&key(source_id, text_offset))
            .copied()
            .unwrap_or(Resolution::Dynamic)
    })
}

/// Clear the thread-local resolution table. Called at the top-level
/// (`nesting == 0`) re-entry of `eval_with_file`, alongside
/// `clear_ident_cache()`, so offsets from previous top-level evals don't
/// persist. No-op when the flag is off.
pub fn clear() {
    if !enabled() {
        return;
    }
    RESOLVE_TABLE.with(|c| c.borrow_mut().clear());
}
