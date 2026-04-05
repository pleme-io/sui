# Sui (粋) — Rust-Native Nix Replacement

API-first Nix reimplementation in Rust. Every Nix operation exposed via REST, GraphQL, and gRPC.
Auto-generates SDKs, IaC providers, MCP server, and shell completions from a single OpenAPI spec.

## Architecture

6-crate Cargo workspace + root binary/library:

| Crate | Purpose | Phase |
|-------|---------|-------|
| `sui-compat` | Clean-room Nix formats (NAR, store paths, derivations, wire protocol) | 2 |
| `sui-store` | Store abstraction + SeaORM metadata | 3 |
| `sui-eval` | Bytecode VM Nix evaluator | 4 |
| `sui-build` | Sandboxed builder (Linux namespaces, macOS sandbox-exec) | 5 |
| `sui-daemon` | Worker protocol server (nix-daemon replacement) | 5 |
| `sui-orchestrate` | System rebuild + fleet deployment | 6 |
| `sui` (root) | CLI (clap multicall) + triple-stack API server | 0-1 |

## API Stack

```
REST (axum :8080) ←─┐
GraphQL (async-graphql :8080/graphql) ←── SuiService ←── Domain crates
gRPC (tonic :50051) ←─┘
```

## Build

```bash
cargo check          # workspace check
cargo test           # all tests
cargo run -- serve   # start API server
cargo run -- --help  # CLI help
```

## Conventions

- Edition 2024, Rust 1.89.0+, MIT license
- `clippy::pedantic` warnings enabled
- Release profile: `codegen-units = 1`, `lto = true`, `opt-level = "z"`, `strip = true`
- All code clean-room — no vendored GPL code
- SeaORM for store metadata (1:1 mapping to Nix SQLite schema)
- OpenAPI spec at `spec/openapi.yaml` is the source of truth for all APIs

## Forge Pipeline

`forge-gen.toml` drives auto-generation from the OpenAPI spec:
- MCP server (mcp-forge)
- Terraform + Pulumi providers (iac-forge)
- SDKs: Rust, Python, TypeScript, Go
- Shell completions (completion-forge)
- JSON Schema + API docs
