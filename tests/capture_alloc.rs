//! CLAUDE.md rule 4 for the whole screen hot path: Wayland event parsing →
//! screencopy frame cycle → zone sampling → color pipeline → razer encode → sendto
//! allocates nothing after warm-up. Runs against tests/mock_wl (a fake compositor on
//! a socketpair, its own thread — not counted) instead of niri, so it works in CI.

mod mock_wl;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gowl::capture::Capture;
use gowl::capture::screencopy::{Globals, Screencopy};
use gowl::capture::wire::Conn;
use gowl::config::Config;
use gowl::devices::{Device, SegSource};
use gowl::net::Ports;
use gowl::runtime::{Pipeline, Streams};

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
thread_local! {
    static COUNT: Cell<bool> = const { Cell::new(false) };
}

fn counted() {
    if COUNT.with(Cell::get) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: forwards unchanged to `System`; counting has no effect on the memory.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        counted();
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        counted();
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn socketpair() -> (OwnedFd, OwnedFd) {
    let mut sv = [0; 2];
    // SAFETY: socketpair fills two fds we then own.
    let r = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
            sv.as_mut_ptr(),
        )
    };
    assert_eq!(r, 0);
    // SAFETY: fresh fds.
    unsafe { (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1])) }
}

/// One frame the way `runtime::screen_loop` drives it (D-051): at the "timer" a
/// held (prepared) frame first reads its offers, then `request_next` copies; on
/// `ready` the next frame is prepared and flushed *before* the frame is processed.
fn one_frame(cap: &mut Screencopy, pipe: &mut Pipeline, now: u64) {
    if cap.held() {
        wait_readable(cap); // the offers were sent while we sampled the last frame
        let st = cap.dispatch().unwrap();
        assert!(!st.ready && !st.failed && !st.lost);
    }
    assert!(cap.request_next().unwrap());
    loop {
        cap.flush().unwrap();
        wait_readable(cap);
        let st = cap.dispatch().unwrap();
        assert!(!st.failed && !st.lost);
        if st.ready {
            assert!(cap.prepare().unwrap());
            cap.flush().unwrap();
            let f = cap.frame().unwrap();
            pipe.on_frame(&f, now);
            pipe.on_timer(now);
            return;
        }
    }
}

