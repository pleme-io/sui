//! The live face of a realize: cppnix activity events folded into a kazari
//! progress board on stderr, finished derivations committed above it.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kazari::progress::{Board, Labels, LaneKind};
use kazari::{Capability, Fragment, Live, Role, Stream};
use sui_compat::activity::{ActivityEvent, ActivitySink, ActivityType, Field, ResultType, SharedSink};

enum Msg {
    Plan(HashMap<String, Vec<String>>),
    Event(ActivityEvent),
    Done,
}

/// A running face. Drop or [`BuildFace::finish`] paints the last frame.
pub struct BuildFace {
    tx: Mutex<mpsc::Sender<Msg>>,
    painter: Mutex<Option<JoinHandle<()>>>,
}

struct Relay(Mutex<mpsc::Sender<Msg>>);

impl ActivitySink for Relay {
    fn emit(&self, event: ActivityEvent) {
        if let Ok(tx) = self.0.lock() {
            let _ = tx.send(Msg::Event(event));
        }
    }
}

impl BuildFace {
    /// A face titled `sui · <verb> <target>`.
    #[must_use]
    pub fn start(verb: &str, target: &str) -> Arc<Self> {
        let (tx, rx) = mpsc::channel();
        let labels = Labels {
            owner: "sui".into(),
            verb: verb.into(),
            target: target.into(),
            work: "built".into(),
            transfer: "fetched".into(),
        };
        let painter = std::thread::spawn(move || paint(&rx, labels));
        Arc::new(Self { tx: Mutex::new(tx), painter: Mutex::new(Some(painter)) })
    }

    /// Paint the last frame and wait for the painter.
    pub fn finish(&self) {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(Msg::Done);
        }
        if let Some(handle) = self.painter.lock().ok().and_then(|mut p| p.take()) {
            let _ = handle.join();
        }
    }
}

impl Drop for BuildFace {
    fn drop(&mut self) {
        self.finish();
    }
}

impl sui_orchestrate::system::RealizeObserver for BuildFace {
    fn planned(&self, closure: &sui_build::BuildClosure) {
        let inputs = needed_graph(closure, |path| std::path::Path::new(path).exists());
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(Msg::Plan(inputs));
        }
    }

    fn sink(&self) -> SharedSink {
        let tx = self.tx.lock().map(|t| t.clone()).unwrap_or_else(|_| mpsc::channel().0);
        Arc::new(Relay(Mutex::new(tx)))
    }
}

/// The derivations a realize of the closure's target will build, with their
/// inputs among them: walked down from the target, entering an input only
/// while one of its outputs is missing, as the planner does.
fn needed_graph(closure: &sui_build::BuildClosure, present: impl Fn(&str) -> bool) -> HashMap<String, Vec<String>> {
    let index: HashMap<&str, &sui_compat::derivation::Derivation> =
        closure.derivations.iter().map(|(p, d)| (p.as_str(), d)).collect();
    let missing = |d: &sui_compat::derivation::Derivation| d.outputs.values().any(|o| !o.path.is_empty() && !present(&o.path));
    let mut graph = HashMap::new();
    let mut stack: Vec<&str> = closure.derivations.last().map(|(p, _)| p.as_str()).into_iter().collect();
    while let Some(path) = stack.pop() {
        if graph.contains_key(path) {
            continue;
        }
        let Some(drv) = index.get(path) else { continue };
        if !missing(drv) {
            continue;
        }
        let deps: Vec<&str> = drv
            .input_derivations
            .keys()
            .map(String::as_str)
            .filter(|k| index.get(k).is_some_and(|d| missing(d)))
            .collect();
        graph.insert(path.to_string(), deps.iter().map(|d| (*d).to_string()).collect());
        stack.extend(deps);
    }
    graph
}

