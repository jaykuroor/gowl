//! End-to-end `gowl screen` without niri or a light: the binary connects to
//! tests/mock_wl (a fake compositor on a Unix socket given as WAYLAND_DISPLAY) and
//! streams to a UDP receiver on loopback test ports. Checks the runtime contract:
//! razer ON first, frames while the screen changes, keepalive only while it is
//! static, pause = razer OFF + silence, resume = razer ON, SIGTERM = razer OFF ×2.

mod mock_wl;

use std::net::UdpSocket;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use gowl::proto;

const BASE: u16 = 16000;

fn razer_mode_msg(on: bool) -> Vec<u8> {
    let mut m = [0u8; 64];
    let n = proto::razer_msg(&proto::razer_mode_frame(on), &mut m);
    m[..n].to_vec()
}

#[derive(Debug, PartialEq)]
enum Kind {
    On,
    Off,
    Frame,
}

#[test]
fn screen_against_mock_compositor() {
    let tmp = std::env::temp_dir().join(format!("gowl-screen-mock-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let sock_path = tmp.join("wayland-mock");
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path).unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let s2 = served.clone();
    let opts = mock_wl::Opts {
        width: 320,
        height: 180,
        transform: 0,
        changing: 20,
    };
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            mock_wl::serve(OwnedFd::from(stream), opts, s2);
        }
    });

    let rx = UdpSocket::bind(("127.0.0.1", BASE + 3)).unwrap();
    rx.set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_gowl"))
        .args(["screen", "--fps", "30"])
        .env("WAYLAND_DISPLAY", &sock_path)
        .env("GOWL_DEVICE", "127.0.0.1")
        .env("GOWL_PORT_BASE", BASE.to_string())
        .env("XDG_CONFIG_HOME", &tmp)
        .env("XDG_STATE_HOME", &tmp)
        .env("HOME", &tmp)
        .env_remove("GOWL_CONFIG")
        .env_remove("GOWL_LOG")
        .env_remove("GOWL_CAPTURE")
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    let (on, off) = (razer_mode_msg(true), razer_mode_msg(false));
    let t0 = Instant::now();
    let mut log: Vec<(f64, Kind)> = Vec::new();
    let mut buf = [0u8; 2048];
    let mut collect = |until: f64, log: &mut Vec<(f64, Kind)>| {
        while t0.elapsed().as_secs_f64() < until {
            if let Ok(n) = rx.recv(&mut buf) {
                let k = if buf[..n] == on[..] {
                    Kind::On
                } else if buf[..n] == off[..] {
                    Kind::Off
                } else {
                    assert_eq!(n, 92, "10-segment razer frame");
                    Kind::Frame
                };
                log.push((t0.elapsed().as_secs_f64(), k));
            }
        }
    };
    let signal = |s: i32| {
        // SAFETY: plain kill(2) on our child.
        assert_eq!(unsafe { libc::kill(pid, s) }, 0);
    };

    collect(2.5, &mut log); // ~0.7 s of changing frames, then static
    signal(libc::SIGUSR1);
    collect(3.3, &mut log);
    signal(libc::SIGUSR2);
    collect(3.6, &mut log);
    signal(libc::SIGTERM);
    let ts = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            ts.elapsed() < Duration::from_secs(2),
            "gowl ignored SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let exit_ms = ts.elapsed().as_millis();
    collect(t0.elapsed().as_secs_f64() + 0.2, &mut log);
    let mut err = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut err).unwrap();
    let _ = std::fs::remove_dir_all(&tmp);

    assert!(status.success(), "exit {status:?}: {err}");
    assert!(exit_ms < 500, "exit took {exit_ms} ms");
    assert_eq!(served.load(Ordering::SeqCst), 20, "mock served frames");
    assert_eq!(log.first().map(|e| &e.1), Some(&Kind::On), "{log:?}");
    let frames = |a: f64, b: f64| {
        log.iter()
            .filter(|(t, k)| *k == Kind::Frame && *t > a && *t < b)
            .count()
    };
    // Changing phase (20 frames at 30 fps, smoothing fades in): plenty of sends.
    assert!(frames(0.0, 1.0) >= 10, "{log:?}");
    // Static phase: keepalive only (500 ms) — about 2 datagrams per second.
    let ka = frames(1.4, 2.4);
    assert!((1..=3).contains(&ka), "static phase sent {ka}: {log:?}");
    // Pause: one razer OFF, then silence.
    let paused: Vec<&(f64, Kind)> = log.iter().filter(|(t, _)| *t > 2.5 && *t < 3.3).collect();
    assert_eq!(
        paused.iter().map(|e| &e.1).collect::<Vec<_>>(),
        [&Kind::Off],
        "{log:?}"
    );
    // Resume: razer ON and the current colors again.
    let resumed: Vec<&Kind> = log
        .iter()
        .filter(|(t, _)| *t > 3.3 && *t < 3.6)
        .map(|e| &e.1)
        .collect();
    assert_eq!(resumed.first(), Some(&&Kind::On), "{log:?}");
    assert!(resumed.contains(&&Kind::Frame), "{log:?}");
    // Shutdown contract: razer OFF twice at the end.
    let tail: Vec<&Kind> = log.iter().rev().take(2).map(|e| &e.1).collect();
    assert_eq!(tail, [&Kind::Off, &Kind::Off], "{log:?}");
}
