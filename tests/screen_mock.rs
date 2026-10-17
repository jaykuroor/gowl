//! End-to-end `gowl screen` without niri or a light: the binary connects to
//! tests/mock_wl (a fake compositor on a Unix socket given as WAYLAND_DISPLAY) and
//! streams to a UDP receiver on loopback test ports. Checks the runtime contract:
//! razer ON first, frames while the screen changes, keepalive only while it is
//! static, pause = razer OFF ×2 + silence, resume = razer ON, output unplugged =
//! razer OFF ×2 (like pause) + silence, replugged = razer ON + frames,
//! SIGTERM = razer OFF ×2. Runtime control through the CLI: one instance per
//! pidfile, `gowl pause|resume|stop`, `gowl toggle` (start and stop).

mod mock_wl;

use std::net::UdpSocket;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};

use gowl::proto;

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

fn gowl_cmd(tmp: &std::path::Path, wl: &std::path::Path, base: u16) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_gowl"));
    c.env("WAYLAND_DISPLAY", wl)
        .env("GOWL_DEVICE", "127.0.0.1")
        .env("GOWL_PORT_BASE", base.to_string())
        .env("XDG_CONFIG_HOME", tmp)
        .env("XDG_STATE_HOME", tmp)
        .env("XDG_RUNTIME_DIR", tmp)
        .env("HOME", tmp)
        .env_remove("GOWL_CONFIG")
        .env_remove("GOWL_LOG")
        .env_remove("GOWL_CAPTURE");
    c
}

struct Harness {
    base: u16,
    tmp: PathBuf,
    child: Child,
    rx: UdpSocket,
    t0: Instant,
    log: Vec<(f64, Kind)>,
    served: Arc<AtomicUsize>,
    ctl: Sender<mock_wl::Ctl>,
    on: Vec<u8>,
    off: Vec<u8>,
}

impl Harness {
    fn start(name: &str, base: u16, changing: usize) -> Harness {
        Harness::start_with(name, base, changing, &["screen", "--fps", "30"])
    }

    fn start_with(name: &str, base: u16, changing: usize, args: &[&str]) -> Harness {
        let tmp = std::env::temp_dir().join(format!("gowl-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let sock_path = tmp.join("wayland-mock");
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();
        let served = Arc::new(AtomicUsize::new(0));
        let s2 = served.clone();
        let (ctl, ctl_rx) = channel();
        let opts = mock_wl::Opts {
            width: 320,
            height: 180,
            transform: 0,
            changing,
        };
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                mock_wl::serve_ctl(OwnedFd::from(stream), opts, s2, Some(ctl_rx));
            }
        });
        let rx = UdpSocket::bind(("127.0.0.1", base + 3)).unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let child = gowl_cmd(&tmp, &sock_path, base)
            .args(args)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Harness {
            base,
            tmp,
            child,
            rx,
            t0: Instant::now(),
            log: Vec::new(),
            served,
            ctl,
            on: razer_mode_msg(true),
            off: razer_mode_msg(false),
        }
    }

    /// Run a gowl CLI command with this harness's environment (same pidfile).
    fn cli(&self, args: &[&str]) -> (i32, String) {
        let out = gowl_cmd(&self.tmp, &self.tmp.join("wayland-mock"), self.base)
            .args(args)
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn now(&self) -> f64 {
        self.t0.elapsed().as_secs_f64()
    }

    /// The child has exited (after a CLI stop/toggle): collect the tail, check exit 0.
    fn exited(&mut self) -> String {
        let ts = Instant::now();
        let status = loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                break s;
            }
            assert!(ts.elapsed() < Duration::from_secs(2), "gowl still running");
            std::thread::sleep(Duration::from_millis(5));
        };
        self.collect(self.now() + 0.2);
        let mut err = String::new();
        std::io::Read::read_to_string(self.child.stderr.as_mut().unwrap(), &mut err).unwrap();
        assert!(status.success(), "exit {status:?}: {err}");
        assert!(!self.tmp.join("gowl.pid").exists(), "pidfile left behind");
        let _ = std::fs::remove_dir_all(&self.tmp);
        err
    }

    /// Record datagrams until `until` seconds after start.
    fn collect(&mut self, until: f64) {
        let mut buf = [0u8; 2048];
        while self.t0.elapsed().as_secs_f64() < until {
            if let Ok(n) = self.rx.recv(&mut buf) {
                let k = if buf[..n] == self.on[..] {
                    Kind::On
                } else if buf[..n] == self.off[..] {
                    Kind::Off
                } else {
                    assert_eq!(n, 92, "10-segment razer frame");
                    Kind::Frame
                };
                self.log.push((self.t0.elapsed().as_secs_f64(), k));
            }
        }
    }

    fn signal(&self, s: i32) {
        // SAFETY: plain kill(2) on our child.
        assert_eq!(unsafe { libc::kill(self.child.id() as i32, s) }, 0);
    }

    /// SIGTERM, wait, collect the tail; returns (exit ms, stderr).
    fn stop(&mut self) -> (u128, String) {
        self.signal(libc::SIGTERM);
        let ts = Instant::now();
        let status = loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                break s;
            }
            assert!(
                ts.elapsed() < Duration::from_secs(2),
                "gowl ignored SIGTERM"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        let exit_ms = ts.elapsed().as_millis();
        self.collect(self.t0.elapsed().as_secs_f64() + 0.2);
        let mut err = String::new();
        std::io::Read::read_to_string(self.child.stderr.as_mut().unwrap(), &mut err).unwrap();
        let _ = std::fs::remove_dir_all(&self.tmp);
        assert!(status.success(), "exit {status:?}: {err}");
        (exit_ms, err)
    }

    fn frames(&self, a: f64, b: f64) -> usize {
        self.log
            .iter()
            .filter(|(t, k)| *k == Kind::Frame && *t > a && *t < b)
            .count()
    }

    fn kinds(&self, a: f64, b: f64) -> Vec<&Kind> {
        self.log
            .iter()
            .filter(|(t, _)| *t > a && *t < b)
            .map(|e| &e.1)
            .collect()
    }

    fn tail_is_double_off(&self) {
        let tail: Vec<&Kind> = self.log.iter().rev().take(2).map(|e| &e.1).collect();
        assert_eq!(tail, [&Kind::Off, &Kind::Off], "{:?}", self.log);
    }
}

