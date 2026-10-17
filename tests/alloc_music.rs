//! CLAUDE.md rule 4 for music mode: after warm-up, window → FFT → bands → AutoGain
//! → render → EMA → brightness → reorder → gate → encode → send allocates nothing,
//! from the very first window, for every reaction (incl. the Auto Director) and
//! palette. Counting global allocator; one test in this binary. Not covered:
//! `Capture::read` / `stream_loop` (libpulse peek + memcpy into a fixed buffer; Strings
//! only on error paths) — they need a live sound server.

use std::alloc::{GlobalAlloc, Layout, System};
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};

use gowl::audio::fft::N;
use gowl::audio::render::{Palette, Reaction};
use gowl::audio::run::{Music, Settings};
use gowl::config::Config;
use gowl::devices::{Device, SegSource};
use gowl::net::Ports;
use gowl::runtime::Streams;

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards unchanged to `System`; the counter is a relaxed
// atomic with no effect on the returned memory.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Deterministic "music": bass thump + mid tone + hiss, with silent gaps.
fn window(t: usize, out: &mut [f32; N]) {
    let mut seed = (t as u32).wrapping_mul(2_654_435_761) | 1;
    for (i, o) in out.iter_mut().enumerate() {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let s = (t * N + i) as f32 / 48_000.0;
        let beat = if t % 20 < 4 { 0.6 } else { 0.1 };
        let v = beat * (2.0 * std::f32::consts::PI * 70.0 * s).sin()
            + 0.15 * (2.0 * std::f32::consts::PI * 900.0 * s).sin()
            + 0.02 * (seed as f32 / u32::MAX as f32 - 0.5);
        *o = if t % 50 >= 45 { 0.0 } else { v };
    }
}

#[test]
fn music_hot_path_allocates_nothing() {
    // ---- setup (allocation allowed)
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    rx.set_nonblocking(true).unwrap();
    let ports = Ports {
        scan: 1,
        reply: 2,
        control: rx.local_addr().unwrap().port(),
    };
    let dev = |segments: u8, reverse: bool, offset: u16| Device {
        ip: Ipv4Addr::LOCALHOST,
        mac: None,
        sku: None,
        alias: None,
        segments,
        seg_source: SegSource::Default,
        reverse,
        offset,
        layout: None,
    };
    let base = Settings::from_config(&Config::default()).unwrap();
    let mut musics: Vec<Music> = Vec::new();
    for (_, reaction) in Reaction::ALL {
        for (_, palette) in Palette::ALL {
            let s = Settings {
                reaction,
                palette,
                ..base
            };
            // Two lights, different segment counts and orientation (same receiver).
            let streams = Streams::open(
                &[dev(10, false, 0), dev(14, true, 3)],
                None,
                ports,
                s.threshold,
                s.keepalive_ms as u32,
                false,
            )
            .unwrap();
            musics.push(Music::new(s, streams));
        }
    }
    let mut buf = [0f32; N];
    let mut drain = [0u8; 2048];
    let mut sent = 0usize;

    // ---- from the first window on: 400 windows (~10 s at 40 renders/s) per combination
    let before = ALLOCS.load(Ordering::Relaxed);
    for t in 0..400usize {
        window(t, &mut buf);
        for m in &mut musics {
            sent += m.tick(&buf, t as u64 * 25);
        }
        while rx.recv(&mut drain).is_ok() {}
    }
    let after = ALLOCS.load(Ordering::Relaxed);
    let combos = musics.len();
    drop(musics);
    assert!(
        sent > combos * 2 * 100,
        "only {sent} datagrams for {combos} combinations"
    );
    assert_eq!(
        after - before,
        0,
        "music hot path allocated {} times",
        after - before
    );
    eprintln!(
        "music hot path: {sent} datagrams over {combos} reaction×palette combinations, 0 allocations"
    );
}
