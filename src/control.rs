//! Runtime control of a running sync (SPEC.md "CLI", "Shutdown contract"): the
//! pidfile `$XDG_RUNTIME_DIR/gowl.pid` and `gowl pause|resume|stop|toggle`.
//!
//! The pidfile is created with `O_EXCL`; a leftover file is stale unless its pid is
//! alive (`kill(pid, 0)`) *and* that process is gowl (`/proc/PID/comm`). Every
//! streaming mode (screen, music, calibrate) holds it, so only one session drives
//! the lights at a time. Signals go through a pidfd (`pidfd_open` +
//! `pidfd_send_signal`), so a pid recycled between the check and the signal can't
//! be hit, and `stop`/`toggle` wait for the exit by polling the pidfd — no sleep loop.

use std::io::{self, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::config;
use crate::error::Error;

/// `$XDG_RUNTIME_DIR/gowl.pid`.
pub fn pidfile_path() -> Option<PathBuf> {
    config::runtime_dir().map(|d| d.join("gowl.pid"))
}

fn comm(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim_end().to_string())
}

/// Is `pid` a live gowl process? Our own pid counts (same process, second claim).
fn is_gowl(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: getpid has no preconditions.
    let me = unsafe { libc::getpid() };
    if pid == me {
        return true;
    }
    // SAFETY: signal 0 only checks existence/permission.
    let alive = unsafe { libc::kill(pid, 0) } == 0
        || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !alive {
        return false;
    }
    // "gowl", or whatever this binary is called (a renamed or test build).
    comm(pid).is_some_and(|c| c == "gowl" || Some(&c) == comm(me).as_ref())
}

fn read_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The pid of the running gowl that holds `path`, if any.
pub fn running_at(path: &Path) -> Option<i32> {
    read_pid(path).filter(|&p| is_gowl(p))
}

/// Held for the lifetime of a sync session; removes the file on drop.
#[derive(Debug)]
pub struct Pidfile {
    path: PathBuf,
    pid: i32,
}

impl Pidfile {
    /// Claim `$XDG_RUNTIME_DIR/gowl.pid`. Without XDG_RUNTIME_DIR the session runs
    /// unclaimed (with a warning): pause/resume/stop can't find it then.
    pub fn acquire() -> Result<Option<Pidfile>, Error> {
        match pidfile_path() {
            Some(p) => Pidfile::acquire_at(&p).map(Some),
            None => {
                warn!(
                    "XDG_RUNTIME_DIR is unset: no pidfile, so `gowl pause|resume|stop|toggle` can't reach this sync"
                );
                Ok(None)
            }
        }
    }

    /// Claim `path` (O_EXCL). A stale file (dead pid, or a pid that isn't gowl) is
    /// replaced; a live gowl is an error (exit 1).
    pub fn acquire_at(path: &Path) -> Result<Pidfile, Error> {
        // SAFETY: getpid has no preconditions.
        let pid = unsafe { libc::getpid() };
        for _ in 0..3 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .open(path)
            {
                Ok(mut f) => {
                    f.write_all(format!("{pid}\n").as_bytes())
                        .map_err(|e| Error::Io("write pidfile", e))?;
                    return Ok(Pidfile {
                        path: path.to_path_buf(),
                        pid,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let old = read_pid(path);
                    if let Some(p) = old.filter(|&p| is_gowl(p)) {
                        return Err(Error::Runtime(format!(
                            "gowl is already running (pid {p}); `gowl stop` or `gowl toggle` stops it"
                        )));
                    }
                    // Stale: remove it only if nobody replaced it meanwhile.
                    if read_pid(path) == old {
                        debug!("removing stale pidfile {} (pid {old:?})", path.display());
                        let _ = std::fs::remove_file(path);
                    }
                }
                Err(e) => return Err(Error::Io("create pidfile", e)),
            }
        }
        Err(Error::Runtime(format!(
            "cannot claim {} (another gowl starting?)",
            path.display()
        )))
    }
}

impl Drop for Pidfile {
    fn drop(&mut self) {
        if read_pid(&self.path) == Some(self.pid) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// ------------------------------------------------------------------ signalling

/// A handle on another process: a pidfd when the kernel has them (≥ 5.3).
struct Proc {
    pid: i32,
    fd: Option<OwnedFd>,
}

impl Proc {
    fn open(pid: i32) -> Proc {
        // SAFETY: pidfd_open(pid, 0) returns a new fd or -1.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        Proc {
            pid,
            // SAFETY: a fresh fd we own.
            fd: (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as i32) }),
        }
    }

