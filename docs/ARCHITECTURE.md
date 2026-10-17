# ARCHITECTURE

## Shape
One Rust crate, one binary, one thread. No async runtime. Modules:

```
src/
  main.rs        arg parsing (hand-written), dispatch, exit codes
  config.rs      key=value parser, device sections, env overrides, XDG paths
  devices.rs     device cache (load/save atomic), DEV resolution, SKU table
  proto.rs       message templates, razer frame encoder, base64 encoder, reply scanner
  net.rs         UDP sockets bound to the LAN interface (VPN-proof, see NETWORK.md),
                 discovery, status query, Sender (per-device prealloc buffers)
  netwatch.rs    rtnetlink change listener in the poll set → re-select interface, rebind
  doctor.rs      `gowl doctor` diagnostics
  color.rs       sRGB↔linear LUTs, zone mapping (port of led_mapping.py), saturation,
                 brightness, EMA smoother, resample, reverse/offset, delta check — all fixed-point
  capture/
    mod.rs       trait Capture { fn fd(); fn dispatch() -> Option<FrameRef>; fn request_next(); }
    wire.rs        hand-written Wayland wire client (D-016): fixed buffers, fd passing, id reuse
    screencopy.rs  zwlr_screencopy_manager_v1 (v3) — default on niri; outputs + hot-plug
    imagecopy.rs   ext_image_copy_capture_v1 — stub until niri advertises it (D-006)
    shm.rs         memfd + mmap + wl_shm_pool buffer, reused across frames
    sample.rs      precomputed per-zone sample offsets, pixel-format readers, damage→zone mask
  audio/
    pulse.rs     libpulse FFI via dlopen (non-threaded pa_mainloop, peek/drop; D-030)
    run.rs       `gowl music`: read loop, render pacing, signals, razer ON/OFF
    fft.rs       radix-2 real FFT, N=1024 (packed 512-pt complex), twiddles + Hann table
    bands.rs     band RMS (precomputed bin ranges), AutoGain
    render.rs    reactions × palettes (port of MusicPatternRenderer)
  runtime.rs     poll loop (wayland fd, timerfd, signalfd), pidfile, pause/stop, keepalive
  calibrate.rs   segment walk
```

## Screen-sync data flow (per captured frame)
```
niri ──wlr-screencopy copy_with_damage──▶ SHM buffer (mmap'd once)
  ▼  ready event (+ damage rects, y_invert flag)
damage rects → dirty-zone bitmask (skip clean zones; reuse their last linear sums)
  ▼
for each dirty zone: Σ LUT_lin16[r,g,b] over precomputed byte offsets (legacy stride) → mean → inv-LUT → sRGB8
  ▼
saturation (fixed-point) → brightness ×k → EMA (u16 fixed-point)
  (zones = device segment count, as in legacy; with several devices of different N,
   build one zone set per distinct N — usually there is exactly one)
  ▼  per device: reverse/offset → delta check vs last sent
razer frame bytes → XOR → base64 into fixed datagram slot → sendto()
  ▼
arm timerfd: next capture request at max(now, last_request + 1/fps); keepalive deadline
```
Everything after setup reuses buffers. Zone offsets, LUTs, mapping, datagram templates
are rebuilt only when output size/format/transform or device N changes.

## Why this capture path (niri)
niri implements `wlr-screencopy` (v3) and the portal/PipeWire screencast. Screencopy is
the lean choice:
- **Pull model:** we request a frame only when our rate cap allows → we never receive
  frames we'd discard. PipeWire pushes at the compositor's rate.
- **`copy_with_damage`:** the request completes only when the screen changes → zero
  work on a static screen (keepalive timer still fires).
- **No D-Bus, no portal dialog, no PipeWire, no restore tokens.** Fewer libraries
  mapped, fewer processes woken, faster start.
- **Cursor excluded** with `overlay_cursor = 0` (niri ≥ 26.04 honours this with damage).

