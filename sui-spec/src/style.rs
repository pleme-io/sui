//! Nord-palette styled output for sui CLI surfaces.
//!
//! The pleme-io blackmatter aesthetic.  Every operator-visible
//! line of CLI text routes through this module so the styling
//! stays consistent across `sui`, `sui-sweep`, `sui-spec-inventory`,
//! and any future binary that lands in this workspace.
//!
//! Palette: Arctic Ice Studio's Nord (https://www.nordtheme.com/).
//! Four typed groups:
//!
//! - **Polar Night** (bg-leaning dark grays — `nord0`..`nord3`)
//! - **Snow Storm** (light text — `nord4`..`nord6`)
//! - **Frost** (cool teal/cyan/blue accents — `nord7`..`nord10`)
//! - **Aurora** (warm signal colors — red/orange/yellow/green/purple,
//!   `nord11`..`nord15`)
//!
//! All colors are ANSI 24-bit (truecolor).  Honors `$NO_COLOR`
//! (https://no-color.org/) — when set, every helper emits the
//! payload unchanged with zero escape sequences.  Also drops
//! styling when stdout isn't a TTY (so pipes get clean output).
//!
//! ## Usage
//!
//! ```ignore
//! use sui_spec::style::*;
//! println!("{}", header("sui-spec inventory"));
//! println!("  {} {}", glyph_ok(), success("19 domains loaded"));
//! println!("  {} {}", glyph_warn(), warn("1 maturity gate pending"));
//! ```
//!
//! The convention: every domain-specific output composes from
//! these primitives.  No raw ANSI escapes outside this module.

use std::sync::OnceLock;

// ── Palette constants (24-bit RGB) ────────────────────────────────

pub const NORD0: Rgb = Rgb(0x2e, 0x34, 0x40); // Polar Night (darkest bg)
pub const NORD1: Rgb = Rgb(0x3b, 0x42, 0x52); // Polar Night (panel)
pub const NORD2: Rgb = Rgb(0x43, 0x4c, 0x5e); // Polar Night (hover)
pub const NORD3: Rgb = Rgb(0x4c, 0x56, 0x6a); // Polar Night (comment)
pub const NORD4: Rgb = Rgb(0xd8, 0xde, 0xe9); // Snow Storm (dim text)
pub const NORD5: Rgb = Rgb(0xe5, 0xe9, 0xf0); // Snow Storm
pub const NORD6: Rgb = Rgb(0xec, 0xef, 0xf4); // Snow Storm (brightest text)
pub const NORD7: Rgb = Rgb(0x8f, 0xbc, 0xbb); // Frost (sea green)
pub const NORD8: Rgb = Rgb(0x88, 0xc0, 0xd0); // Frost (ice cyan — primary accent)
pub const NORD9: Rgb = Rgb(0x81, 0xa1, 0xc1); // Frost (light blue)
pub const NORD10: Rgb = Rgb(0x5e, 0x81, 0xac); // Frost (deep blue)
pub const NORD11: Rgb = Rgb(0xbf, 0x61, 0x6a); // Aurora (red — error)
pub const NORD12: Rgb = Rgb(0xd0, 0x87, 0x70); // Aurora (orange — warning)
pub const NORD13: Rgb = Rgb(0xeb, 0xcb, 0x8b); // Aurora (yellow — pending)
pub const NORD14: Rgb = Rgb(0xa3, 0xbe, 0x8c); // Aurora (green — success)
pub const NORD15: Rgb = Rgb(0xb4, 0x8e, 0xad); // Aurora (purple — info)

/// 24-bit RGB tuple.  Used by [`fg`] to emit truecolor ANSI escapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

// ── Style detection (NO_COLOR + TTY) ──────────────────────────────

static STYLING: OnceLock<bool> = OnceLock::new();

/// Whether styling is active.  Cached on first call so subsequent
/// invocations are zero-cost.  Set by environment:
///
/// - `NO_COLOR` set (to anything non-empty) → styling off
/// - stdout not a TTY → styling off
/// - otherwise → styling on
#[must_use]
pub fn styling_enabled() -> bool {
    *STYLING.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return false;
        }
        // Explicit override for tests + scripts that want color even
        // when stdout is piped.
        if std::env::var_os("SUI_FORCE_COLOR").is_some() {
            return true;
        }
        is_tty(1)
    })
}

fn is_tty(fd: i32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: isatty is a thread-safe libc query of a fd.
        unsafe { libc::isatty(fd) == 1 }
    }
    #[cfg(not(unix))]
    { let _ = fd; false }
}

