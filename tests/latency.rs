//! BUDGETS.md "frame ready → sendto returns: p99 ≤ 1 ms". Times `Pipeline::on_frame`
//! (damage → sample → saturation/brightness → EMA → reorder → gate → encode → sendto)
//! on a 3440x1440 XRGB8888 frame like the human's output, with the calibrated
//! 10-segment layout, every zone dirty and new pixels each frame. The buffer is
//! rewritten between frames (outside the timed part), so the sampled cache lines are
//! as cold as after a compositor copy.
//! `cargo test --release --test latency -- --ignored --nocapture`

use std::net::{Ipv4Addr, UdpSocket};
use std::time::Instant;

use gowl::capture::{DamageRect, Format, Frame};
use gowl::config::Config;
use gowl::devices::{Device, SegSource};
use gowl::net::Ports;
use gowl::runtime::{Pipeline, Streams};

#[test]
#[ignore]
fn frame_to_sendto_latency() {
    let (w, h) = (3440u32, 1440u32);
    let stride = w * 4;
    let mut data = vec![0u8; (stride * h) as usize];
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
    let streams = Streams::open(&[dev], None, ports, 3, 5600, true).unwrap();
    let mut pipe = Pipeline::new(streams, &Config::default(), 0.28, 30);
    let damage = [DamageRect { x: 0, y: 0, w, h }];
    let mut times = Vec::with_capacity(300);
    let mut now = 1_000_000_000u64;
    let mut sent = 0;
    let mut buf = [0u8; 512];
    for k in 0..320u32 {
        // New content every frame: a moving gradient, so every zone changes.
        let v = (k.wrapping_mul(2654435761) >> 8) as u8;
        for (i, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = (i as u32 % w) as u8;
            px.copy_from_slice(&[x ^ v, v, x.wrapping_add(v), 0xff]);
        }
        let f = Frame {
            data: &data,
            width: w,
            height: h,
            stride,
            format: Format::Xrgb8888,
            transform: 0,
            y_invert: false,
            damage: &damage,
            full_damage: false,
        };
        let t = Instant::now();
        pipe.on_frame(&f, now);
        let dt = t.elapsed();
        if k >= 20 {
            times.push(dt.as_nanos() as u64);
        }
        now += 34_000_000;
        while rx.recv(&mut buf).is_ok() {
            sent += 1;
        }
    }
    times.sort_unstable();
    let p = |q: f64| times[((times.len() - 1) as f64 * q) as usize] as f64 / 1000.0;
    println!(
        "on_frame 3440x1440, 10 zones all dirty: p50 {:.1} µs  p99 {:.1} µs  max {:.1} µs  ({} datagrams)",
        p(0.5),
        p(0.99),
        p(1.0),
        sent
    );
    assert!(sent > 200, "only {sent} datagrams");
    assert!(p(0.99) < 1000.0, "p99 {} µs over the 1 ms budget", p(0.99));
}
