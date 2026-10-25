//! Build activity events: CppNix's `Logger` activity model (`logging.hh`)
//! and its worker-protocol framing (`daemon.cc` `TunnelLogger`,
//! `worker-protocol-connection.cc` `processStderrReturn`).
//!
//! A builder or substitutor emits [`ActivityEvent`]s into an
//! [`ActivitySink`]; the daemon frames them as `STDERR_START_ACTIVITY`,
//! `STDERR_STOP_ACTIVITY`, `STDERR_RESULT` and `STDERR_NEXT`; the client
//! parses the frames back into the same events.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::wire::{self, StderrMsg, WireError};

/// An activity's id: CppNix's `nextId++ + (pid << 32)`.
pub type ActivityId = u64;

/// The first client minor version that receives structured activity frames;
/// older clients get the activity text as a `STDERR_NEXT` line.
pub const STRUCTURED_ACTIVITY_MINOR: u64 = 20;

macro_rules! wire_code {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $code:literal,)* }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)*
            /// A code this enum does not name, carried through unchanged.
            Other(u64),
        }

        impl $name {
            /// The code CppNix puts on the wire.
            #[must_use]
            pub const fn code(self) -> u64 {
                match self {
                    $(Self::$variant => $code,)*
                    Self::Other(code) => code,
                }
            }

            /// The variant for a wire code.
            #[must_use]
            pub const fn from_code(code: u64) -> Self {
                match code {
                    $($code => Self::$variant,)*
                    other => Self::Other(other),
                }
            }
        }
    };
}

wire_code! {
    /// CppNix `Verbosity`.
    Verbosity {
        Error = 0,
        Warn = 1,
        Notice = 2,
        Info = 3,
        Talkative = 4,
        Chatty = 5,
        Debug = 6,
        Vomit = 7,
    }
}

wire_code! {
    /// CppNix `ActivityType`.
    ActivityType {
        Unknown = 0,
        CopyPath = 100,
        FileTransfer = 101,
        Realise = 102,
        CopyPaths = 103,
        Builds = 104,
        Build = 105,
        OptimiseStore = 106,
        VerifyPaths = 107,
        Substitute = 108,
        QueryPathInfo = 109,
        PostBuildHook = 110,
        BuildWaiting = 111,
        FetchTree = 112,
    }
}

wire_code! {
    /// CppNix `ResultType`.
    ResultType {
        FileLinked = 100,
        BuildLogLine = 101,
        UntrustedPath = 102,
        CorruptedPath = 103,
        SetPhase = 104,
        Progress = 105,
        SetExpected = 106,
        PostBuildLogLine = 107,
        FetchStatus = 108,
    }
}

/// One `Logger::Field`: `tInt = 0` or `tString = 1` on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    /// `tInt`.
    Int(u64),
    /// `tString`.
    String(String),
}

impl From<u64> for Field {
    fn from(v: u64) -> Self {
        Self::Int(v)
    }
}

impl From<&str> for Field {
    fn from(v: &str) -> Self {
        Self::String(v.to_string())
    }
}

impl From<String> for Field {
    fn from(v: String) -> Self {
        Self::String(v)
    }
}

/// One logger call, as `TunnelLogger` forwards it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityEvent {
    /// `startActivity`.
    Start {
        id: ActivityId,
        level: Verbosity,
        activity: ActivityType,
        text: String,
        fields: Vec<Field>,
        parent: ActivityId,
    },
    /// `stopActivity`.
    Stop { id: ActivityId },
    /// `result`.
    Result { id: ActivityId, result: ResultType, fields: Vec<Field> },
    /// `log`: one line of text, without its trailing newline.
    Log { level: Verbosity, text: String },
}

/// Where activity events go.
pub trait ActivitySink: Send + Sync {
    /// Receive one event.
    fn emit(&self, event: ActivityEvent);
}

/// A sink that drops every event.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSink;