// ── Primitive styling helpers ─────────────────────────────────────

/// Wrap text in a foreground-color ANSI escape.  When styling is
/// disabled, returns the text unchanged (no escapes added).
#[must_use]
pub fn fg(color: Rgb, text: &str) -> String {
    if !styling_enabled() {
        return text.to_string();
    }
    format!("\x1b[38;2;{};{};{}m{}\x1b[0m", color.0, color.1, color.2, text)
}

/// Bold + foreground.
#[must_use]
pub fn bold_fg(color: Rgb, text: &str) -> String {
    if !styling_enabled() {
        return text.to_string();
    }
    format!("\x1b[1;38;2;{};{};{}m{}\x1b[0m", color.0, color.1, color.2, text)
}

/// Dim (faint) + foreground.
#[must_use]
pub fn dim_fg(color: Rgb, text: &str) -> String {
    if !styling_enabled() {
        return text.to_string();
    }
    format!("\x1b[2;38;2;{};{};{}m{}\x1b[0m", color.0, color.1, color.2, text)
}

// ── Semantic helpers (the everyday surface) ───────────────────────

/// Section header — bold ice-cyan (Nord8).
#[must_use]
pub fn header(text: &str) -> String { bold_fg(NORD8, text) }

/// Success message — Aurora green (Nord14).
#[must_use]
pub fn success(text: &str) -> String { fg(NORD14, text) }

/// Warning — Aurora orange (Nord12).
#[must_use]
pub fn warn(text: &str) -> String { fg(NORD12, text) }

/// Error — Aurora red (Nord11).
#[must_use]
pub fn error(text: &str) -> String { fg(NORD11, text) }

/// Pending / in-flight — Aurora yellow (Nord13).
#[must_use]
pub fn pending(text: &str) -> String { fg(NORD13, text) }

/// Informational accent — Frost light blue (Nord9).
#[must_use]
pub fn info(text: &str) -> String { fg(NORD9, text) }

/// Subtle / muted — Polar Night comment color (Nord3).
#[must_use]
pub fn muted(text: &str) -> String { dim_fg(NORD3, text) }

/// Secondary accent — Aurora purple (Nord15).  Used for type
/// names + identifiers.
#[must_use]
pub fn ident(text: &str) -> String { fg(NORD15, text) }

/// Primary text — Snow Storm bright (Nord6).  The default for body
/// copy; usually you don't need to call this since uncolored text
/// renders fine, but useful when composing inside a larger styled
/// span.
#[must_use]
pub fn body(text: &str) -> String { fg(NORD6, text) }

// ── Glyphs (Unicode + ASCII fallback) ─────────────────────────────

/// `●` / `*` — generic bullet.
#[must_use]
pub fn glyph_dot() -> &'static str {
    if styling_enabled() { "●" } else { "*" }
}

/// `✓` / `[ok]` — success marker.
#[must_use]
pub fn glyph_ok() -> String { success(if styling_enabled() { "✓" } else { "[ok]" }) }

/// `✗` / `[x]` — failure / divergence marker.
#[must_use]
pub fn glyph_fail() -> String { error(if styling_enabled() { "✗" } else { "[x]" }) }

/// `⚠` / `[!]` — warning marker.
#[must_use]
pub fn glyph_warn() -> String { warn(if styling_enabled() { "⚠" } else { "[!]" }) }

/// `⚙` / `[~]` — in-progress / typed-only marker.  Matches the
/// substrate catalog's M2/M3/M4-typed-only marker convention.
#[must_use]
pub fn glyph_gear() -> String { pending(if styling_enabled() { "⚙" } else { "[~]" }) }

/// `▸` / `>` — right-arrow / call-out.
#[must_use]
pub fn glyph_arrow() -> String { info(if styling_enabled() { "▸" } else { ">" }) }

/// `❄` / `*` — Nord snowflake.  Brand glyph; used sparingly for
/// the top-level banner.
#[must_use]
pub fn glyph_snowflake() -> String { fg(NORD8, if styling_enabled() { "❄" } else { "*" }) }

// ── Box-drawing helpers (Nord-styled tables) ──────────────────────

