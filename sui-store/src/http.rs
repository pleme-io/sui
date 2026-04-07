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
}

impl HttpResponse {
    /// Returns `true` if the status code is in the 2xx range.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Default [`HttpClient`] backed by reqwest.
pub struct ReqwestHttpClient {
    inner: reqwest::Client,
}

impl ReqwestHttpClient {
    /// Create a new HTTP client with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: reqwest::Client::new(),
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
        let mut req = self.inner.get(url);
        for &(key, value) in headers {
            req = req.header(key, value);
        }

        let response = req
            .send()
            .await
            .map_err(|e| HttpError::Request(e.to_string()))?;

        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| HttpError::Decode(e.to_string()))?;

        Ok(HttpResponse { status, body })
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, HttpError> {
        let response = self
            .inner
            .get(url)
            .send()
            .await
            .map_err(|e| HttpError::Request(e.to_string()))?;

        if !response.status().is_success() {
            return Err(HttpError::Request(format!(
                "HTTP {}: {url}",
                response.status()
            )));
        }

        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| HttpError::Decode(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
