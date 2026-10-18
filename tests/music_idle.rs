//! D-054 / D-063: loud music that stops abruptly (song paused, sink suspended or
//! moved — the passive stream then delivers no more windows) must not leave the
//! light frozen on the last loud frame. The run loop's idle bookkeeping (`Quiet`)
//! says "audio stopped" 1.5–2.0 s after the last window; `Music` then fades the
//! last output to black over `music_fade_ms` (ease-out, timer steps, normal gate),
//! or blanks at once with `music_fade_ms = 0`. Audio that returns mid-fade cancels
//! the fade. Driven here with a simulated clock and a loopback receiver standing in
//! for the light.

use std::net::{Ipv4Addr, UdpSocket};

use gowl::audio::fft::N;
use gowl::audio::run::{FADE_STEP_NS, Music, Quiet, QuietAction, Settings, fade_level_q16};
use gowl::config::Config;
use gowl::devices::{Device, SegSource};
use gowl::net::Ports;
use gowl::runtime::Streams;

const MS: u64 = 1_000_000;
const SEGS: usize = 10;

/// A frame's colors and the (simulated) time it went out.
type Timed = (u64, Vec<[u8; 3]>);

/// Colors of a razer color frame datagram, None for anything else (razer ON/OFF).
fn colors(dgram: &[u8]) -> Option<Vec<[u8; 3]>> {
    let s = std::str::from_utf8(dgram).ok()?;
    let pt = s.split("\"pt\":\"").nth(1)?.split('"').next()?;
    let mut raw = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in pt.bytes().filter(|&c| c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            raw.push((acc >> bits) as u8);
        }
    }
    // BB LEN_HI LEN_LO B0 01 N (R G B)×N XOR
    if raw.len() < 7 || raw[3] != 0xB0 {
        return None;
    }
    let n = usize::from(raw[5]);
    Some(
        raw[6..6 + 3 * n]
            .chunks(3)
            .map(|c| [c[0], c[1], c[2]])
            .collect(),
    )
}

struct Rig {
    rx: UdpSocket,
    music: Music,
    quiet: Quiet,
    now: u64,
    k: usize,
    next_render: u64,
    /// Time of the last window played.
    heard: u64,
}

impl Rig {
    fn new(fade_ms: u32) -> Rig {
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
            segments: SEGS as u8,
            seg_source: SegSource::Default,
            reverse: false,
            offset: 0,
            layout: None,
            port: None,
        };
        let streams = Streams::open(&[dev], None, ports, 3, 5600, false).unwrap();
        let cfg = Config {
            music_fade_ms: fade_ms,
            ..Config::default()
        };
        let mut music = Music::new(Settings::from_config(&cfg).unwrap(), streams);
        music.streams.razer(true);
        let t0 = 1_000 * MS * 1000; // arbitrary epoch, ns
        Rig {
            rx,
            music,
            quiet: Quiet::new(t0),
            now: t0,
            k: 0,
            next_render: t0,
            heard: t0,
        }
    }

    fn drain(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 2048];
        while let Ok(n) = self.rx.recv(&mut buf) {
            out.push(buf[..n].to_vec());
        }
        out
    }

    /// `ms` of a loud 80 Hz tone, one window every 21.33 ms, rendered at ≤ 40 Hz
    /// (what the run loop does on a window, including cancelling a fade).
    fn play(&mut self, ms: u64) {
        let mut samples = [0f32; N];
        let end = self.now + ms * MS;
        while self.now < end {
            for (i, s) in samples.iter_mut().enumerate() {
                let t = (self.k * N + i) as f32 / 48_000.0;
                *s = 0.8 * (2.0 * std::f32::consts::PI * 80.0 * t).sin();
            }
            self.quiet.heard(self.now, false);
            self.heard = self.now;
            if self.music.fading() {
                self.music.cancel_fade();
            }
            if self.now >= self.next_render {
                self.music.tick(&samples, self.now / MS);
                self.next_render = self.now + 25 * MS;
            }
            assert!(
                self.quiet.deadline() > self.now,
                "nothing due while windows flow"
            );
            self.k += 1;
            self.now += (N as u64 * 1_000_000_000) / 48_000;
        }
    }

    /// The audio stopped: run timer wakeups (quiet + fade deadlines, as the loop
    /// does) until `until`; returns (fade start, [(time, frame colors)]).
    fn silence(&mut self, until: u64) -> (Option<u64>, Vec<Timed>) {
        let mut started = None;
        let mut frames = Vec::new();
        loop {
            let dl = self
                .music
                .fade_due()
                .map_or(self.quiet.deadline(), |f| f.min(self.quiet.deadline()));
            if dl > until {
                break;
            }
            self.now = dl;
            if self.music.fade_due().is_some_and(|t| t <= dl) {
                self.music.fade_step(dl);
            }
            match self.quiet.on_timer(dl) {
                QuietAction::Blank => {
                    started = Some(dl);
                    self.music.fade_start(dl);
                }
                QuietAction::Restart => panic!("restart while fading"),
                QuietAction::None => {}
            }
            for d in self.drain() {
                let c = colors(&d).unwrap_or_else(|| {
                    panic!("not a color frame: {}", String::from_utf8_lossy(&d))
                });
                frames.push((dl, c));
            }
        }
        self.now = until;
        (started, frames)
    }
}

