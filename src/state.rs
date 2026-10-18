//! Status for bars and shells (quickshell, waybar, …), D-060.
//!
//! A running sync keeps `$XDG_RUNTIME_DIR/gowl.state` — one JSON line, replaced by
//! an atomic rename — and rewrites it only when something visible changes: start,
//! pause, resume, a device rebound / unreachable / recovered, output or audio lost /
//! back. Nothing is written while steady (rules 5/7): the loop compares one integer
//! per turn (`Streams::health_gen`). The file goes away on exit (and from the panic
//! hook); a crash that leaves it behind is recognised because the pidfile lock is
//! gone (`control::running_at`).
//!
//! `gowl state` prints the current line (`{"running":false}` when nothing runs);
//! `gowl watch` prints it now and again on every change: inotify on the runtime
//! dir, one blocking `poll()`, zero wakeups between changes.
//!
//! Schema (keys always present while running; `error` is null or a short text):
//! `{"running":true,"pid":1234,"mode":"screen","paused":false,"fps":30,
//!   "devices":[{"name":"desk","ip":"192.168.1.3","iface":"enp5s0","ok":true}],"error":null}`
//! music adds `"reaction"` and `"palette"`; `mode` is `screen`, `music`,
//! `calibrate` or `unknown` (a gowl that doesn't write this file, seen via its
//! pidfile).

use std::ffi::CString;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config;
use crate::control::{self, Pidfile};
use crate::error::Error;
use crate::runtime::{Sig, Signals, Streams};

pub const FILE: &str = "gowl.state";
const PIDFILE: &str = "gowl.pid";
pub const NOT_RUNNING: &str = "{\"running\":false}";

/// What a session is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Screen,
    Music,
    Calibrate,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Screen => "screen",
            Mode::Music => "music",
            Mode::Calibrate => "calibrate",
        }
    }

    /// For notifications: "Screen sync", "Music sync", "Calibration".
    pub fn title(self) -> &'static str {
        match self {
            Mode::Screen => "Screen sync",
            Mode::Music => "Music sync",
            Mode::Calibrate => "Calibration",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "screen" => Some(Mode::Screen),
            "music" => Some(Mode::Music),
            "calibrate" => Some(Mode::Calibrate),
            _ => None,
        }
    }
}

/// A session-wide reason the light isn't following (besides per-device trouble).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    None,
    /// Screen: the captured output was unplugged / powered off.
    OutputGone,
    /// Music: the sound server can't be reached.
    NoAudio,
}

/// `$XDG_RUNTIME_DIR/gowl.state`.
pub fn path() -> Option<PathBuf> {
    config::runtime_dir().map(|d| d.join(FILE))
}

static PANIC_PATH: OnceLock<CString> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// From the panic hook (`panic = "abort"` skips Drop): remove the status file.
pub fn panic_cleanup() {
    if ACTIVE.load(Ordering::SeqCst)
        && let Some(p) = PANIC_PATH.get()
    {
        // SAFETY: valid C string.
        unsafe { libc::unlink(p.as_ptr()) };
    }
}

