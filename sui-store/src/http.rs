//! HTTP client abstraction for binary cache access.
//!
//! Defines the [`HttpClient`] trait so `BinaryCacheStore` can be tested
//! without making real network requests.

/// HTTP response returned by [`HttpClient`] methods.
#[derive(Debug, Clone)]
#[must_use]
pub struct HttpResponse {
    /// HTTP status code (e.g., 200, 404, 500).
    pub status: u16,
    /// Response body decoded as UTF-8 text.
    pub body: String,
}

/// HTTP client errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HttpError {
    /// The HTTP request could not be sent (network error, DNS failure, etc.).
    #[error("request failed: {0}")]
    Request(String),
    /// The response body could not be decoded.
    #[error("decode error: {0}")]
    Decode(String),
}

/// Async HTTP client trait — abstracts over reqwest for testability.
#[async_trait::async_trait]
pub trait HttpClient: Send + Sync {
    /// Send a GET request with custom headers and return the text response.
    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, HttpError>;
    /// Send a GET request and return the raw response bytes.
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, HttpError>;

    /// [`Self::get_bytes`], reporting `(received, expected)` bytes as they
    /// arrive (`expected` is 0 when the server sends no length) and
    /// `(total, total)` at the end, as CppNix's file transfer does.
    async fn get_bytes_observed(&self, url: &str, progress: &(dyn Fn(u64, u64) + Send + Sync)) -> Result<Vec<u8>, HttpError> {
        let bytes = self.get_bytes(url).await?;
        progress(bytes.len() as u64, bytes.len() as u64);
        Ok(bytes)
    }
}

impl HttpResponse {
    /// Returns `true` if the status code is in the 2xx range.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// How a transfer survives a slow or silent server: cppnix's
/// `connect-timeout`, `stalled-download-timeout` and `download-attempts`
/// (libstore filetransfer), with cppnix's defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferPolicy {
    pub connect_timeout: Option<std::time::Duration>,
    pub stall: std::time::Duration,
    pub attempts: u32,
    pub base_retry: std::time::Duration,
}

impl Default for TransferPolicy {
    fn default() -> Self {
        Self {
            connect_timeout: None,
            stall: std::time::Duration::from_secs(300),
            attempts: 5,
            base_retry: std::time::Duration::from_millis(250),
        }
    }
}

impl TransferPolicy {
    /// The policy a nix.conf lookup describes; an absent or unparsable
    /// setting keeps cppnix's default.
    pub fn from_settings<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Self {
        let d = Self::default();
        let num = |k: &str| get(k).and_then(|v| v.trim().parse::<u64>().ok());
        Self {
            connect_timeout: num("connect-timeout").filter(|s| *s > 0).map(std::time::Duration::from_secs),
            stall: num("stalled-download-timeout").map_or(d.stall, std::time::Duration::from_secs),
            attempts: num("download-attempts").map_or(d.attempts, |n| u32::try_from(n.max(1)).unwrap_or(u32::MAX)),
            base_retry: d.base_retry,
        }
    }

    /// The policy this host's nix.conf describes.
    #[must_use]
    pub fn from_nix_conf() -> Self {
        sui_compat::nix_conf::NixConfig::load().map_or_else(|_| Self::default(), |c| Self::from_settings(|k| c.get(k)))
    }

    fn delay(&self, attempt: u32) -> std::time::Duration {
        self.base_retry.saturating_mul(1u32 << attempt.saturating_sub(1).min(10))
    }
}

/// Default [`HttpClient`] backed by reqwest.
pub struct ReqwestHttpClient {
    inner: reqwest::Client,
    policy: TransferPolicy,
}

impl ReqwestHttpClient {
    /// A client under this host's nix.conf transfer policy.
    #[must_use]
    pub fn new() -> Self {
        Self::with_policy(TransferPolicy::from_nix_conf())
    }

    /// A client under `policy`.
    #[must_use]
    pub fn with_policy(policy: TransferPolicy) -> Self {
        let mut builder = reqwest::Client::builder().read_timeout(policy.stall);
        if let Some(t) = policy.connect_timeout {
            builder = builder.connect_timeout(t);
        }
        Self { inner: builder.build().unwrap_or_default(), policy }
    }

