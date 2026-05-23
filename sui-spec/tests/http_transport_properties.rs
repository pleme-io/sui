//! Property tests for the HttpTransport substrate primitive.
//!
//! No network access — all properties exercise FsTransport,
//! MockTransport, and SchemeRouter using temp files + canned
//! responses.

use proptest::prelude::*;
use sui_spec::fetcher::{
    FsTransport, HttpError, HttpTransport, MockTransport, SchemeRouter,
};

// ── FsTransport properties ─────────────────────────────────

fn tmpfile_with(content: &[u8]) -> std::path::PathBuf {
    let id = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let p = std::env::temp_dir().join(format!("sui-spec-fs-prop-{id}-{nanos}"));
    std::fs::write(&p, content).unwrap();
    p
}

proptest! {
    /// FsTransport always returns the same bytes the file holds.
    #[test]
    fn fs_transport_roundtrips_bytes(
        content in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        let path = tmpfile_with(&content);
        let url = format!("file://{}", path.display());
        let got = FsTransport.get(&url).unwrap();
        let _ = std::fs::remove_file(&path);
        prop_assert_eq!(got, content);
    }

    /// Non-file URLs always return UnsupportedScheme.
    #[test]
    fn fs_transport_rejects_non_file_scheme(
        host in "[a-z]{3,10}\\.[a-z]{2,5}",
    ) {
        let url = format!("https://{host}/x");
        match FsTransport.get(&url).unwrap_err() {
            HttpError::UnsupportedScheme(s) => prop_assert_eq!(s, "https"),
            other => prop_assert!(false, "unexpected error: {other:?}"),
        }
    }

    /// Malformed URLs always error with BadUrl.
    #[test]
    fn fs_transport_rejects_malformed_url(
        s in "[^:/].{0,30}",
    ) {
        // Reject inputs that happen to parse — focus the property
        // on truly-malformed inputs.
        if url::Url::parse(&s).is_ok() {
            return Ok(());
        }
        match FsTransport.get(&s).unwrap_err() {
            HttpError::BadUrl(_) => {},
            other => prop_assert!(false, "expected BadUrl, got: {other:?}"),
        }
    }
}

// ── MockTransport properties ──────────────────────────────

proptest! {
    /// MockTransport always returns the registered bytes for
    /// registered URLs.
    #[test]
    fn mock_returns_registered_bytes(
        url in "https?://[a-z]{2,8}/[a-z]{1,6}",
        content in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let mock = MockTransport::default().with(&url, content.clone());
        let got = mock.get(&url).unwrap();
        prop_assert_eq!(got, content);
    }

    /// Unregistered URLs always return NotFound.
    #[test]
    fn mock_unregistered_url_is_not_found(
        url in "https?://[a-z]{2,8}/[a-z]{1,6}",
    ) {
        let mock = MockTransport::default();
        match mock.get(&url).unwrap_err() {
            HttpError::NotFound(u) => prop_assert_eq!(u, url),
            other => prop_assert!(false, "unexpected: {other:?}"),
        }
    }
}

// ── SchemeRouter properties ───────────────────────────────

proptest! {
    /// Router dispatches `file://` through FsTransport.
    #[test]
    fn router_routes_file_urls_to_fs(
        content in proptest::collection::vec(any::<u8>(), 1..256),
    ) {
        let path = tmpfile_with(&content);
        let url = format!("file://{}", path.display());
        let router = SchemeRouter::new(MockTransport::default());
        let got = router.get(&url).unwrap();
        let _ = std::fs::remove_file(&path);
        prop_assert_eq!(got, content);
    }

    /// Router dispatches `http://` and `https://` through the
    /// remote transport.
    #[test]
    fn router_routes_http_urls_to_remote(
        bytes in proptest::collection::vec(any::<u8>(), 0..128),
    ) {
        let url = "http://example.com/x";
        let mock = MockTransport::default().with(url, bytes.clone());
        let router = SchemeRouter::new(mock);
        let got = router.get(url).unwrap();
        prop_assert_eq!(got, bytes);
    }

    /// Router refuses unknown schemes with UnsupportedScheme.
    #[test]
    fn router_rejects_unknown_schemes(
        scheme in prop::sample::select(&["ftp", "ssh", "git", "ldap"][..]),
        host in "[a-z]{3,8}",
    ) {
        let url = format!("{scheme}://{host}/x");
        let router = SchemeRouter::new(MockTransport::default());
        match router.get(&url).unwrap_err() {
            HttpError::UnsupportedScheme(s) => prop_assert_eq!(s, scheme.to_string()),
            other => prop_assert!(false, "unexpected: {other:?}"),
        }
    }
}

// ── HttpError shape tests ─────────────────────────────────

#[test]
fn http_error_display_includes_category() {
    let err = HttpError::NetworkFailure("timeout".into());
    let msg = format!("{err}");
    assert!(msg.contains("network"));
    assert!(msg.contains("timeout"));
}

#[test]
fn http_error_is_clone_eq() {
    let a = HttpError::NotFound("x".into());
    let b = a.clone();
    assert_eq!(a, b);
}

#[test]
fn missing_file_returns_not_found() {
    let url = "file:///nonexistent/path/x";
    match FsTransport.get(url).unwrap_err() {
        HttpError::NotFound(_) => {},
        other => panic!("expected NotFound, got: {other:?}"),
    }
}
