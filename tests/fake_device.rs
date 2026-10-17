//! End-to-end against tools/fake_govee.py on test ports (base 14000, loopback).
//! Needs python3; fails without it unless GOWL_TEST_NO_PYTHON=1 opts out.

mod common;

use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use common::Json;
use gowl::net::{self, Ports, Sender};

const BASE: u16 = 14000;

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
    fn start() -> Option<Fake> {
        if Command::new("python3").arg("--version").output().is_err() {
            // Never pass silently: skipping needs an explicit opt-out.
            assert!(
                std::env::var_os("GOWL_TEST_NO_PYTHON").is_some(),
                "python3 not found: install it, or set GOWL_TEST_NO_PYTHON=1 to skip the fake-device tests"
            );
            eprintln!("python3 not found and GOWL_TEST_NO_PYTHON set: skipping fake-device test");
            return None;
        }
        let mut child = Command::new("python3")
            .arg("-I")
            .arg(common::root().join("tools/fake_govee.py"))
            .args([
                "--port-base",
                &BASE.to_string(),
                "--json-log",
                "--quiet",
                "--bind",
                "127.0.0.1",
            ])
            .args(["--segments", "10"])
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
        Some(Fake { child, rx })
    }

    /// Next logged datagram with `cmd`, skipping others (status queries etc.).
    fn next(&self, cmd: &str) -> Json {
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let j = self
                .rx
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("fake never logged cmd {cmd:?}"));
            if j.opt("cmd").is_some_and(|c| !c.is_null() && c.str() == cmd) {
                return j;
            }
        }
    }

    fn drain(&self) {
        while self.rx.try_recv().is_ok() {}
    }
}

fn gowl(tmp: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_gowl"))
        .args(args)
        .env("GOWL_DEVICE", "127.0.0.1")
        .env("GOWL_PORT_BASE", BASE.to_string())
        .env("XDG_CONFIG_HOME", tmp)
        .env("XDG_STATE_HOME", tmp)
        .env("XDG_RUNTIME_DIR", tmp)
        .env("HOME", tmp)
        .env_remove("GOWL_CONFIG")
        .env_remove("GOWL_LOG")
        .output()
        .expect("run gowl");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn one_shots_status_and_razer_against_fake_device() {
    let Some(fake) = Fake::start() else { return };
    let tmp = std::env::temp_dir().join(format!("gowl-fake-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();

    // Wait until the fake answers (it needs a moment to bind).
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let (code, _, _) = gowl(&tmp, &["status"]);
        if code == 0 {
            break;
        }
        assert!(Instant::now() < end, "fake device never answered status");
    }
    fake.drain();

    let (code, _, err) = gowl(&tmp, &["on"]);
    assert_eq!(code, 0, "{err}");
    let j = fake.next("turn");
    assert!(j.get("valid").bool());
    assert_eq!(j.get("value").u64(), 1);

    assert_eq!(gowl(&tmp, &["brightness", "42"]).0, 0);
    let j = fake.next("brightness");
    assert!(j.get("valid").bool());
    assert_eq!(j.get("value").u64(), 42);

    assert_eq!(gowl(&tmp, &["color", "ff8000"]).0, 0);
    let j = fake.next("colorwc");
    assert_eq!(j.get("color").arr().len(), 3);
    assert_eq!(
        j.get("color")
            .arr()
            .iter()
            .map(Json::u64)
            .collect::<Vec<_>>(),
        [255, 128, 0]
    );
    assert_eq!(j.get("kelvin").u64(), 0);

    let (code, out, err) = gowl(&tmp, &["status"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("power on brightness 42 color ff8000"), "{out}");

    assert_eq!(gowl(&tmp, &["temp", "2700"]).0, 0);
    let j = fake.next("colorwc");
    assert_eq!(j.get("kelvin").u64(), 2700);
    let c = j.get("color").arr();
    assert_eq!([c[0].u64(), c[1].u64(), c[2].u64()], [255, 167, 87]);

    assert_eq!(gowl(&tmp, &["off"]).0, 0);
    assert_eq!(fake.next("turn").get("value").u64(), 0);
    let (code, out, _) = gowl(&tmp, &["status"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("power off") && out.contains("temp 2700K"),
        "{out}"
    );

    // Usage errors never touch the network.
    assert_eq!(gowl(&tmp, &["brightness", "101"]).0, 2);
    assert_eq!(gowl(&tmp, &["temp", "1000"]).0, 2);

    // Razer streaming through the preallocated Sender, on a socket bound to `lo`.
    let ifaces = net::interfaces().unwrap();
    let lo = net::select_for(&ifaces, Ipv4Addr::LOCALHOST, None)
        .unwrap()
        .iface;
    let sock = net::bound_socket(&lo, None).unwrap();
    let ports = Ports {
        scan: BASE + 1,
        reply: BASE + 2,
        control: BASE + 3,
    };
    let mut s = Sender::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, ports.control), 10);
    s.razer_mode(&sock, true).unwrap();
    let j = fake.next("razer_mode");
    assert!(j.get("valid").bool() && j.get("on").bool());
    let colors: Vec<[u8; 3]> = (0..10u8).map(|i| [i * 20, 255 - i, i]).collect();
    s.send_colors(&sock, &colors).unwrap();
    let j = fake.next("razer_frame");
    assert!(j.get("valid").bool() && j.get("razer_on").bool());
    assert_eq!(j.get("n").u64(), 10);
    assert_eq!(j.get("size").u64(), 92);
    assert_eq!(j.get("colors").rgb_list(), colors);
    s.razer_mode(&sock, false).unwrap();
    assert!(!fake.next("razer_mode").get("on").bool());

    let _ = std::fs::remove_dir_all(&tmp);
}
