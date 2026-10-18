//! Multi-device fan-out: one `gowl screen` (against tests/mock_wl) streams to two
//! `tools/fake_govee.py` instances on two loopback IPs (127.0.0.2 and 127.0.0.3)
//! with different segment counts and orientation — one capture, one zone set per
//! segment count, per-device razer ON/OFF, frames and pause. The same light named
//! twice (alias and IP) is streamed to once. Needs python3 (GOWL_TEST_NO_PYTHON=1
//! skips explicitly).

mod common;
mod mock_wl;

use std::io::{BufRead, BufReader};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixListener;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use common::Json;

const BASE: u16 = 16500;

struct Fake {
    child: Child,
    rx: Receiver<Json>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Fake {
    fn start(ip: &str, segments: usize) -> Fake {
        let mut child = Command::new("python3")
            .arg("-I")
            .arg(common::root().join("tools/fake_govee.py"))
            .args(["--port-base", &BASE.to_string(), "--json-log", "--quiet"])
            .args(["--bind", ip, "--ip", ip])
            .args(["--segments", &segments.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fake_govee.py");
        let out = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(common::parse(&line)).is_err() {
                    break;
                }
            }
        });
        Fake { child, rx }
    }

    /// Everything logged so far, as (cmd, record).
    fn log(&self) -> Vec<(String, Json)> {
        let mut v = Vec::new();
        while let Ok(j) = self.rx.try_recv() {
            let cmd = match j.opt("cmd") {
                Some(c) if !c.is_null() => c.str().to_string(),
                _ => "?".into(),
            };
            v.push((cmd, j));
        }
        v
    }
}

fn mode(e: &(String, Json)) -> Option<bool> {
    (e.0 == "razer_mode").then(|| e.1.get("on").bool())
}

fn check_stream(name: &str, log: &[(String, Json)], segments: u64) {
    assert!(!log.is_empty(), "{name}: nothing received");
    for (cmd, j) in log {
        assert!(
            j.get("valid").bool(),
            "{name}: invalid datagram {cmd}: {j:?}"
        );
    }
    assert_eq!(mode(&log[0]), Some(true), "{name}: razer ON first");
    let frames: Vec<&Json> = log
        .iter()
        .filter(|e| e.0 == "razer_frame")
        .map(|e| &e.1)
        .collect();
    assert!(frames.len() >= 5, "{name}: only {} frames", frames.len());
    assert!(
        frames.iter().all(|f| f.get("n").u64() == segments),
        "{name}: frames must have {segments} segments"
    );
    let tail: Vec<Option<bool>> = log.iter().rev().take(2).map(mode).collect();
    assert_eq!(
        tail,
        [Some(false), Some(false)],
        "{name}: razer OFF ×2 last"
    );
}

#[test]
fn screen_fans_out_to_two_fake_devices() {
    if Command::new("python3").arg("--version").output().is_err() {
        assert!(
            std::env::var_os("GOWL_TEST_NO_PYTHON").is_some(),
            "python3 not found: install it, or set GOWL_TEST_NO_PYTHON=1 to skip"
        );
        return;
    }
    let a = Fake::start("127.0.0.2", 10);
    let b = Fake::start("127.0.0.3", 6);
    std::thread::sleep(Duration::from_millis(400)); // let both bind

    let tmp = std::env::temp_dir().join(format!("gowl-multi-{}", std::process::id()));
    std::fs::create_dir_all(tmp.join("gowl")).unwrap();
    std::fs::write(
        tmp.join("gowl/config"),
        "# two lights\n[device desk]\nip = 127.0.0.2\nsegments = 10\n\n[device shelf]\nip = 127.0.0.3\nsegments = 6\nreverse = true\n",
    )
    .unwrap();
    let sock_path = tmp.join("wayland-mock");
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path).unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let s2 = served.clone();
    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let opts = mock_wl::Opts {
                width: 320,
                height: 180,
                transform: 0,
                changing: 20,
            };
            mock_wl::serve(OwnedFd::from(stream), opts, s2);
        }
    });
    let mut child = common::fake_desktop(&mut Command::new(env!("CARGO_BIN_EXE_gowl")), &tmp)
        // `127.0.0.2` is desk again: streamed to once.
        .args(["screen", "--fps", "30", "desk", "shelf", "127.0.0.2"])
        .env("WAYLAND_DISPLAY", &sock_path)
        .env("GOWL_PORT_BASE", BASE.to_string())
        .env("XDG_CONFIG_HOME", &tmp)
        .env("XDG_STATE_HOME", &tmp)
        .env("XDG_RUNTIME_DIR", &tmp)
        .env("HOME", &tmp)
        .env_remove("GOWL_DEVICE")
        .env_remove("GOWL_CONFIG")
        .env_remove("GOWL_LOG")
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    // SAFETY: plain kill(2) on our child.
    unsafe { libc::kill(child.id() as i32, libc::SIGUSR1) };
    std::thread::sleep(Duration::from_millis(300));
    let paused_a = a.log();
    let paused_b = b.log();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        a.log().is_empty() && b.log().is_empty(),
        "silence while paused"
    );
    // SAFETY: as above.
    unsafe { libc::kill(child.id() as i32, libc::SIGUSR2) };
    std::thread::sleep(Duration::from_millis(400));
    // SAFETY: as above.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let t = Instant::now();
    let status = child.wait().unwrap();
    let mut err = String::new();
    std::io::Read::read_to_string(child.stderr.as_mut().unwrap(), &mut err).unwrap();
    assert!(status.success(), "{err}");
    assert!(t.elapsed() < Duration::from_millis(500));
    assert!(err.contains("streaming to it once"), "{err}");
    std::thread::sleep(Duration::from_millis(300));

    for (name, paused, fake, n) in [("desk", paused_a, &a, 10), ("shelf", paused_b, &b, 6)] {
        assert_eq!(
            paused.last().and_then(mode),
            Some(false),
            "{name}: pause = razer OFF"
        );
        let mut all = paused;
        let rest = fake.log();
        assert_eq!(
            rest.first().and_then(mode),
            Some(true),
            "{name}: resume = razer ON"
        );
        all.extend(rest);
        check_stream(name, &all, n);
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