#[test]
fn screen_against_mock_compositor() {
    let mut h = Harness::start("screen-mock", 16000, 20);
    h.collect(2.5); // ~0.7 s of changing frames, then static
    h.signal(libc::SIGUSR1);
    h.collect(3.3);
    h.signal(libc::SIGUSR2);
    h.collect(3.6);
    let (exit_ms, _) = h.stop();

    assert!(exit_ms < 500, "exit took {exit_ms} ms");
    assert_eq!(h.served.load(Ordering::SeqCst), 20, "mock served frames");
    assert_eq!(h.log.first().map(|e| &e.1), Some(&Kind::On), "{:?}", h.log);
    // Changing phase (20 frames at 30 fps, smoothing fades in): plenty of sends.
    assert!(h.frames(0.0, 1.0) >= 10, "{:?}", h.log);
    // Static phase: keepalive only (500 ms) — about 2 datagrams per second.
    let ka = h.frames(1.4, 2.4);
    assert!((1..=3).contains(&ka), "static phase sent {ka}: {:?}", h.log);
    // Pause: razer OFF ×2, then silence.
    assert_eq!(h.kinds(2.5, 3.3), [&Kind::Off, &Kind::Off], "{:?}", h.log);
    // Resume: razer ON and the current colors again.
    let resumed = h.kinds(3.3, 3.6);
    assert_eq!(resumed.first(), Some(&&Kind::On), "{:?}", h.log);
    assert!(resumed.contains(&&Kind::Frame), "{:?}", h.log);
    h.tail_is_double_off();
}

#[test]
fn output_unplug_and_replug() {
    let mut h = Harness::start("screen-hotplug", 16100, 10);
    // 10 frames (~0.35 s), then the fade finishes and only keepalives remain.
    h.collect(1.5);
    h.ctl.send(mock_wl::Ctl::RemoveOutput).unwrap();
    h.collect(3.0);
    h.ctl
        .send(mock_wl::Ctl::AddOutput { more_frames: 10 })
        .unwrap();
    h.collect(4.0);
    let (exit_ms, err) = h.stop();

    assert!(exit_ms < 500, "exit took {exit_ms} ms");
    assert!(err.contains("output gone"), "{err}");
    assert!(err.contains("is back"), "{err}");
    assert!(h.frames(0.0, 1.5) >= 5, "{:?}", h.log);
    // Unplugged: razer OFF ×2, then nothing — no keepalives, no fade steps.
    assert_eq!(h.kinds(1.5, 3.0), [&Kind::Off, &Kind::Off], "{:?}", h.log);
    // Replugged: razer ON first, then frames from the new output.
    let back = h.kinds(3.0, 4.0);
    assert_eq!(back.first(), Some(&&Kind::On), "{:?}", h.log);
    assert!(h.frames(3.0, 4.0) >= 5, "{:?}", h.log);
    assert_eq!(h.served.load(Ordering::SeqCst), 20, "mock served frames");
    h.tail_is_double_off();
}

#[test]
fn cli_pause_resume_stop_and_one_instance() {
    let mut h = Harness::start("screen-ctl", 16200, 20);
    h.collect(1.2);
    assert!(h.tmp.join("gowl.pid").exists(), "pidfile");
    // A second sync refuses to start (and sends nothing).
    let (code, err) = h.cli(&["screen"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("already running"), "{err}");
    let (code, err) = h.cli(&["pause"]);
    assert_eq!(code, 0, "{err}");
    h.collect(1.8);
    let (code, _) = h.cli(&["pause"]); // idempotent: no second OFF
    assert_eq!(code, 0);
    h.collect(2.4);
    assert_eq!(h.kinds(1.2, 2.4), [&Kind::Off, &Kind::Off], "{:?}", h.log);
    let (code, err) = h.cli(&["resume"]);
    assert_eq!(code, 0, "{err}");
    h.collect(2.8);
    let resumed = h.kinds(2.4, 2.8);
    assert_eq!(resumed.first(), Some(&&Kind::On), "{:?}", h.log);
    assert!(resumed.contains(&&Kind::Frame), "{:?}", h.log);
    let t = Instant::now();
    let (code, err) = h.cli(&["stop"]);
    assert_eq!(code, 0, "{err}");
    assert!(t.elapsed() < Duration::from_millis(1000));
    let rx = h.cli(&["pause"]);
    h.exited();
    h.tail_is_double_off();
    assert_eq!(rx.0, 1, "pause with nothing running: {}", rx.1);
    assert!(rx.1.contains("no gowl sync is running"), "{}", rx.1);
}

#[test]
fn toggle_starts_then_stops() {
    let mut h = Harness::start_with("screen-toggle", 16300, 10, &["toggle", "--fps", "30"]);
    h.collect(1.0);
    assert_eq!(h.log.first().map(|e| &e.1), Some(&Kind::On), "{:?}", h.log);
    assert!(h.frames(0.0, 1.0) >= 5, "{:?}", h.log);
    let t = Instant::now();
    let (code, err) = h.cli(&["toggle"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        t.elapsed() < Duration::from_millis(700),
        "toggle waited {:?}",
        t.elapsed()
    );
    h.exited();
    h.tail_is_double_off();
}
