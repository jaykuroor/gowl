//! CLAUDE.md rule 4: the hot path (sample → process → encode → send) allocates
//! nothing after setup. Counting global allocator; one test in this crate.

use std::alloc::{GlobalAlloc, Layout, System};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};

use gowl::color::{self, Rgb, Rgb8Frame, SendGate, Smoother};
use gowl::net::{self, Sender};

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

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

#[test]
fn hot_path_allocates_nothing() {
    // ---- setup (allocation allowed)
    let (w, h, n) = (640usize, 360usize, 10usize);
    let mut frame = vec![0u8; w * h * 3];
    let rects = color::generate_screen_mapping(n, w as f64 / h as f64, 0.28);
    let zones = color::build_zones(&rects, w as u32, h as u32);
    let mut smoother = Smoother::new(0.55, n);
    let mut gate = SendGate::new(3, 500);
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    let dst = match rx.local_addr().unwrap() {
        std::net::SocketAddr::V4(a) => a,
        _ => unreachable!(),
    };
    let ifaces = net::interfaces().unwrap();
    let lo = net::select_for(&ifaces, Ipv4Addr::LOCALHOST, None)
        .unwrap()
        .iface;
    let sock = net::bound_socket(&lo, None).unwrap();
    let mut sender = Sender::new(SocketAddrV4::new(*dst.ip(), dst.port()), n);
    let (sat, bri) = (color::q16(1.15), color::q16(0.75));
    let mut zone_rgb: [Rgb; 255] = [[0; 3]; 255];
    let mut smooth: [Rgb; 255] = [[0; 3]; 255];
    let mut seg: [Rgb; 255] = [[0; 3]; 255];

    let mut step = |t: u64, frame: &[u8]| {
        let src = Rgb8Frame {
            data: frame,
            width: w as u32,
            stride: w * 3,
        };
        color::average_zones(&zones, &src, true, &mut zone_rgb[..n]);
        color::saturate(&mut zone_rgb[..n], sat);
        color::scale(&mut zone_rgb[..n], bri);
        smoother.update(&zone_rgb[..n], &mut smooth[..n]);
        color::reorder(&smooth[..n], true, 3, &mut seg[..n]);
        if gate.needs_send(&seg[..n], t) {
            sender.send_colors(&sock, &seg[..n]).unwrap();
            gate.mark_sent(&seg[..n], t);
        }
    };
    step(0, &frame); // warm-up

    // ---- steady state
    let before = ALLOCS.load(Ordering::Relaxed);
    for t in 1..200u64 {
        let len = frame.len();
        frame[(t as usize * 7919) % len] ^= 0xFF;
        frame
            .iter_mut()
            .step_by(97)
            .for_each(|b| *b = b.wrapping_add(t as u8));
        step(t * 33, &frame);
    }
    let after = ALLOCS.load(Ordering::Relaxed);
    assert_eq!(
        after - before,
        0,
        "hot path allocated {} times",
        after - before
    );
}