/// Keeps the status file of a running session; owns the pidfile so the two go
/// away in the right order (pidfile first: a reader then sees "not running" at
/// once instead of a half-gone session).
pub struct Writer {
    pidfile: Option<Pidfile>,
    /// (final, temp) paths; None = no runtime dir (nothing written).
    paths: Option<(CString, CString)>,
    buf: Vec<u8>,
    pid: i32,
    mode: Mode,
    fps: u32,
    extra: Vec<(&'static str, String)>,
    paused: bool,
    problem: Problem,
    seen: u64,
    /// Inode of the file we last renamed into place (exit removes only ours).
    ino: u64,
    #[cfg(test)]
    writes: u32,
}

impl Writer {
    /// Write the first state line (setup done, razer ON sent). `extra`: more
    /// string fields (music: reaction, palette).
    pub fn start(
        pidfile: Option<Pidfile>,
        mode: Mode,
        fps: u32,
        extra: Vec<(&'static str, String)>,
        streams: &Streams,
    ) -> Writer {
        Writer::start_in(path(), pidfile, mode, fps, extra, streams)
    }

    /// `start` with an explicit file (None = write nothing; unit tests).
    pub fn start_in(
        path: Option<PathBuf>,
        pidfile: Option<Pidfile>,
        mode: Mode,
        fps: u32,
        extra: Vec<(&'static str, String)>,
        streams: &Streams,
    ) -> Writer {
        let paths = path.and_then(|p| {
            let tmp = p.with_extension("state.tmp");
            Some((
                CString::new(p.as_os_str().as_bytes()).ok()?,
                CString::new(tmp.as_os_str().as_bytes()).ok()?,
            ))
        });
        if let Some((p, _)) = &paths {
            let _ = PANIC_PATH.set(p.clone());
        }
        let mut w = Writer {
            pidfile,
            paths,
            buf: Vec::with_capacity(4096),
            // SAFETY: getpid has no preconditions.
            pid: unsafe { libc::getpid() },
            mode,
            fps,
            extra,
            paused: false,
            problem: Problem::None,
            seen: streams.health_gen(),
            ino: 0,
            #[cfg(test)]
            writes: 0,
        };
        w.write(streams);
        w
    }

    pub fn set_paused(&mut self, paused: bool, streams: &Streams) {
        if self.paused != paused {
            self.paused = paused;
            self.write(streams);
        }
    }

    pub fn set_problem(&mut self, problem: Problem, streams: &Streams) {
        if self.problem != problem {
            self.problem = problem;
            self.write(streams);
        }
    }

    /// Rewrite if a device's interface / IP / reachability changed. One integer
    /// compare when nothing did; call once per loop turn.
    #[inline]
    pub fn sync(&mut self, streams: &Streams) {
        let g = streams.health_gen();
        if g != self.seen {
            self.seen = g;
            self.write(streams);
        }
    }

    /// The line as it would be written now (tests).
    #[cfg(test)]
    pub fn line(&mut self, streams: &Streams) -> String {
        self.compose(streams);
        String::from_utf8(self.buf.clone()).unwrap()
    }

    /// Lines written so far (tests: proves "only on change").
    #[cfg(test)]
    pub fn writes(&self) -> u32 {
        self.writes
    }

    fn compose(&mut self, streams: &Streams) {
        let b = &mut self.buf;
        b.clear();
        // Writes into a Vec with spare capacity: no allocation once warm.
        let _ = write!(
            b,
            "{{\"running\":true,\"pid\":{},\"mode\":\"{}\",\"paused\":{},\"fps\":{}",
            self.pid,
            self.mode.name(),
            self.paused,
            self.fps
        );
        for (k, v) in &self.extra {
            let _ = write!(b, ",\"{k}\":");
            json_str(b, v);
        }
        b.extend_from_slice(b",\"devices\":[");
        for (i, s) in streams.devs.iter().enumerate() {
            if i > 0 {
                b.push(b',');
            }
            b.extend_from_slice(b"{\"name\":");
            match s.alias() {
                Some(a) => json_str(b, a),
                None => {
                    let _ = write!(b, "\"{}\"", s.ip());
                }
            }
            let _ = write!(b, ",\"ip\":\"{}\",\"iface\":", s.ip());
            json_str(b, s.iface_name().unwrap_or(""));
            let _ = write!(b, ",\"ok\":{}}}", s.problem().is_none());
        }
        b.extend_from_slice(b"],\"error\":");
        let dev_problem = streams
            .devs
            .iter()
            .find_map(|s| s.problem().map(|p| (s, p)));
        match (self.problem, dev_problem) {
            (Problem::OutputGone, _) => json_str(b, "screen output gone"),
            (Problem::NoAudio, _) => json_str(b, "sound server unreachable"),
            (Problem::None, Some((s, p))) => {
                b.push(b'"');
                match s.alias() {
                    Some(a) => json_escape(b, a),
                    None => {
                        let _ = write!(b, "{}", s.ip());
                    }
                }
                b.extend_from_slice(b": ");
                json_escape(b, p);
                b.push(b'"');
            }
            (Problem::None, None) => b.extend_from_slice(b"null"),
        }
        b.push(b'}');
    }

    fn write(&mut self, streams: &Streams) {
        #[cfg(test)]
        {
            self.writes += 1;
        }
        self.compose(streams);
        self.buf.push(b'\n');
        let Some((path, tmp)) = &self.paths else {
            return;
        };
        match write_atomic(path, tmp, &self.buf) {
            Ok(ino) => {
                self.ino = ino;
                ACTIVE.store(true, Ordering::SeqCst);
            }
            Err(e) => debug!("cannot write the status file: {e}"),
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        drop(self.pidfile.take());
        if let Some((path, _)) = &self.paths {
            ACTIVE.store(false, Ordering::SeqCst);
            // Only if it is still ours (a newer session may have replaced it).
            // SAFETY: valid C string and stat buffer.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::stat(path.as_ptr(), &mut st) } == 0 && st.st_ino == self.ino {
                // SAFETY: valid C string.
                unsafe { libc::unlink(path.as_ptr()) };
            }
        }
    }
}

/// Write `data` to `tmp`, rename it over `path`; returns the inode.
fn write_atomic(path: &CString, tmp: &CString, data: &[u8]) -> io::Result<u64> {
    // SAFETY: valid C string; the fd is owned below.
    let fd = unsafe {
        libc::open(
            tmp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC,
            0o644,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut off = 0;
    while off < data.len() {
        // SAFETY: valid fd and buffer range.
        let n = unsafe {
            libc::write(
                fd.as_raw_fd(),
                data[off..].as_ptr().cast(),
                data.len() - off,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        off += n as usize;
    }
    // SAFETY: valid fd and stat buffer.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
    drop(fd);
    // SAFETY: valid C strings.
    if unsafe { libc::rename(tmp.as_ptr(), path.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.st_ino)
}

fn json_escape(b: &mut Vec<u8>, s: &str) {
    for c in s.chars() {
        match c {
            '"' => b.extend_from_slice(b"\\\""),
            '\\' => b.extend_from_slice(b"\\\\"),
            '\n' => b.extend_from_slice(b"\\n"),
            c if (c as u32) < 0x20 => {
                let _ = write!(b, "\\u{:04x}", c as u32);
            }
            c => {
                let mut tmp = [0u8; 4];
                b.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
            }
        }
    }
}

fn json_str(b: &mut Vec<u8>, s: &str) {
    b.push(b'"');
    json_escape(b, s);
    b.push(b'"');
}

// ------------------------------------------------------------------ reading

/// What a running session is, as far as `toggle`/`pause` need to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    pub pid: i32,
    /// None = a gowl that doesn't say (older build, or unreadable).
    pub mode: Option<Mode>,
    pub paused: bool,
}

/// Value of `"key":` in one of our own JSON lines (string unquoted, or the raw
/// token). A field scanner, not a parser: it only reads what `Writer` writes.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":");
    let rest = &line[line.find(&pat)? + pat.len()..];
    if let Some(s) = rest.strip_prefix('"') {
        return s.find('"').map(|e| &s[..e]);
    }
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim())
}

/// Mode of process `pid` from its command line (`gowl screen …`, `gowl toggle
/// music …`): the fallback when there is no status file for it.
pub fn cmdline_mode(pid: i32) -> Option<Mode> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let mut args = raw
        .split(|&b| b == 0)
        .skip(1)
        .map(|a| std::str::from_utf8(a).unwrap_or(""));
    match args.next()? {
        "toggle" => Some(if args.next() == Some("music") {
            Mode::Music
        } else {
            Mode::Screen
        }),
        m => Mode::parse(m),
    }
}

/// The status file's line, if it belongs to `pid`.
fn file_line_for(dir: &std::path::Path, pid: i32) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(FILE)).ok()?;
    let line = text.lines().next()?.trim().to_string();
    (field(&line, "pid")?.parse::<i32>().ok()? == pid).then_some(line)
}

/// The running session in `dir` (a runtime dir): pid from the locked pidfile,
/// line from the status file when it is that session's.
fn session_in(dir: &std::path::Path) -> Option<(i32, Option<String>)> {
    let pid = control::running_at(&dir.join(PIDFILE))?;
    Some((pid, file_line_for(dir, pid)))
}

/// The running session's facts, if one runs.
pub fn info() -> Option<Info> {
    let dir = config::runtime_dir()?;
    let (pid, line) = session_in(&dir)?;
    Some(match line {
        Some(l) => Info {
            pid,
            mode: field(&l, "mode").and_then(Mode::parse),
            paused: field(&l, "paused") == Some("true"),
        },
        None => Info {
            pid,
            mode: cmdline_mode(pid),
            paused: false,
        },
    })
}

fn fallback_line(pid: i32) -> String {
    format!(
        "{{\"running\":true,\"pid\":{pid},\"mode\":\"{}\",\"paused\":false,\"fps\":0,\"devices\":[],\"error\":null}}",
        cmdline_mode(pid).map_or("unknown", Mode::name)
    )
}

/// The current state line for `dir` (no newline).
fn current_in(dir: &std::path::Path) -> String {
    match session_in(dir) {
        None => NOT_RUNNING.into(),
        Some((_, Some(line))) => line,
        Some((pid, None)) => fallback_line(pid),
    }
}

/// The current state line (no newline): `{"running":false}` when nothing runs.
pub fn current() -> String {
    config::runtime_dir().map_or_else(|| NOT_RUNNING.into(), |d| current_in(&d))
}

/// `gowl state`.
pub fn cmd_state() -> Result<(), Error> {
    println!("{}", current());
    Ok(())
}

/// Print `line` + newline and flush; false if stdout is gone (EPIPE: SIGPIPE is
/// ignored by the Rust runtime, so it arrives as an error).
fn emit(line: &str) -> bool {
    let mut out = io::stdout().lock();
    out.write_all(line.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush())
        .is_ok()
}

/// `gowl watch`: the state line now and after every change, until SIGTERM/SIGINT/
/// SIGHUP, the reader goes away (SIGPIPE/EPIPE or POLLERR on stdout) or the
/// runtime dir does (logout).
pub fn cmd_watch() -> Result<(), Error> {
    let dir = config::runtime_dir()
        .ok_or_else(|| Error::Runtime("XDG_RUNTIME_DIR is unset: nothing to watch".into()))?;
    let sig = Signals::new()?;
    // SAFETY: plain syscall; the fd is owned below.
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(Error::Io("inotify_init1", io::Error::last_os_error()));
    }
    // SAFETY: fresh fd we own.
    let ino = unsafe { OwnedFd::from_raw_fd(fd) };
    let cdir = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| Error::Runtime("bad XDG_RUNTIME_DIR".into()))?;
    // Renames into place (state written), deletes (exit), and a closed writable fd
    // on the pidfile (the holder died without cleaning up: its lock is gone).
    let mask = libc::IN_MOVED_TO
        | libc::IN_DELETE
        | libc::IN_CLOSE_WRITE
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF
        | libc::IN_ONLYDIR;
    // SAFETY: valid fd and C string.
    if unsafe { libc::inotify_add_watch(ino.as_raw_fd(), cdir.as_ptr(), mask) } < 0 {
        return Err(Error::Io("inotify_add_watch", io::Error::last_os_error()));
    }
    let mut last = current_in(&dir);
    if !emit(&last) {
        return Ok(());
    }
    let mut buf = [0u64; 512]; // 4 KiB, aligned for inotify_event
    let mut pfds = [
        libc::pollfd {
            fd: ino.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: sig.fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        // events 0: only POLLERR/POLLHUP — the reading end of the pipe closed.
        libc::pollfd {
            fd: 1,
            events: 0,
            revents: 0,
        },
    ];
    loop {
        for p in &mut pfds {
            p.revents = 0;
        }
        // SAFETY: valid pollfd array.
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, -1) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::Io("poll", e));
        }
        if pfds[1].revents != 0 {
            while let Some(s) = sig.read() {
                if s == Sig::Stop {
                    return Ok(());
                }
            }
        }
        if pfds[2].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            debug!("watch: stdout closed");
            return Ok(());
        }
        if pfds[0].revents == 0 {
            continue;
        }
        let mut relevant = false;
        let mut gone = false;
        loop {
            // SAFETY: valid fd and buffer.
            let n = unsafe {
                libc::read(
                    ino.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    std::mem::size_of_val(&buf),
                )
            };
            if n <= 0 {
                break;
            }
            let bytes =
                // SAFETY: the kernel wrote n bytes into buf.
                unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n as usize) };
            let hdr = std::mem::size_of::<libc::inotify_event>();
            let mut off = 0;
            while off + hdr <= bytes.len() {
                // SAFETY: a whole event header lies at off (kernel-aligned records).
                let ev: libc::inotify_event =
                    unsafe { std::ptr::read_unaligned(bytes[off..].as_ptr().cast()) };
                let name_end = (off + hdr + ev.len as usize).min(bytes.len());
                let name = &bytes[off + hdr..name_end];
                let name = &name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())];
                if ev.mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED) != 0 {
                    gone = true;
                }
                if ev.mask & libc::IN_Q_OVERFLOW != 0
                    || name == FILE.as_bytes()
                    || name == PIDFILE.as_bytes()
                {
                    relevant = true;
                }
                off = name_end;
            }
        }
        debug!("watch: wakeup (relevant {relevant}, dir gone {gone})");
        if gone {
            if last != NOT_RUNNING {
                emit(NOT_RUNNING);
            }
            return Ok(());
        }
        if relevant {
            let now = current_in(&dir);
            if now != last {
                if !emit(&now) {
                    return Ok(());
                }
                last = now;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_scanner_reads_what_the_writer_writes() {
        let l = r#"{"running":true,"pid":42,"mode":"music","paused":true,"fps":40,"reaction":"flow","devices":[{"name":"a \"b\"","ip":"1.2.3.4","iface":"lan0","ok":true}],"error":null}"#;
        assert_eq!(field(l, "pid"), Some("42"));
        assert_eq!(field(l, "mode"), Some("music"));
        assert_eq!(field(l, "paused"), Some("true"));
        assert_eq!(field(l, "fps"), Some("40"));
        assert_eq!(field(l, "error"), Some("null"));
        assert_eq!(field(l, "nope"), None);
    }

    #[test]
    fn json_strings_are_escaped() {
        let mut b = Vec::new();
        json_str(&mut b, "a\"b\\c\nd\u{1}é");
        assert_eq!(std::str::from_utf8(&b).unwrap(), r#""a\"b\\c\nd\u0001é""#);
    }

    #[test]
    fn own_cmdline_has_no_mode() {
        // The test binary's argv[1] is not a gowl mode.
        // SAFETY: getpid has no preconditions.
        let me = unsafe { libc::getpid() };
        assert_eq!(cmdline_mode(me), None);
    }

    #[test]
    fn stale_state_file_without_a_lock_reads_as_not_running() {
        let d = std::env::temp_dir().join(format!("gowl-state-stale-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(FILE), "{\"running\":true,\"pid\":1}\n").unwrap();
        std::fs::write(d.join(PIDFILE), "1\n").unwrap(); // unlocked = stale
        assert_eq!(current_in(&d), NOT_RUNNING);
        let _ = std::fs::remove_dir_all(&d);
    }
}
