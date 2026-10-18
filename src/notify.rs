//! Desktop notifications without any D-Bus code (CLAUDE.md rule 10, D-059): spawn
//! `notify-send -a gowl -t 2500 -h string:x-canonical-private-synchronous:gowl
//! SUMMARY [BODY]` and don't wait for it. The synchronous hint makes each one
//! replace the previous, so a burst of toggles shows one bubble.
//!
//! Spawned by a double fork: the intermediate child forks the real child and exits
//! at once, so gowl reaps it immediately (a waitpid that returns in microseconds,
//! not a wait for notify-send) and a long-running sync never collects a zombie.
//! Between fork and exec only async-signal-safe calls run (argv is built before
//! the fork), so this is also safe from a multi-threaded test harness. A missing
//! `notify-send` (or no notification server) is silent: the exec fails or its
//! output goes to /dev/null.

use std::ffi::CString;
use std::os::raw::c_char;

/// Spawn one notification if `enabled` (config `notify`, default on).
pub fn send(enabled: bool, summary: &str, body: Option<&str>) {
    if !enabled {
        return;
    }
    debug!(
        "notify: {summary}{}",
        body.map_or(String::new(), |b| format!(" — {b}"))
    );
    let mut args: Vec<&str> = vec![
        "notify-send",
        "-a",
        "gowl",
        "-t",
        "2500",
        "-h",
        "string:x-canonical-private-synchronous:gowl",
        "--",
        summary,
    ];
    if let Some(b) = body {
        args.push(b);
    }
    let argv: Vec<CString> = args
        .iter()
        .map(|a| CString::new(a.replace('\0', " ")).unwrap_or_default())
        .collect();
    let mut ptrs: Vec<*const c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    spawn_detached(&ptrs);
}

/// fork → fork → exec `argv` with stdio on /dev/null, signals unblocked and
/// SIGPIPE back to default; the middle process exits at once and is reaped here.
fn spawn_detached(argv: &[*const c_char]) {
    // SAFETY: only async-signal-safe calls between fork and exec/_exit; argv is a
    // NULL-terminated array of valid C strings that outlives the calls.
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            return;
        }
        if pid == 0 {
            if libc::fork() == 0 {
                libc::setsid();
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                if fd >= 0 {
                    libc::dup2(fd, 0);
                    libc::dup2(fd, 1);
                    libc::dup2(fd, 2);
                    if fd > 2 {
                        libc::close(fd);
                    }
                }
                libc::execvp(argv[0], argv.as_ptr());
                libc::_exit(127);
            }
            libc::_exit(0);
        }
        let mut status = 0;
        while libc::waitpid(pid, &mut status, 0) < 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
        {}
    }
}
