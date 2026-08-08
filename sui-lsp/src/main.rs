//! `sui-lsp` over stdio.
//!
//! Logging goes to **stderr**, and that is not a style preference: stdout is the
//! JSON-RPC channel, so a single stray line printed there corrupts the framing
//! and the editor sees a dead server with no explanation.

use sui_lsp::server::Backend;
use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("SUI_LSP_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