fn brightest(c: &[[u8; 3]]) -> u8 {
    c.iter().flat_map(|p| p.iter().copied()).max().unwrap_or(0)
}

#[test]
fn ease_out_level_is_monotone_and_ends_at_zero() {
    let d = 1_000_000;
    assert_eq!(fade_level_q16(0, d), 65536);
    assert_eq!(fade_level_q16(d / 2, d), 16384); // (1/2)² — past half way already dim
    assert_eq!(fade_level_q16(d, d), 0);
    assert_eq!(fade_level_q16(5, 0), 0);
    let mut prev = u32::MAX;
    for e in (0..=d).step_by(1000) {
        let l = fade_level_q16(e, d);
        assert!(l <= prev);
        prev = l;
    }
}

#[test]
fn abrupt_stop_fades_monotonically_to_black() {
    let mut r = Rig::new(1000);
    r.play(3000);
    let loud: Vec<_> = r.drain().iter().filter_map(|d| colors(d)).collect();
    assert!(
        loud.len() >= 5,
        "only {} frames while the tone played",
        loud.len()
    );
    let last_loud = loud.last().unwrap().clone();
    assert!(
        brightest(&last_loud) > 20,
        "still loud when the audio stops"
    );

    let stopped = r.heard;
    let (started, frames) = r.silence(stopped + 6_000 * MS);
    let started = started.expect("never started fading");
    let after = started - stopped;
    assert!(
        (1_500 * MS..=2_100 * MS).contains(&after),
        "fade began {} ms after the last window",
        after / MS
    );
    // Steps down, channel by channel, never up; the last frame is exact black and
    // arrives within the margin + the fade time (+ one step).
    let mut prev = last_loud.clone();
    for (t, f) in &frames {
        for (a, b) in f.iter().zip(&prev) {
            assert!(
                (0..3).all(|k| a[k] <= b[k]),
                "brighter at {} ms: {f:?} after {prev:?}",
                (t - stopped) / MS
            );
        }
        prev = f.clone();
    }
    let (t_black, black) = frames.last().expect("fade frames");
    assert!(black.iter().all(|c| *c == [0, 0, 0]), "{black:?}");
    let to_black = t_black - stopped;
    assert!(
        to_black <= 2_100 * MS + 1_000 * MS + FADE_STEP_NS,
        "black {} ms after the last window",
        to_black / MS
    );
    assert!(
        t_black - started >= 950 * MS,
        "faded in {} ms",
        (t_black - started) / MS
    );
    // A gradual dim, not a drop: many steps, the first one only a little darker.
    assert!(frames.len() >= 10, "{} steps", frames.len());
    assert!(
        brightest(&frames[0].1) * 10 >= brightest(&last_loud) * 7,
        "first step {:?} vs {:?}",
        frames[0].1,
        last_loud
    );
    // Then quiet: no fade deadline, nothing until the 30 s restart.
    assert!(!r.music.fading());
    assert!(r.quiet.blanked());
    assert!(r.quiet.deadline() >= stopped + 30_000 * MS);
    // A later blank sends nothing (the gate sees black again).
    r.music.blank(r.now / MS + 10);
    assert!(r.drain().is_empty());
}

#[test]
fn audio_returning_mid_fade_cancels_it() {
    let mut r = Rig::new(1000);
    r.play(3000);
    r.drain();
    let stopped = r.heard;
    // Silence until ~300 ms into the fade.
    let (started, frames) = r.silence(stopped + 2_400 * MS);
    let started = started.expect("fade started");
    assert!(r.music.fading());
    assert!(!frames.is_empty());
    let dimmed = frames.last().unwrap().1.clone();
    assert!(brightest(&dimmed) > 0);
    // The music is back: the fade stops at once and the light follows the audio,
    // continuing from the dimmed output (no jump back to the pre-fade frame first).
    r.play(200);
    assert!(!r.music.fading(), "fade still running");
    assert_eq!(r.music.fade_due(), None);
    let back: Vec<_> = r.drain().iter().filter_map(|d| colors(d)).collect();
    assert!(!back.is_empty(), "nothing sent after the audio came back");
    assert!(back.iter().all(|f| brightest(f) > 0), "went black anyway");
    assert!(
        started > stopped && r.quiet.deadline() > r.now,
        "nothing due while audio plays"
    );
}

#[test]
fn fade_zero_is_the_old_instant_black() {
    let mut r = Rig::new(0);
    r.play(3000);
    r.drain();
    let stopped = r.heard;
    let (started, frames) = r.silence(stopped + 6_000 * MS);
    let at = started.expect("black");
    assert!((1_500 * MS..=2_100 * MS).contains(&(at - stopped)));
    assert_eq!(frames.len(), 1, "exactly one frame");
    assert!(frames[0].1.iter().all(|c| *c == [0, 0, 0]));
    assert_eq!(frames[0].0, at);
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
