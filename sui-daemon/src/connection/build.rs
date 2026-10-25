use std::collections::{BTreeSet, HashSet, VecDeque};

use tokio::io::{AsyncRead, AsyncWrite};

use sui_compat::derivation::{Derivation, DerivationOutput};
use sui_compat::realisation::{DrvOutput, Realisation};
use sui_compat::store_path::StorePath;
use sui_store::traits::Store;

use super::dispatch::op_err;
use super::wire::{read_string, read_string_list, read_u64, write_bool, write_string, write_u64};
use super::{Connection, ConnectionError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OutputsSpec {
    All,
    Names(Vec<String>),
}

impl OutputsSpec {
    fn wants(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Names(names) => names.iter().any(|n| n == name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DerivedPath {
    Opaque(StorePath),
    Built { drv: StorePath, outputs: OutputsSpec },
}

impl DerivedPath {
    pub(super) fn render(&self) -> String {
        match self {
            Self::Opaque(p) => p.to_absolute_path(),
            Self::Built { drv, outputs: OutputsSpec::All } => format!("{}!*", drv.to_absolute_path()),
            Self::Built { drv, outputs: OutputsSpec::Names(names) } => {
                format!("{}!{}", drv.to_absolute_path(), names.join(","))
            }
        }
    }
}

#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BuildStatus {
    Built = 0,
    Substituted = 1,
    AlreadyValid = 2,
    MiscFailure = 9,
    NoSubstituters = 14,
}

#[derive(Debug, Clone)]
pub(super) struct BuildOutcome {
    pub status: BuildStatus,
    pub error: String,
    pub times_built: u64,
    pub start: u64,
    pub stop: u64,
    pub built: Vec<Realisation>,
}

impl BuildOutcome {
    fn with_status(status: BuildStatus, error: String) -> Self {
        Self { status, error, times_built: 0, start: 0, stop: 0, built: Vec::new() }
    }

    fn succeeded(&self) -> bool {
        matches!(self.status, BuildStatus::Built | BuildStatus::Substituted | BuildStatus::AlreadyValid)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Missing {
    pub will_build: BTreeSet<String>,
    pub will_substitute: BTreeSet<String>,
    pub unknown: BTreeSet<String>,
    pub download_size: u64,
    pub nar_size: u64,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl<S, R, W> Connection<S, R, W>
where
    S: Store + 'static,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    fn parse_derived_path(&self, s: &str) -> Result<DerivedPath, ConnectionError> {
        if self.minor() >= 30 {
            return match s.rsplit_once('!') {
                None => Ok(DerivedPath::Opaque(self.parse_path(s)?)),
                Some((drv, _)) if drv.contains('!') => Err(ConnectionError::Refused(format!(
                    "dynamic derivations are not supported by sui-daemon: '{s}'"
                ))),
                Some((drv, outputs)) => Ok(DerivedPath::Built { drv: self.parse_path(drv)?, outputs: parse_outputs(s, outputs)? }),
            };
        }
        let (path, outputs) = s.split_once('!').unwrap_or((s, ""));
        let sp = self.parse_path(path)?;
        if !path.ends_with(".drv") {
            return Ok(DerivedPath::Opaque(sp));
        }
        let outputs = if outputs.is_empty() { OutputsSpec::All } else { parse_outputs(s, outputs)? };
        Ok(DerivedPath::Built { drv: sp, outputs })
    }

    async fn read_derived_paths(&mut self) -> Result<Vec<DerivedPath>, ConnectionError> {
        let raw = read_string_list(&mut self.reader).await?;
        raw.iter().map(|s| self.parse_derived_path(s)).collect()
    }

    async fn read_build_mode(&mut self) -> Result<u64, ConnectionError> {
        let mode = read_u64(&mut self.reader).await?;
        if mode > 2 {
            return Err(ConnectionError::Refused("Invalid build mode".to_string()));
        }
        if mode == 1 && !self.trusted() {
            return Err(ConnectionError::Refused(
                "repairing is not allowed because you are not in 'trusted-users'".to_string(),
            ));
        }
        Ok(mode)
    }

    pub(super) fn load_drv(&self, drv: &StorePath) -> Result<Derivation, String> {
        let file = self.store_dir.join(drv.to_basename());
        let bytes = std::fs::read(&file).map_err(|e| format!("cannot read derivation '{}': {e}", drv.to_absolute_path()))?;
        Derivation::parse(&bytes).map_err(|e| format!("cannot parse derivation '{}': {e}", drv.to_absolute_path()))
    }

    pub(super) async fn output_map(&self, drv: &StorePath) -> Result<Vec<(String, String)>, ConnectionError> {
        match self.store.query_derivation_output_map(drv).await {
            Ok(Some(map)) if !map.is_empty() => return Ok(map),
            Ok(None) => return Err(op_err(format!("path '{}' is not valid", drv.to_absolute_path()))),
            Ok(Some(_)) => {}
            Err(e) if e.is_not_supported() => {
                if !self.store.is_valid_path(drv).await? {
                    return Err(op_err(format!("path '{}' is not valid", drv.to_absolute_path())));
                }
            }
            Err(e) => return Err(e.into()),
        }
        let d = self.load_drv(drv).map_err(op_err)?;
        Ok(d.outputs.into_iter().map(|(name, o)| (name, o.path)).collect())
    }

    fn realisations(&self, drv: &StorePath, outputs: &[(String, String)]) -> Result<Vec<Realisation>, String> {
        let d = self.load_drv(drv)?;
        let mut memo = std::collections::HashMap::new();
        let mut load = |p: &str| {
            let sp = StorePath::from_absolute_path(p).map_err(|e| sui_compat::derivation::DerivationError::Parse {
                pos: 0,
                message: e.to_string(),
            })?;
            let bytes = std::fs::read(self.store_dir.join(sp.to_basename()))?;
            Derivation::parse(&bytes)
        };
        let hashes = sui_compat::derivation::output_hashes_modulo(&d, true, &mut load, &mut memo).map_err(|e| e.to_string())?;
        outputs
            .iter()
            .map(|(name, path)| {
                let hex = hashes.get(name).ok_or_else(|| format!("no hash for output '{name}' of '{}'", drv.to_absolute_path()))?;
                Ok(Realisation {
                    id: DrvOutput { drv_hash: format!("sha256:{hex}"), output_name: name.clone() },
                    out_path: path.rsplit('/').next().unwrap_or(path).to_string(),
                    signatures: BTreeSet::new(),
                    dependent_realisations: std::collections::BTreeMap::new(),
                })
            })
            .collect()
    }

    async fn all_valid(&self, paths: &[(String, String)]) -> Result<bool, ConnectionError> {
        for (_, p) in paths {
            let valid = match StorePath::from_absolute_path(p) {
                Ok(sp) => self.store.is_valid_path(&sp).await?,
                Err(_) => false,
            };
            if !valid {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) async fn realize(&self, target: &DerivedPath, mode: u64) -> BuildOutcome {
        match self.realize_inner(target, mode).await {
            Ok(outcome) => outcome,
            Err(e) => BuildOutcome::with_status(BuildStatus::MiscFailure, e.to_string()),
        }
    }

    async fn realize_inner(&self, target: &DerivedPath, mode: u64) -> Result<BuildOutcome, ConnectionError> {
        if mode != 0 {
            return Ok(BuildOutcome::with_status(
                BuildStatus::MiscFailure,
                format!(
                    "sui-daemon does not implement build mode '{}' for '{}'",
                    if mode == 1 { "repair" } else { "check" },
                    target.render()
                ),
            ));
        }
        match target {
            DerivedPath::Opaque(sp) => {
                if self.store.is_valid_path(sp).await? {
                    return Ok(BuildOutcome::with_status(BuildStatus::AlreadyValid, String::new()));
                }
                let missing = || {
                    BuildOutcome::with_status(
                        BuildStatus::NoSubstituters,
                        format!("path '{}' is required, but there is no substituter that can build it", sp.to_absolute_path()),
                    )
                };
                if !self.use_substitutes {
                    return Ok(missing());
                }
                let start = now();
                match self.substitutor().substitute(sp).await {
                    Ok(sui_store::SubstituteResult::AlreadyPresent) => {
                        Ok(BuildOutcome::with_status(BuildStatus::AlreadyValid, String::new()))
                    }
                    Ok(sui_store::SubstituteResult::Substituted { .. }) => Ok(BuildOutcome {
                        status: BuildStatus::Substituted,
                        error: String::new(),
                        times_built: 0,
                        start,
                        stop: now(),
                        built: Vec::new(),
                    }),
                    Ok(_) => Ok(missing()),
                    Err(e) => Ok(BuildOutcome::with_status(BuildStatus::MiscFailure, e.to_string())),
                }
            }
            DerivedPath::Built { drv, outputs } => {
                if !self.store.is_valid_path(drv).await? {
                    return Ok(BuildOutcome::with_status(
                        BuildStatus::MiscFailure,
                        format!("cannot build missing derivation '{}'", drv.to_absolute_path()),
                    ));
                }
                let map = self.output_map(drv).await?;
                let wanted: Vec<(String, String)> = map.into_iter().filter(|(n, _)| outputs.wants(n)).collect();
                if let OutputsSpec::Names(names) = outputs
                    && let Some(absent) = names.iter().find(|n| !wanted.iter().any(|(w, _)| w == *n))
                {
                    return Ok(BuildOutcome::with_status(
                        BuildStatus::MiscFailure,
                        format!("derivation '{}' does not have wanted outputs '{absent}'", drv.to_absolute_path()),
                    ));
                }
                if self.all_valid(&wanted).await? {
                    let built = self.realisations(drv, &wanted).map_err(op_err)?;
                    return Ok(BuildOutcome { built, ..BuildOutcome::with_status(BuildStatus::AlreadyValid, String::new()) });
                }
                let start = now();
                if let Err(e) = sui_orchestrate::realize_drv(&drv.to_absolute_path()).await {
                    return Ok(BuildOutcome { start, stop: now(), ..BuildOutcome::with_status(BuildStatus::MiscFailure, e.to_string()) });
                }
                if !self.all_valid(&wanted).await? {
                    return Ok(BuildOutcome::with_status(
                        BuildStatus::MiscFailure,
                        format!("realizing '{}' left a wanted output invalid", target.render()),
                    ));
                }
                let built = self.realisations(drv, &wanted).map_err(op_err)?;
                Ok(BuildOutcome { status: BuildStatus::Built, error: String::new(), times_built: 1, start, stop: now(), built })
            }
        }
    }

    pub(super) async fn write_build_result(&mut self, outcome: &BuildOutcome) -> Result<(), ConnectionError> {
        write_u64(&mut self.writer, outcome.status as u64).await?;
        write_string(&mut self.writer, &outcome.error).await?;
        if self.minor() >= 29 {
            write_u64(&mut self.writer, outcome.times_built).await?;
            write_bool(&mut self.writer, false).await?;
            write_u64(&mut self.writer, outcome.start).await?;
            write_u64(&mut self.writer, outcome.stop).await?;
        }
        if self.minor() >= 37 {
            write_u64(&mut self.writer, 0).await?;
            write_u64(&mut self.writer, 0).await?;
        }
        if self.minor() >= 28 {
            let mut built: Vec<&Realisation> = outcome.built.iter().collect();
            built.sort_by(|a, b| a.id.cmp(&b.id));
            write_u64(&mut self.writer, built.len() as u64).await?;
            for r in built {
                write_string(&mut self.writer, &r.id.to_string()).await?;
                write_string(&mut self.writer, &r.to_json()).await?;
            }
        }
        Ok(())
    }

    pub(super) async fn handle_build_paths(&mut self) -> Result<(), ConnectionError> {
        let targets = self.read_derived_paths().await?;
        let mode = if self.minor() >= 15 { self.read_build_mode().await? } else { 0 };
        let mut failed = Vec::new();
        for target in &targets {
            let outcome = self.realize(target, mode).await;
            if !outcome.succeeded() {
                tracing::warn!(target = %target.render(), error = %outcome.error, "BuildPaths failed");
                failed.push((target.render(), outcome.error));
            }
        }
        match failed.as_slice() {
            [] => {}
            [(_, error)] => return Err(op_err(error)),
            many => {
                let names: Vec<String> = many.iter().map(|(t, _)| format!("'{t}'")).collect();
                return Err(op_err(format!("build of {} failed", names.join(", "))));
            }
        }
        self.finish().await?;
        write_u64(&mut self.writer, 1).await?;
        Ok(())
    }

    pub(super) async fn handle_build_paths_with_results(&mut self) -> Result<(), ConnectionError> {
        let targets = self.read_derived_paths().await?;
        let mode = self.read_build_mode().await?;
        let mut outcomes = Vec::with_capacity(targets.len());
        for target in &targets {
            outcomes.push(self.realize(target, mode).await);
        }
        self.finish().await?;
        write_u64(&mut self.writer, targets.len() as u64).await?;
        for (target, outcome) in targets.iter().zip(&outcomes) {
            write_string(&mut self.writer, &target.render()).await?;
            self.write_build_result(outcome).await?;
        }
        Ok(())
    }

    pub(super) async fn handle_build_derivation(&mut self) -> Result<(), ConnectionError> {
        let drv_path = self.read_path().await?;
        let count = read_u64(&mut self.reader).await?;
        let mut outputs = std::collections::BTreeMap::new();
        for _ in 0..count {
            let name = read_string(&mut self.reader).await?;
            let path = read_string(&mut self.reader).await?;
            let hash_algo = read_string(&mut self.reader).await?;
            let hash = read_string(&mut self.reader).await?;
            outputs.insert(name, DerivationOutput { path, hash_algo, hash });
        }
        let _input_srcs = self.read_path_set().await?;
        let _platform = read_string(&mut self.reader).await?;
        let _builder = read_string(&mut self.reader).await?;
        let _args = read_string_list(&mut self.reader).await?;
        let env_count = read_u64(&mut self.reader).await?;
        for _ in 0..env_count {
            let _k = read_string(&mut self.reader).await?;
            let _v = read_string(&mut self.reader).await?;
        }
        let mode = read_u64(&mut self.reader).await?;
        if mode > 2 {
            return Err(ConnectionError::Refused("Invalid build mode".to_string()));
        }
        let content_addressed = outputs.values().all(|o| !o.hash_algo.is_empty());
        if !(content_addressed || self.trusted()) {
            return Err(op_err("you are not privileged to build input-addressed derivations"));
        }
        let outcome = match self.load_drv(&drv_path) {
            Ok(stored) if self.store.is_valid_path(&drv_path).await? && same_outputs(&stored.outputs, &outputs) => {
                self.realize(&DerivedPath::Built { drv: drv_path.clone(), outputs: OutputsSpec::All }, mode).await
            }
            _ => BuildOutcome::with_status(
                BuildStatus::MiscFailure,
                format!(
                    "sui-daemon builds a derivation sent by the client only when '{}' is valid in the store with the same outputs",
                    drv_path.to_absolute_path()
                ),
            ),
        };
        self.finish().await?;
        self.write_build_result(&outcome).await
    }

    pub(super) async fn handle_ensure_path(&mut self) -> Result<(), ConnectionError> {
        let sp = self.read_path().await?;
        let outcome = self.realize(&DerivedPath::Opaque(sp.clone()), 0).await;
        if !outcome.succeeded() {
            return Err(op_err(match outcome.status {
                BuildStatus::NoSubstituters => {
                    format!("path '{}' does not exist and cannot be created", sp.to_absolute_path())
                }
                _ => outcome.error,
            }));
        }
        self.finish().await?;
        write_u64(&mut self.writer, 1).await?;
        Ok(())
    }

    pub(super) async fn query_missing(&self, targets: Vec<DerivedPath>) -> Result<Missing, ConnectionError> {
        let mut res = Missing::default();
        let mut done = HashSet::new();
        let mut queue: VecDeque<DerivedPath> = targets.into();
        while let Some(req) = queue.pop_front() {
            if !done.insert(req.render()) {
                continue;
            }
            match req {
                DerivedPath::Built { drv, outputs } => {
                    if !self.store.is_valid_path(&drv).await? {
                        res.unknown.insert(drv.to_absolute_path());
                        continue;
                    }
                    let mut invalid = Vec::new();
                    for (name, path) in self.output_map(&drv).await? {
                        if !outputs.wants(&name) {
                            continue;
                        }
                        let sp = StorePath::from_absolute_path(&path).map_err(op_err)?;
                        if !self.store.is_valid_path(&sp).await? {
                            invalid.push(sp);
                        }
                    }
                    if invalid.is_empty() {
                        continue;
                    }
                    let d = self.load_drv(&drv).map_err(op_err)?;
                    let substitutable = self.use_substitutes
                        && d.substitutes_allowed()
                        && self.substitutable_infos(&invalid).await.len() == invalid.len();
                    if substitutable {
                        queue.extend(invalid.into_iter().map(DerivedPath::Opaque));
                    } else {
                        res.will_build.insert(drv.to_absolute_path());
                        for (input, wanted) in &d.input_derivations {
                            let input = StorePath::from_absolute_path(input).map_err(op_err)?;
                            queue.push_back(DerivedPath::Built { drv: input, outputs: OutputsSpec::Names(wanted.clone()) });
                        }
                    }
                }
                DerivedPath::Opaque(sp) => {
                    if self.store.is_valid_path(&sp).await? {
                        continue;
                    }
                    let infos = self.substitutable_infos(std::slice::from_ref(&sp)).await;
                    let Some((_, info)) = infos.into_iter().next() else {
                        res.unknown.insert(sp.to_absolute_path());
                        continue;
                    };
                    res.will_substitute.insert(sp.to_absolute_path());
                    res.download_size += info.download_size;
                    res.nar_size += info.nar_size;
                    for r in info.references {
                        if let Ok(rp) = StorePath::from_absolute_path(&r) {
                            queue.push_back(DerivedPath::Opaque(rp));
                        }
                    }
                }
            }
        }
        Ok(res)
    }

    pub(super) async fn handle_query_missing(&mut self) -> Result<(), ConnectionError> {
        let targets = self.read_derived_paths().await?;
        let missing = self.query_missing(targets).await?;
        self.finish().await?;
        for set in [&missing.will_build, &missing.will_substitute, &missing.unknown] {
            let list: Vec<String> = set.iter().cloned().collect();
            super::wire::write_string_list(&mut self.writer, &list).await?;
        }
        write_u64(&mut self.writer, missing.download_size).await?;
        write_u64(&mut self.writer, missing.nar_size).await?;
        Ok(())
    }

    pub(super) async fn handle_register_drv_output(&mut self) -> Result<(), ConnectionError> {
        if self.minor() < 31 {
            let _id = read_string(&mut self.reader).await?;
            let _path = read_string(&mut self.reader).await?;
        } else {
            let _realisation = read_string(&mut self.reader).await?;
        }
        Err(op_err(
            "experimental Nix feature 'ca-derivations' is disabled; add '--extra-experimental-features ca-derivations' to enable it",
        ))
    }

    pub(super) async fn handle_query_realisation(&mut self) -> Result<(), ConnectionError> {
        let id = read_string(&mut self.reader).await?;
        DrvOutput::parse(&id).map_err(op_err)?;
        self.finish().await?;
        write_u64(&mut self.writer, 0).await?;
        Ok(())
    }
}

fn parse_outputs(whole: &str, outputs: &str) -> Result<OutputsSpec, ConnectionError> {
    if outputs == "*" {
        return Ok(OutputsSpec::All);
    }
    let names: Vec<String> = outputs.split(',').map(String::from).collect();
    if names.iter().any(String::is_empty) {
        return Err(ConnectionError::Refused(format!("invalid outputs specifier in '{whole}'")));
    }
    Ok(OutputsSpec::Names(names))
}

fn same_outputs(
    a: &std::collections::BTreeMap<String, DerivationOutput>,
    b: &std::collections::BTreeMap<String, DerivationOutput>,
) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|((an, ao), (bn, bo))| an == bn && ao.path == bo.path)
}
