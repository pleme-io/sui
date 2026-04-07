//! HTTP client abstraction for binary cache access.
//!
//! Defines the [`HttpClient`] trait so `BinaryCacheStore` can be tested
//! without making real network requests.

/// HTTP response returned by [`HttpClient`] methods.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// HTTP client errors.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("request failed: {0}")]
    Request(String),
    #[error("decode error: {0}")]
    Decode(String),
}

/// Async HTTP client trait — abstracts over reqwest for testability.
#[async_trait::async_trait]
pub trait HttpClient: Send + Sync {
    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, HttpError>;
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, HttpError>;
}

/// Default [`HttpClient`] backed by reqwest.
pub struct ReqwestHttpClient {
    inner: reqwest::Client,
}

impl ReqwestHttpClient {
    pub fn new() -> Self {
        Self { inner: reqwest::Client::new() }
    }
}

impl Default for ReqwestHttpClient {
    fn default() -> Self { Self::new() }
}

#[async_trait::async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, HttpError> {
        let mut req = self.inner.get(url);
        for &(key, value) in headers {
            req = req.header(key, value);
        }
        let response = req.send().await.map_err(|e| HttpError::Request(e.to_string()))?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(|e| HttpError::Decode(e.to_string()))?;
        Ok(HttpResponse { status, body })
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, HttpError> {
        let response = self.inner.get(url).send().await.map_err(|e| HttpError::Request(e.to_string()))?;
        if !response.status().is_success() {
            return Err(HttpError::Request(format!("HTTP {}: {url}", response.status())));
        }
        response.bytes().await.map(|b| b.to_vec()).map_err(|e| HttpError::Decode(e.to_string()))
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
}
