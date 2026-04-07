//! Sui — Rust-native Nix replacement with API-first design.
//!
//! This is the root crate of the Sui workspace. It provides the CLI binary
//! (`sui`) and a triple-stack API server (REST + GraphQL + gRPC) that delegates
//! to the domain crates (`sui-compat`, `sui-store`, `sui-eval`, `sui-build`,
//! `sui-daemon`, `sui-orchestrate`).

pub mod api;

/// Default path to the Nix SQLite database.
pub const NIX_DB_PATH: &str = "/nix/var/nix/db/db.sqlite";
