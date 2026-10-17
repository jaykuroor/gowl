//! `gowl calibrate --keepalive` end to end against a UDP receiver on loopback:
//! razer ON + a fixed frame, keepalives at the step's interval, the next step slower,
//! "n" ends the walk, "y" saves keepalive_ms = 70 % of the longest hold into the
//! config without touching anything else, razer OFF ×2 at the end.

use std::io::Write;
use std::net::UdpSocket;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use gowl::proto;

const BASE: u16 = 16700;

fn razer(on: bool) -> Vec<u8> {
    let mut m = [0u8; 64];
    let n = proto::razer_msg(&proto::razer_mode_frame(on), &mut m);
    m[..n].to_vec()
}

/// (ON, OFF, frames) received during `secs`.
fn count(rx: &UdpSocket, secs: f64) -> (usize, usize, usize) {
    let (on, off) = (razer(true), razer(false));
    let mut c = (0, 0, 0);
    let mut buf = [0u8; 512];
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        if let Ok(n) = rx.recv(&mut buf) {
            if buf[..n] == on[..] {
                c.0 += 1;
            } else if buf[..n] == off[..] {
                c.1 += 1;
            } else {
                c.2 += 1;
            }
        }
    }
    c
}

#[test]
fn keepalive_walk_steps_slower_and_saves_70_percent() {
    let tmp = std::env::temp_dir().join(format!("gowl-ka-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let cfg = tmp.join("config");
    let before = "fps = 30   # mine\nmystery = 1\n\n[device desk]\nip = 10.0.0.1\n";
    std::fs::write(&cfg, before).unwrap();
    let rx = UdpSocket::bind(("127.0.0.1", BASE + 3)).unwrap();
    rx.set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_gowl"))
        .args(["calibrate", "--keepalive"])
        .env("GOWL_DEVICE", "127.0.0.1")
        .env("GOWL_PORT_BASE", BASE.to_string())
        .env("GOWL_CONFIG", &cfg)
        .env("XDG_STATE_HOME", &tmp)
        .env("XDG_RUNTIME_DIR", &tmp)
        .env("HOME", &tmp)
        .env_remove("GOWL_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();

    // Step 1: razer ON, the frame, then one keepalive every 0.5 s.
    let (on, off, frames) = count(&rx, 1.3);
    assert_eq!((on, off), (1, 0));
    assert!(
        (3..=4).contains(&frames),
        "0.5 s step: {frames} frames in 1.3 s"
    );
    stdin.write_all(b"y\n").unwrap();
    // Step 2: razer ON again, the frame, then one every 1 s.
    let (on, _, frames) = count(&rx, 2.3);
    assert_eq!(on, 1);
    assert!(
        (2..=3).contains(&frames),
        "1 s step: {frames} frames in 2.3 s"
    );
    // Dropped out at 1 s: the longest hold was 0.5 s → 350 ms; save it.
    stdin.write_all(b"n\ny\n").unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("keepalive_ms = 350"), "{stdout}");
    let (_, off, _) = count(&rx, 0.2);
    assert_eq!(off, 2, "razer OFF ×2 at the end");
    let after = std::fs::read_to_string(&cfg).unwrap();
    assert_eq!(
        after,
        "fps = 30   # mine\nmystery = 1\nkeepalive_ms = 350   # light held 500 ms between frames (gowl calibrate --keepalive)\n\n[device desk]\nip = 10.0.0.1\n"
    );
    assert!(!tmp.join("gowl.pid").exists());
    let _ = std::fs::remove_dir_all(&tmp);
}