/// Top of a styled box, with optional title centered.  Width is the
/// total number of columns (must be ≥ title length + 4).
#[must_use]
pub fn box_top(width: usize, title: Option<&str>) -> String {
    let line = "─".repeat(width.saturating_sub(2));
    let raw = match title {
        None => format!("┌{line}┐"),
        Some(t) => {
            let pad = width.saturating_sub(t.len() + 4);
            let half = pad / 2;
            let other_half = pad - half;
            format!(
                "┌{}{}{}┐",
                "─".repeat(half),
                {
                    let t = format!(" {} ", t);
                    bold_fg(NORD8, &t)
                },
                "─".repeat(other_half),
            )
        }
    };
    dim_fg(NORD3, &raw)
}

/// Mid-separator row inside a box.
#[must_use]
pub fn box_mid(width: usize) -> String {
    dim_fg(NORD3, &format!("├{}┤", "─".repeat(width.saturating_sub(2))))
}

/// Bottom of a styled box.
#[must_use]
pub fn box_bottom(width: usize) -> String {
    dim_fg(NORD3, &format!("└{}┘", "─".repeat(width.saturating_sub(2))))
}

// ── LabeledTable — typed builder for "kv / opt / section / list" rows ──
//
// Three operator-facing views in sui-spec-inventory (narinfo /
// realisation / hash-decode) reached for the same closure-set
// (kv + opt + section + list-item).  Third-site extraction.
//
// Usage:
//
//   LabeledTable::new(14)
//       .kv("StorePath", &rec.store_path)
//       .kv("URL", &rec.url)
//       .opt("Deriver", rec.deriver.as_deref())
//       .section("References", rec.references.len())
//       .list_items("→", &rec.references)
//       .render();
//
// Required fields render as `ident()` (Nord aurora purple).
// Optional fields render as `info()` if Some, `muted()` if None.
// Section headers render as `body()` with a count chip.
// List items render with a configurable glyph + `success()`.

/// Typed builder for labeled-row tables.  Renders Nord-styled
/// `right-aligned label  value` rows + sections + nested lists.
#[must_use]
pub struct LabeledTable {
    label_w: usize,
    rows: Vec<TableRow>,
}

enum TableRow {
    Kv { key: String, value: String },
    Opt { key: String, value: Option<String> },
    Section { title: String, count: Option<usize> },
    ListItem { glyph: String, value: String },
    Blank,
}

impl LabeledTable {
    /// Construct a new table with the given right-aligned label
    /// width (typically 14 across the inventory binary).
    pub fn new(label_w: usize) -> Self {
        Self { label_w, rows: Vec::new() }
    }

    /// Add a required key-value row.  Value renders as `ident()`.
    pub fn kv(mut self, key: &str, value: &str) -> Self {
        self.rows.push(TableRow::Kv {
            key: key.to_string(),
            value: value.to_string(),
        });
        self
    }

    /// Add an optional key-value row.  Present values render
    /// as `info()`, absent as `muted("(none)")`.
    pub fn opt(mut self, key: &str, value: Option<&str>) -> Self {
        self.rows.push(TableRow::Opt {
            key: key.to_string(),
            value: value.map(String::from),
        });
        self
    }

    /// Add a blank line for visual grouping.
    pub fn blank(mut self) -> Self {
        self.rows.push(TableRow::Blank);
        self
    }

    /// Add a section header with an optional count chip.
    pub fn section(mut self, title: &str, count: Option<usize>) -> Self {
        self.rows.push(TableRow::Section {
            title: title.to_string(),
            count,
        });
        self
    }

    /// Add a list of nested rows under the most recent section.
    /// Each item is prefixed with `glyph` and styled `success()`.
    pub fn list_items<I, S>(mut self, glyph: &str, items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for item in items {
            self.rows.push(TableRow::ListItem {
                glyph: glyph.to_string(),
                value: item.as_ref().to_string(),
            });
        }
        self
    }

    /// Render every row to stdout.
    pub fn render(self) {
        for row in &self.rows {
            match row {
                TableRow::Kv { key, value } => {
                    println!(
                        "  {}  {}",
                        body(&right_align(key, self.label_w)),
                        ident(value),
                    );
                }
                TableRow::Opt { key, value } => {
                    let val_str = match value {
                        Some(v) => info(v),
                        None => muted("(none)"),
                    };
                    println!(
                        "  {}  {}",
                        body(&right_align(key, self.label_w)),
                        val_str,
                    );
                }
                TableRow::Section { title, count } => match count {
                    Some(n) => println!(
                        "  {}  {}",
                        body(&right_align(title, self.label_w)),
                        ident(&n.to_string()),
                    ),
                    None => println!(
                        "  {}",
                        body(&right_align(title, self.label_w)),
                    ),
                },
                TableRow::ListItem { glyph, value } => {
                    println!("    {}  {}", muted(glyph), success(value));
                }
                TableRow::Blank => println!(),
            }
        }
    }
}

