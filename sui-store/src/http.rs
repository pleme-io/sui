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
}
