//! End-to-end `gowl screen` without niri or a light: the binary connects to
//! tests/mock_wl (a fake compositor on a Unix socket given as WAYLAND_DISPLAY) and
//! streams to a UDP receiver on loopback test ports. Checks the runtime contract:
//! razer ON first, frames while the screen changes, keepalive only while it is
//! static, pause = razer OFF ×2 + silence, resume = razer ON, output unplugged =
//! razer OFF ×2 (like pause) + silence, replugged = razer ON + frames,
//! SIGTERM = razer OFF ×2. Runtime control through the CLI: one instance per
//! pidfile, `gowl pause|resume|stop`, `gowl toggle` (start and stop).

mod common;
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
    common::fake_desktop(&mut c, tmp);
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

/// D-051 widens the gap between `capture_output` and `copy` (the next frame is
/// negotiated on `ready`, copied at the next slot). A `failed` sent in that gap is
/// read at the timer wakeup and takes the normal back-off; capture then resumes.
#[test]
fn failed_while_held_backs_off_and_recovers() {
    let mut h = Harness::start("screen-held-fail", 16500, usize::MAX);
    h.collect(1.0);
    let before = h.served.load(Ordering::SeqCst);
    h.ctl.send(mock_wl::Ctl::FailBeforeCopy).unwrap();
    h.collect(2.0);
    let after = h.served.load(Ordering::SeqCst);
    let (exit_ms, err) = h.stop();
    assert!(exit_ms < 500, "exit took {exit_ms} ms");
    assert!(err.contains("capture failed; retrying"), "{err}");
    assert!(err.contains("capture works again after 1 failure"), "{err}");
    // 30 fps for 1 s minus one 100 ms back-off.
    assert!(
        after - before >= 20,
        "only {} frames after the failure",
        after - before
    );
    h.tail_is_double_off();
}

