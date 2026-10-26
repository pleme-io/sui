//! Opcode dispatch loop and per-operation handlers.
//!
//! After the handshake completes, [`Connection::run`] reads opcodes in a
//! loop and delegates to the appropriate `handle_*` method.

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use sui_compat::store_path::StorePath;
use sui_compat::wire::WorkerOp;
use sui_store::traits::Store;

use super::wire::{
    read_string, read_string_list, read_u64, write_bool, write_stderr_error, write_stderr_error_struct,
    write_stderr_last, write_string, write_string_list, write_u64,
};
use super::{
    Connection, ConnectionError, PROTOCOL_MINOR_BUILD_CORES, PROTOCOL_MINOR_OBSOLETE_LOG_FIELDS,
    PROTOCOL_MINOR_OVERRIDES, PROTOCOL_MINOR_USE_BUILD_HOOK, PROTOCOL_MINOR_USE_SUBSTITUTES,
    PROTOCOL_MINOR_VERBOSE_BUILD,
};

pub(super) fn op_err(e: impl std::fmt::Display) -> ConnectionError {
    ConnectionError::Op(e.to_string())
}

impl<S, R, W> Connection<S, R, W>
where
    S: Store + 'static,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Run the main opcode dispatch loop.
    ///
    /// Reads opcodes from the client, dispatches to the appropriate handler,
    /// and writes responses. Returns when the connection is closed or an
    /// unrecoverable error occurs.
    pub async fn run(&mut self) -> Result<(), ConnectionError> {
        loop {
            let op_raw = match read_u64(&mut self.reader).await {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    tracing::debug!("client disconnected");
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };
            tracing::debug!(op_raw, "received daemon op");
            match self.perform(op_raw).await {
                Ok(()) => {}
                Err(ConnectionError::Op(message)) => {
                    tracing::debug!(op_raw, %message, "operation failed");
                    self.write_error(&message).await?;
                }
                Err(ConnectionError::Store(e)) => {
                    tracing::debug!(op_raw, error = %e, "operation failed");
                    self.write_error(&e.to_string()).await?;
                }
                Err(ConnectionError::Refused(message)) => {
                    tracing::warn!(op_raw, %message, "request refused; closing the connection");
                    self.write_error(&message).await?;
                    self.writer.flush().await?;
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
            self.writer.flush().await?;
        }
    }

    async fn perform(&mut self, op_raw: u64) -> Result<(), ConnectionError> {
        let Some(op) = WorkerOp::from_u64(op_raw) else {
            return Err(ConnectionError::Refused(format!("invalid operation {op_raw}")));
        };
        match op {
            WorkerOp::IsValidPath => self.handle_is_valid_path().await,
            WorkerOp::QueryPathInfo => self.handle_query_path_info().await,
            WorkerOp::QueryAllValidPaths => self.handle_query_all_valid_paths().await,
            WorkerOp::SetOptions => self.handle_set_options().await,
            WorkerOp::HasSubstitutes => self.handle_has_substitutes().await,
            WorkerOp::QueryReferences => self.handle_query_references().await,
            WorkerOp::QueryReferrers => self.handle_query_referrers().await,
            WorkerOp::QueryDeriver => self.handle_query_deriver().await,
            WorkerOp::QueryValidPaths => self.handle_query_valid_paths().await,
            WorkerOp::AddTempRoot => self.handle_add_temp_root().await,
            WorkerOp::AddIndirectRoot => self.handle_add_indirect_root().await,
            WorkerOp::AddPermRoot => self.handle_add_perm_root().await,
            WorkerOp::SyncWithGC => self.handle_sync_with_gc().await,
            WorkerOp::AddToStore => self.handle_add_to_store().await,
            WorkerOp::AddTextToStore => self.handle_add_text_to_store().await,
            WorkerOp::AddToStoreNar => self.handle_add_to_store_nar().await,
            WorkerOp::AddMultipleToStore => self.handle_add_multiple_to_store().await,
            WorkerOp::NarFromPath => self.handle_nar_from_path().await,
            WorkerOp::ExportPath => self.handle_export_path().await,
            WorkerOp::ImportPaths => self.handle_import_paths().await,
            WorkerOp::BuildPaths => self.handle_build_paths().await,
            WorkerOp::BuildPathsWithResults => self.handle_build_paths_with_results().await,
            WorkerOp::BuildDerivation => self.handle_build_derivation().await,
            WorkerOp::EnsurePath => self.handle_ensure_path().await,
            WorkerOp::QueryMissing => self.handle_query_missing().await,
            WorkerOp::RegisterDrvOutput => self.handle_register_drv_output().await,
            WorkerOp::QueryRealisation => self.handle_query_realisation().await,
            WorkerOp::QueryPathHash => self.handle_query_path_hash().await,
            WorkerOp::QueryValidDerivers => self.handle_query_valid_derivers().await,
            WorkerOp::QueryDerivationOutputs => self.handle_query_derivation_outputs().await,
            WorkerOp::QueryDerivationOutputNames => self.handle_query_derivation_output_names().await,
            WorkerOp::QueryDerivationOutputMap => self.handle_query_derivation_output_map().await,
            WorkerOp::QueryPathFromHashPart => self.handle_query_path_from_hash_part().await,
            WorkerOp::QuerySubstitutablePaths => self.handle_query_substitutable_paths().await,
            WorkerOp::QuerySubstitutablePathInfo => self.handle_query_substitutable_path_info().await,
            WorkerOp::QuerySubstitutablePathInfos => self.handle_query_substitutable_path_infos().await,
            WorkerOp::AddSignatures => self.handle_add_signatures().await,
            WorkerOp::FindRoots => self.handle_find_roots().await,
            WorkerOp::CollectGarbage => self.handle_collect_garbage().await,
            WorkerOp::OptimiseStore => self.handle_optimise_store().await,
            WorkerOp::VerifyStore => self.handle_verify_store().await,
            WorkerOp::AddBuildLog => self.handle_add_build_log().await,
            WorkerOp::QueryFailedPaths | WorkerOp::ClearFailedPaths => {
                Err(ConnectionError::Refused(format!("Removed operation {op_raw}")))
            }
            other => Err(ConnectionError::Refused(format!("invalid operation {}", other as u64))),
        }
    }

    pub(super) async fn write_error(&mut self, message: &str) -> Result<(), ConnectionError> {
        if self.minor() >= 26 {
            write_stderr_error_struct(&mut self.writer, message).await?;
        } else {
            write_stderr_error(&mut self.writer, message).await?;
        }
        Ok(())
    }

    pub(super) fn parse_path(&self, s: &str) -> Result<StorePath, ConnectionError> {
        let dir = sui_compat::store_path::DEFAULT_STORE_DIR;
        if !s.starts_with(&format!("{dir}/")) {
            return Err(ConnectionError::Refused(format!("path '{s}' is not in the Nix store")));
        }
        StorePath::from_absolute_path(s).map_err(|e| ConnectionError::Refused(format!("path '{s}': {e}")))
    }

    pub(super) async fn read_path(&mut self) -> Result<StorePath, ConnectionError> {
        let s = read_string(&mut self.reader).await?;
        self.parse_path(&s)
    }

    pub(super) async fn read_path_set(&mut self) -> Result<Vec<StorePath>, ConnectionError> {
        let raw = read_string_list(&mut self.reader).await?;
        let mut out = Vec::with_capacity(raw.len());
        for s in raw {
            out.push(self.parse_path(&s)?);
        }
        out.sort_by_key(StorePath::to_absolute_path);
        out.dedup();
        Ok(out)
    }

    pub(super) async fn require_info(&self, sp: &StorePath) -> Result<sui_store::traits::PathInfo, ConnectionError> {
        self.store
            .query_path_info(sp)
            .await?
            .ok_or_else(|| op_err(format!("path '{}' is not valid", sp.to_absolute_path())))
    }

    pub(super) async fn finish(&mut self) -> Result<(), ConnectionError> {
        write_stderr_last(&mut self.writer).await?;
        Ok(())
    }

    pub(super) async fn write_paths(&mut self, paths: &[StorePath]) -> Result<(), ConnectionError> {
        let strings: Vec<String> = paths.iter().map(StorePath::to_absolute_path).collect();
        write_string_list(&mut self.writer, &strings).await?;
        Ok(())
    }

    async fn handle_is_valid_path(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        let valid = self.store.is_valid_path(&sp).await?;
        self.finish().await?;
        write_bool(&mut self.writer, valid).await?;
        Ok(())
    }

    async fn handle_query_path_info(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        let info = self.store.query_path_info(&sp).await?;
        if info.is_none() && self.minor() < 17 {
            return Err(op_err(format!("path '{}' is not valid", sp.to_absolute_path())));
        }
        self.finish().await?;
        match info {
            Some(pi) => {
                if self.minor() >= 17 {
                    write_bool(&mut self.writer, true).await?;
                }
                self.write_unkeyed_path_info(&pi).await?;
            }
            None => write_bool(&mut self.writer, false).await?,
        }
        Ok(())
    }

    async fn handle_query_all_valid_paths(&mut self) -> Result<(), ConnectionError> {
        let paths = self.store.query_all_valid_paths().await?;
        self.finish().await?;
        self.write_paths(&paths).await
    }

    /// `SetOptions` (op 19). The layout mirrors CppNix's
    /// `RemoteStore::setOptions`: every `GET_PROTOCOL_MINOR(ver) >= N` write
    /// on the client is a conditional read here; one wrong conditional
    /// desyncs the stream.
    async fn handle_set_options(&mut self) -> Result<(), ConnectionError> {
        let _keep_failed = read_u64(&mut self.reader).await?;
        let _keep_going = read_u64(&mut self.reader).await?;
        let _try_fallback = read_u64(&mut self.reader).await?;
        self.verbosity = sui_compat::activity::Verbosity::from_code(read_u64(&mut self.reader).await?);
        let _max_build_jobs = read_u64(&mut self.reader).await?;
        let _max_silent_time = read_u64(&mut self.reader).await?;
        if self.client_version >= PROTOCOL_MINOR_USE_BUILD_HOOK {
            let _use_build_hook = read_u64(&mut self.reader).await?;
        }
        if self.client_version >= PROTOCOL_MINOR_VERBOSE_BUILD {
            let _verbose_build = read_u64(&mut self.reader).await?;
        }
        if self.client_version >= PROTOCOL_MINOR_OBSOLETE_LOG_FIELDS {
            let _log_type = read_u64(&mut self.reader).await?;
            let _print_build_trace = read_u64(&mut self.reader).await?;
        }
        if self.client_version >= PROTOCOL_MINOR_BUILD_CORES {
            let _build_cores = read_u64(&mut self.reader).await?;
        }
        if self.client_version >= PROTOCOL_MINOR_USE_SUBSTITUTES {
            self.use_substitutes = read_u64(&mut self.reader).await? != 0;
        }
        if self.client_version >= PROTOCOL_MINOR_OVERRIDES {
            let count = read_u64(&mut self.reader).await?;
            for _ in 0..count {
                let _name = read_string(&mut self.reader).await?;
                let _value = read_string(&mut self.reader).await?;
            }
        }
        self.finish().await
    }

    async fn handle_has_substitutes(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        let found = !self.substitutable_infos(std::slice::from_ref(&sp)).await.is_empty();
        self.finish().await?;
        write_bool(&mut self.writer, found).await?;
        Ok(())
    }

    async fn handle_query_references(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        let mut refs = self.require_info(&sp).await?.references;
        refs.sort();
        self.finish().await?;
        write_string_list(&mut self.writer, &refs).await?;
        Ok(())
    }

    async fn handle_query_referrers(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        let refs = match self.store.query_referrers(&sp).await {
            Ok(r) => r,
            Err(e) if e.is_not_supported() => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        self.finish().await?;
        self.write_paths(&refs).await
    }

    async fn handle_query_deriver(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        let deriver = self.require_info(&sp).await?.deriver.unwrap_or_default();
        self.finish().await?;
        write_string(&mut self.writer, &absolute(&deriver)).await?;
        Ok(())
    }

    async fn handle_query_valid_paths(&mut self) -> Result<(), ConnectionError> {
        let paths = self.read_path_set().await?;
        let substitute = self.minor() >= 27 && read_u64(&mut self.reader).await? != 0;
        if substitute && self.use_substitutes {
            let substitutor = self.substitutor();
            for sp in &paths {
                if let Err(e) = substitutor.substitute(sp).await {
                    tracing::warn!(path = %sp.to_absolute_path(), error = %e, "substitution failed");
                }
            }
        }
        let mut valid = Vec::with_capacity(paths.len());
        for sp in paths {
            if self.store.is_valid_path(&sp).await? {
                valid.push(sp);
            }
        }
        self.finish().await?;
        self.write_paths(&valid).await
    }

    async fn handle_add_temp_root(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        super::temp_roots::add(&self.state_dir, self.connection_id.id(), &sp.to_absolute_path()).map_err(op_err)?;
        self.finish().await?;
        write_u64(&mut self.writer, 1).await?;
        Ok(())
    }

    async fn handle_add_indirect_root(&mut self) -> Result<(), ConnectionError> {
        let link = absolute_path(&read_string(&mut self.reader).await?);
        self.add_indirect_root(&link).map_err(op_err)?;
        self.finish().await?;
        write_u64(&mut self.writer, 1).await?;
        Ok(())
    }

    fn add_indirect_root(&self, link: &str) -> std::io::Result<()> {
        let digest = sui_compat::hash::NixHash::of(sui_compat::hash::HashAlgorithm::Sha1, link.as_bytes()).to_nix32();
        make_symlink(&self.state_dir.join("gcroots/auto").join(digest), std::path::Path::new(link))
            .map_err(|e| std::io::Error::new(e.kind(), format!("AddIndirectRoot {link}: {e}")))
    }

    async fn handle_add_perm_root(&mut self) -> Result<(), ConnectionError> {
        if !self.trusted() {
            return Err(ConnectionError::Refused(
                "you are not privileged to create perm roots\n\nhint: you can just do this client-side without special privileges, and probably want to do that instead.".to_string(),
            ));
        }
        let sp = self.read_path().await?;
        let gc_root = absolute_path(&read_string(&mut self.reader).await?);
        let store_dir = sui_compat::store_path::DEFAULT_STORE_DIR;
        if gc_root == store_dir || gc_root.starts_with(&format!("{store_dir}/")) {
            return Err(op_err(format!(
                "creating a garbage collector root ({gc_root}) in the Nix store is forbidden (are you running nix-build inside the store?)"
            )));
        }
        let root = std::path::Path::new(&gc_root);
        if std::fs::symlink_metadata(root).is_ok() {
            let points_into_store = std::fs::read_link(root)
                .map(|t| t.to_string_lossy().starts_with(&format!("{store_dir}/")))
                .unwrap_or(false);
            if !points_into_store {
                return Err(op_err(format!("cannot create symlink '{gc_root}'; already exists")));
            }
        }
        make_symlink(root, std::path::Path::new(&sp.to_absolute_path())).map_err(op_err)?;
        self.add_indirect_root(&gc_root).map_err(op_err)?;
        self.finish().await?;
        write_string(&mut self.writer, &gc_root).await?;
        Ok(())
    }

    async fn handle_sync_with_gc(&mut self) -> Result<(), ConnectionError> {
        self.finish().await?;
        write_u64(&mut self.writer, 1).await?;
        Ok(())
    }

    pub(super) async fn write_unkeyed_path_info(
        &mut self,
        info: &sui_store::traits::PathInfo,
    ) -> Result<(), ConnectionError> {
        let mut references = info.references.clone();
        references.sort();
        write_string(&mut self.writer, &absolute(info.deriver.as_deref().unwrap_or(""))).await?;
        write_string(&mut self.writer, &nar_hash_base16(&info.nar_hash)).await?;
        write_string_list(&mut self.writer, &references).await?;
        write_u64(&mut self.writer, u64::try_from(info.registration_time).unwrap_or(0)).await?;
        write_u64(&mut self.writer, u64::try_from(info.nar_size).unwrap_or(0)).await?;
        if self.minor() >= 16 {
            write_bool(&mut self.writer, false).await?;
            let mut sigs = info.signatures.clone();
            sigs.sort();
            sigs.dedup();
            write_string_list(&mut self.writer, &sigs).await?;
            write_string(&mut self.writer, info.content_address.as_deref().unwrap_or("")).await?;
        }
        Ok(())
    }

    pub(super) async fn write_valid_path_info(
        &mut self,
        info: &sui_store::traits::PathInfo,
    ) -> Result<(), ConnectionError> {
        write_string(&mut self.writer, &info.path).await?;
        self.write_unkeyed_path_info(info).await
    }
}

pub(super) fn absolute(path: &str) -> String {
    if path.is_empty() || path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{path}", sui_compat::store_path::DEFAULT_STORE_DIR)
    }
}

pub(super) fn absolute_path(path: &str) -> String {
    let joined = if path.starts_with('/') {
        std::path::PathBuf::from(path)
    } else {
        std::env::current_dir().unwrap_or_else(|_| "/".into()).join(path)
    };
    let mut out = std::path::PathBuf::from("/");
    for c in joined.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::Normal(n) => out.push(n),
            _ => {}
        }
    }
    out.to_string_lossy().into_owned()
}

