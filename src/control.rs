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
use crate::notify;
use crate::state::{self, Mode};

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

/// Who holds the pidfile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
    /// Nobody (no file, unlocked, or it is us).
    Free,
    /// Locked, but the pid isn't readable yet: a session is claiming it right now
    /// (or wrote a pid that isn't a live gowl: the stale text of a crashed one,
    /// about to be overwritten).
    Starting,
    /// A live gowl session.
    Pid(i32),
}

/// Is `pid` a live process whose name is gowl's?
fn is_gowl(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim_end() == "gowl")
}

/// Who holds `path` (see `Holder`).
pub fn holder_at(path: &Path) -> Holder {
    let Ok(f) = File::open(path) else {
        return Holder::Free;
    };
    if !is_locked(&f) {
        return Holder::Free;
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    // SAFETY: getpid has no preconditions.
    let me = unsafe { libc::getpid() };
    match parse_pid(&text) {
        Some(pid) if pid == me => Holder::Free,
        Some(pid) if is_gowl(pid) => Holder::Pid(pid),
        _ => Holder::Starting,
    }
}

/// The pid of the live session holding `path`, if any: the file is locked and
/// names a live gowl other than us.
pub fn running_at(path: &Path) -> Option<i32> {
    match holder_at(path) {
        Holder::Pid(p) => Some(p),
        _ => None,
    }
}

/// A session is claiming the pidfile right now (no readable pid yet).
pub fn starting() -> bool {
    pidfile_path().is_some_and(|p| holder_at(&p) == Holder::Starting)
}

/// The error for "nothing to act on": "being started" when a claim is in flight
/// (two toggles racing), else "not running".
pub fn none_running() -> Error {
    if starting() {
        Error::Runtime("gowl is just starting (another toggle?); try again in a moment".into())
    } else {
        Error::Runtime("no gowl sync is running".into())
    }
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
            // Write, then cut to length: the file is never empty once it existed (a
            // reader sees a stale pid, which isn't a live gowl → "starting").
            std::os::unix::fs::FileExt::write_all_at(&file, text.as_bytes(), 0)
                .map_err(|e| Error::Io("write pidfile", e))?;
            file.set_len(text.len() as u64)
                .map_err(|e| Error::Io("write pidfile", e))?;
            set_panic_path(path);
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

static PANIC_PIDFILE: std::sync::OnceLock<std::ffi::CString> = std::sync::OnceLock::new();

fn set_panic_path(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
        let _ = PANIC_PIDFILE.set(c);
    }
}

/// From the panic hook (`panic = "abort"` skips Drop): unlink the pidfile first, so
/// a reader sees "not running" before the status file goes too.
pub fn panic_unlink_pidfile() {
    if let Some(p) = PANIC_PIDFILE.get() {
        // SAFETY: valid C string.
        unsafe { libc::unlink(p.as_ptr()) };
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

/// The running session: a signalable handle plus what it is doing (mode and
/// paused, from its status file or, for a gowl that writes none, its command line).
pub struct Session {
    proc: Proc,
    pub info: state::Info,
}

/// The running session, if there is one.
pub fn session() -> Option<Session> {
    let path = pidfile_path()?;
    let info = state::info()?;
    let p = Proc::open(info.pid);
    // Re-check after the pidfd pins the process: still the holder of the lock?
    if running_at(&path) == Some(info.pid) {
        Some(Session { proc: p, info })
    } else {
        debug!(
            "pidfile {} changed hands (pid {})",
            path.display(),
            info.pid
        );
        None
    }
}

impl Session {
    /// "Screen sync", "Music sync", "Calibration" or "gowl sync" (mode unknown).
    pub fn title(&self) -> &'static str {
        self.info.mode.map_or("gowl sync", Mode::title)
    }

    /// SIGUSR1 (pause) / SIGUSR2 (resume).
    pub fn pause(&self, resume: bool) -> Result<(), Error> {
        let sig = if resume { libc::SIGUSR2 } else { libc::SIGUSR1 };
        self.proc.signal(sig).map_err(|e| Error::Io("signal", e))
    }

    /// SIGTERM, then wait up to `ms` for the exit. True if it exited.
    pub fn stop(&self, ms: i32) -> Result<bool, Error> {
        match self.proc.signal(libc::SIGTERM) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => return Ok(true),
            Err(e) => return Err(Error::Io("signal", e)),
        }
        let exited = self.proc.wait_exit(ms);
        debug!(
            "pid {} {}",
            self.proc.pid,
            if exited { "exited" } else { "still running" }
        );
        Ok(exited)
    }
}

/// `gowl pause` / `gowl resume`: SIGUSR1 / SIGUSR2 to the running sync, and a
/// notification ("Screen sync paused"). Errors are notified by main.
pub fn cmd_pause(resume: bool) -> Result<(), Error> {
    let s = session().ok_or_else(none_running)?;
    if s.info.mode == Some(Mode::Calibrate) {
        return Err(Error::Runtime(
            "calibration is running; pause and resume don't apply to it".into(),
        ));
    }
    s.pause(resume)?;
    let what = if resume { "resumed" } else { "paused" };
    notify::send(
        config::load_quiet().notify,
        &format!("{} {what}", s.title()),
        None,
    );
    Ok(())
}

/// `gowl stop`: SIGTERM, then wait up to 1 s for the exit (the shutdown contract
/// promises 100 ms); "Screen sync off".
pub fn cmd_stop() -> Result<(), Error> {
    let s = session().ok_or_else(none_running)?;
    if !s.stop(1000)? {
        return Err(Error::Runtime(
            "gowl got the stop signal but has not exited after 1 s".into(),
        ));
    }
    notify::send(
        config::load_quiet().notify,
        &format!("{} off", s.title()),
        None,
    );
    Ok(())
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
    fn a_locked_file_without_a_live_gowl_pid_is_being_claimed() {
        let p = tmp("claim");
        std::fs::write(&p, "").unwrap();
        // Another open file description holds the lock: a claim in flight.
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&p)
            .unwrap();
        assert!(try_lock(&f).unwrap());
        assert_eq!(holder_at(&p), Holder::Starting, "empty");
        std::fs::write(&p, "1\n").unwrap(); // stale text: pid 1 is not gowl
        assert_eq!(holder_at(&p), Holder::Starting, "not a gowl");
        assert_eq!(running_at(&p), None);
        // SAFETY: getpid has no preconditions.
        let me = unsafe { libc::getpid() };
        std::fs::write(&p, format!("{me}\n")).unwrap();
        assert_eq!(holder_at(&p), Holder::Free, "our own pid never counts");
        drop(f);
        std::fs::write(&p, "").unwrap();
        assert_eq!(holder_at(&p), Holder::Free, "unlocked");
        let _ = std::fs::remove_file(&p);
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