    async fn attempt(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<(u16, Vec<u8>), HttpError> {
        let mut req = self.inner.get(url);
        for &(key, value) in headers {
            req = req.header(key, value);
        }
        let mut response = req.send().await.map_err(|e| HttpError::Request(e.to_string()))?;
        let status = response.status().as_u16();
        let expected = response.content_length().unwrap_or(0);
        let mut body = Vec::with_capacity(usize::try_from(expected).unwrap_or(0));
        while let Some(chunk) = response.chunk().await.map_err(|e| HttpError::Decode(e.to_string()))? {
            body.extend_from_slice(&chunk);
            progress(body.len() as u64, expected);
        }
        Ok((status, body))
    }

    async fn fetch(&self, url: &str, headers: &[(&str, &str)]) -> Result<(u16, Vec<u8>), HttpError> {
        self.fetch_observed(url, headers, &|_, _| {}).await
    }

    async fn fetch_observed(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        progress: &(dyn Fn(u64, u64) + Send + Sync),
    ) -> Result<(u16, Vec<u8>), HttpError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = self.attempt(url, headers, progress).await;
            let transient = match &result {
                Err(_) => true,
                Ok((status, _)) => *status == 429 || *status >= 500,
            };
            if !transient || attempt >= self.policy.attempts {
                return result;
            }
            tracing::warn!(url, attempt, "transfer failed; retrying");
            tokio::time::sleep(self.policy.delay(attempt)).await;
        }
    }
}

impl Default for ReqwestHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, HttpError> {
        let (status, body) = self.fetch(url, headers).await?;
        let body = String::from_utf8(body).map_err(|e| HttpError::Decode(e.to_string()))?;
        Ok(HttpResponse { status, body })
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, HttpError> {
        self.get_bytes_observed(url, &|_, _| {}).await
    }

