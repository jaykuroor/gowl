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

/// Request one frame and dispatch until it is ready; process it.
fn one_frame(cap: &mut Screencopy, pipe: &mut Pipeline, now: u64) {
    assert!(cap.request_next().unwrap());
    loop {
        cap.flush().unwrap();
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
        let st = cap.dispatch().unwrap();
        assert!(!st.failed && !st.lost);
        if st.ready {
            let f = cap.frame().unwrap();
            pipe.on_frame(&f, now);
            pipe.on_timer(now);
            return;
        }
    }
}

#[test]
fn capture_to_send_allocates_nothing() {
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
    };
    let ports = Ports {
        scan: 1,
        reply: 2,
        control: port,
    };
    let streams = Streams::open(&[dev], None, ports, 3, 500).unwrap();
    let mut pipe = Pipeline::new(streams, &Config::default(), 0.28, 30);
    let g = Globals::with_conn(Conn::from_fd(client)).unwrap();
    assert_eq!(g.outputs.len(), 1);
    assert_eq!(g.outputs[0].name, "MOCK-1");
    let mut cap = Screencopy::new(g, Some("MOCK-1")).unwrap();
    let mut now = 1_000_000_000u64;
    for _ in 0..5 {
        one_frame(&mut cap, &mut pipe, now); // warm-up: buffer + zone tables
        now += 34_000_000;
    }

    // ---- steady state
    COUNT.with(|c| c.set(true));
    let before = ALLOCS.load(Ordering::Relaxed);
    for _ in 0..200 {
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
    assert_eq!(served.load(Ordering::SeqCst), 205);
    assert!(got > 150, "only {got} datagrams reached the receiver");
    assert_eq!(
        after - before,
        0,
        "capture→send allocated {} times in 200 frames",
        after - before
    );
}
