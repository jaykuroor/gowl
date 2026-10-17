//! Sync runtime: one thread, one `poll()` over the Wayland fd, a timerfd and a
//! signalfd (ARCHITECTURE.md "Event loop"). No periodic tick: the timerfd is armed
//! to the nearest deadline (next capture request, next smoothing step, keepalive)
//! and disarmed when there is none. Static screen ⇒ keepalive wakeups only;
//! paused ⇒ none.
//!
//! The generic pieces (`Signals`, `Timer`, `Streams`) are mode-independent so music
//! sync can reuse the razer ON/OFF + shutdown contract; `Pipeline` and `run_screen`
//! are the screen part.

use std::io;
use std::net::{SocketAddrV4, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::capture::sample::{Geometry, Sampler};
use crate::capture::{Capture, Frame};
use crate::color::{self, Rgb, SendGate, Smoother};
use crate::config::Config;
use crate::devices::Device;
use crate::error::Error;
use crate::net::{self, Ports, Sender};
use crate::proto::MAX_SEGMENTS;

/// Heap allocations seen by the `count-alloc` global allocator (main.rs); stays 0
/// without that feature. Printed in the once-per-second debug line.
pub static ALLOCS: AtomicUsize = AtomicUsize::new(0);

/// Legacy device cap: never more than 40 frames/s to one light.
pub const DEVICE_MAX_HZ: u32 = 40;

const MS: u64 = 1_000_000;

/// CLOCK_MONOTONIC in nanoseconds.
pub fn now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid timespec out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

// ------------------------------------------------------------------ signals

/// What a signal asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sig {
    /// SIGINT / SIGTERM / SIGHUP.
    Stop,
    /// SIGUSR1.
    Pause,
    /// SIGUSR2.
    Resume,
}

/// signalfd for INT/TERM/HUP/USR1/USR2 (blocked for normal delivery).
pub struct Signals {
    fd: OwnedFd,
}

impl Signals {
    pub fn new() -> Result<Signals, Error> {
        // SAFETY: sigset manipulation on a local set, then process mask + signalfd.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for s in [
                libc::SIGINT,
                libc::SIGTERM,
                libc::SIGHUP,
                libc::SIGUSR1,
                libc::SIGUSR2,
            ] {
                libc::sigaddset(&mut set, s);
            }
            if libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
                return Err(Error::Io("sigprocmask", io::Error::last_os_error()));
            }
            let fd = libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC);
            if fd < 0 {
                return Err(Error::Io("signalfd", io::Error::last_os_error()));
            }
            Ok(Signals {
                fd: OwnedFd::from_raw_fd(fd),
            })
        }
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Next pending signal, if any.
    pub fn read(&self) -> Option<Sig> {
        loop {
            // SAFETY: signalfd_siginfo is plain old data.
            let mut si: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
            let sz = std::mem::size_of::<libc::signalfd_siginfo>();
            // SAFETY: reading into a properly sized buffer.
            let n = unsafe {
                libc::read(
                    self.fd(),
                    (&mut si as *mut libc::signalfd_siginfo).cast(),
                    sz,
                )
            };
            if n != sz as isize {
                return None;
            }
            match si.ssi_signo as i32 {
                libc::SIGINT | libc::SIGTERM | libc::SIGHUP => return Some(Sig::Stop),
                libc::SIGUSR1 => return Some(Sig::Pause),
                libc::SIGUSR2 => return Some(Sig::Resume),
                _ => continue,
            }
        }
    }
}

// ------------------------------------------------------------------ timer

/// One absolute CLOCK_MONOTONIC timerfd, re-armed only when the deadline changes.
pub struct Timer {
    fd: OwnedFd,
    armed: Option<u64>,
}

