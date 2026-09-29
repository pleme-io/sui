//! Reaping a child with its resource usage: the crate's one `wait4` seam.
//!
//! `std::process::Child::wait` returns only the exit status; the peak RSS of a
//! child is only available from `wait4`'s `rusage`. This module is the single
//! place that calls it.

use std::io;

/// A reaped child's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildUsage {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// Peak resident set size in bytes. Darwin reports `ru_maxrss` in bytes,
    /// Linux in KiB; this is always bytes.
    pub max_rss_bytes: u64,
}

/// Block until child `pid` exits, reap it and return its usage.
///
/// The caller must not also `wait` on the child: this reaps it.
///
/// # Errors
/// The OS error from `wait4`.
#[allow(unsafe_code)]
pub fn wait(pid: u32) -> io::Result<ChildUsage> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| io::Error::other("pid out of range"))?;
    let mut status: libc::c_int = 0;
    // SAFETY: `rusage` is plain old data, valid zeroed; `status` and `usage`
    // are live exclusive references for the duration of the call.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = loop {
        // SAFETY: as above; wait4 writes only through the two pointers.
        let rc = unsafe { libc::wait4(pid, &raw mut status, 0, &raw mut usage) };
        if rc == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        break rc;
    };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    let (exit_code, signal) = if libc::WIFEXITED(status) {
        (Some(libc::WEXITSTATUS(status)), None)
    } else if libc::WIFSIGNALED(status) {
        (None, Some(libc::WTERMSIG(status)))
    } else {
        (None, None)
    };
    let raw = u64::try_from(usage.ru_maxrss).unwrap_or(0);
    let max_rss_bytes = if cfg!(target_os = "macos") { raw } else { raw * 1024 };
    Ok(ChildUsage { exit_code, signal, max_rss_bytes })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_child_is_reaped_with_status_and_a_nonzero_peak() {
        let child = std::process::Command::new("/bin/sh").args(["-c", "exit 3"]).spawn().unwrap();
        let u = super::wait(child.id()).unwrap();
        assert_eq!(u.exit_code, Some(3));
        assert_eq!(u.signal, None);
        assert!(u.max_rss_bytes > 0);
    }
}
