//! Runtime control of a running sync (SPEC.md "CLI", "Shutdown contract"): the
//! pidfile `$XDG_RUNTIME_DIR/gowl.pid` and `gowl pause|resume|stop|toggle`.
//!
//! The pidfile is held with an exclusive open-file-description lock
//! (`F_OFD_SETLK`) for the whole session: the lock *is* the claim, so exactly one
//! of two racing starts wins, and a crashed holder's claim vanishes with its
//! process (no stale-file guessing, no recycled-pid confusion). The file holds the
//! pid for `pause|resume|stop`, which only believe it while the lock is held
//! (`F_OFD_GETLK`) and it is not their own pid (D-043, review fix). Every streaming
//! mode (screen, music, calibrate) holds it, so only one session drives the lights
//! at a time. Signals go through a pidfd (`pidfd_open` + `pidfd_send_signal`), so
//! a pid recycled between the check and the signal can't be hit, and
//! `stop`/`toggle` wait for the exit by polling the pidfd — no sleep loop.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::config;
use crate::error::Error;

/// `$XDG_RUNTIME_DIR/gowl.pid`.
pub fn pidfile_path() -> Option<PathBuf> {
    config::runtime_dir().map(|d| d.join("gowl.pid"))
}

fn whole_file_lock(kind: i32) -> libc::flock {
    // SAFETY: flock is plain old data; zero = whole file from offset 0, pid 0
    // (required for OFD locks).
    let mut l: libc::flock = unsafe { std::mem::zeroed() };
    l.l_type = kind as libc::c_short;
    l.l_whence = libc::SEEK_SET as libc::c_short;
    l
}

/// Try to take the exclusive lock on `f`. Ok(false) = someone else holds it.
fn try_lock(f: &File) -> io::Result<bool> {
    let l = whole_file_lock(libc::F_WRLCK);
    // SAFETY: valid fd and flock struct.
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_OFD_SETLK, &l) } == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EAGAIN) | Some(libc::EACCES) => Ok(false),
        _ => Err(e),
    }
}

/// Is `f` locked by another open file description (a live holder)?
fn is_locked(f: &File) -> bool {
    let mut l = whole_file_lock(libc::F_WRLCK);
    // SAFETY: valid fd and flock struct (filled in by the kernel).
    let r = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_OFD_GETLK, &mut l) };
    r == 0 && i32::from(l.l_type) != libc::F_UNLCK
}

fn parse_pid(text: &str) -> Option<i32> {
    text.trim().parse().ok().filter(|&p| p > 0)
}

/// The pid of the live session holding `path`, if any: the file is locked and
/// names a pid other than ours.
pub fn running_at(path: &Path) -> Option<i32> {
    let f = File::open(path).ok()?;
    if !is_locked(&f) {
        return None;
    }
    let pid = parse_pid(&std::fs::read_to_string(path).ok()?)?;
    // SAFETY: getpid has no preconditions.
    (pid != unsafe { libc::getpid() }).then_some(pid)
}

/// Held for the lifetime of a sync session: the locked file. Removed on drop.
#[derive(Debug)]
pub struct Pidfile {
    path: PathBuf,
    file: File,
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

    /// Claim `path`: open/create it, take the exclusive lock (a holder = error,
    /// exit 1), make sure the locked file is still the one at `path` (a holder
    /// that just exited unlinks it), then write our pid.
    pub fn acquire_at(path: &Path) -> Result<Pidfile, Error> {
        for _ in 0..5 {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o644)
                .open(path)
                .map_err(|e| Error::Io("open pidfile", e))?;
            if !try_lock(&file).map_err(|e| Error::Io("lock pidfile", e))? {
                let who = std::fs::read_to_string(path)
                    .ok()
                    .and_then(|t| parse_pid(&t))
                    .map_or_else(|| "starting".into(), |p| format!("pid {p}"));
                return Err(Error::Runtime(format!(
                    "gowl is already running ({who}); `gowl stop` or `gowl toggle` stops it"
                )));
            }
            let same = match (file.metadata(), std::fs::metadata(path)) {
                (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
                _ => false,
            };
            if !same {
                continue; // locked a file its last holder just unlinked: retry
            }
            // SAFETY: getpid has no preconditions.
            let pid = unsafe { libc::getpid() };
            let text = format!("{pid}\n");
            file.set_len(0).map_err(|e| Error::Io("write pidfile", e))?;
            std::os::unix::fs::FileExt::write_all_at(&file, text.as_bytes(), 0)
                .map_err(|e| Error::Io("write pidfile", e))?;
            return Ok(Pidfile {
                path: path.to_path_buf(),
                file,
            });
        }
        Err(Error::Runtime(format!(
            "cannot claim {} (another gowl starting?)",
            path.display()
        )))
    }
}

impl Drop for Pidfile {
    fn drop(&mut self) {
        // Unlink while still holding the lock; a racing start that opened this
        // inode notices it is gone and retries. The lock goes with the fd.
        let ours = match (self.file.metadata(), std::fs::metadata(&self.path)) {
            (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
            _ => false,
        };
        if ours {
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
    let pid = running_at(&path)?;
    let p = Proc::open(pid);
    // Re-check after the pidfd pins the process: still the holder of the lock?
    if running_at(&path) == Some(pid) {
        Some(p)
    } else {
        debug!("pidfile {} changed hands (pid {pid})", path.display());
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
        assert_eq!(parse_pid(&std::fs::read_to_string(&p).unwrap()), Some(me));
        // Our own pid never counts as "a running sync" (stop must not kill itself).
        assert_eq!(running_at(&p), None);
        let e = Pidfile::acquire_at(&p).unwrap_err();
        assert!(e.to_string().contains("already running"), "{e}");
        assert_eq!(e.exit_code(), 1);
        drop(held);
        assert!(!p.exists());
        let again = Pidfile::acquire_at(&p).unwrap();
        drop(again);
    }

    #[test]
    fn unlocked_leftovers_are_stale_whatever_they_say() {
        let p = tmp("stale");
        // SAFETY: getpid has no preconditions.
        let me = unsafe { libc::getpid() };
        for stale in [
            format!("{me}\n"),
            "1\n".into(),
            "garbage".into(),
            String::new(),
        ] {
            std::fs::write(&p, &stale).unwrap();
            assert_eq!(running_at(&p), None, "{stale:?}");
            let held = Pidfile::acquire_at(&p).unwrap();
            drop(held);
            assert!(!p.exists());
        }
    }

    #[test]
    fn exactly_one_of_many_racing_claims_wins() {
        let p = tmp("race");
        for _round in 0..20 {
            let _ = std::fs::remove_file(&p);
            let go = std::sync::Arc::new(std::sync::Barrier::new(8));
            let held = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    let (p, go, held) = (p.clone(), go.clone(), held.clone());
                    std::thread::spawn(move || {
                        go.wait();
                        if let Ok(h) = Pidfile::acquire_at(&p) {
                            held.lock().unwrap().push(h);
                        }
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
            assert_eq!(held.lock().unwrap().len(), 1, "exactly one winner");
        }
    }
}