impl Timer {
    pub fn new() -> Result<Timer, Error> {
        // SAFETY: plain timerfd_create.
        let fd = unsafe {
            libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(Error::Io("timerfd_create", io::Error::last_os_error()));
        }
        Ok(Timer {
            // SAFETY: fresh fd we own.
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            armed: None,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Arm for absolute time `at` (ns), or disarm with `None`.
    pub fn arm(&mut self, at: Option<u64>) -> Result<(), Error> {
        if at == self.armed {
            return Ok(());
        }
        let v = at.map_or(0, |t| t.max(1)); // 0 would disarm
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: (v / 1_000_000_000) as libc::time_t,
                tv_nsec: (v % 1_000_000_000) as libc::c_long,
            },
        };
        // SAFETY: valid itimerspec; old value not requested.
        let r = unsafe {
            libc::timerfd_settime(
                self.fd(),
                libc::TFD_TIMER_ABSTIME,
                &spec,
                std::ptr::null_mut(),
            )
        };
        if r != 0 {
            return Err(Error::Io("timerfd_settime", io::Error::last_os_error()));
        }
        self.armed = at;
        Ok(())
    }

    /// Consume an expiry (after poll said readable).
    pub fn drain(&mut self) {
        let mut v = 0u64;
        // SAFETY: reading 8 bytes into a u64.
        unsafe { libc::read(self.fd(), (&mut v as *mut u64).cast(), 8) };
        self.armed = None;
    }
}

// ------------------------------------------------------------------ streams

/// One device being streamed to.
pub struct Stream {
    pub label: String,
    sock: UdpSocket,
    sender: Sender,
    gate: SendGate,
    reverse: bool,
    offset: usize,
    /// Current segment colors (after reverse/offset).
    seg: [Rgb; MAX_SEGMENTS],
    last_ms: Option<u64>,
    failing: bool,
}

impl Stream {
    fn n(&self) -> usize {
        self.sender.segments()
    }

    /// Send the current segments if the gate says so (delta or keepalive).
    fn maybe_send(&mut self, now_ms: u64) -> bool {
        let n = self.n();
        if !self.gate.needs_send(&self.seg[..n], now_ms) {
            return false;
        }
        let r = self.sender.send_colors(&self.sock, &self.seg[..n]);
        self.note(r, "frame");
        self.gate.mark_sent(&self.seg[..n], now_ms);
        self.last_ms = Some(now_ms);
        true
    }

    /// Log a send failure once per failing streak (UDP: carry on regardless).
    fn note(&mut self, r: io::Result<()>, what: &str) {
        match r {
            Ok(()) => {
                if self.failing {
                    warn!("{}: sending works again", self.label);
                    self.failing = false;
                }
            }
            Err(e) => {
                if !self.failing {
                    if net::is_blocked(&e) {
                        warn!(
                            "{}: {what} refused ({e}): a VPN kill switch blocks LAN traffic (see `gowl doctor`)",
                            self.label
                        );
                    } else {
                        warn!("{}: {what} send failed: {e}", self.label);
                    }
                    self.failing = true;
                }
            }
        }
    }
}

/// All devices of one sync session.
pub struct Streams {
    pub devs: Vec<Stream>,
    keepalive_ms: u64,
    /// Datagrams sent (frames + keepalives), for the debug line.
    pub sent: u64,
}

impl Streams {
    /// Resolve each device's LAN interface and bind one socket per device to it
    /// (NETWORK.md: SO_BINDTODEVICE, never a tunnel).
    pub fn open(
        devices: &[Device],
        lan_if: Option<&str>,
        ports: Ports,
        threshold: u8,
        keepalive_ms: u32,
    ) -> Result<Streams, Error> {
        let ifaces = net::interfaces().map_err(|e| Error::Io("getifaddrs", e))?;
        let mut devs = Vec::with_capacity(devices.len());
        for d in devices {
            let sel = net::select_for(&ifaces, d.ip, lan_if)?;
            debug!("{}: lan interface {} — {}", d.label(), sel.iface, sel.why);
            let sock = net::bound_socket(&sel.iface, None)?;
            sock.set_nonblocking(true)
                .map_err(|e| Error::Io("set_nonblocking", e))?;
            let n = usize::from(d.segments).clamp(1, MAX_SEGMENTS);
            devs.push(Stream {
                label: d.label(),
                sock,
                sender: Sender::new(SocketAddrV4::new(d.ip, ports.control), n),
                gate: SendGate::new(threshold, u64::from(keepalive_ms)),
                reverse: d.reverse,
                offset: usize::from(d.offset) % n,
                seg: [[0; 3]; MAX_SEGMENTS],
                last_ms: None,
                failing: false,
            });
        }
        Ok(Streams {
            devs,
            keepalive_ms: u64::from(keepalive_ms),
            sent: 0,
        })
    }

