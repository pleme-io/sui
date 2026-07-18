//! `reconcile::driver` — the streaming async loop that drives the
//! [`SystemReconciler`](super::SystemReconciler) forever.
//!
//! The controller ([`super::SystemReconciler`]) owns one *tick*; this driver
//! owns the *cadence*. Two trigger sources feed one coalescing loop:
//!
//! - **FSEvents streaming** (`notify`) on the flake directory — the instant a
//!   `*.nix` / `*.lock` source file changes, a reconcile fires (this is the
//!   "streaming `nix run .#rebuild`" the node is kept in place by).
//! - **The interval drift-catch tick** — a periodic re-check that catches drift
//!   introduced out of band (a manual profile change, a half-finished
//!   activation), even when no source file moved.
//!
//! Both feed one `tokio::mpsc` channel; the loop coalesces a burst of triggers
//! into a single tick (a source save fires many FSEvents; the Diff-gate makes a
//! redundant tick a cheap no-op anyway) and runs until a shutdown signal.
//!
//! The *loop* ([`run_with_triggers`](ReconcileDriver::run_with_triggers)) is
//! separated from the *trigger construction* (the notify watcher + signal), so
//! the loop is unit-tested over a hand-fed channel with no filesystem + no
//! signals — the impure edge (notify/interval/signal) is the only untested part,
//! and it is thin.

use std::future::Future;
use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use super::env::ReconcileEnvironment;
use super::{Controller, ReconcileConfig, SystemReconciler};

/// The debounce/coalesce window is implicit: the loop drains all pending
/// triggers before each tick, so a burst collapses to one reconcile.
///
/// A trigger to run one reconcile tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// A watched source file (`*.nix` / `*.lock`) changed.
    SourceChanged,
    /// The periodic drift-catch interval elapsed.
    Interval,
}

/// The streaming reconcile driver — wraps a [`SystemReconciler`] + its config
/// and runs the seven-beat tick on every trigger until shutdown.
pub struct ReconcileDriver<E: ReconcileEnvironment> {
    controller: SystemReconciler<E>,
    config: ReconcileConfig,
}

impl<E: ReconcileEnvironment> ReconcileDriver<E> {
    /// Build a driver from a reconciler + its config.
    #[must_use]
    pub fn new(controller: SystemReconciler<E>, config: ReconcileConfig) -> Self {
        Self { controller, config }
    }

    /// The wrapped controller (for inspection / tests).
    #[must_use]
    pub fn controller(&self) -> &SystemReconciler<E> {
        &self.controller
    }

    /// Run exactly one reconcile tick and log its outcome.
    async fn run_tick(&self, trigger: Trigger) {
        match self.controller.tick().await {
            Ok(outcome) => {
                tracing::info!(
                    trigger = ?trigger,
                    examined = outcome.report.objects_examined,
                    converged = outcome.report.objects_changed,
                    skipped = outcome.report.objects_skipped,
                    attested_ticks = self.controller.attested_ticks(),
                    note = outcome.report.note.as_deref().unwrap_or(""),
                    "system-reconcile tick"
                );
            }
            Err(err) => {
                tracing::error!(error = %err, "system-reconcile tick failed; will retry on next trigger");
            }
        }
    }

    /// **The loop.** Consume triggers until the channel closes or `shutdown`
    /// fires. Coalesces a burst of pending triggers into one tick (drains the
    /// channel before each reconcile). Testable over a hand-fed channel — no
    /// filesystem, no signals.
    pub async fn run_with_triggers(
        &self,
        mut triggers: mpsc::UnboundedReceiver<Trigger>,
        shutdown: impl Future<Output = ()> + Send,
    ) {
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                biased;
                () = &mut shutdown => {
                    tracing::info!("system-reconcile: shutdown signal — stopping loop");
                    break;
                }
                maybe = triggers.recv() => {
                    let Some(trigger) = maybe else {
                        tracing::info!("system-reconcile: all trigger sources closed — stopping loop");
                        break;
                    };
                    // Coalesce a burst (a source save fires many FSEvents) into
                    // one tick — drain everything already queued.
                    while triggers.try_recv().is_ok() {}
                    self.run_tick(trigger).await;
                }
            }
        }
    }

    /// **The service entry.** Wire the interval tick + (optionally) the FSEvents
    /// watcher into one channel and run the loop until `shutdown`. Returns the
    /// number of attested ticks for the caller to report.
    ///
    /// # Errors
    ///
    /// Returns [`notify::Error`] if the FSEvents watcher cannot be created / the
    /// flake directory cannot be watched (watch mode only).
    pub async fn run(
        &self,
        shutdown: impl Future<Output = ()> + Send,
    ) -> Result<usize, notify::Error> {
        let (tx, rx) = mpsc::unbounded_channel();

        // The interval drift-catch tick (its first tick fires immediately, which
        // is the initial converge at startup).
        let interval_tx = tx.clone();
        let interval = self.config.interval();
        let interval_handle = tokio::spawn(async move {
            let mut ivl = tokio::time::interval(interval);
            loop {
                ivl.tick().await;
                if interval_tx.send(Trigger::Interval).is_err() {
                    break; // the loop stopped
                }
            }
        });

        // The FSEvents watcher — kept alive for the loop's lifetime (dropping the
        // Watcher stops watching). `None` when watch is disabled.
        let _watcher = if self.config.watch {
            match watch_dir(&self.config.flake) {
                Some(dir) => Some(spawn_watcher(&dir, tx.clone())?),
                None => {
                    tracing::warn!(
                        flake = %self.config.flake,
                        "system-reconcile: cannot resolve a flake directory to watch — interval-only"
                    );
                    None
                }
            }
        } else {
            None
        };

        // Drop the original sender so the channel closes once the tasks stop.
        drop(tx);

        self.run_with_triggers(rx, shutdown).await;
        interval_handle.abort();
        Ok(self.controller.attested_ticks())
    }
}

