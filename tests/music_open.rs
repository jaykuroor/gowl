//! Regression test for the Phase 4 review bug: opening the monitor stream through
//! the poll hook must not be aborted by a ready timerfd or netlink fd (that spun
//! at 100 % CPU, opening a new connection per iteration). Only a pending signal
//! may interrupt an open.
//!
//! libpulse (dlopen'd, as in `gowl music`) talks to a fake server on a Unix socket
//! (PULSE_SERVER) that accepts and then says nothing, so the connect stays in
//! progress; always-readable pipes stand in for the timer, netlink and signal fds.
//! Needs libpulse.so.0 (fails without it unless GOWL_TEST_NO_PULSE=1).

use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use gowl::audio::pulse::{Hook, Pulse, ReadError};
use gowl::audio::run::{H_NET, H_SIG, H_TIMER, open_capture};

fn readable_pipe() -> i32 {
    let mut fds = [0; 2];
    // SAFETY: valid 2-int array; one byte makes the read end permanently readable.
    unsafe {
        assert_eq!(libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC), 0);
        assert_eq!(libc::write(fds[1], b"x".as_ptr().cast(), 1), 1);
    }
    fds[0] // the write end leaks for the test's lifetime: fine
}

#[test]
fn open_ignores_ready_timer_and_netlink_but_not_signals() {
    let pulse = match Pulse::load() {
        Ok(p) => p,
        Err(e) => {
            assert!(
                std::env::var_os("GOWL_TEST_NO_PULSE").is_some(),
                "{e}; install pulseaudio-libs or set GOWL_TEST_NO_PULSE=1"
            );
            return;
        }
    };
    let dir = std::env::temp_dir().join(format!("gowl-pulse-open-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("native");
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    // SAFETY: the only test in this binary; set before libpulse reads it.
    unsafe {
        std::env::set_var("PULSE_SERVER", format!("unix:{}", sock.display()));
        std::env::set_var("PULSE_COOKIE", dir.join("no-cookie"));
    }
    // Fake server: accept, stay silent 400 ms, hang up. Twice (one per open below).
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (c, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(400));
            drop(c);
        }
    });

    // Timer and netlink ready the whole time, no signal: the open must keep waiting
    // for the server (and fail when it hangs up), not return Interrupted at once.
    let hook = Hook::default();
    hook.set_fd(H_TIMER, readable_pipe());
    hook.set_fd(H_NET, readable_pipe());
    let t = Instant::now();
    let r = open_capture(&pulse, &hook, c"@DEFAULT_MONITOR@");
    let took = t.elapsed();
    assert!(
        matches!(r, Err(ReadError::Failed(_))),
        "expected the connect to fail when the server hangs up, got {:?} after {took:?}",
        r.as_ref().err()
    );
    assert!(took >= Duration::from_millis(300), "gave up after {took:?}");
    assert!(!hook.fired(), "timer/netlink must not fire during an open");

    // A pending signal does interrupt the open, promptly.
    let hook = Hook::default();
    hook.set_fd(H_TIMER, readable_pipe());
    hook.set_fd(H_SIG, readable_pipe());
    let t = Instant::now();
    let r = open_capture(&pulse, &hook, c"@DEFAULT_MONITOR@");
    assert!(matches!(r, Err(ReadError::Interrupted)), "{:?}", r.err());
    assert!(t.elapsed() < Duration::from_millis(300));
    assert!(hook.fired());
    let ev = hook.take();
    assert_ne!(ev[H_SIG], 0);
    assert_eq!(ev[H_TIMER], 0, "inactive during the open");
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