    /// Razer mode ON/OFF on every device. ON also forgets the last sent frame so the
    /// next one goes out unconditionally.
    pub fn razer(&mut self, on: bool) {
        for s in &mut self.devs {
            let r = s.sender.razer_mode(&s.sock, on);
            s.note(r, if on { "razer on" } else { "razer off" });
            if on {
                s.gate.reset();
                s.last_ms = None;
            }
        }
    }

    /// Shutdown contract (SPEC.md): razer OFF twice, 10 ms apart.
    pub fn shutdown(&mut self) {
        self.razer(false);
        std::thread::sleep(Duration::from_millis(10));
        self.razer(false);
    }

    /// New colors (zone order, `colors.len() == segments`) for device `i`.
    pub fn push(&mut self, i: usize, colors: &[Rgb], now_ms: u64) {
        let s = &mut self.devs[i];
        let n = s.n();
        color::reorder(&colors[..n], s.reverse, s.offset, &mut s.seg[..n]);
        if s.maybe_send(now_ms) {
            self.sent += 1;
        }
    }

    /// Resend to devices whose keepalive is due.
    pub fn keepalive(&mut self, now_ms: u64) {
        for s in &mut self.devs {
            if s.maybe_send(now_ms) {
                self.sent += 1;
            }
        }
    }

    /// Earliest keepalive deadline (ms), if keepalive is on and anything was sent.
    pub fn keepalive_due_ms(&self) -> Option<u64> {
        if self.keepalive_ms == 0 {
            return None;
        }
        self.devs
            .iter()
            .filter_map(|s| s.last_ms)
            .min()
            .map(|t| t + self.keepalive_ms)
    }
}

// ------------------------------------------------------------------ screen pipeline

/// Devices sharing one segment count share one zone set and smoother.
struct Group {
    sampler: Sampler,
    smoother: Smoother,
    /// Zone means after saturation + brightness (the EMA target).
    target: Vec<Rgb>,
    /// Smoothed output.
    out: Vec<Rgb>,
    settled: bool,
    devs: Vec<usize>,
}

/// frame → zones → saturation → brightness → EMA → per device reorder → gate → send.
pub struct Pipeline {
    groups: Vec<Group>,
    pub streams: Streams,
    sat_q16: u32,
    bri_q16: u32,
    pub interval_ns: u64,
    last_step: Option<u64>,
    step_at: Option<u64>,
    /// Frames processed, for the debug line.
    pub frames: u64,
}

impl Pipeline {
    pub fn new(streams: Streams, cfg: &Config, depth: f64, fps: u32) -> Pipeline {
        let mut groups: Vec<Group> = Vec::new();
        for (i, s) in streams.devs.iter().enumerate() {
            let n = s.n();
            match groups.iter_mut().find(|g| g.sampler.zone_count() == n) {
                Some(g) => g.devs.push(i),
                None => groups.push(Group {
                    sampler: Sampler::new(n, depth),
                    smoother: Smoother::new(cfg.smoothing, n),
                    target: vec![[0; 3]; n],
                    out: vec![[0; 3]; n],
                    settled: false,
                    devs: vec![i],
                }),
            }
        }
        let hz = u64::from(fps.clamp(1, DEVICE_MAX_HZ));
        Pipeline {
            groups,
            streams,
            sat_q16: color::q16(cfg.saturation),
            bri_q16: color::q16(cfg.screen_brightness),
            interval_ns: 1_000_000_000 / hz,
            last_step: None,
            step_at: None,
            frames: 0,
        }
    }

