//! Phase 7 leftovers from the Phase 6 re-review (D-065, D-068, D-069): a music sync
//! ends with the desktop session even while paused, music with no sound server never
//! switches the light to stream mode, and the systemd service stands down when the
//! user takes the light from a key. Everything runs against loopback receivers, a
//! stand-in "compositor" process and the fake notify-send of tests/common.

mod common;

use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gowl-session-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn gowl(tmp: &Path, base: u16) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_gowl"));
    common::fake_desktop(&mut c, tmp);
    c.env("GOWL_DEVICE", "127.0.0.1")
        .env("GOWL_PORT_BASE", base.to_string())
        .env("XDG_CONFIG_HOME", tmp)
        .env("XDG_STATE_HOME", tmp)
        .env("XDG_RUNTIME_DIR", tmp)
        .env("HOME", tmp)
        .env(
            "PULSE_SERVER",
            format!("unix:{}", tmp.join("no-pulse").display()),
        )
        .env("PULSE_COOKIE", tmp.join("no-cookie"))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("GOWL_UNIT")
        .env_remove("GOWL_CONFIG")
        .env_remove("GOWL_LOG");
    c
}

/// A process listening on a Unix socket, standing in for the compositor: gowl
/// finds the listener's pid through the socket (SO_PEERCRED) and watches it.
fn fake_compositor(path: &Path) -> Child {
    let mut c = Command::new("python3")
        .args([
            "-I",
            "-c",
            "import socket, sys, signal\n\
             s = socket.socket(socket.AF_UNIX)\n\
             s.bind(sys.argv[1]); s.listen(8)\n\
             print('ready', flush=True)\n\
             signal.pause()\n",
        ])
        .arg(path)
        .stdout(Stdio::piped())
        .spawn()
        .expect("python3");
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(c.stdout.take().unwrap()),
        &mut line,
    )
    .unwrap();
    assert_eq!(line.trim(), "ready");
    c
}

