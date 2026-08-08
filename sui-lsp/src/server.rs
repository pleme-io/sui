//! The `tower-lsp` shell: document state + protocol wiring, and as little
//! judgement as possible. Everything worth testing lives in
//! [`crate::diagnostics`].

use std::collections::HashMap;

use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::{
    Diagnostic as LspDiagnostic, DiagnosticSeverity, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, InitializeParams, InitializeResult,
    InitializedParams, MessageType, NumberOrString, Position as LspPosition, Range as LspRange,
    ServerCapabilities, ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use tower_lsp::{Client, LanguageServer};

use crate::diagnostics::{check, Diagnostic, Severity};

/// Open documents, by URI.
///
/// The server declares [`TextDocumentSyncKind::FULL`], so a change notification
/// carries the entire new text and this map is simply overwritten. Incremental
/// sync would be less traffic and a great deal more state — an editor and a
/// server disagreeing about the result of a patch sequence is a class of bug
/// worth not having at M0. Revisit when a file is large enough to measure.
#[derive(Debug, Default)]
struct Documents(RwLock<HashMap<Url, String>>);

pub struct Backend {
    client: Client,
    docs: Documents,
}

impl Backend {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client,
            docs: Documents::default(),
        }
    }

    /// Re-check one document and publish the result.
    ///
    /// Publishing an **empty** vector is not a no-op and must not be skipped:
    /// it is the only way to tell the editor that previously-reported problems
    /// are gone. A server that publishes only when it has something to say
    /// leaves stale squiggles on a file the user just fixed.
    async fn publish(&self, uri: Url, text: &str, version: Option<i32>) {
        let found = check(text);
        let lsp: Vec<LspDiagnostic> = found.iter().map(to_lsp).collect();
        self.client.publish_diagnostics(uri, lsp, version).await;
    }
}

fn to_lsp(d: &Diagnostic) -> LspDiagnostic {
    LspDiagnostic {
        range: LspRange {
            start: LspPosition::new(d.range.start.line, d.range.start.character),
            end: LspPosition::new(d.range.end.line, d.range.end.character),
        },
        severity: Some(match d.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
        }),
        code: Some(NumberOrString::String(d.finding.code().to_string())),
        source: Some("sui".to_string()),
        message: d.message(),
        ..Default::default()
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> RpcResult<InitializeResult> {
        Ok(InitializeResult {
            // Advertise ONLY what is implemented. A capability claimed and not
            // served makes the editor offer a feature that silently does
            // nothing, which reads to the user as the editor being broken.
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "sui-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "sui-lsp ready")
            .await;
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, p: DidOpenTextDocumentParams) {
        let doc = p.text_document;
        self.docs
            .0
            .write()
            .await
            .insert(doc.uri.clone(), doc.text.clone());
        self.publish(doc.uri, &doc.text, Some(doc.version)).await;
    }

    async fn did_change(&self, mut p: DidChangeTextDocumentParams) {
        // FULL sync: the last change carries the whole document. Taking the
        // last (not the first) matters if a client ever batches.
        let Some(change) = p.content_changes.pop() else {
            return;
        };
        let uri = p.text_document.uri;
        self.docs
            .0
            .write()
            .await
            .insert(uri.clone(), change.text.clone());
        self.publish(uri, &change.text, Some(p.text_document.version))
            .await;
    }

    async fn did_close(&self, p: DidCloseTextDocumentParams) {
        let uri = p.text_document.uri;
        self.docs.0.write().await.remove(&uri);
        // Clear on close: a diagnostic outliving its document is a squiggle on
        // a file the user cannot see to fix.
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }
}

#[cfg(test)]
mod tests {
    use super::to_lsp;
    use crate::diagnostics::check;
    use tower_lsp::lsp_types::{DiagnosticSeverity, NumberOrString};

    #[test]
    fn a_finding_maps_onto_the_wire_type_without_losing_its_code() {
        let found = check("{ x = ; }");
        assert!(!found.is_empty());
        let d = to_lsp(&found[0]);
        assert_eq!(d.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(d.source.as_deref(), Some("sui"));
        assert!(!d.message.is_empty());
        match d.code {
            Some(NumberOrString::String(c)) => assert!(c.starts_with("sui/")),
            other => panic!("expected a string code, got {other:?}"),
        }
    }

    /// The wire range must be the same numbers the pure layer computed —
    /// a transcription bug here would misplace every squiggle while both
    /// layers' own tests stayed green.
    #[test]
    fn the_wire_range_matches_the_computed_range() {
        let found = check("let a = 1;\n    b = ;\nin a");
        assert!(!found.is_empty());
        for f in &found {
            let d = to_lsp(f);
            assert_eq!(d.range.start.line, f.range.start.line);
            assert_eq!(d.range.start.character, f.range.start.character);
            assert_eq!(d.range.end.line, f.range.end.line);
            assert_eq!(d.range.end.character, f.range.end.character);
        }
    }
}
