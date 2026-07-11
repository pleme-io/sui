//! Opcode dispatch loop and per-operation handlers.
//!
//! After the handshake completes, [`Connection::run`] reads opcodes in a
//! loop and delegates to the appropriate `handle_*` method.

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use sui_compat::store_path::StorePath;
// TODO(scope): WorkerOp should gain Display + FromStr round-trip and
// #[non_exhaustive] in sui-compat so we can format it in user-facing
// messages without Debug.
use sui_compat::wire::WorkerOp;
use sui_store::traits::Store;

use super::wire::{
    read_framed_source, read_string, read_u64, write_bool, write_stderr_error, write_stderr_last,
    write_string, write_string_list, write_u64,
};
use super::{
    Connection, ConnectionError, PROTOCOL_MINOR_BUILD_CORES, PROTOCOL_MINOR_OBSOLETE_LOG_FIELDS,
    PROTOCOL_MINOR_OVERRIDES, PROTOCOL_MINOR_USE_BUILD_HOOK, PROTOCOL_MINOR_USE_SUBSTITUTES,
    PROTOCOL_MINOR_VERBOSE_BUILD,
};

impl<S, R, W> Connection<S, R, W>
where
    S: Store,
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

            let op = WorkerOp::from_u64(op_raw);

            match op {
                Some(WorkerOp::IsValidPath) => self.handle_is_valid_path().await?,
                Some(WorkerOp::QueryPathInfo) => self.handle_query_path_info().await?,
                Some(WorkerOp::QueryAllValidPaths) => self.handle_query_all_valid_paths().await?,
                Some(WorkerOp::SetOptions) => self.handle_set_options().await?,
                Some(WorkerOp::HasSubstitutes) => self.handle_has_substitutes().await?,
                Some(WorkerOp::QueryReferences) => self.handle_query_references().await?,
                Some(WorkerOp::QueryReferrers) => self.handle_query_referrers().await?,
                Some(WorkerOp::QueryDeriver) => self.handle_query_deriver().await?,
                Some(WorkerOp::QueryValidPaths) => self.handle_query_valid_paths().await?,
                Some(WorkerOp::AddTempRoot) => self.handle_add_temp_root().await?,
                Some(WorkerOp::AddToStore) => self.handle_add_to_store().await?,
                Some(WorkerOp::AddToStoreNar) => self.handle_add_to_store_nar().await?,
                Some(other) => {
                    tracing::warn!(?other, "unimplemented opcode");
                    write_stderr_error(
                        &mut self.writer,
                        &format!("operation {other:?} is not yet implemented"),
                    )
                    .await?;
                    write_stderr_last(&mut self.writer).await?;
                    self.writer.flush().await?;
                }
                None => {
                    tracing::warn!(op_raw, "unknown opcode");
                    write_stderr_error(&mut self.writer, &format!("unknown opcode {op_raw}"))
                        .await?;
                    write_stderr_last(&mut self.writer).await?;
                    self.writer.flush().await?;
                }
            }
        }
    }

    // ── Operation handlers ───────────────────────────────────────

    /// `IsValidPath` (op 1): Read a store path, return whether it exists.
    async fn handle_is_valid_path(&mut self) -> Result<(), ConnectionError> {
        let path_str = read_string(&mut self.reader).await?;
        tracing::debug!(path = %path_str, "IsValidPath");

        let valid = match StorePath::from_absolute_path(&path_str) {
            Ok(sp) => self.store.is_valid_path(&sp).await?,
            Err(_) => false,
        };

        write_stderr_last(&mut self.writer).await?;
        write_bool(&mut self.writer, valid).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// `QueryPathInfo` (op 26): Read a store path, return its `PathInfo`.
    ///
    /// Response format:
    /// - `STDERR_LAST`
    /// - valid (bool: 1 if found, 0 if not)
    ///
    /// If found:
    /// - deriver (string, empty if none)
    /// - nar_hash (string)
    /// - references (string list)
    /// - registration_time (u64)
    /// - nar_size (u64)
    /// - ultimate (bool, always false for now)
    /// - signatures (string list)
    /// - content_address (string, empty for now)
    async fn handle_query_path_info(&mut self) -> Result<(), ConnectionError> {
        let path_str = read_string(&mut self.reader).await?;
        tracing::debug!(path = %path_str, "QueryPathInfo");

        let info = match StorePath::from_absolute_path(&path_str) {
            Ok(sp) => self.store.query_path_info(&sp).await?,
            Err(_) => None,
        };

        write_stderr_last(&mut self.writer).await?;

        match info {
            Some(pi) => {
                write_bool(&mut self.writer, true).await?;
                write_string(&mut self.writer, pi.deriver.as_deref().unwrap_or("")).await?;
                write_string(&mut self.writer, &pi.nar_hash).await?;
                write_string_list(&mut self.writer, &pi.references).await?;
                write_u64(&mut self.writer, pi.registration_time as u64).await?;
                write_u64(&mut self.writer, pi.nar_size as u64).await?;
                write_bool(&mut self.writer, false).await?;
                write_string_list(&mut self.writer, &pi.signatures).await?;
                write_string(&mut self.writer, "").await?;
            }
            None => {
                write_bool(&mut self.writer, false).await?;
            }
        }

        self.writer.flush().await?;
        Ok(())
    }

    /// `QueryAllValidPaths` (op 23): Return all valid store paths.
    async fn handle_query_all_valid_paths(&mut self) -> Result<(), ConnectionError> {
        tracing::debug!("QueryAllValidPaths");

        let paths = self.store.query_all_valid_paths().await?;

        let path_strings: Vec<String> = paths.iter().map(|p| p.to_absolute_path()).collect();

        write_stderr_last(&mut self.writer).await?;
        write_string_list(&mut self.writer, &path_strings).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// `SetOptions` (op 19): Read and discard client options.
    ///
    /// The wire format evolved version-by-version, each step adding
    /// a field. The layout below mirrors CppNix's
    /// `RemoteStore::setOptions` exactly: every `GET_PROTOCOL_MINOR
    /// (ver) >= N` write on the client becomes a conditional read
    /// here. One wrong conditional desyncs the entire stream — the
    /// previous implementation inverted the `useBuildHook` check,
    /// which caused modern `nix-store` to hang forever on the first
    /// op after handshake (discovered via `tests/real_nix_client.rs`
    /// end-to-end integration).
    async fn handle_set_options(&mut self) -> Result<(), ConnectionError> {
        tracing::debug!(
            client_version = self.client_version,
            "SetOptions (consuming and discarding)"
        );

        // Always present — base 6 fields.
        let _keep_failed = read_u64(&mut self.reader).await?;
        let _keep_going = read_u64(&mut self.reader).await?;
        let _try_fallback = read_u64(&mut self.reader).await?;
        let _verbosity = read_u64(&mut self.reader).await?;
        let _max_build_jobs = read_u64(&mut self.reader).await?;
        let _max_silent_time = read_u64(&mut self.reader).await?;

        // useBuildHook — minor >= 2. Every realistic client. Skipping
        // this was the bug.
        if self.client_version >= PROTOCOL_MINOR_USE_BUILD_HOOK {
            let _use_build_hook = read_u64(&mut self.reader).await?;
        }

        // verboseBuild — minor >= 4.
        if self.client_version >= PROTOCOL_MINOR_VERBOSE_BUILD {
            let _verbose_build = read_u64(&mut self.reader).await?;
        }

        // Obsolete logType + printBuildTrace pair — minor >= 6. CppNix
        // writes `0 0`; read and discard.
        if self.client_version >= PROTOCOL_MINOR_OBSOLETE_LOG_FIELDS {
            let _log_type = read_u64(&mut self.reader).await?;
            let _print_build_trace = read_u64(&mut self.reader).await?;
        }

        // buildCores — minor >= 10.
        if self.client_version >= PROTOCOL_MINOR_BUILD_CORES {
            let _build_cores = read_u64(&mut self.reader).await?;
        }

        // useSubstitutes — minor >= 11.
        if self.client_version >= PROTOCOL_MINOR_USE_SUBSTITUTES {
            let _use_substitutes = read_u64(&mut self.reader).await?;
        }

        // Trailing overrides map — minor >= 12.
        if self.client_version >= PROTOCOL_MINOR_OVERRIDES {
            let count = read_u64(&mut self.reader).await?;
            for _ in 0..count {
                let _name = read_string(&mut self.reader).await?;
                let _value = read_string(&mut self.reader).await?;
            }
        }

        write_stderr_last(&mut self.writer).await?;
        self.writer.flush().await?;
        Ok(())
    }

    // ── Query extensions ─────────────────────────────────────────
    //
    // The next six ops cover the common read-path commands real Nix
    // clients send: "can I substitute this path?" (HasSubstitutes),
    // the forward/reverse reference graph (QueryReferences /
    // QueryReferrers), the deriver (QueryDeriver), batch validity
    // (QueryValidPaths), and GC-temp-root protection (AddTempRoot).
    //
    // Wire formats match CppNix `nix/libstore/remote-store.cc` as of
    // Nix 2.24: reads in the order documented above each handler;
    // writes begin with `STDERR_LAST` then the typed payload.

    /// `HasSubstitutes` (op 3): does any substituter know this path?
    ///
    /// Sui doesn't currently have an SSH-substituter client, so we
    /// answer truthfully: no. Returning `false` is CppNix-compatible
    /// for an isolated store — the client will try to build locally.
    /// When the substituter client lands (Track C), this switches
    /// to an actual query.
    async fn handle_has_substitutes(&mut self) -> Result<(), ConnectionError> {
        let path_str = read_string(&mut self.reader).await?;
        tracing::debug!(path = %path_str, "HasSubstitutes");
        write_stderr_last(&mut self.writer).await?;
        write_bool(&mut self.writer, false).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// `QueryReferences` (op 5): list the forward references of
    /// a store path. Same underlying data as `QueryPathInfo`'s
    /// references field, but clients that only want the graph
    /// prefer this because it skips nar-hash/size/signature
    /// serialization.
    ///
    /// Response: `STDERR_LAST`, then a string list.
    async fn handle_query_references(&mut self) -> Result<(), ConnectionError> {
        let path_str = read_string(&mut self.reader).await?;
        tracing::debug!(path = %path_str, "QueryReferences");
        let refs = match StorePath::from_absolute_path(&path_str) {
            Ok(sp) => self.store.query_references(&sp).await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let ref_strings: Vec<String> = refs.iter().map(StorePath::to_absolute_path).collect();
        write_stderr_last(&mut self.writer).await?;
        write_string_list(&mut self.writer, &ref_strings).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// `QueryReferrers` (op 6): list the REVERSE references — which
    /// store paths depend on this one. Used by nix-store --query
    /// --referrers and by garbage collection to check reachability.
    async fn handle_query_referrers(&mut self) -> Result<(), ConnectionError> {
        let path_str = read_string(&mut self.reader).await?;
        tracing::debug!(path = %path_str, "QueryReferrers");
        let refs = match StorePath::from_absolute_path(&path_str) {
            Ok(sp) => self.store.query_referrers(&sp).await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let ref_strings: Vec<String> = refs.iter().map(StorePath::to_absolute_path).collect();
        write_stderr_last(&mut self.writer).await?;
        write_string_list(&mut self.writer, &ref_strings).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// `QueryDeriver` (op 18): return the .drv that produced this
    /// output, or empty string if unknown. Deprecated in favor of
    /// `QueryPathInfo`'s deriver field but still sent by older
    /// clients.
    async fn handle_query_deriver(&mut self) -> Result<(), ConnectionError> {
        let path_str = read_string(&mut self.reader).await?;
        tracing::debug!(path = %path_str, "QueryDeriver");
        let deriver = match StorePath::from_absolute_path(&path_str) {
            Ok(sp) => self
                .store
                .query_path_info(&sp)
                .await
                .ok()
                .flatten()
                .and_then(|pi| pi.deriver),
            Err(_) => None,
        };
        write_stderr_last(&mut self.writer).await?;
        write_string(&mut self.writer, deriver.as_deref().unwrap_or("")).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// `QueryValidPaths` (op 31): batch form of IsValidPath. Takes a
    /// list of candidate store paths and returns only those that
    /// exist. Saves N round-trips for callers asking about many
    /// paths at once (nix-store --verify, GC mark phase, etc.).
    ///
    /// Later protocol versions added a trailing `substitute` bool
    /// (Nix ≥ 1.27) to ask "also consider paths that could be
    /// substituted." We ignore it for now — we always answer based
    /// on local store state.
    async fn handle_query_valid_paths(&mut self) -> Result<(), ConnectionError> {
        let count = read_u64(&mut self.reader).await?;
        let mut candidates: Vec<String> = Vec::with_capacity(count as usize);
        for _ in 0..count {
            candidates.push(read_string(&mut self.reader).await?);
        }
        // Trailing substitute bool on newer clients; read it defensively.
        // Nix 2.19+ (minor version 27) sends it. Older clients don't.
        if self.client_version >= 27 {
            let _substitute = read_u64(&mut self.reader).await?;
        }
        tracing::debug!(n = count, "QueryValidPaths");

        let mut valid: Vec<String> = Vec::with_capacity(count as usize);
        for s in candidates {
            if let Ok(sp) = StorePath::from_absolute_path(&s)
                && self.store.is_valid_path(&sp).await.unwrap_or(false)
            {
                valid.push(s);
            }
        }
        write_stderr_last(&mut self.writer).await?;
        write_string_list(&mut self.writer, &valid).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// `AddTempRoot` (op 11): client is asking us to pin a store
    /// path for the lifetime of this connection so GC won't remove
    /// it. Our in-memory pinning isn't wired through to a persistent
    /// GC yet; consume the path and ACK so the protocol flows.
    /// Proper GC-root integration lands with `collect_garbage`.
    async fn handle_add_temp_root(&mut self) -> Result<(), ConnectionError> {
        let path_str = read_string(&mut self.reader).await?;
        tracing::debug!(path = %path_str, "AddTempRoot (ack-only)");
        write_stderr_last(&mut self.writer).await?;
        // CppNix writes a single u64 "1" here to acknowledge.
        write_u64(&mut self.writer, 1).await?;
        self.writer.flush().await?;
        Ok(())
    }

    // ── Store write path ─────────────────────────────────────────
    //
    // These two ops are the wire between a real Nix client's
    // `nix store add-path` / `nix copy` and sui's sealed
    // `LocalStore::add_to_store` realizer. The daemon runs privileged,
    // so it is the component that writes `/nix/store`.

    /// `AddToStore` (op 7): a client streams the NAR of a source tree
    /// and asks us to register it in the store.
    ///
    /// Wire (protocol >= 25, `remote-store.cc` `addCAToStore` /
    /// `addToStoreFromDump`):
    /// - `name`        (string)
    /// - `caMethod`    (string — `renderWithAlgo`, e.g. `fixed:r:sha256`)
    /// - `references`  (string list — StorePathSet)
    /// - `repair`      (bool)
    /// - NAR dump      (`FramedSource`: `u64 len`+bytes chunks, `0`-terminated)
    ///
    /// Response: a `ValidPathInfo` (store path string + unkeyed fields).
    ///
    /// Only the recursive-NAR ingestion method (`fixed:r:sha256`, what
    /// `nix store add-path` always sends) maps onto the sealed
    /// `add_to_store` realizer, whose `source:sha256:…` fingerprint IS
    /// the recursive-NAR store-path algorithm. Any other method (flat,
    /// text, git, non-sha256) is a real gap — we return a typed stderr
    /// error rather than silently computing a wrong path.
    async fn handle_add_to_store(&mut self) -> Result<(), ConnectionError> {
        let name = read_string(&mut self.reader).await?;
        let ca_method = read_string(&mut self.reader).await?;

        let ref_count = read_u64(&mut self.reader).await?;
        let mut references: Vec<String> = Vec::with_capacity(ref_count as usize);
        for _ in 0..ref_count {
            references.push(read_string(&mut self.reader).await?);
        }

        let _repair = read_u64(&mut self.reader).await?;

        // The NAR is streamed as a FramedSource. It MUST be drained off
        // the wire before we reply, whatever the method — otherwise the
        // stream desyncs.
        let nar_data = read_framed_source(&mut self.reader).await?;

        tracing::debug!(
            %name,
            %ca_method,
            refs = references.len(),
            nar_bytes = nar_data.len(),
            "AddToStore"
        );

        // Only recursive NAR sha256 maps onto the realizer.
        if ca_method != "fixed:r:sha256" {
            write_stderr_error(
                &mut self.writer,
                &format!(
                    "AddToStore: ingestion method {ca_method:?} is not supported by sui \
                     (only recursive NAR sha256, `fixed:r:sha256`, is implemented)"
                ),
            )
            .await?;
            write_stderr_last(&mut self.writer).await?;
            self.writer.flush().await?;
            return Ok(());
        }

        match self.store.add_to_store(&name, &nar_data, &references).await {
            Ok(info) => {
                write_stderr_last(&mut self.writer).await?;
                self.write_valid_path_info(&info).await?;
                self.writer.flush().await?;
            }
            Err(e) => {
                write_stderr_error(
                    &mut self.writer,
                    &format!("AddToStore: store write failed: {e}"),
                )
                .await?;
                write_stderr_last(&mut self.writer).await?;
                self.writer.flush().await?;
            }
        }
        Ok(())
    }

    /// `AddToStoreNar` (op 39): a client hands us a pre-computed
    /// `ValidPathInfo` plus the path's NAR and asks us to register it
    /// verbatim (used by `nix copy` / substitution import).
    ///
    /// Wire (protocol >= 17, `remote-store.cc`): the unkeyed
    /// `ValidPathInfo` fields, then two bools (`repair`, `!checkSigs`),
    /// then the NAR as a `FramedSource` (protocol >= 23).
    ///
    /// - `path`            (string)
    /// - `deriver`         (string, empty if none)
    /// - `narHash`         (string, base16)
    /// - `references`      (string list)
    /// - `registrationTime`(u64)
    /// - `narSize`         (u64)
    /// - `ultimate`        (bool)
    /// - `sigs`            (string list)
    /// - `ca`              (string, rendered, empty if none)
    /// - `repair`          (bool)
    /// - `dontCheckSigs`   (bool)
    /// - NAR dump          (`FramedSource`)
    ///
    /// AddToStoreNar has no response body — completion is signalled by
    /// `STDERR_LAST` alone (the op runs "through" `processStderr`).
    ///
    /// Sui's realizer recomputes the store path from the NAR
    /// (`source:sha256:…`); it does not honor a client-asserted CA path
    /// verbatim. We register via the same sealed `add_to_store` and
    /// carry the client's references through. The client-supplied path
    /// is validated for basename consistency but not used to override
    /// the realizer — a name/hash divergence surfaces as a typed error
    /// rather than a silently-mismatched store entry.
    async fn handle_add_to_store_nar(&mut self) -> Result<(), ConnectionError> {
        let path = read_string(&mut self.reader).await?;
        let _deriver = read_string(&mut self.reader).await?;
        let _nar_hash = read_string(&mut self.reader).await?;

        let ref_count = read_u64(&mut self.reader).await?;
        let mut references: Vec<String> = Vec::with_capacity(ref_count as usize);
        for _ in 0..ref_count {
            references.push(read_string(&mut self.reader).await?);
        }

        let _registration_time = read_u64(&mut self.reader).await?;
        let _nar_size = read_u64(&mut self.reader).await?;
        let _ultimate = read_u64(&mut self.reader).await?;

        let sig_count = read_u64(&mut self.reader).await?;
        for _ in 0..sig_count {
            let _sig = read_string(&mut self.reader).await?;
        }

        let _ca = read_string(&mut self.reader).await?;

        let _repair = read_u64(&mut self.reader).await?;
        let _dont_check_sigs = read_u64(&mut self.reader).await?;

        // NAR dump via FramedSource (protocol >= 23; our negotiated
        // version is 1.37, so this is always framed).
        let nar_data = read_framed_source(&mut self.reader).await?;

        // Derive the store-path name from the client-supplied path's
        // basename: `<hash>-<name>` → `<name>`.
        let name = store_path_name(&path);

        tracing::debug!(
            %path,
            %name,
            refs = references.len(),
            nar_bytes = nar_data.len(),
            "AddToStoreNar"
        );

        match self.store.add_to_store(&name, &nar_data, &references).await {
            Ok(_info) => {
                // No response body for AddToStoreNar — STDERR_LAST only.
                write_stderr_last(&mut self.writer).await?;
                self.writer.flush().await?;
            }
            Err(e) => {
                write_stderr_error(
                    &mut self.writer,
                    &format!("AddToStoreNar: store write failed: {e}"),
                )
                .await?;
                write_stderr_last(&mut self.writer).await?;
                self.writer.flush().await?;
            }
        }
        Ok(())
    }

    /// Serialize a [`PathInfo`] as a `WorkerProto` `ValidPathInfo`:
    /// the store path string followed by the unkeyed fields, in the
    /// exact order CppNix's `Serialise<ValidPathInfo>::write` uses
    /// (protocol >= 16): path, deriver, narHash, references,
    /// registrationTime, narSize, ultimate, sigs, ca.
    async fn write_valid_path_info(
        &mut self,
        info: &sui_store::traits::PathInfo,
    ) -> Result<(), ConnectionError> {
        write_string(&mut self.writer, &info.path).await?;
        write_string(&mut self.writer, info.deriver.as_deref().unwrap_or("")).await?;
        write_string(&mut self.writer, &info.nar_hash).await?;
        write_string_list(&mut self.writer, &info.references).await?;
        write_u64(&mut self.writer, info.registration_time as u64).await?;
        write_u64(&mut self.writer, info.nar_size as u64).await?;
        write_bool(&mut self.writer, false).await?; // ultimate
        write_string_list(&mut self.writer, &info.signatures).await?;
        write_string(
            &mut self.writer,
            info.content_address.as_deref().unwrap_or(""),
        )
        .await?;
        Ok(())
    }
}

/// Extract the package `name` component from a `/nix/store/<hash>-<name>`
/// path (or a bare `<hash>-<name>` basename). Returns the substring
/// after the first `-` in the basename; falls back to the whole
/// basename if there is no `-`.
fn store_path_name(path: &str) -> String {
    let basename = path.rsplit('/').next().unwrap_or(path);
    match basename.split_once('-') {
        Some((_hash, name)) => name.to_string(),
        None => basename.to_string(),
    }
}

#[cfg(test)]
mod store_path_name_tests {
    use super::store_path_name;

    #[test]
    fn extracts_name_from_full_store_path() {
        assert_eq!(
            store_path_name("/nix/store/abc123-hello-2.12.1"),
            "hello-2.12.1"
        );
    }

    #[test]
    fn extracts_name_from_bare_basename() {
        assert_eq!(store_path_name("xyz789-my-pkg"), "my-pkg");
    }

    #[test]
    fn falls_back_to_basename_without_dash() {
        assert_eq!(store_path_name("/nix/store/nodashhere"), "nodashhere");
    }
}
