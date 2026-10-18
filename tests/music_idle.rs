//! D-054 regression: loud music that stops abruptly (song paused, sink suspended or
//! moved — the passive stream then delivers no more windows) must not leave the
//! light frozen on the last loud frame. The run loop's idle bookkeeping (`Quiet`)
//! asks for black 1.0–1.5 s after the last window; `Music::blank` sends it. Driven
//! here with a simulated clock and a loopback receiver standing in for the light.

use std::net::{Ipv4Addr, UdpSocket};
use std::time::Duration;

use gowl::audio::fft::N;
use gowl::audio::run::{Music, Quiet, QuietAction, Settings};
use gowl::config::Config;
use gowl::devices::{Device, SegSource};
use gowl::net::{Ports, Sender};
use gowl::runtime::Streams;

const MS: u64 = 1_000_000;

fn drain(rx: &UdpSocket, out: &mut Vec<Vec<u8>>) {
    let mut buf = [0u8; 2048];
    while let Ok(n) = rx.recv(&mut buf) {
        out.push(buf[..n].to_vec());
    }
}

#[test]
fn abrupt_stop_goes_black_within_two_seconds() {
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    rx.set_nonblocking(true).unwrap();
    let ports = Ports {
        scan: 1,
        reply: 2,
        control: rx.local_addr().unwrap().port(),
    };
    let dev = Device {
        ip: Ipv4Addr::LOCALHOST,
        mac: None,
        sku: None,
        alias: None,
        segments: 10,
        seg_source: SegSource::Default,
        reverse: false,
        offset: 0,
        layout: None,
        port: None,
    };
    // The exact datagram black encodes to, from a Sender of the same size.
    let black_dgram = {
        let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
        probe
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dst = match probe.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a,
            _ => unreachable!(),
        };
        let mut s = Sender::new(dst, 10);
        s.send_colors(&tx, &[[0, 0, 0]; 10]).unwrap();
        let mut b = [0u8; 2048];
        let n = probe.recv(&mut b).unwrap();
        b[..n].to_vec()
    };
    let streams = Streams::open(&[dev], None, ports, 3, 5600, false).unwrap();
    let settings = Settings::from_config(&Config::default()).unwrap();
    let mut music = Music::new(settings, streams);
    music.streams.razer(true);

    // 3 s of a loud 80 Hz tone, one window every 21.33 ms, rendered at ≤ 40 Hz.
    let t0 = 1_000 * MS * 1000; // arbitrary epoch, ns
    let mut quiet = Quiet::new(t0);
    let mut got = Vec::new();
    let mut samples = [0f32; N];
    let mut now = t0;
    let mut next_render = t0;
    let mut k = 0usize;
    let mut last = now;
    while now < t0 + 3_000 * MS {
        for (i, s) in samples.iter_mut().enumerate() {
            let t = (k * N + i) as f32 / 48_000.0;
            *s = 0.8 * (2.0 * std::f32::consts::PI * 80.0 * t).sin();
        }
        quiet.heard(now, false);
        last = now;
        if now >= next_render {
            music.tick(&samples, now / MS);
            next_render += 25 * MS;
        }
        assert!(quiet.deadline() > now, "nothing due while windows flow");
        k += 1;
        now = t0 + (k as u64 * N as u64 * 1_000_000_000) / 48_000;
    }
    drain(&rx, &mut got);
    let loud = got.iter().filter(|d| **d != black_dgram).count();
    assert!(
        loud >= 5,
        "only {loud} non-black frames while the tone played"
    );
    assert_ne!(
        got.last(),
        Some(&black_dgram),
        "still loud when the audio stops"
    );

    // The audio stops: no more windows. Timer wakeups happen at quiet's deadlines.
    let stopped = last;
    let mut blank_at = None;
    for _ in 0..10 {
        let dl = quiet.deadline().min(stopped + 5_000 * MS);
        match quiet.on_timer(dl) {
            QuietAction::Blank => {
                music.blank(dl / MS);
                blank_at = Some(dl);
                break;
            }
            QuietAction::Restart => panic!("restart before black"),
            QuietAction::None => {}
        }
    }
    let blank_at = blank_at.expect("never went black");
    let after = blank_at - stopped;
    assert!(
        (1_000 * MS..=1_600 * MS).contains(&after),
        "black {} ms after the last window",
        after / MS
    );
    let mut tail = Vec::new();
    drain(&rx, &mut tail);
    assert_eq!(
        tail,
        std::slice::from_ref(&black_dgram),
        "exactly one black frame"
    );
    assert!(quiet.blanked());
    // Nothing more until the restart deadline (30 s without sound).
    assert!(quiet.deadline() >= stopped + 30_000 * MS);
    // A second blank sends nothing (the gate sees black again).
    music.blank(blank_at / MS + 10);
    drain(&rx, &mut tail);
    assert_eq!(tail.len(), 1);
}

#[test]
fn quiet_restart_backs_off_and_resets_on_sound() {
    let s = 1_000 * MS;
    let mut q = Quiet::new(0);
    // Silent windows keep coming (sink running, nothing audible): restarts back off.
    let mut restarts = Vec::new();
    let mut now = 0;
    while now < 1_200 * s {
        q.heard(now, true);
        if now >= q.deadline() && q.on_timer(now) == QuietAction::Restart {
            q.opened(now);
            restarts.push(now / s);
        }
        now += s / 4;
    }
    let gaps: Vec<u64> = restarts.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(restarts.first(), Some(&30), "{restarts:?}");
    // Doubling 30 → 60 → 120 → 240 → 300 s (+ ≤ 1 s for the half-second grid).
    let want = [60u64, 120, 240, 300, 300];
    assert!(gaps.len() >= want.len(), "{restarts:?}");
    for (g, w) in gaps.iter().zip(want) {
        assert!((w..=w + 1).contains(g), "{restarts:?} gaps {gaps:?}");
    }
    // Sound resets the back-off.
    q.heard(now, false);
    assert_eq!(q.restart_due(), now - now % (s / 2) + s / 2 + 30 * s);
}
