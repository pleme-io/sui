//! The process's one long-lived daemon connection, and the GC roots it holds.
//!
//! CppNix keeps its `RemoteStore` connections open for the life of the process,
//! and the daemon ties per-client state to a connection: the temporary GC roots
//! it registers for every path the client adds (`AddTextToStore`) or names
//! (`AddTempRoot`). They last until the connection closes. A client that opens a
//! connection per operation throws its roots away after each call, and a
//! collector running between "built" and "linked" deletes what was just built.
//!
//! This module holds one connection per daemon socket for as long as the
//! process runs, shared by every thread, and remembers every path it rooted. If
//! the connection has to be re-opened, the roots are registered again on the new
//! one before anything else runs on it, so a reconnect cannot silently drop
//! them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::worker_client::{WorkerConn, WorkerError};

struct Session {
    conn: Option<WorkerConn>,
    rooted: BTreeSet<String>,
}

/// One session per daemon socket, each behind its own lock, so a daemon that
/// stalls holds up only the callers talking to it.
static SESSIONS: Mutex<BTreeMap<PathBuf, Arc<Mutex<Session>>>> = Mutex::new(BTreeMap::new());

/// Bound on any single session read or write. A stalled daemon then surfaces
/// as an error (and a reconnect) instead of wedging every caller forever.
const SESSION_IO_TIMEOUT: Duration = Duration::from_secs(120);

fn session_for(socket: &Path) -> Arc<Mutex<Session>> {
    let mut map = SESSIONS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    map.entry(socket.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(Session { conn: None, rooted: BTreeSet::new() })))
        .clone()
}

fn lock(s: &Mutex<Session>) -> std::sync::MutexGuard<'_, Session> {
    s.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run `f` on the process's connection to `socket`, connecting on first use.
///
/// A transport or framing failure drops the connection and the next call
/// reconnects, re-registering every temp root first. A daemon refusal leaves
/// the connection in place: the daemon answered in-protocol and the
/// connection's state is still valid.
///
/// # Errors
/// A connect failure, a failed re-registration, or whatever `f` returns.
pub fn with_session<T>(
    socket: &Path,
    f: impl FnOnce(&mut WorkerConn) -> Result<T, WorkerError>,
) -> Result<T, WorkerError> {
    let handle = session_for(socket);
    let mut session = lock(&handle);
    if session.conn.is_none() {
        let mut conn = WorkerConn::connect(socket, Some(SESSION_IO_TIMEOUT))?;
        for path in &session.rooted {
            conn.add_temp_root(path)?;
        }
        session.conn = Some(conn);
    }
    let conn = session.conn.as_mut().expect("connected above");
    let result = f(conn);
    if matches!(result, Err(WorkerError::Protocol(_) | WorkerError::Unreachable { .. })) {
        session.conn = None;
    }
    result
}

fn remember(socket: &Path, path: &str) {
    lock(&session_for(socket)).rooted.insert(path.to_string());
}

/// Add a text store object and keep it rooted for the life of the process.
///
/// # Errors
/// Any [`WorkerError`].
pub fn add_text_to_store(
    socket: &Path,
    name: &str,
    text: &[u8],
    references: &[String],
) -> Result<String, WorkerError> {
    let stored = with_session(socket, |c| c.add_text_to_store(name, text, references))?;
    remember(socket, &stored);
    Ok(stored)
}

/// Keep `path` alive against garbage collection for the life of the process.
/// The path need not exist yet: rooting an output before building it closes
/// the window between the build finishing and the caller linking it.
///
/// # Errors
/// Any [`WorkerError`].
pub fn add_temp_root(socket: &Path, path: &str) -> Result<(), WorkerError> {
    with_session(socket, |c| c.add_temp_root(path))?;
    remember(socket, path);
    Ok(())
}

/// Register `link` (an absolute symlink outside the store pointing into it) as
/// a permanent indirect GC root, as CppNix does for `./result`.
///
/// # Errors
/// Any [`WorkerError`].
pub fn add_indirect_root(socket: &Path, link: &str) -> Result<(), WorkerError> {
    with_session(socket, |c| c.add_indirect_root(link))
}

/// The paths this process currently holds temp roots on through `socket`.
#[must_use]
pub fn rooted(socket: &Path) -> Vec<String> {
    lock(&session_for(socket)).rooted.iter().cloned().collect()
}

/// Close the connection to `socket` and release every temp root it holds, as
/// process exit would.
pub fn release(socket: &Path) {
    SESSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(socket);
}