fn wait_readable(cap: &Screencopy) {
    let mut p = libc::pollfd {
        fd: cap.fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    assert_eq!(
        unsafe { libc::poll(&mut p, 1, 5000) },
        1,
        "mock never answered"
    );
}

/// D-051: `prepare` negotiates the next frame without copying; the last frame's
/// pixels and damage stay readable until `request_next`, which copies at once.
#[test]
fn prepare_holds_the_copy_and_keeps_the_last_frame() {
    let (client, server) = socketpair();
    let served = Arc::new(AtomicUsize::new(0));
    let opts = mock_wl::Opts {
        width: 64,
        height: 32,
        transform: 0,
        changing: usize::MAX,
    };
    let s2 = served.clone();
    let srv = std::thread::spawn(move || mock_wl::serve(server, opts, s2));
    let g = Globals::with_conn(Conn::from_fd(client)).unwrap();
    let mut cap = Screencopy::new(g, None).unwrap();
    let px = |cap: &Screencopy| {
        let f = cap.frame().expect("frame readable");
        assert!(!f.damage.is_empty() || f.full_damage);
        [f.data[2], f.data[1], f.data[0]]
    };
    // Frame 0 the ordinary way.
    assert!(cap.request_next().unwrap());
    loop {
        cap.flush().unwrap();
        wait_readable(&cap);
        if cap.dispatch().unwrap().ready {
            break;
        }
    }
    assert_eq!(px(&cap), mock_wl::color_of(0));
    for k in 1..4 {
        assert!(cap.prepare().unwrap());
        assert!(cap.held());
        cap.flush().unwrap();
        wait_readable(&cap); // offers (buffer, buffer_done) arrive …
        let st = cap.dispatch().unwrap();
        assert!(!st.ready && !st.failed, "no copy before request_next");
        assert!(cap.held());
        assert_eq!(px(&cap), mock_wl::color_of(k - 1), "last frame intact");
        assert_eq!(served.load(Ordering::SeqCst), k);
        assert!(cap.request_next().unwrap()); // … and the copy goes out right away
        assert!(!cap.held());
        loop {
            cap.flush().unwrap();
            wait_readable(&cap);
            if cap.dispatch().unwrap().ready {
                break;
            }
        }
        assert_eq!(px(&cap), mock_wl::color_of(k));
    }
    // Cancel while held: the frame is dropped, nothing is copied.
    assert!(cap.prepare().unwrap());
    cap.cancel().unwrap();
    assert!(!cap.held());
    drop(cap);
    srv.join().unwrap();
    assert_eq!(served.load(Ordering::SeqCst), 4);
}

/// Warm up for `warm` frames, then count allocations over `frames` more.
/// Returns (allocations, datagrams received, frames served).
fn run(warm: usize, frames: usize) -> (usize, usize, usize) {
    let (client, server) = socketpair();
    let served = Arc::new(AtomicUsize::new(0));
    let opts = mock_wl::Opts {
        width: 640,
        height: 360,
        transform: 0,
        changing: usize::MAX,
    };
    let s2 = served.clone();
    let srv = std::thread::spawn(move || mock_wl::serve(server, opts, s2));

    // ---- setup (allocation allowed)
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    rx.set_nonblocking(true).unwrap();
    let port = rx.local_addr().unwrap().port();
    let dev = Device {
        ip: Ipv4Addr::LOCALHOST,
        mac: None,
        sku: None,
        alias: None,
        segments: 10,
        seg_source: SegSource::Default,
        reverse: true,
        offset: 2,
        layout: None,
        port: None,
    };
    let ports = Ports {
        scan: 1,
        reply: 2,
        control: port,
    };
    let streams = Streams::open(&[dev], None, ports, 3, 500, true).unwrap();
    let mut pipe = Pipeline::new(streams, &Config::default(), 0.28, 30);
    let g = Globals::with_conn(Conn::from_fd(client)).unwrap();
    assert_eq!(g.outputs.len(), 1);
    assert_eq!(g.outputs[0].name, "MOCK-1");
    let mut cap = Screencopy::new(g, Some("MOCK-1")).unwrap();
    let mut now = 1_000_000_000u64;
    for _ in 0..warm {
        one_frame(&mut cap, &mut pipe, now); // warm-up: buffer + zone tables
        now += 34_000_000;
    }

    // ---- steady state
    COUNT.with(|c| c.set(true));
    let before = ALLOCS.load(Ordering::Relaxed);
    for _ in 0..frames {
        one_frame(&mut cap, &mut pipe, now);
        now += 34_000_000;
    }
    let after = ALLOCS.load(Ordering::Relaxed);
    COUNT.with(|c| c.set(false));

    let mut buf = [0u8; 2048];
    let mut got = 0;
    while rx.recv(&mut buf).is_ok() {
        got += 1;
    }
    drop(cap);
    srv.join().unwrap();
    (after - before, got, served.load(Ordering::SeqCst))
}

#[test]
fn capture_to_send_allocates_nothing() {
    let (allocs, got, served) = run(5, 200);
    assert_eq!(served, 205);
    assert!(got > 150, "only {got} datagrams reached the receiver");
    assert_eq!(
        allocs, 0,
        "capture→send allocated {allocs} times in 200 frames"
    );
}

/// BUDGETS.md "Allocation count": 300 synthetic frames through capture → sample →
/// process → encode → send, counted from frame 10.
/// `cargo test --release --features count-alloc -- --ignored alloc_steady_state`
#[test]
#[ignore]
fn alloc_steady_state() {
    let (allocs, got, served) = run(10, 290);
    assert_eq!(served, 300);
    assert!(got > 200, "only {got} datagrams reached the receiver");
    assert_eq!(allocs, 0, "{allocs} allocations after frame 10");
}
