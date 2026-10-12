#![allow(clippy::missing_errors_doc)]

use crate::push::{NarCodec, PushResult, push_path};
use crate::signing::CacheSigner;
use crate::{CacheError, LocalStorage, StorageBackend};

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("cache url {0:?} is not an http(s) url")]
    BaseUrl(String),
    #[error("{method} {url}: {source}")]
    Transport {
        method: &'static str,
        url: String,
        source: reqwest::Error,
    },
    #[error("{method} {url}: HTTP {status}")]
    Status {
        method: &'static str,
        url: String,
        status: u16,
    },
    #[error("staging {hash}: {source}")]
    Stage { hash: String, source: CacheError },
    #[error("staging {hash}: the staged narinfo or its NAR is missing")]
    Staged { hash: String },
}

#[derive(Debug, Clone)]
pub enum RemoteOutcome {
    Present,
    Pushed(PushResult),
}

#[derive(Debug, Clone)]
pub struct RemoteCache {
    client: reqwest::Client,
    base: String,
}

impl RemoteCache {
    pub fn new(base_url: &str) -> Result<Self, RemoteError> {
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(RemoteError::BaseUrl(base_url.to_string()));
        }
        Ok(Self {
            client: reqwest::Client::new(),
            base: base_url.trim_end_matches('/').to_string(),
        })
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub async fn has_narinfo(&self, hash: &str) -> Result<bool, RemoteError> {
        let url = format!("{}/{hash}.narinfo", self.base);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|source| RemoteError::Transport {
                method: "GET",
                url: url.clone(),
                source,
            })?;
        match resp.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            status => Err(RemoteError::Status {
                method: "GET",
                url,
                status,
            }),
        }
    }

    pub async fn put_nar(&self, nar_url: &str, data: Vec<u8>) -> Result<(), RemoteError> {
        self.put(format!("{}/{nar_url}", self.base), data).await
    }

    pub async fn put_narinfo(&self, hash: &str, narinfo: &str) -> Result<(), RemoteError> {
        self.put(
            format!("{}/{hash}.narinfo", self.base),
            narinfo.as_bytes().to_vec(),
        )
        .await
    }

    async fn put(&self, url: String, body: Vec<u8>) -> Result<(), RemoteError> {
        let resp = self
            .client
            .put(&url)
            .body(body)
            .send()
            .await
            .map_err(|source| RemoteError::Transport {
                method: "PUT",
                url: url.clone(),
                source,
            })?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(RemoteError::Status {
                method: "PUT",
                url,
                status: resp.status().as_u16(),
            })
        }
    }
}

