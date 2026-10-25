use tokio::io::{AsyncRead, AsyncWrite};

use sui_compat::store_path::StorePath;
use sui_store::traits::Store;

use super::dispatch::op_err;
use super::wire::{read_framed_source, read_string, read_u64, write_bool, write_string, write_u64};
use super::{Connection, ConnectionError};

impl<S, R, W> Connection<S, R, W>
where
    S: Store + 'static,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub(super) async fn handle_find_roots(&mut self) -> Result<(), ConnectionError> {
        let store_dir = sui_compat::store_path::DEFAULT_STORE_DIR;
        let mut links = sui_store::find_root_links(&self.state_dir, store_dir);
        let censor = !self.trusted();
        let state_dir = self.state_dir.clone();
        let runtime = tokio::task::spawn_blocking(move || {
            let mut found = sui_store::find_runtime_roots(store_dir);
            found.extend(sui_store::find_temp_roots(&state_dir, store_dir));
            found
        })
        .await
        .unwrap_or_default();
        links.extend(runtime.into_iter().map(|(link, target)| (if censor { "{censored}".to_string() } else { link }, target)));
        let mut roots = Vec::with_capacity(links.len());
        for (link, target) in links {
            let Ok(sp) = StorePath::from_absolute_path(&target) else {
                continue;
            };
            if self.store.is_valid_path(&sp).await? {
                roots.push((target, link));
            }
        }
        roots.sort();
        roots.dedup();
        self.finish().await?;
        write_u64(&mut self.writer, roots.len() as u64).await?;
        for (target, link) in &roots {
            write_string(&mut self.writer, link).await?;
            write_string(&mut self.writer, target).await?;
        }
        Ok(())
    }

    pub(super) async fn handle_collect_garbage(&mut self) -> Result<(), ConnectionError> {
        let _action = read_u64(&mut self.reader).await?;
        let _paths = self.read_path_set().await?;
        let ignore_liveness = read_u64(&mut self.reader).await? != 0;
        let _max_freed = read_u64(&mut self.reader).await?;
        for _ in 0..3 {
            read_u64(&mut self.reader).await?;
        }
        if ignore_liveness {
            return Err(op_err("you are not allowed to ignore liveness"));
        }
        Err(op_err(
            "sui-daemon does not collect garbage: it does not record temporary roots, so it cannot prove a path dead",
        ))
    }

    pub(super) async fn handle_optimise_store(&mut self) -> Result<(), ConnectionError> {
        let optimised = self.store.optimise_store(false).await?;
        tracing::info!(%optimised, "OptimiseStore");
        self.finish().await?;
        write_u64(&mut self.writer, 1).await?;
        Ok(())
    }

    pub(super) async fn handle_verify_store(&mut self) -> Result<(), ConnectionError> {
        let check_contents = read_u64(&mut self.reader).await? != 0;
        let repair = read_u64(&mut self.reader).await? != 0;
        if repair && !self.trusted() {
            return Err(op_err("you are not privileged to repair paths"));
        }
        if repair {
            return Err(op_err("sui-daemon does not repair paths"));
        }
        let errors = if check_contents {
            !self.store.verify_store().await?.corrupt.is_empty()
        } else {
            let mut missing = false;
            for sp in self.store.query_all_valid_paths().await? {
                if std::fs::symlink_metadata(self.store_dir.join(sp.to_basename())).is_err() {
                    tracing::warn!(path = %sp.to_absolute_path(), "path disappeared");
                    missing = true;
                }
            }
            missing
        };
        self.finish().await?;
        write_bool(&mut self.writer, errors).await?;
        Ok(())
    }

    pub(super) async fn handle_add_build_log(&mut self) -> Result<(), ConnectionError> {
        let base = read_string(&mut self.reader).await?;
        let drv = StorePath::from_basename(&base).map_err(|e| ConnectionError::Refused(format!("{base}: {e}")))?;
        let log = read_framed_source(&mut self.reader).await?;
        if !self.trusted() {
            return Err(op_err("you are not privileged to add logs"));
        }
        if !base.ends_with(".drv") {
            return Err(op_err(format!("'{}' is not a derivation", drv.to_absolute_path())));
        }
        let log_dir = self.state_dir.parent().map_or_else(|| self.state_dir.join("log"), |p| p.join("log/nix"));
        let path = log_dir.join("drvs").join(&base[..2]).join(&base[2..]);
        let bz2 = path.with_file_name(format!("{}.bz2", &base[2..]));
        if !path.exists() && !bz2.exists() {
            let parent = path.parent().unwrap_or(&log_dir);
            std::fs::create_dir_all(parent).map_err(op_err)?;
            let tmp = parent.join(format!("{}.tmp.{}", &base[2..], std::process::id()));
            std::fs::write(&tmp, &log).map_err(op_err)?;
            std::fs::rename(&tmp, &path).map_err(op_err)?;
        }
        self.finish().await?;
        write_u64(&mut self.writer, 1).await?;
        Ok(())
    }
}