fn text_field(fields: &[Field], i: usize) -> Option<&str> {
    match fields.get(i) {
        Some(Field::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

fn int_field(fields: &[Field], i: usize) -> u64 {
    match fields.get(i) {
        Some(Field::Int(n)) => *n,
        _ => 0,
    }
}

struct Fold {
    board: Board<String>,
    kinds: HashMap<u64, ActivityType>,
    parents: HashMap<u64, u64>,
}

impl Fold {
    fn apply<E: kazari::RenderEnvironment>(&mut self, event: ActivityEvent, now: Duration, live: &mut Live<E>, caps: &Capability) {
        match event {
            ActivityEvent::Start { id, activity, fields, parent, .. } => {
                self.kinds.insert(id, activity);
                self.parents.insert(id, parent);
                match activity {
                    ActivityType::Build => {
                        let drv = text_field(&fields, 0).unwrap_or_default().to_string();
                        let name = display_name(&drv);
                        self.board.begin(id, Some(&drv), name, LaneKind::Work, now);
                    }
                    ActivityType::Substitute => {
                        let path = text_field(&fields, 0).unwrap_or_default();
                        self.board.begin(id, None, display_name(path), LaneKind::Transfer, now);
                    }
                    _ => {}
                }
            }
            ActivityEvent::Result { id, result, fields } => match (result, self.kinds.get(&id).copied()) {
                (ResultType::SetPhase, _) => {
                    if let Some(phase) = text_field(&fields, 0) {
                        self.board.phase(id, phase);
                    }
                }
                (ResultType::Progress, Some(ActivityType::Builds)) => {
                    self.board.planned_work = self.board.planned_work.max(int_field(&fields, 1) as usize);
                }
                (ResultType::Progress, Some(ActivityType::CopyPaths)) => {
                    self.board.planned_transfer = self.board.planned_transfer.max(int_field(&fields, 1) as usize);
                }
                (ResultType::Progress, Some(ActivityType::FileTransfer | ActivityType::CopyPath)) => {
                    let mut owner = id;
                    while let Some(&p) = self.parents.get(&owner) {
                        if self.board.lanes.contains_key(&owner) || p == 0 {
                            break;
                        }
                        owner = p;
                    }
                    self.board.progress(owner, int_field(&fields, 0), int_field(&fields, 1));
                }
                _ => {}
            },
            ActivityEvent::Stop { id } => {
                self.kinds.remove(&id);
                self.parents.remove(&id);
                if let Some(finished) = self.board.end(id, true, now) {
                    let _ = live.commit(&Board::<String>::finished_line(&finished, caps));
                }
            }
            ActivityEvent::Log { text, .. } => {
                let _ = live.commit(&vec![Fragment::styled(text, Role::TextMuted)]);
            }
        }
    }
}

/// A store path as people say it: no hash, no `.drv`.
#[must_use]
pub fn display_name(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    let base = base.strip_suffix(".drv").unwrap_or(base);
    match base.split_once('-') {
        Some((hash, rest)) if hash.len() == 32 => rest.to_string(),
        _ => base.to_string(),
    }
}

fn paint(rx: &mpsc::Receiver<Msg>, labels: Labels) {
    let caps = Capability::cached(Stream::Stderr);
    let mut live = Live::stderr();
    let start = Instant::now();
    let mut fold = Fold { board: Board::new(labels), kinds: HashMap::new(), parents: HashMap::new() };
    loop {
        match rx.recv_timeout(Duration::from_millis(80)) {
            Ok(Msg::Plan(inputs)) => {
                let transfers = fold.board.planned_transfer;
                fold.board.plan(&inputs, transfers);
            }
            Ok(Msg::Event(event)) => fold.apply(event, start.elapsed(), &mut live, &caps),
            Ok(Msg::Done) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let width = usize::from(caps.width);
        let _ = live.frame(&fold.board.frame(start.elapsed(), &caps, width));
    }
    let width = usize::from(caps.width);
    let _ = live.frame(&fold.board.frame(start.elapsed(), &caps, width));
    let _ = live.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_path_shows_its_name_without_hash_or_drv() {
        assert_eq!(display_name("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-rust_tokio-1.47.drv"), "rust_tokio-1.47");
        assert_eq!(display_name("plain"), "plain");
    }

    #[test]
    fn only_what_the_target_still_needs_is_planned() {
        let drv = |out: &str, inputs: &[&str]| {
            let mut d = sui_compat::derivation::Derivation::default();
            d.outputs.insert(
                "out".into(),
                sui_compat::derivation::DerivationOutput { path: out.into(), ..Default::default() },
            );
            for i in inputs {
                d.input_derivations.insert((*i).to_string(), vec!["out".into()]);
            }
            d
        };
        let closure = sui_build::BuildClosure {
            derivations: vec![
                ("src.drv".into(), drv("/o/src", &[])),
                ("built.drv".into(), drv("/o/built", &["src.drv"])),
                ("leaf.drv".into(), drv("/o/leaf", &[])),
                ("top.drv".into(), drv("/o/top", &["built.drv", "leaf.drv"])),
            ],
        };
        let graph = needed_graph(&closure, |p| p == "/o/built");
        let mut keys: Vec<&str> = graph.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["leaf.drv", "top.drv"]);
        assert_eq!(graph["top.drv"], vec!["leaf.drv".to_string()]);
    }

    #[test]
    fn transfer_bytes_land_on_the_substitution_that_owns_them() {
        let labels = Labels { owner: "sui".into(), verb: "v".into(), target: "t".into(), work: "w".into(), transfer: "f".into() };
        let mut fold = Fold { board: Board::new(labels), kinds: HashMap::new(), parents: HashMap::new() };
        let caps = Capability::plain();
        let mut live = Live::new(kazari::MockEnvironment::new(caps));
        let start = |id, activity, fields, parent| ActivityEvent::Start {
            id,
            level: sui_compat::activity::Verbosity::Info,
            activity,
            text: String::new(),
            fields,
            parent,
        };
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src".to_string();
        fold.apply(start(1, ActivityType::Substitute, vec![Field::String(path), Field::String("c".into())], 0), Duration::ZERO, &mut live, &caps);
        fold.apply(start(2, ActivityType::CopyPath, vec![], 1), Duration::ZERO, &mut live, &caps);
        fold.apply(start(3, ActivityType::FileTransfer, vec![], 2), Duration::ZERO, &mut live, &caps);
        fold.apply(
            ActivityEvent::Result { id: 3, result: ResultType::Progress, fields: vec![Field::Int(40), Field::Int(100), Field::Int(0), Field::Int(0)] },
            Duration::ZERO,
            &mut live,
            &caps,
        );
        assert_eq!(fold.board.lanes[&1].bytes, (40, 100));
        assert_eq!(fold.board.lanes[&1].name, "src");
        fold.apply(ActivityEvent::Stop { id: 1 }, Duration::from_secs(1), &mut live, &caps);
        assert_eq!((fold.board.transferred, fold.board.bytes), (1, 100));
    }
}