    fn signal(&self, sig: i32) -> io::Result<()> {
        let r = match &self.fd {
            // SAFETY: valid pidfd; NULL siginfo = like kill(2).
            Some(fd) => unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    sig,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            },
            // SAFETY: plain kill(2).
            None => i64::from(unsafe { libc::kill(self.pid, sig) }),
        };
        if r == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Wait up to `ms` for the process to exit. True if it did.
    fn wait_exit(&self, ms: i32) -> bool {
        match &self.fd {
            Some(fd) => {
                let mut p = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd; a pidfd turns readable when the process exits.
                unsafe { libc::poll(&mut p, 1, ms) == 1 }
            }
            None => {
                // No pidfd (kernel < 5.3): check every 10 ms. CLI only, never in sync.
                for _ in 0..ms.max(10) / 10 {
                    // SAFETY: signal 0 only checks existence.
                    if unsafe { libc::kill(self.pid, 0) } != 0 {
                        return true;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                false
            }
        }
    }
}

/// The running sync, as a signalable handle, if there is one.
fn running() -> Option<Proc> {
    let path = pidfile_path()?;
    let pid = read_pid(&path)?;
    let p = Proc::open(pid);
    // Re-check after the pidfd pins the process: a recycled pid is not gowl.
    if is_gowl(pid) {
        Some(p)
    } else {
        debug!("pidfile {} is stale (pid {pid})", path.display());
        None
    }
}

fn not_running() -> Error {
    Error::Runtime("no gowl sync is running".into())
}

/// `gowl pause` / `gowl resume`: SIGUSR1 / SIGUSR2 to the running sync.
pub fn cmd_pause(resume: bool) -> Result<(), Error> {
    let p = running().ok_or_else(not_running)?;
    let sig = if resume { libc::SIGUSR2 } else { libc::SIGUSR1 };
    p.signal(sig).map_err(|e| Error::Io("signal", e))
}

/// `gowl stop`: SIGTERM, then wait up to 1 s for the exit (the shutdown contract
/// promises 100 ms).
pub fn cmd_stop() -> Result<(), Error> {
    if stop_running(1000)?.ok_or_else(not_running)? {
        Ok(())
    } else {
        Err(Error::Runtime(
            "gowl got the stop signal but has not exited after 1 s".into(),
        ))
    }
}

/// Stop the running sync if there is one: `None` = nothing was running,
/// `Some(exited_within_ms)` otherwise.
pub fn stop_running(ms: i32) -> Result<Option<bool>, Error> {
    let Some(p) = running() else {
        return Ok(None);
    };
    match p.signal(libc::SIGTERM) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => return Ok(Some(true)),
        Err(e) => return Err(Error::Io("signal", e)),
    }
    let exited = p.wait_exit(ms);
    debug!(
        "pid {} {}",
        p.pid,
        if exited { "exited" } else { "still running" }
    );
    Ok(Some(exited))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gowl-pid-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.join("gowl.pid")
    }

    #[test]
    fn second_claim_fails_and_drop_removes() {
        let p = tmp("excl");
        let _ = std::fs::remove_file(&p);
        let held = Pidfile::acquire_at(&p).unwrap();
        // SAFETY: getpid has no preconditions.
        let me = unsafe { libc::getpid() };
        assert_eq!(read_pid(&p), Some(me));
        assert_eq!(running_at(&p), Some(me));
        let e = Pidfile::acquire_at(&p).unwrap_err();
        assert!(e.to_string().contains("already running"), "{e}");
        assert_eq!(e.exit_code(), 1);
        drop(held);
        assert!(!p.exists());
    }

    #[test]
    fn stale_pidfiles_are_replaced() {
        let p = tmp("stale");
        // A pid that has exited.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id() as i32;
        child.wait().unwrap();
        for stale in [
            format!("{dead}\n"),
            "1\n".into(),
            "garbage".into(),
            String::new(),
        ] {
            // pid 1 is alive but not gowl.
            std::fs::write(&p, &stale).unwrap();
            assert_eq!(running_at(&p), None, "{stale:?}");
            let held = Pidfile::acquire_at(&p).unwrap();
            drop(held);
            assert!(!p.exists());
        }
    }

    #[test]
    fn drop_leaves_a_file_someone_else_owns() {
        let p = tmp("owner");
        let held = Pidfile::acquire_at(&p).unwrap();
        std::fs::write(&p, "1\n").unwrap();
        drop(held);
        assert!(p.exists(), "not ours any more: left alone");
        let _ = std::fs::remove_file(&p);
    }
}