pub(super) fn nar_hash_base16(hash: &str) -> String {
    sui_compat::hash::NixHash::parse_any(sui_compat::hash::HashAlgorithm::Sha256, hash)
        .map_or_else(|_| hash.to_string(), |h| h.to_hex())
}

pub(super) fn make_symlink(link: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = std::path::PathBuf::from(format!("{}.tmp-{}", link.display(), std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, link)
}

pub(super) fn store_path_name(path: &str) -> String {
    let basename = path.rsplit('/').next().unwrap_or(path);
    match basename.split_once('-') {
        Some((_hash, name)) => name.to_string(),
        None => basename.to_string(),
    }
}

#[cfg(test)]
mod store_path_name_tests {
    use super::{absolute_path, nar_hash_base16, store_path_name};

    #[test]
    fn extracts_name_from_full_store_path() {
        assert_eq!(store_path_name("/nix/store/abc123-hello-2.12.1"), "hello-2.12.1");
    }

    #[test]
    fn extracts_name_from_bare_basename() {
        assert_eq!(store_path_name("xyz789-my-pkg"), "my-pkg");
    }

    #[test]
    fn falls_back_to_basename_without_dash() {
        assert_eq!(store_path_name("/nix/store/nodashhere"), "nodashhere");
    }

    #[test]
    fn absolute_path_canonicalises_like_cppnix_abs_path() {
        assert_eq!(absolute_path("/a/b/../c/./d"), "/a/c/d");
        assert_eq!(absolute_path("/a//b/"), "/a/b");
    }

    #[test]
    fn a_nar_hash_is_written_as_unprefixed_base16() {
        let hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(nar_hash_base16(&format!("sha256:{hex}")), hex);
        assert_eq!(nar_hash_base16("sha256:deadbeef"), "sha256:deadbeef");
    }
}
