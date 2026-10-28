//! Realisation of derived paths, off the connection task, with its
//! activity events framed onto the connection as CppNix's `TunnelLogger`
//! frames them.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use sui_compat::activity::{self, ActivityEvent, ActivitySink, SharedSink, Worker};
use sui_compat::derivation::Derivation;
use sui_compat::realisation::{DrvOutput, Realisation};
use sui_compat::store_path::StorePath;
use sui_store::traits::Store;

use super::build::{now, BuildOutcome, BuildStatus, DerivedPath, OutputsSpec};
use super::dispatch::op_err;
use super::{Connection, ConnectionError};

/// Builds a derivation's closure, reporting to a sink.
pub trait DrvRealizer: Send + Sync {
    /// Realize `drv_path`, returning its output paths or the failure message.
    fn realize<'a>(
        &'a self,
        drv_path: &'a str,
        sink: SharedSink,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>>;

    fn build_basic<'a>(
        &'a self,
        _drv: &'a Derivation,
        drv_path: &'a str,
        _sink: SharedSink,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async move { Err(format!("this realizer cannot build '{drv_path}' as sent by the client")) })
    }
}

/// The local pipeline: [`sui_orchestrate::realize_drv_observed`].
#[derive(Debug, Clone, Copy, Default)]
pub struct OrchestrateRealizer;

impl DrvRealizer for OrchestrateRealizer {
    fn realize<'a>(
        &'a self,
        drv_path: &'a str,
        sink: SharedSink,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async move {
            sui_orchestrate::realize_drv_observed(drv_path, sink).await.map_err(|e| e.to_string())
        })
    }

    fn build_basic<'a>(
        &'a self,
        drv: &'a Derivation,
        drv_path: &'a str,
        sink: SharedSink,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async move { sui_orchestrate::build_basic_observed(drv, drv_path, sink).await.map_err(|e| e.to_string()) })
    }
}

struct FrameSink(tokio::sync::mpsc::UnboundedSender<ActivityEvent>);

impl ActivitySink for FrameSink {
    fn emit(&self, event: ActivityEvent) {
        let _ = self.0.send(event);
    }
}

pub(super) struct Realizer<S> {
    store: Arc<S>,
    caches: Vec<Arc<sui_store::BinaryCacheStore>>,
    use_substitutes: bool,
    store_dir: std::path::PathBuf,
    drvs: Arc<dyn DrvRealizer>,
}

impl<S, R, W> Connection<S, R, W>
where
    S: Store + 'static,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub(super) fn realizer(&self) -> Realizer<S> {
        Realizer {
            store: self.store.clone(),
            caches: self.caches.clone(),
            use_substitutes: self.use_substitutes,
            store_dir: self.store_dir.clone(),
            drvs: self.drvs.clone(),
        }
    }

    pub(super) fn load_drv(&self, drv: &StorePath) -> Result<Derivation, String> {
        self.realizer().load_drv(drv)
    }

    pub(super) async fn output_map(&self, drv: &StorePath) -> Result<Vec<(String, String)>, ConnectionError> {
        self.realizer().output_map(drv).await
    }

    pub(super) fn substitutor(&self) -> sui_store::Substitutor {
        self.realizer().substitutor()
    }

    /// Realize `targets` on their own task while this one frames the
    /// activity events they emit onto the connection, as they happen and
    /// before the operation's `STDERR_LAST`.
    pub(super) async fn realize_observed(&mut self, targets: Vec<DerivedPath>, mode: u64) -> Result<Vec<BuildOutcome>, ConnectionError> {
        let realizer = self.realizer();
        self.observed(move |sink| async move {
            let mut outcomes = Vec::with_capacity(targets.len());
            for target in &targets {
                outcomes.push(realizer.realize(target, mode, &sink).await);
            }
            outcomes
        })
        .await
    }

    pub(super) async fn build_basic_observed(&mut self, drv_path: StorePath, drv: Derivation) -> Result<BuildOutcome, ConnectionError> {
        let realizer = self.realizer();
        self.observed(move |sink| async move { realizer.build_basic(&drv_path, &drv, &sink).await }).await
    }

    async fn observed<T, F, Fut>(&mut self, work: F) -> Result<T, ConnectionError>
    where
        T: Send + 'static,
        F: FnOnce(SharedSink) -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink: SharedSink = Arc::new(FrameSink(tx));
        let mut task = tokio::spawn(work(sink));
        loop {
            tokio::select! {
                biased;
                Some(event) = rx.recv() => {
                    if let Err(e) = self.write_activity(&event).await {
                        task.abort();
                        return Err(e);
                    }
                }
                joined = &mut task => {
                    let outcomes = joined.map_err(|e| ConnectionError::Protocol(format!("realize task: {e}")))?;
                    while let Ok(event) = rx.try_recv() {
                        self.write_activity(&event).await?;
                    }
                    return Ok(outcomes);
                }
            }
        }
    }

    async fn write_activity(&mut self, event: &ActivityEvent) -> Result<(), ConnectionError> {
        let mut frame = Vec::new();
        activity::write_frame(&mut frame, event, self.client_version, self.verbosity)?;
        if !frame.is_empty() {
            self.writer.write_all(&frame).await?;
            self.writer.flush().await?;
        }
        Ok(())
    }
}