impl ActivitySink for NoopSink {
    fn emit(&self, _event: ActivityEvent) {}
}

impl ActivitySink for Mutex<Vec<ActivityEvent>> {
    fn emit(&self, event: ActivityEvent) {
        self.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(event);
    }
}

impl ActivitySink for std::sync::mpsc::Sender<ActivityEvent> {
    fn emit(&self, event: ActivityEvent) {
        let _ = self.send(event);
    }
}

/// A shared sink handle.
pub type SharedSink = Arc<dyn ActivitySink>;

/// A sink that drops every event.
#[must_use]
pub fn noop_sink() -> SharedSink {
    Arc::new(NoopSink)
}

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn next_id() -> ActivityId {
    NEXT_ID.fetch_add(1, Ordering::Relaxed) + (u64::from(std::process::id()) << 32)
}

/// A started activity; dropping it stops it (CppNix `Activity`).
pub struct Activity {
    sink: SharedSink,
    id: ActivityId,
}

impl std::fmt::Debug for Activity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Activity").field("id", &self.id).finish_non_exhaustive()
    }
}

impl Activity {
    /// Start an activity under `parent` (0 for none).
    pub fn start(
        sink: &SharedSink,
        level: Verbosity,
        activity: ActivityType,
        text: impl Into<String>,
        fields: Vec<Field>,
        parent: ActivityId,
    ) -> Self {
        let id = next_id();
        sink.emit(ActivityEvent::Start { id, level, activity, text: text.into(), fields, parent });
        Self { sink: sink.clone(), id }
    }

    /// This activity's id.
    #[must_use]
    pub fn id(&self) -> ActivityId {
        self.id
    }

    /// The sink this activity reports to.
    #[must_use]
    pub fn sink(&self) -> &SharedSink {
        &self.sink
    }

    /// Report a result on this activity.
    pub fn result(&self, result: ResultType, fields: Vec<Field>) {
        self.sink.emit(ActivityEvent::Result { id: self.id, result, fields });
    }

    /// `resProgress`: done, expected, running, failed.
    pub fn progress(&self, done: u64, expected: u64, running: u64, failed: u64) {
        self.result(ResultType::Progress, vec![done.into(), expected.into(), running.into(), failed.into()]);
    }

    /// `resSetExpected`: how much of `activity` this one expects.
    pub fn set_expected(&self, activity: ActivityType, expected: u64) {
        self.result(ResultType::SetExpected, vec![activity.code().into(), expected.into()]);
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        self.sink.emit(ActivityEvent::Stop { id: self.id });
    }
}

fn write_fields(w: &mut impl Write, fields: &[Field]) -> io::Result<()> {
    wire::write_u64(w, fields.len() as u64)?;
    for field in fields {
        match field {
            Field::Int(i) => {
                wire::write_u64(w, 0)?;
                wire::write_u64(w, *i)?;
            }
            Field::String(s) => {
                wire::write_u64(w, 1)?;
                wire::write_string(w, s)?;
            }
        }
    }
    Ok(())
}

fn read_fields(r: &mut impl Read) -> Result<Vec<Field>, WireError> {
    let n = wire::read_u64(r)?;
    let mut fields = Vec::new();
    for _ in 0..n {
        match wire::read_u64(r)? {
            0 => fields.push(Field::Int(wire::read_u64(r)?)),
            1 => fields.push(Field::String(wire::read_string(r)?)),
            other => {
                return Err(WireError::Protocol(format!(
                    "got unsupported field type {other:x} from Nix daemon"
                )));
            }
        }
    }
    Ok(fields)
}

fn write_log(w: &mut impl Write, level: Verbosity, verbosity: Verbosity, text: &str) -> io::Result<()> {
    if level.code() > verbosity.code() {
        return Ok(());
    }
    wire::write_u64(w, StderrMsg::Write as u64)?;
    wire::write_string(w, format!("{text}\n"))
}

