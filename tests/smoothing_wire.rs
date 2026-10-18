//! D-058: deferring smoothing steps that can't send (`Pipeline::lazy_ok`) must not
//! change anything on the wire. The same scripted screen — color changes, a change
//! below the threshold, a new frame in the middle of a fade, static spells long
//! enough for keepalives, pause/resume — runs through the real pipeline twice, with
//! timer steps (`lazy_ok = false`, the old behaviour) and with deferred steps, on a
//! simulated clock; every datagram (bytes and send time) must be identical, and the
//! deferred run must need fewer timer wakeups.

use std::net::{Ipv4Addr, UdpSocket};

use gowl::capture::{Format, Frame};
use gowl::config::Config;
use gowl::devices::{Device, SegSource};
use gowl::net::Ports;
use gowl::runtime::{Pipeline, Streams};

const MS: u64 = 1_000_000;

#[derive(Clone, Copy)]
enum Ev {
    /// A frame: left half, right half colors.
    Frame([u8; 3], [u8; 3]),
    Pause,
    Resume,
}

fn script() -> Vec<(u64, Ev)> {
    let red = [255, 0, 0];
    let blue = [0, 0, 255];
    let mut v = vec![(0, Ev::Frame(red, blue))];
    // the same picture a few more times while the fade runs
    for k in 1..5 {
        v.push((k * 33, Ev::Frame(red, blue)));
    }
    // static 3 s; then a change below the threshold
    v.push((3_200, Ev::Frame([253, 1, 0], blue)));
    // a big change, and another one in the middle of its fade
    v.push((5_000, Ev::Frame([0, 255, 0], [255, 255, 0])));
    v.push((5_120, Ev::Frame([0, 128, 255], [255, 255, 255])));
    // pause in the middle of a fade, resume later
    v.push((7_000, Ev::Frame([40, 40, 40], [200, 10, 90])));
    v.push((7_090, Ev::Pause));
    v.push((8_000, Ev::Resume));
    // slow drift: each step below the threshold, the sum above it
    for k in 0..20u64 {
        let c = (100 + 2 * k) as u8;
        v.push((10_000 + k * 400, Ev::Frame([c, c, c], [c, 0, 0])));
    }
    v
}

/// Returns (datagrams as (ms, bytes), timer wakeups).
fn run(
    lazy_ok: bool,
    smoothing: f64,
    threshold: u8,
    keepalive_ms: u32,
) -> (Vec<(u64, Vec<u8>)>, usize) {
    let (w, h) = (64u32, 36u32);
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    rx.set_nonblocking(true).unwrap();
    let dev = Device {
        ip: Ipv4Addr::LOCALHOST,
        mac: None,
        sku: None,
        alias: None,
        segments: 10,
        seg_source: SegSource::Config,
        reverse: false,
        offset: 0,
        layout: Some(vec![1, 7, 8, 8, 8, 9, 6, 3, 2, 2]),
        port: None,
    };
    let ports = Ports {
        scan: 1,
        reply: 2,
        control: rx.local_addr().unwrap().port(),
    };
    let streams = Streams::open(&[dev], None, ports, threshold, keepalive_ms, true).unwrap();
    let cfg = Config {
        smoothing,
        threshold,
        ..Config::default()
    };
    let mut pipe = Pipeline::new(streams, &cfg, 0.28, 30);
    pipe.lazy_ok = lazy_ok;
    pipe.streams.razer(true);
    let mut data = vec![0u8; (w * h * 4) as usize];
    let mut out = Vec::new();
    let mut timers = 0;
    let mut paused = false;
    let mut buf = [0u8; 512];
    let t0 = 1_000_000 * MS;
    let mut drain = |t: u64, out: &mut Vec<(u64, Vec<u8>)>| {
        while let Ok(n) = rx.recv(&mut buf) {
            out.push(((t - t0) / MS, buf[..n].to_vec()));
        }
    };
    drain(t0, &mut out);
    let mut evs = script().into_iter().peekable();
    let end = t0 + 20_000 * MS;
    let mut now = t0;
    loop {
        let te = evs.peek().map(|(ms, _)| t0 + ms * MS);
        let dl = if paused { None } else { pipe.deadline() };
        match (dl, te) {
            (Some(d), te) if te.is_none_or(|te| d < te) && d <= end => {
                now = now.max(d);
                timers += 1;
                pipe.on_timer(now);
            }
            (_, Some(te)) => {
                now = now.max(te);
                let (_, ev) = evs.next().unwrap();
                match ev {
                    Ev::Frame(l, r) => {
                        for (i, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                            let c = if (i as u32 % w) < w / 2 { l } else { r };
                            *px = [c[2], c[1], c[0], 0xff];
                        }
                        let f = Frame {
                            data: &data,
                            width: w,
                            height: h,
                            stride: w * 4,
                            format: Format::Xrgb8888,
                            transform: 0,
                            y_invert: false,
                            damage: &[],
                            full_damage: true,
                        };
                        pipe.on_frame(&f, now);
                    }
                    Ev::Pause => {
                        paused = true;
                        pipe.pause(now);
                        pipe.streams.off();
                    }
                    Ev::Resume => {
                        paused = false;
                        pipe.streams.razer(true);
                        pipe.resume(now);
                    }
                }
            }
            _ => break,
        }
        drain(now, &mut out);
    }
    (out, timers)
}

#[test]
fn deferred_smoothing_steps_change_nothing_on_the_wire() {
    let mut saved = 0;
    for smoothing in [0.05, 0.3, 0.55, 1.0] {
        for threshold in [1u8, 3, 8] {
            for keepalive in [0u32, 500, 5600] {
                let (old, t_old) = run(false, smoothing, threshold, keepalive);
                let (new, t_new) = run(true, smoothing, threshold, keepalive);
                assert!(old.len() > 10, "scenario sent only {}", old.len());
                assert_eq!(
                    old, new,
                    "wire differs: smoothing {smoothing} threshold {threshold} keepalive {keepalive}"
                );
                assert!(t_new <= t_old, "{t_new} > {t_old} timer wakeups");
                saved += t_old - t_new;
            }
        }
    }
    println!("timer wakeups saved over the 36 runs: {saved}");
    assert!(saved > 36, "deferral saved only {saved} wakeups");
}