impl<S: Store + 'static> Realizer<S> {
    pub(super) fn substitutor(&self) -> sui_store::Substitutor {
        let local: Arc<dyn Store> = self.store.clone();
        sui_store::Substitutor::new(local, self.caches.clone())
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

    pub(super) async fn build_basic(&self, drv_path: &StorePath, drv: &Derivation, sink: &SharedSink) -> BuildOutcome {
        let wanted: Vec<(String, String)> = drv.outputs.iter().map(|(n, o)| (n.clone(), o.path.clone())).collect();
        let realisations = |status: BuildStatus, times_built: u64, start: u64| -> BuildOutcome {
            match basic_realisations(drv, drv_path, &wanted) {
                Ok(built) => BuildOutcome { status, error: String::new(), times_built, start, stop: now(), built },
                Err(e) => BuildOutcome::with_status(BuildStatus::MiscFailure, e),
            }
        };
        match self.all_valid(&wanted).await {
            Ok(true) => return realisations(BuildStatus::AlreadyValid, 0, now()),
            Ok(false) => {}
            Err(e) => return BuildOutcome::with_status(BuildStatus::MiscFailure, e.to_string()),
        }
        let start = now();
        if let Err(e) = self.drvs.build_basic(drv, &drv_path.to_absolute_path(), sink.clone()).await {
            return BuildOutcome { start, stop: now(), ..BuildOutcome::with_status(BuildStatus::MiscFailure, e) };
        }
        match self.all_valid(&wanted).await {
            Ok(true) => realisations(BuildStatus::Built, 1, start),
            Ok(false) => BuildOutcome::with_status(
                BuildStatus::MiscFailure,
                format!("building '{}' left an output invalid", drv_path.to_absolute_path()),
            ),
            Err(e) => BuildOutcome::with_status(BuildStatus::MiscFailure, e.to_string()),
        }
    }

    pub(super) async fn realize(&self, target: &DerivedPath, mode: u64, sink: &SharedSink) -> BuildOutcome {
        match self.realize_inner(target, mode, sink).await {
            Ok(outcome) => outcome,
            Err(e) => BuildOutcome::with_status(BuildStatus::MiscFailure, e.to_string()),
        }
    }

    async fn realize_inner(&self, target: &DerivedPath, mode: u64, sink: &SharedSink) -> Result<BuildOutcome, ConnectionError> {
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
                if !self.use_substitutes || self.caches.is_empty() {
                    return Ok(missing());
                }
                let start = now();
                let worker = Worker::start(sink);
                match self.substitutor().substitute_observed(sp, &worker).await {
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
                if let Err(e) = self.drvs.realize(&drv.to_absolute_path(), sink.clone()).await {
                    return Ok(BuildOutcome { start, stop: now(), ..BuildOutcome::with_status(BuildStatus::MiscFailure, e) });
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
}

fn basic_realisations(drv: &Derivation, drv_path: &StorePath, outputs: &[(String, String)]) -> Result<Vec<Realisation>, String> {
    let mut memo = std::collections::HashMap::new();
    let mut load = |p: &str| -> Result<Derivation, sui_compat::derivation::DerivationError> {
        Err(sui_compat::derivation::DerivationError::Parse { pos: 0, message: format!("'{p}' is not an input of a derivation sent by the client") })
    };
    let hashes = sui_compat::derivation::output_hashes_modulo(drv, true, &mut load, &mut memo).map_err(|e| e.to_string())?;
    outputs
        .iter()
        .map(|(name, path)| {
            let hex = hashes.get(name).ok_or_else(|| format!("no hash for output '{name}' of '{}'", drv_path.to_absolute_path()))?;
            Ok(Realisation {
                id: DrvOutput { drv_hash: format!("sha256:{hex}"), output_name: name.clone() },
                out_path: path.rsplit('/').next().unwrap_or(path).to_string(),
                signatures: BTreeSet::new(),
                dependent_realisations: std::collections::BTreeMap::new(),
            })
        })
        .collect()
}