Costs to watch (measured in Phase 5): niri does a GPU→CPU readback of the full output
into our SHM buffer per frame. At 1440p that's ~14.7 MB/frame. If niri's CPU delta exceeds
the budget, the DMA-BUF variant (screencopy `linux_dmabuf` event + GBM linear buffer)
moves the copy to a GPU blit; it needs `libgbm` and the render node from
`zwp_linux_dmabuf_v1` feedback. Phase 5 decides with numbers. Details: `CAPTURE_NIRI.md`.

Seam for other compositors: `trait Capture`. A portal+PipeWire backend can be added later
behind a cargo feature without touching `color.rs`/`net.rs`. Not in v1.

## Event loop (screen mode)
```
poll([wayland_fd, timerfd, signalfd, netlink_fd], -1)
  wayland readable → prepare_read/read/dispatch → on frame ready: process + send
  timerfd          → either "issue next capture request" or "keepalive resend"
  signalfd         → SIGTERM/SIGINT/SIGHUP: razer OFF, exit; SIGUSR1: pause; SIGUSR2: resume
  netlink          → link/addr/route changed: debounce 500 ms, re-select LAN interface, rebind if changed
```
One timerfd armed to the nearest deadline (absolute, CLOCK_MONOTONIC). No periodic
tick. Static screen ⇒ wakeups = keepalive rate only. Paused ⇒ zero wakeups.
The timerfd has a third job: smoothing steps. Legacy steps the EMA once per captured
frame even on a static screen; gowl gets no frames then, so after a change it keeps
stepping on the timer (at the frame rate) until `Smoother::settled` says the EMA has
reached its fixed point, then stops (D-017). Busy screen: 3 wakeups per frame
(timer → `capture_output`, `buffer_done` → copy, `ready` → process); see D-018.
(netlink rebind is Phase 4; not in the loop yet.)

## Music mode
Record stream on `"@DEFAULT_MONITOR@"` (float32le, 48000 Hz, mono, fragsize 1024·4) via
libpulse's *non-threaded* `pa_mainloop`, iterated (blocking) on the main thread — one
thread, one wakeup per 21.3 ms window (D-030; `pa_simple` would add a thread and double
the wakeups). Loop: read 1024 samples → if due under min(music_fps, 40 Hz): FFT → bands →
AutoGain → render → smooth → brightness → reverse/offset → delta check → send. Signals
set an `AtomicBool` via `sigaction` (no SA_RESTART), so the mainloop's `poll()` returns
EINTR; a 1 s `alarm` watchdog covers the race before `poll()`. libpulse is `dlopen`ed so
other modes never map it. Details: `AUDIO.md`.

## Numbers & formats
- Linear light: `LUT_LIN16[256]: u16` (sRGB8 → linear × 65535). Zone sum in `u32`
  (≤ 47·47 = 2209 samples × 65535 < 2³²). Mean → sRGB8 by binary search over 256 midpoint thresholds
  (exact nearest, 8 compares, once per zone per frame).
- Saturation: luma_q16 = 13933·r + 46871·g + 4732·b (Rec.709 ×65536, sum 65536); out = luma + ((c·65536 − luma)·sat_q16 >> 16), rounded, clamp (i64; Q16 keeps it exact vs legacy, D-013).
- EMA state: `u16` = value << 8; `s += ((t<<8) − s) · α_q16 >> 16` (rounded); state starts black (first frames fade in, as legacy `ColorSmoother(alpha, count)` does); a zone-count change snaps (D-012).
- All results within ±1/channel of `tests/vectors/processing.json`.

## Release profile (put in Cargo.toml)
```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
debug = false
incremental = false
```
`-C target-cpu=native` only via an opt-in `.cargo/config.toml` the human enables locally
(the RPM build must stay portable). Try `opt-level = "s"` in Phase 5 and keep whichever
measures better on CPU (binary size is secondary).

## Errors & logging
Own `enum Error` with `Display`; no panics on I/O paths; `?` everywhere. Logging = a
`macro_rules!` over `eprintln!` gated by a level read once from `GOWL_LOG`.
No timestamps (journald adds them).