/// Frame `event` the way `TunnelLogger` does for a client at `client_version`
/// whose `SetOptions` verbosity is `verbosity`. Writes nothing when CppNix
/// would send nothing.
///
/// # Errors
/// Any write failure.
pub fn write_frame(
    w: &mut impl Write,
    event: &ActivityEvent,
    client_version: u64,
    verbosity: Verbosity,
) -> io::Result<()> {
    let structured = (client_version & 0xff) >= STRUCTURED_ACTIVITY_MINOR;
    match event {
        ActivityEvent::Log { level, text } => write_log(w, *level, verbosity, text),
        ActivityEvent::Start { level, text, .. } if !structured => {
            if text.is_empty() { Ok(()) } else { write_log(w, *level, verbosity, &format!("{text}...")) }
        }
        ActivityEvent::Stop { .. } | ActivityEvent::Result { .. } if !structured => Ok(()),
        ActivityEvent::Start { id, level, activity, text, fields, parent } => {
            wire::write_u64(w, StderrMsg::StartActivity as u64)?;
            wire::write_u64(w, *id)?;
            wire::write_u64(w, level.code())?;
            wire::write_u64(w, activity.code())?;
            wire::write_string(w, text)?;
            write_fields(w, fields)?;
            wire::write_u64(w, *parent)
        }
        ActivityEvent::Stop { id } => {
            wire::write_u64(w, StderrMsg::StopActivity as u64)?;
            wire::write_u64(w, *id)
        }
        ActivityEvent::Result { id, result, fields } => {
            wire::write_u64(w, StderrMsg::Result as u64)?;
            wire::write_u64(w, *id)?;
            wire::write_u64(w, result.code())?;
            write_fields(w, fields)
        }
    }
}

/// Read the body of a stderr frame whose code `msg` was already read.
/// `None` when `msg` is not a log or activity frame; the caller handles it.
/// A `STDERR_NEXT` line becomes [`ActivityEvent::Log`] at [`Verbosity::Error`],
/// the level CppNix's client prints it at.
///
/// # Errors
/// Truncated input or an unknown field type.
pub fn read_frame(r: &mut impl Read, msg: u64) -> Result<Option<ActivityEvent>, WireError> {
    let event = match msg {
        m if m == StderrMsg::Write as u64 => {
            let mut text = wire::read_string(r)?;
            if text.ends_with('\n') {
                text.pop();
            }
            ActivityEvent::Log { level: Verbosity::Error, text }
        }
        m if m == StderrMsg::StartActivity as u64 => {
            let id = wire::read_u64(r)?;
            let level = Verbosity::from_code(wire::read_u64(r)?);
            let activity = ActivityType::from_code(wire::read_u64(r)?);
            let text = wire::read_string(r)?;
            let fields = read_fields(r)?;
            let parent = wire::read_u64(r)?;
            ActivityEvent::Start { id, level, activity, text, fields, parent }
        }
        m if m == StderrMsg::StopActivity as u64 => ActivityEvent::Stop { id: wire::read_u64(r)? },
        m if m == StderrMsg::Result as u64 => {
            let id = wire::read_u64(r)?;
            let result = ResultType::from_code(wire::read_u64(r)?);
            let fields = read_fields(r)?;
            ActivityEvent::Result { id, result, fields }
        }
        _ => return Ok(None),
    };
    Ok(Some(event))
}

/// A build's log, line by line, as CppNix's `DerivationBuildingGoal`
/// `handleChildOutput` + `flushLine` read it: `\r` rewinds the line, `\n`
/// ends it, an `@nix {json}` line is a structured message, and any other line
/// is a `resBuildLogLine` on the build activity.
pub struct BuilderLog {
    sink: SharedSink,
    build: ActivityId,
    trusted: bool,
    line: Vec<u8>,
    pos: usize,
    activities: HashMap<u64, Activity>,
}

