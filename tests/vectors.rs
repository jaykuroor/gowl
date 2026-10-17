//! Golden vectors from legacy code (tests/vectors/*.json, CLAUDE.md rule 8).
//! Protocol bytes exact; color math ±1 per channel; mapping within 1e-9.

mod common;

use common::{Json, vector};
use gowl::color::{self, Rect, Rgb, Rgb8Frame, SendGate, Smoother};
use gowl::net::Sender;
use gowl::proto;
use std::net::{Ipv4Addr, SocketAddrV4};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn assert_close(got: &[Rgb], want: &[Rgb], tol: u8, what: &str) -> u8 {
    assert_eq!(got.len(), want.len(), "{what}: length");
    let mut worst = 0;
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        for k in 0..3 {
            let d = g[k].abs_diff(w[k]);
            worst = worst.max(d);
            assert!(d <= tol, "{what}: zone {i} got {g:?} want {w:?}");
        }
    }
    worst
}

/// Legacy json.dumps only adds a space after ':' and ','; strings never contain spaces.
fn compact(legacy: &str) -> String {
    legacy.replace(": ", ":").replace(", ", ",")
}

#[test]
fn json_reader_roundtrip() {
    let v = common::parse(r#" {"a": [1, -2.5e1, true, null, "x\"yA"], "b": {}} "#);
    assert_eq!(v.get("a").arr()[1].f64(), -25.0);
    assert_eq!(v.get("a").arr()[4].str(), "x\"yA");
    assert!(v.get("a").arr()[3].is_null());
}

#[test]
fn protocol_razer_mode() {
    let v = vector("protocol.json");
    for (key, on) in [("on", true), ("off", false)] {
        let m = v.get("razer_mode").get(key);
        let f = proto::razer_mode_frame(on);
        assert_eq!(hex(&f), m.get("bytes_hex").str());
        let mut b = [0u8; 8];
        let n = proto::b64_encode(&f, &mut b);
        assert_eq!(&b[..n], m.get("pt").str().as_bytes());
    }
}

#[test]
fn protocol_segment_frames() {
    let v = vector("protocol.json");
    let frames = v.get("segment_frames").arr();
    assert_eq!(frames.len(), 9);
    for f in frames {
        let colors = f.get("colors").rgb_list();
        assert_eq!(colors.len() as u64, f.get("count").u64());
        let mut raw = [0u8; proto::MAX_FRAME];
        let n = proto::razer_color_frame(&colors, &mut raw);
        assert_eq!(
            hex(&raw[..n]),
            f.get("bytes_hex").str(),
            "N={}",
            colors.len()
        );
        let mut b64 = [0u8; proto::b64_len(proto::MAX_FRAME)];
        let m = proto::b64_encode(&raw[..n], &mut b64);
        assert_eq!(
            &b64[..m],
            f.get("pt").str().as_bytes(),
            "N={}",
            colors.len()
        );

        // The preallocated Sender datagram carries the same pt.
        let mut s = Sender::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9), colors.len());
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        s.send_colors(&sock, &colors).unwrap();
        let want = format!(
            r#"{{"msg":{{"cmd":"razer","data":{{"pt":"{}"}}}}}}"#,
            f.get("pt").str()
        );
        assert_eq!(std::str::from_utf8(s.datagram()).unwrap(), want);
    }
}

#[test]
fn protocol_legacy_commands() {
    let v = vector("protocol.json");
    let c = v.get("legacy_commands");
    let mut b = [0u8; proto::MSG_MAX];
    let check = |name: &str, n: usize, b: &[u8]| {
        assert_eq!(
            std::str::from_utf8(&b[..n]).unwrap(),
            compact(c.get(name).str()),
            "{name}"
        );
    };
    assert_eq!(proto::SCAN_MSG, c.get("scan").str().as_bytes());
    let n = proto::turn(true, &mut b);
    check("turn_on", n, &b);
    let n = proto::turn(false, &mut b);
    check("turn_off", n, &b);
    let n = proto::brightness(42, &mut b);
    check("brightness_42", n, &b);
    let n = proto::colorwc([255, 128, 0], 0, &mut b);
    check("color_ff8000", n, &b);
    let n = proto::status_query(true, &mut b);
    check("dev_status", n, &b);
    let n = proto::status_query(false, &mut b);
    check("status", n, &b);
    let n = proto::razer_msg(&proto::razer_mode_frame(true), &mut b);
    check("razer_on", n, &b);
    let ten = &v.get("segment_frames").arr()[2];
    assert_eq!(ten.get("count").u64(), 10);
    let mut raw = [0u8; proto::MAX_FRAME];
    let fl = proto::razer_color_frame(&ten.get("colors").rgb_list(), &mut raw);
    let n = proto::razer_msg(&raw[..fl], &mut b);
    check("razer_frame_10", n, &b);
    assert_eq!(n, 92, "BUDGETS: 92 bytes per 10-segment datagram");
}

#[test]
fn protocol_ports_and_kelvin() {
    let v = vector("protocol.json");
    let p = v.get("ports");
    assert_eq!(p.get("scan_dst").u64(), u64::from(proto::SCAN_PORT));
    assert_eq!(p.get("reply_listen").u64(), u64::from(proto::REPLY_PORT));
    assert_eq!(p.get("control_dst").u64(), u64::from(proto::CONTROL_PORT));
    assert_eq!(
        p.get("multicast").str(),
        Ipv4Addr::from(proto::MULTICAST).to_string()
    );
    for (k, rgb) in v.get("kelvin_to_rgb").obj() {
        let want = [
            rgb.arr()[0].u64() as u8,
            rgb.arr()[1].u64() as u8,
            rgb.arr()[2].u64() as u8,
        ];
        assert_eq!(proto::kelvin_to_rgb(k.parse().unwrap()), want, "{k} K");
    }
}