/// A child this test started, killed if the test fails half-way (so it can't
/// outlive the test holding its pipes).
struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn wait_exit(c: &mut Child, ms: u64) -> Option<std::process::ExitStatus> {
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(ms) {
        if let Some(s) = c.try_wait().unwrap() {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}

fn wait_for(p: &Path) {
    let t = Instant::now();
    while !p.exists() {
        assert!(
            t.elapsed() < Duration::from_secs(3),
            "{} never appeared",
            p.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Every datagram the fake light got.
fn drain(rx: &UdpSocket) -> Vec<Vec<u8>> {
    let mut got = Vec::new();
    let mut buf = [0u8; 2048];
    while let Ok(n) = rx.recv(&mut buf) {
        got.push(buf[..n].to_vec());
    }
    got
}

fn receiver(base: u16) -> UdpSocket {
    let rx = UdpSocket::bind(("127.0.0.1", base + 3)).unwrap();
    rx.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    rx
}

#[test]
fn music_ends_with_the_session_even_paused_and_never_streams_without_sound() {
    let tmp = tmpdir("music");
    let base = 17500;
    let rx = receiver(base);
    let wl = tmp.join("wayland-fake");
    for paused in [true, false] {
        let mut comp = Kill(fake_compositor(&wl));
        let mut music = Kill(
            gowl(&tmp, base)
                .args(["music"])
                .env("WAYLAND_DISPLAY", &wl)
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let music = &mut music.0;
        wait_for(&tmp.join("gowl/state"));
        if paused {
            let out = gowl(&tmp, base).arg("pause").output().unwrap();
            assert_eq!(out.status.code(), Some(0), "{out:?}");
        }
        std::thread::sleep(Duration::from_millis(300));
        assert!(music.try_wait().unwrap().is_none(), "music ended early");
        // Logout: the compositor exits.
        comp.0.kill().unwrap();
        comp.0.wait().unwrap();
        let st = wait_exit(music, 2000).expect("music outlived the session");
        assert_eq!(st.code(), Some(0), "paused={paused}");
        let mut err = String::new();
        std::io::Read::read_to_string(music.stderr.as_mut().unwrap(), &mut err).unwrap();
        assert!(err.contains("session ended"), "{err}");
        assert!(!tmp.join("gowl.pid").exists() && !tmp.join("gowl/state").exists());
        // No sound server the whole time: the light was never switched to stream
        // mode, so neither start, pause nor exit sent anything (D-068).
        assert!(drain(&rx).is_empty(), "paused={paused}");
        let _ = std::fs::remove_file(&wl);
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn the_service_stands_down_when_a_key_takes_over() {
    let tmp = tmpdir("unit");
    let base = 17600;
    let _rx = receiver(base);
    let unit = |args: &[&str]| {
        gowl(&tmp, base)
            .env("GOWL_UNIT", "1")
            .args(args)
            .output()
            .unwrap()
            .status
            .code()
            .unwrap()
    };
    let retry = tmp.join("gowl/unit-retry");
    let hold = tmp.join("gowl/unit-hold");

    // The service fails in a way systemd retries (no compositor: exit 4).
    assert_eq!(unit(&["screen"]), 4);
    assert!(retry.exists(), "retry marker");
    let n0 = common::wait_notifications(&tmp, 1).len();
    assert_eq!(n0, 1, "the failure is notified once");
    // While it waits, the user says stop: the retry stands down, quietly (exit 6).
    let out = gowl(&tmp, base).arg("stop").output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(common::wait_notifications(&tmp, 2)[1][7], "Screen sync off");
    assert!(hold.exists() && !retry.exists());
    assert_eq!(unit(&["screen"]), 6);
    assert!(!hold.exists(), "the hold is used up");
    // The next start (a manual `systemctl --user start gowl`) runs normally.
    assert_eq!(unit(&["screen"]), 4);
    assert!(retry.exists());
    // A toggle (a key) while it waits does the same.
    assert_eq!(
        gowl(&tmp, base)
            .args(["toggle", "screen"])
            .status()
            .unwrap()
            .code(),
        Some(4)
    );
    assert!(hold.exists());
    assert_eq!(unit(&["screen"]), 6);
    // Not retrying (exit 3, no light: RestartPreventExitStatus): no marker.
    let out = gowl(&tmp, base)
        .env("GOWL_UNIT", "1")
        .env_remove("GOWL_DEVICE")
        .arg("screen")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(!retry.exists());
    // A stale retry marker (the service gave up long ago) is not a reason to hold.
    // Its age is CLOCK_MONOTONIC in the contents (what systemd's timer runs on),
    // not the file's mtime: a fresh mtime with an old time inside is stale.
    let old = gowl::runtime::now_ns() - 600_000_000_000;
    std::fs::write(&retry, old.to_string()).unwrap();
    assert_eq!(
        gowl(&tmp, base).arg("stop").status().unwrap().code(),
        Some(1)
    );
    assert!(!hold.exists());

    // A key-started sync holds the light: the service finds it taken → exit 6
    // without a notification; by hand, the same error is shown.
    let mut music = Kill(gowl(&tmp, base).arg("music").spawn().unwrap());
    wait_for(&tmp.join("gowl/state"));
    // So far: capture unavailable, sync off, capture unavailable (toggle), no
    // light, nothing running.
    let before = common::wait_notifications(&tmp, 5).len();
    assert_eq!(before, 5, "{:?}", common::notifications(&tmp));
    assert_eq!(unit(&["screen"]), 6);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        common::notifications(&tmp).len(),
        before,
        "no bubble for the service"
    );
    let out = gowl(&tmp, base).arg("screen").output().unwrap();
    assert_eq!(out.status.code(), Some(6));
    assert!(String::from_utf8_lossy(&out.stderr).contains("already running"));
    assert_eq!(
        gowl(&tmp, base).arg("stop").status().unwrap().code(),
        Some(0)
    );
    assert!(wait_exit(&mut music.0, 2000).is_some());
    let _ = std::fs::remove_dir_all(&tmp);
}