impl std::fmt::Debug for BuilderLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuilderLog").field("build", &self.build).finish_non_exhaustive()
    }
}

impl BuilderLog {
    /// The log of `build`. An untrusted builder (CppNix's local builder) may
    /// start only `actFileTransfer` activities.
    #[must_use]
    pub fn new(build: &Activity, trusted: bool) -> Self {
        Self {
            sink: build.sink().clone(),
            build: build.id(),
            trusted,
            line: Vec::new(),
            pos: 0,
            activities: HashMap::new(),
        }
    }

    /// Feed builder output bytes.
    pub fn feed(&mut self, data: &[u8]) {
        for &c in data {
            match c {
                b'\r' => self.pos = 0,
                b'\n' => self.flush(),
                c => {
                    if self.pos >= self.line.len() {
                        self.line.resize(self.pos + 1, 0);
                    }
                    self.line[self.pos] = c;
                    self.pos += 1;
                }
            }
        }
    }

    /// End of output: flush a final unterminated line.
    pub fn finish(&mut self) {
        if !self.line.is_empty() {
            self.flush();
        }
    }

    fn flush(&mut self) {
        let line = String::from_utf8_lossy(&self.line).into_owned();
        if !self.handle_json(&line) {
            self.sink.emit(ActivityEvent::Result {
                id: self.build,
                result: ResultType::BuildLogLine,
                fields: vec![Field::String(line)],
            });
        }
        self.line.clear();
        self.pos = 0;
    }

    fn handle_json(&mut self, line: &str) -> bool {
        let Some(json) = line.strip_prefix("@nix ") else {
            return false;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(json) else {
            return false;
        };
        self.handle_message(&json).is_some()
    }

    fn handle_message(&mut self, json: &serde_json::Value) -> Option<()> {
        let action = json.get("action")?.as_str()?;
        match action {
            "start" => {
                let activity = ActivityType::from_code(json.get("type")?.as_u64()?);
                if self.trusted || activity == ActivityType::FileTransfer {
                    let id = json.get("id")?.as_u64()?;
                    let level = Verbosity::from_code(json.get("level")?.as_u64()?);
                    let text = json.get("text")?.as_str()?;
                    let fields = json_fields(json.get("fields"))?;
                    let act = Activity::start(&self.sink, level, activity, text, fields, self.build);
                    self.activities.entry(id).or_insert(act);
                }
            }
            "stop" => {
                let id = json.get("id")?.as_u64()?;
                self.activities.remove(&id);
            }
            "result" => {
                let id = json.get("id")?.as_u64()?;
                if let Some(act) = self.activities.get(&id) {
                    let result = ResultType::from_code(json.get("type")?.as_u64()?);
                    let fields = json_fields(json.get("fields"))?;
                    act.result(result, fields);
                }
            }
            "setPhase" => {
                let phase = json.get("phase")?.as_str()?;
                self.sink.emit(ActivityEvent::Result {
                    id: self.build,
                    result: ResultType::SetPhase,
                    fields: vec![Field::String(phase.to_string())],
                });
            }
            "msg" => {
                let text = json.get("msg")?.as_str()?.to_string();
                let level = Verbosity::from_code(json.get("level")?.as_u64()?);
                self.sink.emit(ActivityEvent::Log { level, text });
            }
            _ => {}
        }
        Some(())
    }
}

fn json_fields(value: Option<&serde_json::Value>) -> Option<Vec<Field>> {
    let Some(value) = value else {
        return Some(Vec::new());
    };
    let items: Vec<&serde_json::Value> = match value {
        serde_json::Value::Null => Vec::new(),
        serde_json::Value::Array(items) => items.iter().collect(),
        serde_json::Value::Object(map) => map.values().collect(),
        scalar => vec![scalar],
    };
    items
        .into_iter()
        .map(|f| match f {
            serde_json::Value::String(s) => Some(Field::String(s.clone())),
            n => n.as_u64().map(Field::Int),
        })
        .collect()
}

/// The three activities CppNix's `Worker` holds for one realisation, and the
/// counters its `updateProgress` reports.
pub struct Worker {
    copies: Activity,
    builds: Activity,
    realise: Activity,
    counters: Mutex<Counters>,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker").field("realise", &self.realise.id()).finish_non_exhaustive()
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Counters {
    expected_builds: u64,
    done_builds: u64,
    running_builds: u64,
    failed_builds: u64,
    expected_substitutions: u64,
    done_substitutions: u64,
    running_substitutions: u64,
    failed_substitutions: u64,
    expected_download: u64,
    done_download: u64,
    expected_nar: u64,
    done_nar: u64,
}

impl Worker {
    /// Start `actRealise`, `actBuilds` and `actCopyPaths`, in that order.
    #[must_use]
    pub fn start(sink: &SharedSink) -> Self {
        let start = |activity| Activity::start(sink, Verbosity::Error, activity, "", Vec::new(), 0);
        Self {
            realise: start(ActivityType::Realise),
            builds: start(ActivityType::Builds),
            copies: start(ActivityType::CopyPaths),
            counters: Mutex::new(Counters::default()),
        }
    }

