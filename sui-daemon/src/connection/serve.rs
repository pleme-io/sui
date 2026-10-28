use std::collections::{BTreeMap, BTreeSet};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use sui_compat::derivation::{Derivation, DerivationOutput};
use sui_compat::hash::{HashAlgorithm, NixHash};
use sui_compat::store_path::StorePath;
use sui_compat::wire::{ServeCommand, SERVE_MAGIC_1, SERVE_MAGIC_2, SERVE_PROTOCOL_VERSION};
use sui_store::traits::{PathInfo, Store};

use super::build::{BuildOutcome, BuildStatus, DerivedPath, OutputsSpec};
use super::dispatch::{absolute, op_err};
use super::wire::{read_string, read_string_list, read_u64, write_bool, write_string, write_string_list, write_u64, Direct};
use super::{Connection, ConnectionError};

const EXIT_BUILD_FAILURE: u64 = 100;
const EXIT_MISC_FAILURE: u64 = 1;

impl<S, R, W> Connection<S, R, W>
where
    S: Store + 'static,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub async fn serve(&mut self, write_allowed: bool) -> Result<(), ConnectionError> {
        self.frames = false;
        let magic = read_u64(&mut self.reader).await?;
        if magic != SERVE_MAGIC_1 {
            return Err(ConnectionError::Protocol("protocol mismatch".into()));
        }
        write_u64(&mut self.writer, SERVE_MAGIC_2).await?;
        write_u64(&mut self.writer, SERVE_PROTOCOL_VERSION).await?;
        self.writer.flush().await?;
        let version = read_u64(&mut self.reader).await?.min(SERVE_PROTOCOL_VERSION);
        loop {
            let raw = match read_u64(&mut self.reader).await {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            let cmd = ServeCommand::try_from(raw).map_err(|_| op_err(format!("unknown serve command {raw}")))?;
            self.serve_one(cmd, version & 0xff, write_allowed).await?;
            self.writer.flush().await?;
        }
    }

    async fn serve_one(&mut self, cmd: ServeCommand, minor: u64, write_allowed: bool) -> Result<(), ConnectionError> {
        let need_write = |what: &str| -> Result<(), ConnectionError> {
            if write_allowed { Ok(()) } else { Err(op_err(format!("{what} is not allowed"))) }
        };
        match cmd {
            ServeCommand::QueryValidPaths => {
                let lock = read_u64(&mut self.reader).await? != 0;
                let substitute = read_u64(&mut self.reader).await? != 0;
                let paths = self.read_path_set().await?;
                if lock && write_allowed {
                    for p in &paths {
                        match self.store.add_temp_root(p).await {
                            Ok(()) => {}
                            Err(e) if e.is_not_supported() => {
                                super::temp_roots::add(&self.state_dir, self.connection_id.id(), &p.to_absolute_path()).map_err(op_err)?;
                            }
                            Err(e) => return Err(e.into()),
                        }
                    }
                }
                if substitute && write_allowed && !self.caches.is_empty() {
                    let substitutor = self.substitutor();
                    for p in &paths {
                        if let Err(e) = substitutor.substitute(p).await {
                            tracing::warn!(path = %p.to_absolute_path(), error = %e, "substitution failed");
                        }
                    }
                }
                let mut valid = Vec::new();
                for p in paths {
                    if self.store.is_valid_path(&p).await? {
                        valid.push(p);
                    }
                }
                self.write_paths(&valid).await
            }
            ServeCommand::QueryPathInfos => {
                let paths = self.read_path_set().await?;
                for p in paths {
                    if let Some(info) = self.store.query_path_info(&p).await? {
                        write_string(&mut self.writer, &p.to_absolute_path()).await?;
                        self.write_serve_path_info(&info, minor).await?;
                    }
                }
                Ok(write_string(&mut self.writer, "").await?)
            }
            ServeCommand::DumpStorePath => {
                let p = self.read_path().await?;
                self.write_nar(&p).await
            }
            ServeCommand::ImportPaths => {
                need_write("importing paths")?;
                let entries = super::add::read_export_stream(&mut Direct(&mut self.reader)).await?;
                self.import_exported(entries, false).await?;
                Ok(write_u64(&mut self.writer, 1).await?)
            }
            ServeCommand::ExportPaths => {
                let _obsolete = read_u64(&mut self.reader).await?;
                let paths = self.read_path_set().await?;
                for p in self.topo_sorted(paths).await? {
                    write_u64(&mut self.writer, 1).await?;
                    let bytes = self.export_bytes(&p).await?;
                    self.writer.write_all(&bytes).await?;
                }
                Ok(write_u64(&mut self.writer, 0).await?)
            }
            ServeCommand::BuildPaths => {
                need_write("building paths")?;
                let raw = read_string_list(&mut self.reader).await?;
                let targets = raw.iter().map(|s| self.legacy_derived_path(s)).collect::<Result<Vec<_>, _>>()?;
                self.read_serve_build_options(minor).await?;
                let outcomes = self.realize_observed(targets, 0).await?;
                match outcomes.iter().find(|o| !o.succeeded()) {
                    None => Ok(write_u64(&mut self.writer, 0).await?),
                    Some(failed) => {
                        let status = if failed.status == BuildStatus::MiscFailure { EXIT_BUILD_FAILURE } else { EXIT_MISC_FAILURE };
                        write_u64(&mut self.writer, status).await?;
                        Ok(write_string(&mut self.writer, &failed.error).await?)
                    }
                }
            }
            ServeCommand::QueryClosure => {
                let include_outputs = read_u64(&mut self.reader).await? != 0;
                let paths = self.read_path_set().await?;
                let closure = self.fs_closure(paths, include_outputs).await?;
                self.write_paths(&closure).await
            }
            ServeCommand::BuildDerivation => {
                need_write("building paths")?;
                let drv_path = self.read_path().await?;
                let drv = self.read_basic_derivation().await?;
                self.read_serve_build_options(minor).await?;
                let outcome = self.build_basic_observed(drv_path, drv).await?;
                self.write_serve_build_result(&outcome, minor).await
            }
            ServeCommand::AddToStoreNar => {
                need_write("importing paths")?;
                let path = self.read_path().await?.to_absolute_path();
                let deriver = read_string(&mut self.reader).await?;
                let nar_hash = read_string(&mut self.reader).await?;
                let references = self.read_path_set().await?;
                let registration_time = read_u64(&mut self.reader).await?;
                let nar_size = read_u64(&mut self.reader).await?;
                let _ultimate = read_u64(&mut self.reader).await?;
                let signatures = read_string_list(&mut self.reader).await?;
                let ca = read_string(&mut self.reader).await?;
                if nar_size == 0 {
                    return Err(op_err("narInfo is too old and missing the narSize field"));
                }
                let mut nar = vec![0u8; usize::try_from(nar_size).map_err(op_err)?];
                tokio::io::AsyncReadExt::read_exact(&mut self.reader, &mut nar).await?;
                let info = PathInfo {
                    path,
                    nar_hash: format!("sha256:{}", NixHash::parse_any(HashAlgorithm::Sha256, &nar_hash).map_err(op_err)?.to_hex()),
                    nar_size: i64::try_from(nar_size).unwrap_or(i64::MAX),
                    references: references.iter().map(StorePath::to_absolute_path).collect(),
                    deriver: (!deriver.is_empty()).then(|| absolute(&deriver)),
                    signatures,
                    registration_time: if registration_time == 0 { super::build::now().cast_signed() } else { registration_time.cast_signed() },
                    content_address: (!ca.is_empty()).then_some(ca),
                };
                self.import_path(&info, &nar, false, false).await?;
                Ok(write_u64(&mut self.writer, 1).await?)
            }
        }
    }

    async fn write_serve_path_info(&mut self, info: &PathInfo, minor: u64) -> Result<(), ConnectionError> {
        let mut references = info.references.clone();
        references.sort();
        write_string(&mut self.writer, &absolute(info.deriver.as_deref().unwrap_or(""))).await?;
        write_string_list(&mut self.writer, &references).await?;
        let size = u64::try_from(info.nar_size).unwrap_or(0);
        write_u64(&mut self.writer, size).await?;
        write_u64(&mut self.writer, size).await?;
        if minor >= 4 {
            let hash = NixHash::parse_any(HashAlgorithm::Sha256, &info.nar_hash).map_err(op_err)?;
            write_string(&mut self.writer, &format!("sha256:{}", hash.to_nix32())).await?;
            write_string(&mut self.writer, info.content_address.as_deref().unwrap_or("")).await?;
            let mut sigs = info.signatures.clone();
            sigs.sort();
            sigs.dedup();
            write_string_list(&mut self.writer, &sigs).await?;
        }
        Ok(())
    }

    async fn read_serve_build_options(&mut self, minor: u64) -> Result<(), ConnectionError> {
        let _max_silent_time = read_u64(&mut self.reader).await?;
        let _build_timeout = read_u64(&mut self.reader).await?;
        if minor >= 2 {
            let _max_log_size = read_u64(&mut self.reader).await?;
        }
        if minor >= 3 {
            if read_u64(&mut self.reader).await? != 0 {
                return Err(op_err("client requested repeating builds, but this is not currently implemented"));
            }
            let _enforce_determinism = read_u64(&mut self.reader).await?;
        }
        if minor >= 7 {
            let _keep_failed = read_u64(&mut self.reader).await?;
        }
        Ok(())
    }

    async fn write_serve_build_result(&mut self, outcome: &BuildOutcome, minor: u64) -> Result<(), ConnectionError> {
        write_u64(&mut self.writer, outcome.status as u64).await?;
        write_string(&mut self.writer, &outcome.error).await?;
        if minor >= 3 {
            write_u64(&mut self.writer, outcome.times_built).await?;
            write_bool(&mut self.writer, false).await?;
            write_u64(&mut self.writer, outcome.start).await?;
            write_u64(&mut self.writer, outcome.stop).await?;
        }
        if minor >= 6 {
            let mut built: Vec<_> = outcome.built.iter().collect();
            built.sort_by(|a, b| a.id.cmp(&b.id));
            write_u64(&mut self.writer, built.len() as u64).await?;
            for r in built {
                write_string(&mut self.writer, &r.id.to_string()).await?;
                write_string(&mut self.writer, &r.to_json()).await?;
            }
        }
        Ok(())
    }

    pub(super) async fn read_basic_derivation(&mut self) -> Result<Derivation, ConnectionError> {
        let count = read_u64(&mut self.reader).await?;
        let mut outputs = BTreeMap::new();
        for _ in 0..count {
            let name = read_string(&mut self.reader).await?;
            let path = read_string(&mut self.reader).await?;
            let hash_algo = read_string(&mut self.reader).await?;
            let hash = read_string(&mut self.reader).await?;
            outputs.insert(name, DerivationOutput { path, hash_algo, hash });
        }
        let input_sources = self.read_path_set().await?;
        let system = read_string(&mut self.reader).await?;
        let builder = read_string(&mut self.reader).await?;
        let args = read_string_list(&mut self.reader).await?;
        let env_count = read_u64(&mut self.reader).await?;
        let mut env = BTreeMap::new();
        for _ in 0..env_count {
            let k = read_string(&mut self.reader).await?;
            let v = read_string(&mut self.reader).await?;
            env.insert(k, v);
        }
        Ok(Derivation {
            outputs,
            input_derivations: BTreeMap::new(),
            input_sources: input_sources.iter().map(StorePath::to_absolute_path).collect(),
            system,
            builder,
            args,
            env,
        })
    }

    fn legacy_derived_path(&self, s: &str) -> Result<DerivedPath, ConnectionError> {
        let (path, outputs) = s.split_once('!').unwrap_or((s, ""));
        let sp = self.parse_path(path)?;
        if !path.ends_with(".drv") {
            return Ok(DerivedPath::Opaque(sp));
        }
        let outputs = match outputs {
            "" | "*" => OutputsSpec::All,
            names => OutputsSpec::Names(names.split(',').map(String::from).collect()),
        };
        Ok(DerivedPath::Built { drv: sp, outputs })
    }

    async fn fs_closure(&self, roots: Vec<StorePath>, include_outputs: bool) -> Result<Vec<StorePath>, ConnectionError> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: Vec<StorePath> = roots;
        let mut out = Vec::new();
        while let Some(p) = queue.pop() {
            let abs = p.to_absolute_path();
            if !seen.insert(abs.clone()) {
                continue;
            }
            let info = self.require_info(&p).await?;
            for r in &info.references {
                queue.push(StorePath::from_absolute_path(r).map_err(op_err)?);
            }
            if include_outputs && abs.ends_with(".drv") {
                for (_, o) in self.output_map(&p).await? {
                    let sp = StorePath::from_absolute_path(&o).map_err(op_err)?;
                    if self.store.is_valid_path(&sp).await? {
                        queue.push(sp);
                    }
                }
            }
            out.push(p);
        }
        out.sort_by_key(StorePath::to_absolute_path);
        Ok(out)
    }

    async fn topo_sorted(&self, paths: Vec<StorePath>) -> Result<Vec<StorePath>, ConnectionError> {
        let set: BTreeMap<String, StorePath> = paths.into_iter().map(|p| (p.to_absolute_path(), p)).collect();
        let mut refs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for abs in set.keys() {
            let info = self.require_info(&set[abs]).await?;
            refs.insert(abs.clone(), info.references.into_iter().filter(|r| r != abs && set.contains_key(r)).collect());
        }
        let mut done: BTreeSet<String> = BTreeSet::new();
        let mut order = Vec::new();
        fn visit(p: &str, refs: &BTreeMap<String, Vec<String>>, done: &mut BTreeSet<String>, order: &mut Vec<String>) {
            if !done.insert(p.to_string()) {
                return;
            }
            for r in &refs[p] {
                visit(r, refs, done, order);
            }
            order.push(p.to_string());
        }
        for p in set.keys() {
            visit(p, &refs, &mut done, &mut order);
        }
        Ok(order.into_iter().map(|p| set[&p].clone()).collect())
    }
}
