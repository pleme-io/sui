use tokio::io::{AsyncRead, AsyncWrite};

use sui_compat::store_path::StorePath;
use sui_store::traits::Store;

use super::dispatch::{absolute, nar_hash_base16, op_err};
use super::wire::{read_string, read_string_list, read_u64, write_string, write_string_list, write_u64};
use super::{Connection, ConnectionError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SubstitutableInfo {
    pub deriver: Option<String>,
    pub references: Vec<String>,
    pub download_size: u64,
    pub nar_size: u64,
}

impl<S, R, W> Connection<S, R, W>
where
    S: Store + 'static,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub(super) async fn substitutable_infos(&self, paths: &[StorePath]) -> Vec<(StorePath, SubstitutableInfo)> {
        if !self.use_substitutes || self.caches.is_empty() {
            return Vec::new();
        }
        let substitutor = self.substitutor();
        let mut out = Vec::new();
        for sp in paths {
            match substitutor.query_caches(sp).await {
                Ok(sui_store::Availability::Substitutable { download_size, nar_size, references, deriver }) => {
                    out.push((sp.clone(), SubstitutableInfo { deriver, references, download_size, nar_size }));
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(path = %sp.to_absolute_path(), error = %e, "substituter query failed"),
            }
        }
        out
    }

    async fn write_substitutable_info(&mut self, info: &SubstitutableInfo) -> Result<(), ConnectionError> {
        let mut references = info.references.clone();
        references.sort();
        write_string(&mut self.writer, &absolute(info.deriver.as_deref().unwrap_or(""))).await?;
        write_string_list(&mut self.writer, &references).await?;
        write_u64(&mut self.writer, info.download_size).await?;
        write_u64(&mut self.writer, info.nar_size).await?;
        Ok(())
    }

    pub(super) async fn handle_query_path_hash(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        let info = self.require_info(&sp).await?;
        self.finish().await?;
        write_string(&mut self.writer, &nar_hash_base16(&info.nar_hash)).await?;
        Ok(())
    }

    pub(super) async fn handle_query_valid_derivers(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        let derivers = self.store.query_valid_derivers(&sp).await?;
        self.finish().await?;
        write_string_list(&mut self.writer, &derivers).await?;
        Ok(())
    }

    pub(super) async fn handle_query_derivation_outputs(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        let map = self.output_map(&sp).await?;
        let mut paths = Vec::with_capacity(map.len());
        for (name, path) in map {
            if path.is_empty() {
                return Err(op_err(format!(
                    "output '{name}' of derivation '{}' has no known path",
                    sp.to_absolute_path()
                )));
            }
            paths.push(path);
        }
        paths.sort();
        paths.dedup();
        self.finish().await?;
        write_string_list(&mut self.writer, &paths).await?;
        Ok(())
    }

    pub(super) async fn handle_query_derivation_output_names(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        if !self.store.is_valid_path(&sp).await? {
            return Err(op_err(format!("path '{}' is not valid", sp.to_absolute_path())));
        }
        let names: Vec<String> = self.load_drv(&sp).map_err(op_err)?.outputs.into_keys().collect();
        self.finish().await?;
        write_string_list(&mut self.writer, &names).await?;
        Ok(())
    }

    pub(super) async fn handle_query_derivation_output_map(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        let map = self.output_map(&sp).await?;
        self.finish().await?;
        write_u64(&mut self.writer, map.len() as u64).await?;
        for (name, path) in &map {
            write_string(&mut self.writer, name).await?;
            write_string(&mut self.writer, path).await?;
        }
        Ok(())
    }

    pub(super) async fn handle_query_path_from_hash_part(&mut self) -> Result<(), ConnectionError> {
        let hash_part = read_string(&mut self.reader).await?;
        let path = self.store.query_path_from_hash_part(&hash_part).await?;
        self.finish().await?;
        write_string(&mut self.writer, path.as_deref().unwrap_or("")).await?;
        Ok(())
    }

    pub(super) async fn handle_query_substitutable_paths(&mut self) -> Result<(), ConnectionError> {
        let paths = self.read_path_set().await?;
        let found: Vec<StorePath> = self.substitutable_infos(&paths).await.into_iter().map(|(p, _)| p).collect();
        self.finish().await?;
        self.write_paths(&found).await
    }

    pub(super) async fn handle_query_substitutable_path_info(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        let infos = self.substitutable_infos(std::slice::from_ref(&sp)).await;
        self.finish().await?;
        match infos.first() {
            None => write_u64(&mut self.writer, 0).await?,
            Some((_, info)) => {
                write_u64(&mut self.writer, 1).await?;
                self.write_substitutable_info(info).await?;
            }
        }
        Ok(())
    }

    pub(super) async fn handle_query_substitutable_path_infos(&mut self) -> Result<(), ConnectionError> {
        let paths = if self.minor() < 22 {
            self.read_path_set().await?
        } else {
            let count = read_u64(&mut self.reader).await?;
            let mut paths = Vec::new();
            for _ in 0..count {
                let path = read_string(&mut self.reader).await?;
                let _ca = read_string(&mut self.reader).await?;
                paths.push(self.parse_path(&path)?);
            }
            paths
        };
        let infos = self.substitutable_infos(&paths).await;
        self.finish().await?;
        write_u64(&mut self.writer, infos.len() as u64).await?;
        for (sp, info) in &infos {
            write_string(&mut self.writer, &sp.to_absolute_path()).await?;
            self.write_substitutable_info(info).await?;
        }
        Ok(())
    }

    pub(super) async fn handle_add_signatures(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        let signatures = read_string_list(&mut self.reader).await?;
        self.store.add_signatures(&sp, &signatures).await?;
        self.finish().await?;
        write_u64(&mut self.writer, 1).await?;
        Ok(())
    }
}
