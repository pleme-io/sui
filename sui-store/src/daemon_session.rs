//! The process's one long-lived daemon connection.
//!
//! CppNix keeps its `RemoteStore` connections open for the life of the process,
//! and the daemon ties per-client state to a connection: most importantly the
//! temporary GC roots it registers for every path the client adds. A client that
//! opens a connection per operation and closes it again throws that state away
//! after each call. This module holds one connection per daemon socket for as
//! long as the process runs, shared by every thread.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::worker_client::{WorkerConn, WorkerError};

struct Session {
    socket: PathBuf,
    conn: WorkerConn,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// Run `f` on the process's connection to `socket`, connecting on first use.
///
/// A transport or framing failure drops the connection (its framing can no
/// longer be trusted) and the next call reconnects. A daemon refusal leaves the
/// connection in place: the daemon answered in-protocol, and the connection's
/// state is still valid.
///
/// # Errors
/// A connect failure or whatever `f` returns.
pub fn with_session<T>(
    socket: &Path,
    f: impl FnOnce(&mut WorkerConn) -> Result<T, WorkerError>,
) -> Result<T, WorkerError> {
    let mut guard = SESSION.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.as_ref().is_some_and(|s| s.socket != socket) {
        *guard = None;
    }
    if guard.is_none() {
        let conn = WorkerConn::connect(socket, None)?;
        *guard = Some(Session { socket: socket.to_path_buf(), conn });
    }
    let session = guard.as_mut().expect("session connected above");
    let result = f(&mut session.conn);
    if matches!(result, Err(WorkerError::Protocol(_) | WorkerError::Unreachable { .. })) {
        *guard = None;
    }
    result
}