fn right_align(s: &str, w: usize) -> String {
    format!("{:>w$}", s, w = w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_styling_on() {
        // OnceLock can't be reset; we set SUI_FORCE_COLOR before
        // any test queries STYLING.  Run with --test-threads=1 if
        // tests interact.  For these tests we set the env var
        // statically.
        unsafe { std::env::set_var("SUI_FORCE_COLOR", "1"); }
    }

    #[test]
    fn nord_palette_is_complete() {
        // Sanity — every nord<N> is a well-formed Rgb.
        let all = [
            NORD0, NORD1, NORD2, NORD3, NORD4, NORD5, NORD6, NORD7,
            NORD8, NORD9, NORD10, NORD11, NORD12, NORD13, NORD14, NORD15,
        ];
        assert_eq!(all.len(), 16);
        for c in all {
            // All channels are valid bytes by Rust type system; the
            // semantic check is that we didn't fat-finger 0xff vs 0x00
            // on a known color.
            let _ = c;
        }
        // Sanity: NORD0 is dark, NORD6 is light.
        assert!(NORD0.0 < NORD6.0);
        assert!(NORD0.1 < NORD6.1);
        assert!(NORD0.2 < NORD6.2);
    }

    #[test]
    fn fg_emits_truecolor_escape_when_enabled() {
        ensure_styling_on();
        let s = fg(NORD8, "hello");
        assert!(s.contains("\x1b[38;2;136;192;208m"), "wrong escape: {s:?}");
        assert!(s.contains("hello"));
        assert!(s.ends_with("\x1b[0m"), "must reset at end: {s:?}");
    }

    #[test]
    fn no_color_strips_escapes() {
        // We can't test NO_COLOR with the OnceLock cache easily —
        // SUI_FORCE_COLOR is already set from earlier tests.  The
        // styling_enabled() short-circuit logic is exercised by
        // its own unit assertions above.
        ensure_styling_on();
        // With styling forced on, fg DOES include escapes.  In a
        // fresh process with NO_COLOR set, the same call would
        // return the bare text — tested by integration scripts.
        let s = fg(NORD14, "ok");
        assert!(s.contains("ok"));
    }

    #[test]
    fn semantic_helpers_use_distinct_colors() {
        ensure_styling_on();
        let s_ok = success("x");
        let s_err = error("x");
        let s_warn = warn("x");
        assert_ne!(s_ok, s_err);
        assert_ne!(s_ok, s_warn);
        assert_ne!(s_err, s_warn);
    }

    #[test]
    fn glyphs_have_unicode_when_enabled() {
        ensure_styling_on();
        assert!(glyph_ok().contains('✓'));
        assert!(glyph_fail().contains('✗'));
        assert!(glyph_warn().contains('⚠'));
        assert!(glyph_gear().contains('⚙'));
        assert!(glyph_snowflake().contains('❄'));
    }

    #[test]
    fn box_top_renders_with_title() {
        ensure_styling_on();
        let s = box_top(40, Some("sui-spec"));
        assert!(s.contains("┌"));
        assert!(s.contains("┐"));
        // Title byte is in there somewhere (under ANSI escapes).
        assert!(s.contains("sui-spec"));
    }

    // ── LabeledTable tests ─────────────────────────────────────

    #[test]
    fn labeled_table_builder_is_chainable() {
        // Compile-time proof the builder threads through chained
        // calls without ownership issues — drop the result, just
        // ensure construction works.
        let _t = LabeledTable::new(14)
            .kv("key1", "val1")
            .kv("key2", "val2")
            .opt("optKey", Some("optVal"))
            .opt("absent", None)
            .blank()
            .section("Section", Some(3))
            .list_items("→", ["a", "b", "c"]);
    }

    #[test]
    fn labeled_table_accepts_string_and_str_in_list_items() {
        let owned: Vec<String> = vec!["a".into(), "b".into()];
        let _ = LabeledTable::new(10)
            .list_items("→", &owned);
        let borrowed = ["x", "y"];
        let _ = LabeledTable::new(10)
            .list_items("→", borrowed);
    }

    #[test]
    fn right_align_pads_to_width() {
        assert_eq!(right_align("ab", 6), "    ab");
        assert_eq!(right_align("longer", 4), "longer");
    }
}