pub async fn push_path_remote(
    remote: &RemoteCache,
    signer: &CacheSigner,
    store_path: &str,
    hash: &str,
    references: &[String],
    deriver: Option<&str>,
    codec: NarCodec,
) -> Result<RemoteOutcome, RemoteError> {
    if remote.has_narinfo(hash).await? {
        return Ok(RemoteOutcome::Present);
    }

    let stage_err = |source: CacheError| RemoteError::Stage {
        hash: hash.to_string(),
        source,
    };
    let staged = || RemoteError::Staged {
        hash: hash.to_string(),
    };

    let stage = tempfile::tempdir().map_err(|e| stage_err(CacheError::Io(e)))?;
    let storage = LocalStorage::new(stage.path());
    let result = push_path(
        &storage, signer, store_path, hash, references, deriver, codec,
    )
    .await
    .map_err(stage_err)?;

    let narinfo = storage
        .get_narinfo(hash)
        .await
        .map_err(stage_err)?
        .ok_or_else(staged)?;
    let nar_url = crate::advertised_url_line(&narinfo)
        .ok_or_else(staged)?
        .to_string();
    let nar = storage
        .get_nar(&nar_url)
        .await
        .map_err(stage_err)?
        .ok_or_else(staged)?;

    remote.put_nar(&nar_url, nar).await?;
    remote.put_narinfo(hash, &narinfo).await?;
    stage.close().map_err(|e| stage_err(CacheError::Io(e)))?;

    Ok(RemoteOutcome::Pushed(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CacheConfig;
    use crate::server::{AppState, build_router};
    use std::sync::Arc;
    use sui_compat::narinfo::NarInfo;

    async fn spawn_origin(
        dir: &std::path::Path,
        upload_keys: Vec<String>,
        origin: Arc<CacheSigner>,
    ) -> String {
        let storage: Arc<dyn StorageBackend> = Arc::new(LocalStorage::new(dir));
        let app = build_router(AppState {
            storage,
            config: CacheConfig {
                upload_keys,
                ..CacheConfig::default()
            },
            signer: Some(origin),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/")
    }

    fn fake_store_path(root: &std::path::Path) -> String {
        let path = root.join("rmt0-hello");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("greeting"), b"hello").unwrap();
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn remote_push_round_trips_through_a_gated_origin() {
        let cache_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let uploader = CacheSigner::generate("ci-1".to_string());
        let origin = Arc::new(CacheSigner::generate("origin-1".to_string()));
        let url = spawn_origin(
            cache_dir.path(),
            vec![uploader.public_key_string()],
            origin.clone(),
        )
        .await;
        let remote = RemoteCache::new(&url).unwrap();
        let store_path = fake_store_path(store_dir.path());

        assert!(!remote.has_narinfo("rmt0").await.unwrap());
        let first = push_path_remote(
            &remote,
            &uploader,
            &store_path,
            "rmt0",
            &[],
            None,
            NarCodec::default(),
        )
        .await
        .unwrap();
        assert!(matches!(first, RemoteOutcome::Pushed(_)));
        assert!(remote.has_narinfo("rmt0").await.unwrap());

        let second = push_path_remote(
            &remote,
            &uploader,
            &store_path,
            "rmt0",
            &[],
            None,
            NarCodec::default(),
        )
        .await
        .unwrap();
        assert!(matches!(second, RemoteOutcome::Present));

        let client = reqwest::Client::new();
        let text = client
            .get(format!("{}/rmt0.narinfo", remote.base_url()))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let info = NarInfo::parse(&text).unwrap();
        assert!(crate::signing::narinfo_signed_by_any(
            &info,
            &[uploader.public_key_string()]
        ));
        assert!(crate::signing::narinfo_signed_by_any(
            &info,
            &[origin.public_key_string()]
        ));
        let nar = client
            .get(format!("{}/{}", remote.base_url(), info.url))
            .send()
            .await
            .unwrap();
        assert_eq!(nar.status().as_u16(), 200);
        assert_eq!(nar.bytes().await.unwrap().len() as u64, info.file_size);
    }

    #[tokio::test]
    async fn remote_push_signed_by_a_non_upload_key_is_refused() {
        let cache_dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let uploader = CacheSigner::generate("ci-1".to_string());
        let stranger = CacheSigner::generate("ci-1".to_string());
        let origin = Arc::new(CacheSigner::generate("origin-1".to_string()));
        let url = spawn_origin(cache_dir.path(), vec![uploader.public_key_string()], origin).await;
        let remote = RemoteCache::new(&url).unwrap();
        let store_path = fake_store_path(store_dir.path());

        let err = push_path_remote(
            &remote,
            &stranger,
            &store_path,
            "rmt0",
            &[],
            None,
            NarCodec::default(),
        )
        .await
        .expect_err("a non-upload key must be refused");
        assert!(
            matches!(err, RemoteError::Status { status: 403, .. }),
            "{err}"
        );
        assert!(!remote.has_narinfo("rmt0").await.unwrap());
    }

    #[test]
    fn a_non_http_cache_url_is_refused() {
        assert!(matches!(
            RemoteCache::new("file:///var/cache/sui"),
            Err(RemoteError::BaseUrl(_))
        ));
    }
}