#[test]
fn led_mapping_all_cases() {
    let v = vector("led_mapping.json");
    let cases = v.get("cases").arr();
    assert_eq!(cases.len(), 72);
    for c in cases {
        let n = c.get("zones").u64() as usize;
        let got =
            color::generate_screen_mapping(n, c.get("aspect_ratio").f64(), c.get("depth").f64());
        let want = c.get("rects").arr();
        assert_eq!(got.len(), want.len(), "zones {n}");
        for (g, w) in got.iter().zip(want) {
            let w = Rect {
                x: w.get("x").f64(),
                y: w.get("y").f64(),
                w: w.get("w").f64(),
                h: w.get("h").f64(),
            };
            for (a, b) in [(g.x, w.x), (g.y, w.y), (g.w, w.w), (g.h, w.h)] {
                assert!(
                    (a - b).abs() <= 1e-9,
                    "zones {n} {} depth {}: {g:?} vs {w:?}",
                    c.get("aspect").str(),
                    c.get("depth").f64()
                );
            }
        }
    }
}

#[test]
fn processing_zone_averages() {
    let v = vector("processing.json");
    assert_eq!(
        v.get("max_samples_per_axis").u64(),
        u64::from(color::MAX_SAMPLES_PER_AXIS)
    );
    let mut frames: Vec<((usize, usize), Vec<u8>)> = Vec::new();
    let mut worst = 0;
    let cases = v.get("average_zone_colors").arr();
    assert_eq!(cases.len(), 12);
    for c in cases {
        let (h, w) = (
            c.get("height").u64() as usize,
            c.get("width").u64() as usize,
        );
        if !frames.iter().any(|(k, _)| *k == (h, w)) {
            frames.push(((h, w), common::synthetic_frame(h, w)));
        }
        let data = &frames.iter().find(|(k, _)| *k == (h, w)).unwrap().1;
        let n = c.get("zones").u64() as usize;
        let gamma = c.get("gamma_correct").bool();
        // Legacy maps with aspect = w/h and the default depth.
        let rects =
            color::generate_screen_mapping(n, w as f64 / h as f64, color::DEFAULT_CAPTURE_DEPTH);
        let zones = color::build_zones(&rects, w as u32, h as u32);
        let src = Rgb8Frame {
            data,
            width: w as u32,
            stride: w * 3,
        };
        let mut out = vec![[0u8; 3]; n];
        color::average_zones(&zones, &src, gamma, &mut out);
        let what = format!("{h}x{w} zones {n} gamma {gamma}");
        worst = worst.max(assert_close(&out, &c.get("colors").rgb_list(), 1, &what));
    }
    eprintln!("zone averages: worst channel error {worst}");
}

#[test]
fn processing_saturation_brightness() {
    let v = vector("processing.json");
    let s = v.get("saturation_1_15");
    let mut c = s.get("in").rgb_list();
    color::saturate(&mut c, color::q16(1.15));
    let w1 = assert_close(&c, &s.get("out").rgb_list(), 1, "saturation 1.15");
    let b = v.get("brightness_0_75");
    let mut c = b.get("in").rgb_list();
    color::scale(&mut c, color::q16(0.75));
    let w2 = assert_close(&c, &b.get("out").rgb_list(), 1, "brightness 0.75");
    eprintln!("saturation worst {w1}, brightness worst {w2}");
}

#[test]
fn processing_resample() {
    let v = vector("processing.json");
    for r in v.get("resample").arr() {
        let src = r.get("in").rgb_list();
        let mut out = vec![[0u8; 3]; r.get("count").u64() as usize];
        color::resample(&src, &mut out);
        assert_close(
            &out,
            &r.get("out").rgb_list(),
            1,
            &format!("resample to {}", out.len()),
        );
    }
}

#[test]
fn processing_smoother() {
    let v = vector("processing.json");
    let s = v.get("smoother_alpha_0_55");
    let mut sm = Smoother::new(0.55, 3);
    for (i, (f, want)) in s.get("in").arr().iter().zip(s.get("out").arr()).enumerate() {
        let f = f.rgb_list();
        let mut out = vec![[0u8; 3]; f.len()];
        sm.update(&f, &mut out);
        assert_close(&out, &want.rgb_list(), 1, &format!("smoother frame {i}"));
    }
}

#[test]
fn processing_frame_needs_send() {
    let v = vector("processing.json");
    for (i, c) in v.get("frame_needs_send").arr().iter().enumerate() {
        let mut g = SendGate::new(c.get("threshold").u64() as u8, 500);
        let ms = |j: &Json| (j.f64() * 1000.0).round() as u64;
        if !c.get("prev").is_null() {
            let at = c
                .opt("last_sent_at")
                .filter(|j| !j.is_null())
                .map(ms)
                .expect("prev implies last_sent_at");
            g.mark_sent(&c.get("prev").rgb_list(), at);
        }
        let got = g.needs_send(&c.get("cur").rgb_list(), ms(c.get("now")));
        assert_eq!(got, c.get("expect").bool(), "case {i}");
    }
}