    /// The sink the worker reports to.
    #[must_use]
    pub fn sink(&self) -> &SharedSink {
        self.realise.sink()
    }

    fn update(&self, change: impl FnOnce(&mut Counters)) {
        let c = {
            let mut counters = self.counters.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            change(&mut counters);
            *counters
        };
        self.builds.progress(c.done_builds, c.expected_builds + c.done_builds, c.running_builds, c.failed_builds);
        self.copies.progress(
            c.done_substitutions,
            c.expected_substitutions + c.done_substitutions,
            c.running_substitutions,
            c.failed_substitutions,
        );
        self.realise.set_expected(ActivityType::FileTransfer, c.expected_download + c.done_download);
        self.realise.set_expected(ActivityType::CopyPath, c.expected_nar + c.done_nar);
    }

    /// `n` more derivations will be built.
    pub fn expect_builds(&self, n: u64) {
        self.update(|c| c.expected_builds += n);
    }

    /// A build starts.
    pub fn build_started(&self) {
        self.update(|c| c.running_builds += 1);
    }

    /// A build ends.
    pub fn build_finished(&self, succeeded: bool) {
        self.update(|c| {
            c.running_builds = c.running_builds.saturating_sub(1);
            c.expected_builds = c.expected_builds.saturating_sub(1);
            if succeeded {
                c.done_builds += 1;
            } else {
                c.failed_builds += 1;
            }
        });
    }

    /// A substitution of `download` compressed and `nar` unpacked bytes starts.
    pub fn substitution_started(&self, download: u64, nar: u64) {
        self.update(|c| {
            c.expected_substitutions += 1;
            c.running_substitutions += 1;
            c.expected_download += download;
            c.expected_nar += nar;
        });
    }

