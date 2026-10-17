//! A tiny fake Wayland compositor for hardware-free capture tests: advertises
//! wl_shm (XRGB8888), one wl_output and zwlr_screencopy_manager_v1 v3, and serves
//! screencopy frames by painting a solid color into the client's SHM buffer.
//! After `changing` frames it stops answering `copy_with_damage` (like a static
//! screen: the compositor only completes a damage copy once something changes).
#![allow(dead_code)]

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gowl::capture::wire::{Arg, Conn, DISPLAY};

#[derive(Clone, Copy, Debug)]
pub struct Opts {
    pub width: u32,
    pub height: u32,
    pub transform: i32,
    /// Frames answered before going "static".
    pub changing: usize,
}

const XRGB8888: u32 = 1;

struct Pool {
    id: u32,
    ptr: *mut u8,
    len: usize,
}

/// The color of frame `k` (XRGB, as R G B).
pub fn color_of(k: usize) -> [u8; 3] {
    [
        (k * 37 % 256) as u8,
        (255 - k * 11 % 256) as u8,
        (k * 5 % 256) as u8,
    ]
}

/// Serve one client until it disconnects. `served` counts completed frames.
pub fn serve(fd: OwnedFd, o: Opts, served: Arc<AtomicUsize>) {
    let mut c = Conn::test_server(fd);
    let (mut reg, mut shm, mut mgr) = (0u32, 0u32, 0u32);
    let mut outputs: Vec<u32> = Vec::new();
    let mut pools: Vec<Pool> = Vec::new();
    let mut buffers: Vec<(u32, usize, u32)> = Vec::new(); // (id, pool index, stride)
    let mut frames: Vec<u32> = Vec::new();
    let stride = o.width * 4 + 64; // padded, to exercise stride handling
    let mut k = 0usize;
    loop {
        loop {
            let ev = match c.next_event() {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(_) => return,
            };
            let mut a = c.args(&ev);
            if ev.obj == DISPLAY {
                match ev.op {
                    0 => {
                        let cb = a.uint();
                        let _ = c.request(cb, 0, &[Arg::Uint(0)]);
                        let _ = c.request(DISPLAY, 1, &[Arg::Uint(cb)]);
                    }
                    1 => {
                        reg = a.uint();
                        for (name, iface, v) in [
                            (1u32, &b"wl_shm"[..], 1u32),
                            (2, b"wl_output", 4),
                            (3, b"zwlr_screencopy_manager_v1", 3),
                        ] {
                            let _ = c.request(
                                reg,
                                0,
                                &[Arg::Uint(name), Arg::Str(iface), Arg::Uint(v)],
                            );
                        }
                    }
                    _ => {}
                }
            } else if ev.obj == reg && ev.op == 0 {
                let name = a.uint();
                let _iface = a.str();
                let _v = a.uint();
                let id = a.uint();
                match name {
                    1 => {
                        shm = id;
                        let _ = c.request(shm, 0, &[Arg::Uint(XRGB8888)]);
                    }
                    2 => {
                        outputs.push(id);
                        let _ = c.request(
                            id,
                            0,
                            &[
                                Arg::Int(0),
                                Arg::Int(0),
                                Arg::Int(600),
                                Arg::Int(340),
                                Arg::Int(0),
                                Arg::Str(b"Mock"),
                                Arg::Str(b"Screen"),
                                Arg::Int(o.transform),
                            ],
                        );
                        let _ = c.request(
                            id,
                            1,
                            &[
                                Arg::Uint(1),
                                Arg::Int(o.width as i32),
                                Arg::Int(o.height as i32),
                                Arg::Int(60000),
                            ],
                        );
                        let _ = c.request(id, 3, &[Arg::Int(1)]);
                        let _ = c.request(id, 4, &[Arg::Str(b"MOCK-1")]);
                        let _ = c.request(id, 2, &[]);
                    }
                    3 => mgr = id,
                    _ => {}
                }
            } else if ev.obj == shm && ev.op == 0 {
                let id = a.uint();
                let len = a.int() as usize;
                let fd = c.take_fd().expect("create_pool without fd");
                // SAFETY: mapping the client's memfd read/write, as a compositor does.
                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        len,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        fd.as_raw_fd(),
                        0,
                    )
                };
                assert_ne!(ptr, libc::MAP_FAILED);
                pools.push(Pool {
                    id,
                    ptr: ptr.cast(),
                    len,
                });
            } else if let Some(pi) = pools.iter().position(|p| p.id == ev.obj) {
                match ev.op {
                    0 => {
                        let id = a.uint();
                        let (_off, w, h, s, f) = (a.int(), a.int(), a.int(), a.int(), a.uint());
                        assert_eq!(
                            (w as u32, h as u32, s as u32, f),
                            (o.width, o.height, stride, XRGB8888)
                        );
                        buffers.push((id, pi, s as u32));
                    }
                    1 => {
                        let _ = c.request(DISPLAY, 1, &[Arg::Uint(ev.obj)]);
                    }
                    _ => {}
                }
            } else if let Some(bi) = buffers.iter().position(|b| b.0 == ev.obj) {
                if ev.op == 0 {
                    buffers.remove(bi);
                    let _ = c.request(DISPLAY, 1, &[Arg::Uint(ev.obj)]);
                }
            } else if ev.obj == mgr && ev.op == 0 {
                let frame = a.uint();
                let _overlay = a.int();
                frames.push(frame);
                let _ = c.request(
                    frame,
                    0,
                    &[
                        Arg::Uint(XRGB8888),
                        Arg::Uint(o.width),
                        Arg::Uint(o.height),
                        Arg::Uint(stride),
                    ],
                );
                let _ = c.request(frame, 6, &[]);
            } else if let Some(fi) = frames.iter().position(|&f| f == ev.obj) {
                match ev.op {
                    2 => {
                        let buf = a.uint();
                        if k >= o.changing {
                            continue; // static screen: never completes
                        }
                        let &(_, pi, s) =
                            buffers.iter().find(|b| b.0 == buf).expect("unknown buffer");
                        let p = &pools[pi];
                        // SAFETY: the pool mapping is live and len bytes long.
                        let mem = unsafe { std::slice::from_raw_parts_mut(p.ptr, p.len) };
                        let [r, g, b] = color_of(k);
                        for y in 0..o.height as usize {
                            let row = &mut mem[y * s as usize..][..o.width as usize * 4];
                            for px in row.as_chunks_mut::<4>().0 {
                                px.copy_from_slice(&[b, g, r, 0xff]);
                            }
                        }
                        k += 1;
                        served.store(k, Ordering::SeqCst);
                        let _ = c.request(ev.obj, 1, &[Arg::Uint(0)]);
                        let _ = c.request(
                            ev.obj,
                            4,
                            &[
                                Arg::Uint(0),
                                Arg::Uint(0),
                                Arg::Uint(o.width),
                                Arg::Uint(o.height),
                            ],
                        );
                        let _ = c.request(ev.obj, 2, &[Arg::Uint(0), Arg::Uint(1), Arg::Uint(0)]);
                    }
                    1 => {
                        frames.remove(fi);
                        let _ = c.request(DISPLAY, 1, &[Arg::Uint(ev.obj)]);
                    }
                    _ => {}
                }
            } else if outputs.contains(&ev.obj) && ev.op == 0 {
                let _ = c.request(DISPLAY, 1, &[Arg::Uint(ev.obj)]);
            }
        }
        if c.flush().is_err() {
            return;
        }
        match c.fill() {
            Ok(true) => {}
            Ok(false) => {
                if c.wait_readable().is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}