/// D-057: copies keep completing but the edges don't change (niri redraws for an
/// animation elsewhere) → after `idle_after_ms` (1 s) capture drops to `idle_fps`
/// (5); the first changed frame brings back the configured 30 fps.
#[test]
fn idle_capture_rate_drops_and_recovers() {
    let mut h = Harness::start("screen-idle", 16600, usize::MAX);
    let served = |h: &Harness| h.served.load(Ordering::SeqCst);
    h.collect(1.0);
    let a = served(&h);
    h.collect(2.0);
    let busy = served(&h) - a;
    h.ctl.send(mock_wl::Ctl::SameContent(true)).unwrap();
    h.collect(3.5); // 1 s to notice, then idle
    let b = served(&h);
    h.collect(5.5);
    let idle = served(&h) - b;
    h.ctl.send(mock_wl::Ctl::SameContent(false)).unwrap();
    let t = h.now();
    let c = served(&h);
    // The first changed frame comes within one idle interval (200 ms); the frame
    // after it already at the full rate (≈ 33 ms later, not another 200 ms).
    let mut stamps = Vec::new();
    let mut last = c;
    while stamps.len() < 3 && h.now() < t + 1.0 {
        let v = served(&h);
        if v != last {
            stamps.push(h.now());
            last = v;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let first = stamps.len();
    let to_first = stamps.first().map_or(9.0, |s| s - t);
    let gap = match stamps[..] {
        [a, b, ..] => b - a,
        _ => 9.0,
    };
    h.collect(t + 0.3);
    let d = served(&h);
    h.collect(t + 1.3);
    let back = served(&h) - d;
    let (_, err) = h.stop();
    println!(
        "busy {busy}/s, idle {}/s, first changed frame after {:.0} ms, next one {:.0} ms later, then {back}/s",
        idle / 2,
        to_first * 1000.0,
        gap * 1000.0
    );
    assert!((27..=33).contains(&busy), "busy {busy}/s; {err}");
    assert!(
        (8..=12).contains(&idle),
        "idle {idle} in 2 s (5 fps); {err}"
    );
    assert!(first >= 2, "rate did not come back ({stamps:?})");
    assert!(to_first < 0.25, "first changed frame after {to_first:.3} s");
    assert!(
        gap < 0.08,
        "frame after the first changed one came {gap:.3} s later"
    );
    assert!((27..=33).contains(&back), "after the change {back}/s");
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
    // Unplugged: razer OFF ×2, then nothing — no keepalives, no fade steps. (A
    // keepalive due in the few ms before the mock's removal lands may precede them.)
    let unplugged: Vec<&Kind> = h
        .kinds(1.5, 3.0)
        .into_iter()
        .skip_while(|k| **k == Kind::Frame)
        .collect();
    assert_eq!(unplugged, [&Kind::Off, &Kind::Off], "{:?}", h.log);
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

fn ctx_switches(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .filter(|l| l.contains("ctxt_switches"))
        .map(|l| l.split_whitespace().nth(1).unwrap().parse::<u64>().unwrap())
        .sum()
}

/// BUDGETS.md S2 on the gowl side: a static screen (the compositor never completes
/// the damage copy) costs only keepalive wakeups, ≤ 1 + 1000/keepalive_ms per second
/// (default keepalive 500 ms → ≤ 3/s). `cargo test --release --test screen_mock -- --ignored s2`
#[test]
#[ignore]
fn s2_static_screen_wakeups() {
    let mut h = Harness::start("screen-s2", 16400, 10);
    h.collect(1.5); // 10 changing frames, then static; smoothing settles
    let pid = h.child.id();
    let c0 = ctx_switches(pid);
    let t0 = h.now();
    h.collect(t0 + 10.0);
    let rate = (ctx_switches(pid) - c0) as f64 / (h.now() - t0);
    let ka = h.frames(t0, t0 + 10.0);
    h.stop();
    println!("S2 (mock, static): {rate:.2} context switches/s, {ka} keepalives in 10 s");
    assert!(rate <= 3.0, "{rate:.2}/s over the S2 budget");
    assert!((15..=25).contains(&ka), "{ka} keepalives in 10 s at 500 ms");
}

// ------------------------------------------------------------------ Phase 6: toggle,
// notifications, status file (D-059, D-060). Notifications go to the fake
// notify-send of tests/common (never the real desktop).

impl Harness {
    /// A gowl command with this harness's environment; music gets a sound server
    /// that doesn't exist (it keeps running and retrying, which is all a switch needs).
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = gowl_cmd(&self.tmp, &self.tmp.join("wayland-mock"), self.base);
        c.args(args)
            .env(
                "PULSE_SERVER",
                format!("unix:{}", self.tmp.join("no-pulse").display()),
            )
            .env("PULSE_COOKIE", self.tmp.join("no-cookie"));
        c
    }

    /// `gowl state`, parsed.
    fn state(&self) -> common::Json {
        let out = self.cmd(&["state"]).output().unwrap();
        assert_eq!(out.status.code(), Some(0));
        common::parse(String::from_utf8(out.stdout).unwrap().trim())
    }

    /// Summaries (the argument after `--`) of the recorded notifications, after
    /// waiting for at least `n`.
    fn notes(&self, n: usize) -> Vec<String> {
        common::wait_notifications(&self.tmp, n)
            .iter()
            .map(|argv| {
                // notify-send -a gowl -t 2500 -h string:x-canonical-private-synchronous:gowl -- SUMMARY [BODY]
                assert_eq!(
                    argv[..7],
                    [
                        "-a",
                        "gowl",
                        "-t",
                        "2500",
                        "-h",
                        "string:x-canonical-private-synchronous:gowl",
                        "--"
                    ],
                    "{argv:?}"
                );
                argv[7].clone()
            })
            .collect()
    }
}

fn wait_child(c: &mut Child, ms: u64) -> std::process::ExitStatus {
    let t = Instant::now();
    loop {
        if let Some(s) = c.try_wait().unwrap() {
            return s;
        }
        assert!(
            t.elapsed() < Duration::from_millis(ms),
            "child still running"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn toggle_resumes_switches_and_notifies() {
    let mut h = Harness::start_with(
        "toggle-switch",
        16800,
        1000,
        &["toggle", "screen", "--fps", "30"],
    );
    // Started by toggle: "Screen sync on" once setup worked, and the status file.
    h.collect(1.0);
    assert_eq!(h.notes(1), ["Screen sync on"]);
    let s = h.state();
    assert!(s.get("running").bool());
    assert_eq!(s.get("mode").str(), "screen");
    assert!(!s.get("paused").bool());
    assert_eq!(s.get("fps").u64(), 30);
    let d = &s.get("devices").arr()[0];
    assert_eq!(d.get("ip").str(), "127.0.0.1");
    assert_eq!(d.get("name").str(), "127.0.0.1");
    assert!(d.get("ok").bool());
    assert!(s.get("error").is_null());
    assert_eq!(s.get("pid").u64(), u64::from(h.child.id()));

    // Steady streaming rewrites nothing (rules 5/7).
    let st = h.tmp.join("gowl/state");
    let m0 = std::fs::metadata(&st).unwrap().modified().unwrap();
    h.collect(1.6);
    assert_eq!(std::fs::metadata(&st).unwrap().modified().unwrap(), m0);

    // pause → notification + paused; toggle on the paused mode resumes it.
    assert_eq!(h.cli(&["pause"]).0, 0);
    assert_eq!(h.notes(2)[1], "Screen sync paused");
    assert!(h.state().get("paused").bool());
    h.collect(2.0);
    let (code, err) = h.cli(&["toggle", "screen"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(h.notes(3)[2], "Screen sync resumed");
    assert!(!h.state().get("paused").bool());
    h.collect(2.5);
    let resumed = h.kinds(2.0, 2.5);
    assert_eq!(resumed.first(), Some(&&Kind::On), "{:?}", h.log);

    // The other mode: screen stops (razer OFF ×2, exit 0), then music starts.
    let mut music = h
        .cmd(&["toggle", "music"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let st = wait_child(&mut h.child, 2000);
    assert!(st.success(), "screen exit {st:?}");
    h.collect(3.3);
    let tail = h.kinds(2.5, 3.3);
    let i = tail
        .iter()
        .position(|k| **k == Kind::Off)
        .expect("razer OFF");
    // Never two syncs: screen's OFF ×2 before music's ON. Then music finds no sound
    // server and gives the light back at once (OFF ×2, no keepalives; D-064).
    assert_eq!(
        tail[i..],
        [&Kind::Off, &Kind::Off, &Kind::On, &Kind::Off, &Kind::Off],
        "{:?}",
        h.log
    );
    let notes = h.notes(4);
    assert_eq!(notes[3], "Switched from screen to music sync", "{notes:?}");
    let s = h.state();
    assert_eq!(s.get("mode").str(), "music");
    assert_eq!(s.get("pid").u64(), u64::from(music.id()));
    assert_eq!(s.get("reaction").str(), "flow");
    assert_eq!(s.get("error").str(), "sound server unreachable");

    // Same mode again: stop. Nothing runs afterwards; the files are gone.
    let (code, err) = h.cli(&["toggle", "music"]);
    assert_eq!(code, 0, "{err}");
    assert!(wait_child(&mut music, 2000).success());
    assert_eq!(h.notes(5)[4], "Music sync off");
    assert!(!h.state().get("running").bool());
    assert!(!h.tmp.join("gowl/state").exists());
    assert!(!h.tmp.join("gowl.pid").exists());
    h.collect(3.8);
    h.tail_is_double_off();
    // Every notify-send started (from the CLI and from the syncs, which block
    // signals for their signalfd and hold sockets) got an empty signal mask and
    // only stdin/stdout/stderr.
    let procs = common::notify_procs(&h.tmp);
    assert!(procs.len() >= 5, "{procs:?}");
    for (mask, fds) in &procs {
        assert_eq!(mask, "0000000000000000", "inherited a blocked signal");
        let nums: Vec<u32> = fds.iter().map(|f| f.0).collect();
        assert_eq!(nums, [0, 1, 2], "{fds:?}");
        assert!(fds.iter().all(|f| f.1 == "/dev/null"), "{fds:?}");
    }
    let _ = std::fs::remove_dir_all(&h.tmp);
}

#[test]
fn compositor_gone_exits_with_razer_off() {
    // A toggle-started `gowl screen` whose session ends (niri exits): the Wayland
    // connection closes → razer OFF ×2, exit, one notification, no files left.
    let mut h = Harness::start_with("wl-gone", 17300, 1000, &["toggle", "screen", "--fps", "30"]);
    h.collect(1.0);
    assert!(h.frames(0.0, 1.0) >= 5, "{:?}", h.log);
    h.ctl.send(mock_wl::Ctl::Disconnect).unwrap();
    let st = wait_child(&mut h.child, 2000);
    assert!(!st.success(), "exit {st:?}");
    h.collect(h.now() + 0.2);
    h.tail_is_double_off();
    let notes = h.notes(2);
    assert_eq!(notes[0], "Screen sync on");
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(!h.tmp.join("gowl.pid").exists() && !h.tmp.join("gowl/state").exists());
    let _ = std::fs::remove_dir_all(&h.tmp);
}

#[test]
fn failures_are_notified_and_notify_off_silences() {
    let tmp = std::env::temp_dir().join(format!("gowl-notify-err-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let run = |args: &[&str], dev: bool| {
        let mut c = gowl_cmd(&tmp, &tmp.join("no-wayland"), 16900);
        if !dev {
            c.env_remove("GOWL_DEVICE");
        }
        c.args(args).output().unwrap().status.code().unwrap()
    };
    // No light at all: a keybind toggle must not fail silently.
    assert_eq!(run(&["toggle", "screen"], false), 3);
    let n = common::wait_notifications(&tmp, 1);
    assert_eq!(n[0][7], "No light configured — run gowl setup", "{n:?}");
    // No compositor: capture unavailable.
    assert_eq!(run(&["toggle"], true), 4);
    assert_eq!(
        common::wait_notifications(&tmp, 2)[1][7],
        "Screen capture unavailable"
    );
    assert!(!tmp.join("gowl.pid").exists() && !tmp.join("gowl/state").exists());
    // Nothing running.
    assert_eq!(run(&["stop"], true), 1);
    assert_eq!(
        common::wait_notifications(&tmp, 3)[2][7],
        "No gowl sync is running"
    );
    // A usage error typed into a terminal isn't a notification.
    assert_eq!(run(&["screen", "--bogus"], true), 2);
    // notify = off: silent.
    std::fs::create_dir_all(tmp.join("gowl")).unwrap();
    std::fs::write(tmp.join("gowl/config"), "notify = off\n").unwrap();
    assert_eq!(run(&["pause"], true), 1);
    std::thread::sleep(Duration::from_millis(300));
    let all = common::notifications(&tmp);
    assert_eq!(all.len(), 3, "{all:?}");
    // `gowl state` with nothing running.
    let out = gowl_cmd(&tmp, &tmp, 16900).arg("state").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "{\"running\":false}\n"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn watch_prints_each_change_once_and_sees_a_crash() {
    use std::io::{BufRead, BufReader};
    let mut h = Harness::start("watch", 17100, 1000);
    h.collect(0.8);
    let mut w = h.cmd(&["watch"]).stdout(Stdio::piped()).spawn().unwrap();
    let (tx, lines) = channel::<String>();
    let out = w.stdout.take().unwrap();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines() {
            let Ok(l) = l else { break };
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    let next = || {
        common::parse(
            &lines
                .recv_timeout(Duration::from_secs(2))
                .expect("a state line"),
        )
    };
    let first = next();
    assert_eq!(first.get("mode").str(), "screen");
    assert!(!first.get("paused").bool());
    h.cli(&["pause"]);
    assert!(next().get("paused").bool());
    h.cli(&["resume"]);
    assert!(!next().get("paused").bool());
    // Steady: no lines, and watch sleeps (no wakeups) meanwhile. (The fake
    // notify-send logs into this runtime dir: let both of its writes land first.)
    h.notes(2);
    h.collect(h.now() + 0.2);
    let c0 = ctx_switches(w.id());
    // Other programs' files in the runtime dir (the human's shell writes vault-*
    // flags there) don't wake it: it only watches $XDG_RUNTIME_DIR/gowl/.
    for i in 0..20 {
        let f = h.tmp.join(format!("vault-flag-{i}"));
        std::fs::write(&f, "1").unwrap();
        std::fs::rename(&f, h.tmp.join("vault-flag")).unwrap();
    }
    h.collect(h.now() + 1.0);
    assert!(lines.try_recv().is_err(), "a line without a change");
    let woke = ctx_switches(w.id()) - c0;
    assert!(woke <= 1, "watch woke {woke}× while nothing changed");
    // A crash (SIGKILL: no cleanup, state file left behind) still reads as stopped.
    h.signal(libc::SIGKILL);
    wait_child(&mut h.child, 2000);
    assert_eq!(
        lines.recv_timeout(Duration::from_secs(2)).unwrap(),
        "{\"running\":false}"
    );
    assert!(
        h.tmp.join("gowl/state").exists(),
        "stale file left by the crash"
    );
    assert!(!h.state().get("running").bool());
    // SIGTERM ends watch cleanly.
    // SAFETY: plain kill(2) on our child.
    unsafe { libc::kill(w.id() as i32, libc::SIGTERM) };
    assert!(wait_child(&mut w, 1000).success());
    let _ = std::fs::remove_dir_all(&h.tmp);
}

#[test]
fn watch_exits_when_its_reader_goes_away() {
    let tmp = std::env::temp_dir().join(format!("gowl-watch-pipe-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut w = gowl_cmd(&tmp, &tmp, 17200)
        .arg("watch")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = [0u8; 18];
    std::io::Read::read_exact(w.stdout.as_mut().unwrap(), &mut first).unwrap();
    assert_eq!(&first, b"{\"running\":false}\n");
    drop(w.stdout.take()); // the reader (a bar) goes away; no change needed to notice
    assert!(wait_child(&mut w, 1000).success());
    let _ = std::fs::remove_dir_all(&tmp);
}