    /// The substitution started with `download` and `nar` ends.
    pub fn substitution_finished(&self, download: u64, nar: u64, succeeded: bool) {
        self.update(|c| {
            c.running_substitutions = c.running_substitutions.saturating_sub(1);
            c.expected_substitutions = c.expected_substitutions.saturating_sub(1);
            c.expected_download = c.expected_download.saturating_sub(download);
            c.expected_nar = c.expected_nar.saturating_sub(nar);
            if succeeded {
                c.done_substitutions += 1;
                c.done_download += download;
                c.done_nar += nar;
            } else {
                c.failed_substitutions += 1;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le(v: u64) -> Vec<u8> {
        v.to_le_bytes().to_vec()
    }

    fn padded(s: &str) -> Vec<u8> {
        let mut out = le(s.len() as u64);
        out.extend_from_slice(s.as_bytes());
        out.resize(8 + s.len().div_ceil(8) * 8, 0);
        out
    }

    fn frame(event: &ActivityEvent, minor: u64) -> Vec<u8> {
        let mut out = Vec::new();
        write_frame(&mut out, event, (1 << 8) | minor, Verbosity::Info).unwrap();
        out
    }

    fn recorder() -> (Arc<Mutex<Vec<ActivityEvent>>>, SharedSink) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink: SharedSink = events.clone();
        (events, sink)
    }

    #[test]
    fn codes_are_cppnix_logging_hh_numbering() {
        assert_eq!(ActivityType::CopyPath.code(), 100);
        assert_eq!(ActivityType::Builds.code(), 104);
        assert_eq!(ActivityType::Build.code(), 105);
        assert_eq!(ActivityType::Substitute.code(), 108);
        assert_eq!(ActivityType::FetchTree.code(), 112);
        assert_eq!(ResultType::BuildLogLine.code(), 101);
        assert_eq!(ResultType::SetPhase.code(), 104);
        assert_eq!(ResultType::Progress.code(), 105);
        assert_eq!(ResultType::SetExpected.code(), 106);
        assert_eq!(ResultType::FetchStatus.code(), 108);
        assert_eq!(ActivityType::from_code(113), ActivityType::Other(113));
        assert_eq!(Verbosity::from_code(3), Verbosity::Info);
    }

    #[test]
    fn start_activity_frame_is_tunnel_logger_layout() {
        let event = ActivityEvent::Start {
            id: 0x0000_1234_0000_0007,
            level: Verbosity::Info,
            activity: ActivityType::Build,
            text: "building '/nix/store/x.drv'".into(),
            fields: vec!["/nix/store/x.drv".into(), "".into(), 1.into(), 1.into()],
            parent: 0x0000_1234_0000_0003,
        };
        let mut want = le(0x5354_5254);
        want.extend(le(0x0000_1234_0000_0007));
        want.extend(le(3));
        want.extend(le(105));
        want.extend(padded("building '/nix/store/x.drv'"));
        want.extend(le(4));
        want.extend(le(1));
        want.extend(padded("/nix/store/x.drv"));
        want.extend(le(1));
        want.extend(padded(""));
        want.extend(le(0));
        want.extend(le(1));
        want.extend(le(0));
        want.extend(le(1));
        want.extend(le(0x0000_1234_0000_0003));
        assert_eq!(frame(&event, 37), want, "daemon.cc:158 STDERR_START_ACTIVITY act lvl type s fields parent; :29-42 fields");
    }

    #[test]
    fn stop_and_result_frames_are_tunnel_logger_layout() {
        let mut want = le(0x5354_4f50);
        want.extend(le(42));
        assert_eq!(frame(&ActivityEvent::Stop { id: 42 }, 37), want, "daemon.cc:167 STDERR_STOP_ACTIVITY act");

        let event = ActivityEvent::Result {
            id: 42,
            result: ResultType::Progress,
            fields: vec![1.into(), 3.into(), 1.into(), 0.into()],
        };
        let mut want = le(0x5253_4c54);
        want.extend(le(42));
        want.extend(le(105));
        want.extend(le(4));
        for v in [1, 3, 1, 0] {
            want.extend(le(0));
            want.extend(le(v));
        }
        assert_eq!(frame(&event, 37), want, "daemon.cc:176 STDERR_RESULT act type fields");
    }

    #[test]
    fn a_pre_1_20_client_gets_activity_text_as_a_next_line_and_no_structured_frames() {
        let start = ActivityEvent::Start {
            id: 1,
            level: Verbosity::Info,
            activity: ActivityType::Build,
            text: "building 'x'".into(),
            fields: vec![],
            parent: 0,
        };
        let mut want = le(0x6f6c_6d67);
        want.extend(padded("building 'x'...\n"));
        assert_eq!(frame(&start, 19), want, "daemon.cc:151-153 log(lvl, s + \"...\") below 1.20");
        let silent = ActivityEvent::Start {
            id: 1,
            level: Verbosity::Info,
            activity: ActivityType::Build,
            text: String::new(),
            fields: vec![],
            parent: 0,
        };
        assert!(frame(&silent, 19).is_empty());
        assert!(frame(&ActivityEvent::Stop { id: 1 }, 19).is_empty());
        let result = ActivityEvent::Result { id: 1, result: ResultType::Progress, fields: vec![] };
        assert!(frame(&result, 19).is_empty());
        assert!(!frame(&result, 20).is_empty());
    }

    #[test]
    fn a_log_line_is_a_next_frame_filtered_by_the_client_verbosity() {
        let mut want = le(0x6f6c_6d67);
        want.extend(padded("hello\n"));
        assert_eq!(
            frame(&ActivityEvent::Log { level: Verbosity::Info, text: "hello".into() }, 37),
            want,
            "daemon.cc:88-92 STDERR_NEXT s + newline unless lvl > verbosity"
        );
        assert!(frame(&ActivityEvent::Log { level: Verbosity::Talkative, text: "hello".into() }, 37).is_empty());
    }

    #[test]
    fn frames_read_back_as_the_events_written() {
        let events = vec![
            ActivityEvent::Start {
                id: 9,
                level: Verbosity::Error,
                activity: ActivityType::Substitute,
                text: String::new(),
                fields: vec!["/nix/store/p".into(), "https://cache.nixos.org".into()],
                parent: 0,
            },
            ActivityEvent::Result { id: 9, result: ResultType::Other(777), fields: vec![5.into()] },
            ActivityEvent::Log { level: Verbosity::Error, text: "a line".into() },
            ActivityEvent::Stop { id: 9 },
        ];
        let mut bytes = Vec::new();
        for e in &events {
            write_frame(&mut bytes, e, (1 << 8) | 37, Verbosity::Error).unwrap();
        }
        let mut r = io::Cursor::new(bytes);
        let mut back = Vec::new();
        while (r.position() as usize) < r.get_ref().len() {
            let msg = wire::read_u64(&mut r).unwrap();
            back.push(read_frame(&mut r, msg).unwrap().unwrap());
        }
        assert_eq!(back, events);
        assert!(read_frame(&mut io::Cursor::new(Vec::new()), StderrMsg::Last as u64).unwrap().is_none());
    }

    fn build_log(trusted: bool, output: &[u8]) -> (ActivityId, Vec<ActivityEvent>) {
        let (events, sink) = recorder();
        let build = Activity::start(&sink, Verbosity::Info, ActivityType::Build, "b", vec![], 0);
        let id = build.id();
        {
            let mut log = BuilderLog::new(&build, trusted);
            log.feed(output);
            log.finish();
        }
        drop(build);
        let mut all = events.lock().unwrap().clone();
        all.remove(0);
        all.pop();
        (id, all)
    }

    fn log_line(id: ActivityId, line: &str) -> ActivityEvent {
        ActivityEvent::Result { id, result: ResultType::BuildLogLine, fields: vec![line.into()] }
    }

    #[test]
    fn a_set_phase_line_is_a_set_phase_result_not_a_log_line() {
        let (id, events) = build_log(
            false,
            b"unpacking\n@nix { \"action\": \"setPhase\", \"phase\": \"buildPhase\" }\nbuilding\n",
        );
        assert_eq!(
            events,
            vec![
                log_line(id, "unpacking"),
                ActivityEvent::Result { id, result: ResultType::SetPhase, fields: vec!["buildPhase".into()] },
                log_line(id, "building"),
            ]
        );
    }

    #[test]
    fn carriage_return_rewinds_and_an_unterminated_tail_is_flushed() {
        let (id, events) = build_log(false, b"10%\r55%\rdone\nlast");
        assert_eq!(events, vec![log_line(id, "done"), log_line(id, "last")]);
        let (id, events) = build_log(false, b"abcdef\rxy\n");
        assert_eq!(events, vec![log_line(id, "xycdef")]);
    }

    #[test]
    fn malformed_nix_lines_stay_log_lines_and_unknown_actions_are_swallowed() {
        let (id, events) = build_log(
            false,
            b"@nix {not json\n@nix {\"action\":\"setPhase\"}\n@nix {\"action\":\"frobnicate\"}\n",
        );
        assert_eq!(
            events,
            vec![log_line(id, "@nix {not json"), log_line(id, "@nix {\"action\":\"setPhase\"}")]
        );
    }

    #[test]
    fn an_untrusted_builder_may_start_only_file_transfers() {
        let out = b"@nix {\"action\":\"start\",\"id\":5,\"type\":105,\"level\":3,\"text\":\"x\",\"fields\":[]}\n\
@nix {\"action\":\"start\",\"id\":6,\"type\":101,\"level\":4,\"text\":\"dl\",\"fields\":[\"u\"]}\n\
@nix {\"action\":\"result\",\"id\":6,\"type\":105,\"fields\":[10,20]}\n\
@nix {\"action\":\"result\",\"id\":5,\"type\":105,\"fields\":[1]}\n\
@nix {\"action\":\"stop\",\"id\":6}\n\
@nix {\"action\":\"msg\",\"level\":0,\"msg\":\"warning\"}\n";
        let (build, events) = build_log(false, out);
        assert_eq!(events.len(), 4);
        let ActivityEvent::Start { id, activity, parent, fields, .. } = &events[0] else { panic!("{events:?}") };
        assert_eq!((*activity, *parent), (ActivityType::FileTransfer, build));
        assert_eq!(fields, &vec![Field::from("u")]);
        assert_eq!(
            events[1],
            ActivityEvent::Result { id: *id, result: ResultType::Progress, fields: vec![10.into(), 20.into()] }
        );
        assert_eq!(events[2], ActivityEvent::Stop { id: *id });
        assert_eq!(events[3], ActivityEvent::Log { level: Verbosity::Error, text: "warning".into() });
        let (_, trusted) = build_log(true, out);
        assert_eq!(trusted.len(), 7, "{trusted:?}");
        let ActivityEvent::Start { id: unstopped, activity: ActivityType::Build, .. } = trusted[0] else {
            panic!("{trusted:?}")
        };
        assert_eq!(trusted[6], ActivityEvent::Stop { id: unstopped }, "the builder's open activities stop with its log");
    }

    #[test]
    fn the_worker_reports_progress_the_way_update_progress_does() {
        let (events, sink) = recorder();
        let worker = Worker::start(&sink);
        worker.expect_builds(2);
        worker.build_started();
        worker.build_finished(true);
        worker.substitution_started(10, 30);
        worker.substitution_finished(10, 30, true);
        let events = events.lock().unwrap().clone();
        let ids: Vec<(ActivityType, ActivityId)> = events[..3]
            .iter()
            .map(|e| match e {
                ActivityEvent::Start { activity, id, parent: 0, .. } => (*activity, *id),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            ids.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
            vec![ActivityType::Realise, ActivityType::Builds, ActivityType::CopyPaths]
        );
        let progress = |id: ActivityId, v: [u64; 4]| ActivityEvent::Result {
            id,
            result: ResultType::Progress,
            fields: v.iter().map(|n| Field::Int(*n)).collect(),
        };
        let expected = |kind: ActivityType, n: u64| ActivityEvent::Result {
            id: ids[0].1,
            result: ResultType::SetExpected,
            fields: vec![kind.code().into(), n.into()],
        };
        let last = &events[events.len() - 4..];
        assert_eq!(
            last,
            &[
                progress(ids[1].1, [1, 2, 0, 0]),
                progress(ids[2].1, [1, 1, 0, 0]),
                expected(ActivityType::FileTransfer, 10),
                expected(ActivityType::CopyPath, 30),
            ]
        );
    }
}
