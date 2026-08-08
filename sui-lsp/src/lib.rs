//! `sui-lsp` — a Nix language server built on sui's own parser and lowering.
//!
//! # Why this is not another `nil` / `nixd`
//!
//! Both existing servers re-implement a Nix front end in order to answer editor
//! questions, which means the editor's idea of a file and the evaluator's idea
//! of the same file are two independent implementations that agree only by
//! effort. sui already owns a parser (`rnix`), a resolver (`sui-resolve`), a
//! lowering pass (`sui-ir`) and an evaluator — so the interesting move is not
//! writing a third front end, it is **exposing the one that already evaluates
//! the file**. A diagnostic here is not a lookalike of what sui would say; it is
//! what sui says.
//!
//! # Shape
//!
//! [`diagnostics`] is **pure** — `&str` in, `Vec<Diagnostic>` out, no async, no
//! LSP types, no IO — and holds every decision worth testing. [`server`] is the
//! thin `tower-lsp` shell that owns document state and does the protocol. The
//! split is the fleet's mockable-seam default: the logic is provable without a
//! client, and the part that needs a client has almost no logic in it.
//!
//! Positions come from [`zahyou`], shared with `escriba-lsp-client` — the same
//! UTF-16 arithmetic on both ends of the wire, which is the only way the two
//! agree about where a squiggle goes.
//!
//! # M0 scope, stated so it is not mistaken for more
//!
//! Diagnostics on open and on change. **No** hover, goto-definition, completion
//! or workspace symbols yet — those need the resolver's scope chain surfaced,
//! which is the next slice. `initialize` advertises exactly what is implemented.

pub mod diagnostics;
pub mod server;

pub use diagnostics::{check, Anchor, Diagnostic, Finding, Severity};