    /// A captured frame: re-sample dirty zones, then step now or at the next slot.
    pub fn on_frame(&mut self, f: &Frame<'_>, now: u64) {
        self.frames += 1;
        if self.frames <= 5 {
            debug!(
                "frame {}: {}damage {:?}{}",
                self.frames,
                if f.full_damage { "full " } else { "" },
                f.damage,
                if f.y_invert { " y_invert" } else { "" }
            );
        }
        let geo = Geometry::of(f);
        for g in &mut self.groups {
            let rebuilt = g.sampler.ensure(geo);
            g.sampler.mark(f.damage, f.full_damage || rebuilt);
            if g.sampler.sample(f.data) {
                g.target.copy_from_slice(g.sampler.colors());
                color::saturate(&mut g.target, self.sat_q16);
                color::scale(&mut g.target, self.bri_q16);
                g.settled = false;
            }
        }
        if self.groups.iter().all(|g| g.settled) {
            return;
        }
        // Frames come one interval apart give or take the compositor's render phase
        // (10 ms at 100 Hz), so allow steps 3/4 of an interval apart — but never
        // closer than the 40 Hz device cap — instead of deferring to a timer wakeup.
        let gap = (self.interval_ns * 3 / 4).max(1_000_000_000 / u64::from(DEVICE_MAX_HZ));
        let due = self.last_step.map_or(0, |t| t + gap);
        if now >= due {
            self.step(now, true);
        } else {
            self.step_at = Some(due);
        }
    }

    /// One EMA step for unsettled groups, then send what changed. After a step
    /// driven by a frame, the fallback timer step waits 1.5 intervals: on a busy
    /// screen the next frame arrives about one interval later and steps instead
    /// (no extra wakeup); on a static screen the copy blocks and the timer
    /// finishes the fade at the frame rate.
    fn step(&mut self, now: u64, from_frame: bool) {
        let now_ms = now / MS;
        for g in &mut self.groups {
            if g.settled {
                continue;
            }
            g.smoother.update(&g.target, &mut g.out);
            g.settled = g.smoother.settled(&g.target);
            for &i in &g.devs {
                self.streams.push(i, &g.out, now_ms);
            }
        }
        self.last_step = Some(now);
        self.step_at = if self.groups.iter().all(|g| g.settled) {
            None
        } else {
            Some(now + self.interval_ns + if from_frame { self.interval_ns / 2 } else { 0 })
        };
    }

    /// Timer expired: smoothing step and/or keepalive if due.
    pub fn on_timer(&mut self, now: u64) {
        if self.step_at.is_some_and(|t| t <= now) {
            self.step(now, false);
        }
        if self
            .streams
            .keepalive_due_ms()
            .is_some_and(|t| t * MS <= now)
        {
            self.streams.keepalive(now / MS);
        }
    }

    /// After razer ON (resume, output back): resend the current colors now.
    pub fn resend(&mut self, now: u64) {
        for g in &self.groups {
            for &i in &g.devs {
                self.streams.push(i, &g.out, now / MS);
            }
        }
    }