/// Resolve the filesystem directory to watch from a flake reference like
/// `.#cid` or `path:/Users/…/nix#cid` — strip the `#attr`, strip a `path:`
/// prefix, default an empty string to `.`.
fn watch_dir(flake: &str) -> Option<PathBuf> {
    let before_attr = flake.split('#').next().unwrap_or(flake);
    let raw = before_attr.strip_prefix("path:").unwrap_or(before_attr);
    let raw = if raw.is_empty() { "." } else { raw };
    let dir = PathBuf::from(raw);
    // Only watch a real, existing directory (a `github:` / URL flake has no
    // local dir to watch — interval-only is correct there).
    if dir.is_dir() { Some(dir) } else { None }
}

/// Whether a changed path is a nix source worth re-converging on — a `*.nix` or
/// `*.lock` file NOT under a `.git` directory (git's internal churn must not
/// trigger a rebuild).
fn is_nix_source(path: &Path) -> bool {
    if path.components().any(|c| c.as_os_str() == ".git") {
        return false;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("nix" | "lock")
    )
}

/// Build + start an FSEvents/inotify watcher on `dir`, sending
/// [`Trigger::SourceChanged`] on every nix-source change. The returned watcher
/// must be held alive for the watch to persist.
fn spawn_watcher(
    dir: &Path,
    tx: mpsc::UnboundedSender<Trigger>,
) -> Result<notify::RecommendedWatcher, notify::Error> {
    use notify::{Event, RecursiveMode, Watcher};

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            if event.paths.iter().any(|p| is_nix_source(p)) {
                // The receiver being gone just means the loop stopped — ignore.
                let _ = tx.send(Trigger::SourceChanged);
            }
        }
    })?;
    watcher.watch(dir, RecursiveMode::Recursive)?;
    tracing::info!(dir = %dir.display(), "system-reconcile: streaming FSEvents on flake source");
    Ok(watcher)
}

/// A shutdown future that completes on `SIGINT` (Ctrl-C) or `SIGTERM` — the
/// graceful stop for a launchd/systemd service. On non-unix it waits on Ctrl-C
/// only.
///
/// # Panics
///
/// If the signal handlers cannot be installed (a process-setup failure that is
/// itself fatal).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            () = ctrl_c => {}
            _ = term.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await;
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::env::mock::MockReconcileEnv;
    use super::*;
    use crate::system::RebuildAction;

    fn cfg() -> ReconcileConfig {
        ReconcileConfig {
            name: "system-in-place".to_string(),
            flake: "path:/x#cid".to_string(),
            action: RebuildAction::Switch,
            interval_secs: 30,
            watch: false,
        }
    }

    fn driver() -> ReconcileDriver<MockReconcileEnv> {
        let env = MockReconcileEnv::in_place("abc-sys", 3);
        let ctl = SystemReconciler::new(cfg(), env);
        ReconcileDriver::new(ctl, cfg())
    }

    #[test]
    fn is_nix_source_matches_nix_and_lock_but_not_git_churn() {
        assert!(is_nix_source(Path::new("/x/nix/flake.nix")));
        assert!(is_nix_source(Path::new("/x/nix/flake.lock")));
        assert!(is_nix_source(Path::new("/x/nix/nodes/cid/default.nix")));
        // git internals must never trigger a rebuild.
        assert!(!is_nix_source(Path::new("/x/nix/.git/index")));
        assert!(!is_nix_source(Path::new("/x/nix/.git/refs/heads/main")));
        // non-source files are ignored.
        assert!(!is_nix_source(Path::new("/x/nix/README.md")));
    }

    #[test]
    fn watch_dir_strips_attr_and_path_prefix() {
        // "." always exists.
        assert_eq!(watch_dir(".#cid"), Some(PathBuf::from(".")));
        // A non-existent dir yields None (nothing to watch).
        assert_eq!(watch_dir("path:/definitely/not/here#cid"), None);
        // A github flake has no local dir.
        assert_eq!(watch_dir("github:pleme-io/nix#cid"), None);
    }

    #[tokio::test]
    async fn loop_coalesces_a_burst_into_one_tick() {
        let d = driver();
        let (tx, rx) = mpsc::unbounded_channel();
        // Three triggers already queued, then close the channel.
        tx.send(Trigger::SourceChanged).unwrap();
        tx.send(Trigger::SourceChanged).unwrap();
        tx.send(Trigger::Interval).unwrap();
        drop(tx);
        // Never-firing shutdown; the loop ends when the channel closes.
        d.run_with_triggers(rx, std::future::pending()).await;
        // The burst coalesced to exactly one reconcile tick.
        assert_eq!(d.controller().attested_ticks(), 1);
    }

    #[tokio::test]
    async fn loop_runs_one_tick_per_separated_trigger() {
        let d = driver();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Trigger::Interval).unwrap();
        drop(tx);
        d.run_with_triggers(rx, std::future::pending()).await;
        assert_eq!(d.controller().attested_ticks(), 1);
    }

    #[tokio::test]
    async fn loop_stops_immediately_on_shutdown_without_ticking() {
        let d = driver();
        let (_tx, rx) = mpsc::unbounded_channel(); // keep tx alive so recv would block
        // An already-ready shutdown wins the biased select before any trigger.
        d.run_with_triggers(rx, std::future::ready(())).await;
        assert_eq!(d.controller().attested_ticks(), 0, "shutdown-first ⇒ no tick");
    }
}