    async fn get_bytes_observed(&self, url: &str, progress: &(dyn Fn(u64, u64) + Send + Sync)) -> Result<Vec<u8>, HttpError> {
        let (status, body) = self.fetch_observed(url, &[], progress).await?;
        if !(200..300).contains(&status) {
            return Err(HttpError::Request(format!("HTTP {status}: {url}")));
        }
        progress(body.len() as u64, body.len() as u64);
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_server_that_never_answers_fails_after_every_attempt_within_the_stall_bound() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = accepted.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                held.push(sock);
            }
        });
        let client = ReqwestHttpClient::with_policy(TransferPolicy {
            connect_timeout: None,
            stall: std::time::Duration::from_millis(200),
            attempts: 3,
            base_retry: std::time::Duration::from_millis(10),
        });
        let url = ["http://", &addr.to_string(), "/x.narinfo"].concat();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), client.get(&url, &[])).await;
        assert!(matches!(result, Ok(Err(_))), "{result:?}");
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_download_reports_its_bytes_as_they_arrive_then_its_total() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<(u64, u64)>();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = [0u8; 1024];
            let _ = sock.read(&mut req).await.unwrap();
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhello").await.unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            sock.write_all(b"world").await.unwrap();
            sock.flush().await.unwrap();
        });
        let client = ReqwestHttpClient::with_policy(TransferPolicy::default());
        let url = ["http://", &addr.to_string(), "/nar/x.nar"].concat();
        let body = client
            .get_bytes_observed(&url, &move |done, expected| {
                let _ = seen_tx.send((done, expected));
            })
            .await
            .unwrap();
        assert_eq!(body, b"helloworld");
        let mut seen = Vec::new();
        while let Ok(p) = seen_rx.try_recv() {
            seen.push(p);
        }
        assert_eq!(seen, vec![(5, 10), (10, 10), (10, 10)]);
    }

    #[test]
    fn the_policy_reads_cppnix_settings_with_cppnix_defaults() {
        let d = TransferPolicy::default();
        assert_eq!((d.stall.as_secs(), d.attempts, d.connect_timeout), (300, 5, None));
        let p = TransferPolicy::from_settings(|k| match k {
            "stalled-download-timeout" => Some("12"),
            "download-attempts" => Some("2"),
            "connect-timeout" => Some("4"),
            _ => None,
        });
        assert_eq!((p.stall.as_secs(), p.attempts, p.connect_timeout.map(|d| d.as_secs())), (12, 2, Some(4)));
    }

    #[test]
    fn http_error_display() {
        let e = HttpError::Request("connection refused".to_string());
        assert!(e.to_string().contains("connection refused"));
    }

    #[test]
    fn reqwest_default() {
        let _client = ReqwestHttpClient::default();
    }

    #[test]
    fn object_safe() {
        fn assert_obj_safe(_: &dyn HttpClient) {}
        assert_obj_safe(&ReqwestHttpClient::new());
    }

    // ── MockHttpClient ────────────────────────────────────

    pub(crate) struct MockHttpClient {
        responses: std::collections::HashMap<String, HttpResponse>,
    }

    impl MockHttpClient {
        pub fn new() -> Self { Self { responses: std::collections::HashMap::new() } }
        pub fn with_response(mut self, url: &str, resp: HttpResponse) -> Self {
            self.responses.insert(url.to_string(), resp);
            self
        }
    }

    #[async_trait::async_trait]
    impl HttpClient for MockHttpClient {
        async fn get(&self, url: &str, _h: &[(&str, &str)]) -> Result<HttpResponse, HttpError> {
            self.responses.get(url).cloned().ok_or_else(|| HttpError::Request(format!("no mock: {url}")))
        }
        async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, HttpError> {
            Ok(self.get(url, &[]).await?.body.into_bytes())
        }
    }

    #[tokio::test]
    async fn mock_client_returns_canned() {
        let client = MockHttpClient::new()
            .with_response("http://test/foo", HttpResponse { status: 200, body: "hello".to_string() });
        let resp = client.get("http://test/foo", &[]).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "hello");
    }

    #[tokio::test]
    async fn mock_client_missing_url() {
        let client = MockHttpClient::new();
        assert!(client.get("http://missing", &[]).await.is_err());
    }

    #[tokio::test]
    async fn mock_client_get_bytes() {
        let client = MockHttpClient::new().with_response(
            "http://test/data",
            HttpResponse {
                status: 200,
                body: "binary-ish content".to_string(),
            },
        );
        let bytes = client.get_bytes("http://test/data").await.unwrap();
        assert_eq!(bytes, b"binary-ish content");
    }

    #[tokio::test]
    async fn mock_client_get_bytes_missing() {
        let client = MockHttpClient::new();
        assert!(client.get_bytes("http://missing").await.is_err());
    }

    #[test]
    fn http_error_decode_display() {
        let e = HttpError::Decode("invalid utf-8".to_string());
        assert!(e.to_string().contains("invalid utf-8"));
    }

    #[test]
    fn http_response_clone() {
        let resp = HttpResponse {
            status: 200,
            body: "ok".to_string(),
        };
        let cloned = resp.clone();
        assert_eq!(cloned.status, 200);
        assert_eq!(cloned.body, "ok");
    }

    #[test]
    fn http_response_debug() {
        let resp = HttpResponse {
            status: 404,
            body: "not found".to_string(),
        };
        let debug = format!("{resp:?}");
        assert!(debug.contains("404"));
        assert!(debug.contains("not found"));
    }

    #[test]
    fn http_error_request_debug() {
        let e = HttpError::Request("timeout".to_string());
        let debug = format!("{e:?}");
        assert!(debug.contains("Request"));
        assert!(debug.contains("timeout"));
    }

    #[tokio::test]
    async fn mock_client_multiple_urls() {
        let client = MockHttpClient::new()
            .with_response(
                "http://test/a",
                HttpResponse { status: 200, body: "alpha".to_string() },
            )
            .with_response(
                "http://test/b",
                HttpResponse { status: 201, body: "beta".to_string() },
            );
        let a = client.get("http://test/a", &[]).await.unwrap();
        let b = client.get("http://test/b", &[]).await.unwrap();
        assert_eq!(a.body, "alpha");
        assert_eq!(b.status, 201);
        assert_eq!(b.body, "beta");
    }

    #[tokio::test]
    async fn mock_client_status_codes() {
        for status in [200, 301, 404, 500, 503] {
            let client = MockHttpClient::new().with_response(
                "http://test/status",
                HttpResponse {
                    status,
                    body: String::new(),
                },
            );
            let resp = client.get("http://test/status", &[]).await.unwrap();
            assert_eq!(resp.status, status);
        }
    }

    #[tokio::test]
    async fn mock_client_empty_body() {
        let client = MockHttpClient::new().with_response(
            "http://test/empty",
            HttpResponse {
                status: 200,
                body: String::new(),
            },
        );
        let resp = client.get("http://test/empty", &[]).await.unwrap();
        assert!(resp.body.is_empty());
    }

    #[tokio::test]
    async fn mock_client_large_body() {
        let large_body = "x".repeat(1_000_000);
        let client = MockHttpClient::new().with_response(
            "http://test/large",
            HttpResponse {
                status: 200,
                body: large_body.clone(),
            },
        );
        let resp = client.get("http://test/large", &[]).await.unwrap();
        assert_eq!(resp.body.len(), 1_000_000);
    }

    #[tokio::test]
    async fn mock_client_get_bytes_returns_utf8_bytes() {
        let client = MockHttpClient::new().with_response(
            "http://test/utf8",
            HttpResponse {
                status: 200,
                body: "héllo wörld".to_string(),
            },
        );
        let bytes = client.get_bytes("http://test/utf8").await.unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "héllo wörld");
    }

    #[tokio::test]
    async fn mock_client_overwrite_response() {
        let client = MockHttpClient::new()
            .with_response(
                "http://test/x",
                HttpResponse { status: 200, body: "first".to_string() },
            )
            .with_response(
                "http://test/x",
                HttpResponse { status: 201, body: "second".to_string() },
            );
        let resp = client.get("http://test/x", &[]).await.unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body, "second");
    }

    // ── HttpResponse::is_success boundary checks ─────────────

    #[test]
    fn is_success_200() {
        let r = HttpResponse {
            status: 200,
            body: String::new(),
        };
        assert!(r.is_success());
    }

    #[test]
    fn is_success_299_inclusive() {
        let r = HttpResponse {
            status: 299,
            body: String::new(),
        };
        assert!(r.is_success());
    }

    #[test]
    fn is_success_300_exclusive() {
        let r = HttpResponse {
            status: 300,
            body: String::new(),
        };
        assert!(!r.is_success());
    }

    #[test]
    fn is_success_199_below_range() {
        let r = HttpResponse {
            status: 199,
            body: String::new(),
        };
        assert!(!r.is_success());
    }

    #[test]
    fn is_success_404() {
        let r = HttpResponse {
            status: 404,
            body: String::new(),
        };
        assert!(!r.is_success());
    }

    #[test]
    fn is_success_500() {
        let r = HttpResponse {
            status: 500,
            body: String::new(),
        };
        assert!(!r.is_success());
    }

    #[test]
    fn is_success_201_created() {
        let r = HttpResponse {
            status: 201,
            body: String::new(),
        };
        assert!(r.is_success());
    }

    #[test]
    fn is_success_204_no_content() {
        let r = HttpResponse {
            status: 204,
            body: String::new(),
        };
        assert!(r.is_success());
    }

    #[test]
    fn is_success_zero_status() {
        let r = HttpResponse {
            status: 0,
            body: String::new(),
        };
        assert!(!r.is_success());
    }

    // ── HttpError equality ──────────────────────────────────

    #[test]
    fn http_error_equality() {
        let a = HttpError::Request("foo".to_string());
        let b = HttpError::Request("foo".to_string());
        assert_eq!(a, b);

        let c = HttpError::Request("bar".to_string());
        assert_ne!(a, c);

        let d = HttpError::Decode("foo".to_string());
        assert_ne!(a, d);
    }

    #[test]
    fn http_error_clone() {
        let a = HttpError::Request("network down".to_string());
        let cloned = a.clone();
        assert_eq!(a, cloned);
    }

    // ── HttpResponse with different headers ─────────────────

    #[tokio::test]
    async fn mock_client_ignores_request_headers() {
        // MockHttpClient doesn't differentiate by headers — verify that
        // requests with different headers all hit the same mocked URL.
        let client = MockHttpClient::new().with_response(
            "http://test/x",
            HttpResponse {
                status: 200,
                body: "ok".to_string(),
            },
        );
        let r1 = client
            .get("http://test/x", &[("Accept", "text/plain")])
            .await
            .unwrap();
        let r2 = client
            .get("http://test/x", &[("Accept", "application/json")])
            .await
            .unwrap();
        let r3 = client.get("http://test/x", &[]).await.unwrap();
        assert_eq!(r1.body, "ok");
        assert_eq!(r2.body, "ok");
        assert_eq!(r3.body, "ok");
    }

    // ── MockHttpClient: malformed JSON body ─────────────────

    #[tokio::test]
    async fn mock_client_malformed_json_body() {
        let client = MockHttpClient::new().with_response(
            "http://test/api",
            HttpResponse {
                status: 200,
                body: "{not valid json".to_string(),
            },
        );
        let resp = client.get("http://test/api", &[]).await.unwrap();
        // The mock client doesn't validate JSON — it returns the body as-is.
        // Consumers must validate.
        assert_eq!(resp.body, "{not valid json");
        assert!(serde_json::from_str::<serde_json::Value>(&resp.body).is_err());
    }

    #[tokio::test]
    async fn mock_client_valid_json_body() {
        let client = MockHttpClient::new().with_response(
            "http://test/api",
            HttpResponse {
                status: 200,
                body: r#"{"key":"value","n":42}"#.to_string(),
            },
        );
        let resp = client.get("http://test/api", &[]).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(parsed["key"], "value");
        assert_eq!(parsed["n"], 42);
    }

    // ── MockHttpClient: get_bytes for binary-ish content ────

    #[tokio::test]
    async fn mock_client_get_bytes_with_pseudo_binary() {
        // MockHttpClient stores the body as String, so true binary
        // (non-UTF8) data isn't representable. Use UTF-8 escape sequences.
        let client = MockHttpClient::new().with_response(
            "http://test/bin",
            HttpResponse {
                status: 200,
                body: "\u{0001}\u{0002}\u{0003}".to_string(),
            },
        );
        let bytes = client.get_bytes("http://test/bin").await.unwrap();
        assert_eq!(bytes.len(), 3);
        assert_eq!(bytes[0], 0x01);
        assert_eq!(bytes[1], 0x02);
        assert_eq!(bytes[2], 0x03);
    }

    // ── HttpResponse Default-ish patterns ───────────────────

    #[test]
    fn http_response_zero_status_construction() {
        let r = HttpResponse {
            status: 0,
            body: String::new(),
        };
        assert_eq!(r.status, 0);
        assert!(r.body.is_empty());
    }

    // ── ReqwestHttpClient construction methods ──────────────

    #[test]
    fn reqwest_new_does_not_panic() {
        let _client = ReqwestHttpClient::new();
    }

    #[test]
    fn reqwest_default_equivalent_to_new() {
        let _a = ReqwestHttpClient::new();
        let _b = ReqwestHttpClient::default();
    }

    #[test]
    fn reqwest_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ReqwestHttpClient>();
    }

    // ── Trait object dispatch ───────────────────────────────

    #[tokio::test]
    async fn dyn_http_client_get_via_trait_object() {
        let client: Box<dyn HttpClient> = Box::new(MockHttpClient::new().with_response(
            "http://test/dyn",
            HttpResponse {
                status: 200,
                body: "via dyn".to_string(),
            },
        ));
        let resp = client.get("http://test/dyn", &[]).await.unwrap();
        assert_eq!(resp.body, "via dyn");
    }

    #[tokio::test]
    async fn dyn_http_client_get_bytes_via_trait_object() {
        let client: Box<dyn HttpClient> = Box::new(MockHttpClient::new().with_response(
            "http://test/dyn",
            HttpResponse {
                status: 200,
                body: "bytes via dyn".to_string(),
            },
        ));
        let bytes = client.get_bytes("http://test/dyn").await.unwrap();
        assert_eq!(bytes, b"bytes via dyn");
    }

    // ── HttpError variant coverage ──────────────────────────

    #[test]
    fn http_error_request_variant_message() {
        let e = HttpError::Request("ENETUNREACH".to_string());
        assert!(e.to_string().contains("request failed"));
        assert!(e.to_string().contains("ENETUNREACH"));
    }

    #[test]
    fn http_error_decode_variant_message() {
        let e = HttpError::Decode("invalid utf-8 sequence".to_string());
        assert!(e.to_string().contains("decode error"));
        assert!(e.to_string().contains("invalid utf-8"));
    }

    // ── HttpResponse field mutation ─────────────────────────

    #[test]
    fn http_response_mutable_fields() {
        let mut r = HttpResponse {
            status: 200,
            body: "old".to_string(),
        };
        r.status = 404;
        r.body = "new".to_string();
        assert_eq!(r.status, 404);
        assert_eq!(r.body, "new");
    }
}
