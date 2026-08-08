//! End-to-end: spawn the real `sui-lsp` binary and speak LSP to it over stdio.
//!
//! The unit tests prove the diagnostics are right. They prove nothing about
//! whether an editor can talk to this process, and that is a separate and
//! easier thing to get wrong — a `Content-Length` counted in characters, a
//! stray line on stdout, a capability advertised but never served. Each of
//! those leaves every unit test green and the editor showing a dead server.
//!
//! So this test uses the actual compiled binary, not an in-process `Backend`.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};

/// Write one LSP frame. `Content-Length` counts **bytes**, which is why the
/// length comes from `as_bytes()` and never from `.len()` on a `&str` the
/// author assumed was ASCII.
async fn send(stdin: &mut ChildStdin, body: &str) {
    let bytes = body.as_bytes();
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
        .await
        .unwrap();
    stdin.write_all(bytes).await.unwrap();
    stdin.flush().await.unwrap();
}

/// Read one LSP frame: headers to the blank line, then exactly that many bytes.
async fn recv(out: &mut BufReader<ChildStdout>) -> String {
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        let n = out.read_line(&mut line).await.unwrap();
        assert_ne!(n, 0, "server closed the stream mid-header");
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some(v) = t.strip_prefix("Content-Length: ") {
            len = v.parse().unwrap();
        }
    }
    assert_ne!(len, 0, "frame had no Content-Length");
    let mut buf = vec![0u8; len];
    out.read_exact(&mut buf).await.unwrap();
    String::from_utf8(buf).unwrap()
}

/// Read frames until one contains `needle`, so the test is not thrown off by
/// the `window/logMessage` the server emits on `initialized`.
async fn recv_until(out: &mut BufReader<ChildStdout>, needle: &str) -> String {
    for _ in 0..10 {
        let f = recv(out).await;
        if f.contains(needle) {
            return f;
        }
    }
    panic!("never saw a frame containing {needle:?}");
}

#[tokio::test]
async fn the_binary_completes_a_handshake_and_reports_a_real_diagnostic() {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_sui-lsp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("sui-lsp binary should spawn");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // Every await is wrapped: a regression that hangs the server must FAIL the
    // suite rather than hang it, which is the whole reason to test the process.
    let body = tokio::time::timeout(Duration::from_secs(20), async {
        send(
            &mut stdin,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#,
        )
        .await;
        let init = recv_until(&mut stdout, r#""id":1"#).await;
        assert!(init.contains("sui-lsp"), "serverInfo missing: {init}");
        assert!(
            init.contains("textDocumentSync"),
            "sync capability missing: {init}"
        );

        send(
            &mut stdin,
            r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        )
        .await;

        // A file that is broken on line 1 — and whose line 0 holds an emoji, so
        // a byte-counted column would put the squiggle in the wrong place.
        //
        // NOTE the `r##"…"##`: the payload contains `"#` (a Nix comment right
        // after a JSON quote), which would close an `r#"…"#` literal.
        send(
            &mut stdin,
            r##"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///t.nix","languageId":"nix","version":1,"text":"# 🎉 header\n{ x = ; }\n"}}}"##,
        )
        .await;
        recv_until(&mut stdout, "publishDiagnostics").await
    })
    .await
    .expect("handshake and first diagnostic must arrive well inside 20s");

    assert!(body.contains("file:///t.nix"), "wrong document: {body}");
    assert!(body.contains(r#""source":"sui""#), "unsourced: {body}");
    assert!(body.contains(r#""sui/"#), "no stable code: {body}");
    assert!(
        body.contains(r#""line":1"#),
        "the error is on line 1, not line 0: {body}"
    );

    child.kill().await.ok();
}

/// Fixing a file must *clear* the squiggles. A server that publishes only when
/// it has something to say leaves stale errors on a file the user just fixed —
/// an empty `diagnostics` array is a message, not a no-op.
#[tokio::test]
async fn correcting_a_document_clears_its_diagnostics() {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_sui-lsp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    let body = tokio::time::timeout(Duration::from_secs(20), async {
        send(
            &mut stdin,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#,
        )
        .await;
        recv_until(&mut stdout, r#""id":1"#).await;
        send(
            &mut stdin,
            r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        )
        .await;
        send(
            &mut stdin,
            r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///f.nix","languageId":"nix","version":1,"text":"{ x = ; }"}}}"#,
        )
        .await;
        let first = recv_until(&mut stdout, "publishDiagnostics").await;
        assert!(
            first.contains(r#""sui/"#),
            "the broken version must report something: {first}"
        );

        send(
            &mut stdin,
            r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///f.nix","version":2},"contentChanges":[{"text":"{ x = 1; }"}]}}"#,
        )
        .await;
        recv_until(&mut stdout, "publishDiagnostics").await
    })
    .await
    .expect("the cleared publish must arrive well inside 20s");

    assert!(
        body.contains(r#""diagnostics":[]"#),
        "fixing the file must publish an EMPTY list, got: {body}"
    );

    child.kill().await.ok();
}