    /// Next step or keepalive deadline (ns).
    pub fn deadline(&self) -> Option<u64> {
        let ka = self.streams.keepalive_due_ms().map(|t| t * MS);
        match (self.step_at, ka) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

// ------------------------------------------------------------------ screen loop

const BACKOFF_FIRST: u64 = 100 * MS;
const BACKOFF_NEXT: u64 = 1000 * MS;

/// `gowl screen`: run until a stop signal. Sends razer ON first and the shutdown
/// sequence (razer OFF ×2) on every exit path.
pub fn run_screen(cap: &mut dyn Capture, pipe: &mut Pipeline) -> Result<(), Error> {
    let sig = Signals::new()?;
    let mut timer = Timer::new()?;
    pipe.streams.razer(true);
    let r = screen_loop(cap, pipe, &sig, &mut timer);
    pipe.streams.shutdown();
    r
}

fn screen_loop(
    cap: &mut dyn Capture,
    pipe: &mut Pipeline,
    sig: &Signals,
    timer: &mut Timer,
) -> Result<(), Error> {
    let interval = pipe.interval_ns;
    let mut next_capture = Some(now_ns());
    let mut last_request = 0u64;
    let mut fails = 0u32;
    let mut paused = false;
    let mut waiting = false; // output gone
    let mut report_at = now_ns() + 1000 * MS;
    let (mut frames0, mut sent0, mut allocs0) = (0u64, 0u64, ALLOCS.load(Ordering::Relaxed));
    let mut wakes = [0u32; 3]; // wayland, timer, signal (debug line)
    let mut pfds = [
        libc::pollfd {
            fd: cap.fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: timer.fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: sig.fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    cap.set_interrupt_fd(sig.fd());
    loop {
        if let Err(e) = cap.flush() {
            // A stop signal aborts a send stuck on a compositor that stopped reading.
            if sig.read() == Some(Sig::Stop) {
                debug!("stop signal while the compositor was not reading: razer off, exit");
                return Ok(());
            }
            return Err(e);
        }
        let cap_dl = next_capture.filter(|_| !paused && !waiting);
        // Paused or output gone: razer is OFF, so no fade steps and no keepalives.
        let pipe_dl = if paused || waiting {
            None
        } else {
            pipe.deadline()
        };
        let dl = match (cap_dl, pipe_dl) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        timer.arm(dl)?;
        for p in &mut pfds {
            p.revents = 0;
        }
        // SAFETY: valid pollfd array.
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, -1) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::Io("poll", e));
        }
        let now = now_ns();
        for (w, p) in wakes.iter_mut().zip(&pfds) {
            *w += u32::from(p.revents != 0);
        }

        if pfds[2].revents != 0 {
            while let Some(s) = sig.read() {
                match s {
                    Sig::Stop => {
                        debug!("stop signal: razer off, exit");
                        return Ok(());
                    }
                    Sig::Pause if !paused => {
                        debug!("paused");
                        paused = true;
                        cap.cancel()?;
                        next_capture = None;
                        pipe.streams.razer(false);
                    }
                    Sig::Resume if paused => {
                        debug!("resumed");
                        paused = false;
                        if !waiting {
                            pipe.streams.razer(true);
                            pipe.resend(now);
                            next_capture = Some(now);
                        }
                    }
                    _ => {}
                }
            }
        }

        if pfds[0].revents != 0 {
            let st = cap.dispatch()?;
            if st.lost {
                warn!("output gone; lights off until it comes back");
                waiting = true;
                next_capture = None;
                if !paused {
                    pipe.streams.razer(false);
                }
            }
            if st.found {
                if waiting {
                    warn!(
                        "output {} is back; resuming",
                        cap.output_name().unwrap_or("?")
                    );
                    waiting = false;
                    if !paused {
                        pipe.streams.razer(true);
                        pipe.resend(now);
                    }
                }
                if !paused {
                    next_capture = Some(now);
                }
            }
            if st.failed {
                fails += 1;
                if fails == 1 {
                    warn!("capture failed; retrying (logged once until it works again)");
                }
                let backoff = if fails == 1 {
                    BACKOFF_FIRST
                } else {
                    BACKOFF_NEXT
                };
                if !paused && !waiting {
                    next_capture = Some(now + backoff);
                }
            }
            if st.ready {
                if fails > 0 {
                    warn!("capture works again after {fails} failure(s)");
                    fails = 0;
                }
                if let Some(f) = cap.frame() {
                    pipe.on_frame(&f, now);
                }
                if !paused && !waiting {
                    let at = last_request + interval;
                    if at <= now {
                        // Slow copy: the slot has passed, ask now (saves a timer wakeup).
                        next_capture = None;
                        if cap.request_next()? {
                            last_request = now;
                        }
                    } else {
                        next_capture = Some(at);
                    }
                }
            }
        }

        if pfds[1].revents != 0 {
            timer.drain();
            if !paused && !waiting && next_capture.is_some_and(|t| t <= now) {
                next_capture = None;
                if cap.request_next()? {
                    last_request = now;
                }
            }
            if !paused && !waiting {
                pipe.on_timer(now);
            }
        }

        if now >= report_at && crate::log::enabled(crate::log::DEBUG) {
            let allocs = ALLOCS.load(Ordering::Relaxed);
            debug!(
                "last {:.1} s: {} frames, {} datagrams, wakeups wayland {} timer {} signal {}{}",
                (now - report_at + 1000 * MS) as f64 / 1e9,
                pipe.frames - frames0,
                pipe.streams.sent - sent0,
                wakes[0],
                wakes[1],
                wakes[2],
                if cfg!(feature = "count-alloc") {
                    format!(", {} heap allocations", allocs - allocs0)
                } else {
                    String::new()
                }
            );
            // The format! above allocates; re-read so the line itself isn't counted.
            (frames0, sent0, allocs0) = (
                pipe.frames,
                pipe.streams.sent,
                ALLOCS.load(Ordering::Relaxed),
            );
            wakes = [0; 3];
            report_at = now + 1000 * MS;
        }
    }
}
