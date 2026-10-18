//! Sync runtime: one thread, one `poll()` over the Wayland fd, a timerfd and a
//! signalfd (ARCHITECTURE.md "Event loop"). No periodic tick: the timerfd is armed
//! to the nearest deadline (next capture request, next smoothing step, keepalive)
//! and disarmed when there is none. Static screen ⇒ keepalive wakeups only;
//! paused ⇒ none.
//!
//! The generic pieces (`Signals`, `Timer`, `Streams`, `Ctl`) are mode-independent:
//! music sync and `calibrate` reuse the razer ON/OFF + shutdown contract, the send
//! gate and the netlink rebind (`netwatch.rs`); `Pipeline` and `run_screen` are the
//! screen part.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::capture::sample::{Geometry, Sampler};
use crate::capture::{Capture, Frame};
use crate::color::{self, Rgb, SendGate, Smoother};
use crate::config::Config;
use crate::devices::Device;
use crate::error::Error;
use crate::net::{self, Iface, Ports, Sender};
use crate::netwatch::{NetWatch, Pending, Verify};
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
    /// SIGINT / SIGTERM / SIGHUP / SIGQUIT.
    Stop,
    /// SIGUSR1.
    Pause,
    /// SIGUSR2.
    Resume,
}

/// signalfd for INT/TERM/HUP/QUIT/USR1/USR2 (blocked for normal delivery).
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
                libc::SIGQUIT,
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
                libc::SIGINT | libc::SIGTERM | libc::SIGHUP | libc::SIGQUIT => {
                    return Some(Sig::Stop);
                }
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

    /// Consume an expiry (after poll said readable) without a read(2): every loop
    /// calls `arm` before it polls again, and the forced `timerfd_settime` (even to
    /// "disarmed") resets the expiry count, so the fd is no longer readable.
    pub fn drain(&mut self) {
        self.armed = Some(u64::MAX);
    }
}

// ------------------------------------------------------------------ streams

/// Send-error back-off before re-selecting the LAN interface (NETWORK.md).
const RESELECT_BACKOFF: [u64; 3] = [250 * MS, 1000 * MS, 5000 * MS];

/// One device being streamed to.
pub struct Stream {
    pub label: String,
    sock: UdpSocket,
    sender: Sender,
    gate: SendGate,
    reverse: bool,
    offset: usize,
    /// Calibrated layout: zones are already in segment order.
    layout: Option<Vec<u8>>,
    /// Current segment colors (after reverse/offset).
    seg: [Rgb; MAX_SEGMENTS],
    last_ms: Option<u64>,
    failing: bool,
    /// Interface (name, index, address) the socket is bound to.
    iface: Iface,
    mac: Option<String>,
    /// The last send failed with "network gone" (ENETUNREACH, ENODEV, …).
    net_down: bool,
    /// Re-selection found no LAN interface for this device (logged once).
    no_lan: bool,
}

/// What one send attempt did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sent {
    Skipped,
    Ok,
    Failed,
}

impl Stream {
    fn n(&self) -> usize {
        self.sender.segments()
    }

    fn ip(&self) -> Ipv4Addr {
        *self.sender.dst.ip()
    }

    /// Send the current segments if the gate says so (delta or keepalive).
    fn maybe_send(&mut self, now_ms: u64) -> Sent {
        let n = self.n();
        if !self.gate.needs_send(&self.seg[..n], now_ms) {
            return Sent::Skipped;
        }
        let r = self.sender.send_colors(&self.sock, &self.seg[..n]);
        let ok = r.is_ok();
        self.note(r, "frame");
        self.gate.mark_sent(&self.seg[..n], now_ms);
        self.last_ms = Some(now_ms);
        if ok { Sent::Ok } else { Sent::Failed }
    }

    /// Log a send failure once per failing streak (UDP: carry on regardless).
    fn note(&mut self, r: io::Result<()>, what: &str) {
        match r {
            Ok(()) => {
                self.net_down = false;
                if self.failing {
                    warn!("{}: sending works again", self.label);
                    self.failing = false;
                }
            }
            Err(e) => {
                self.net_down = net::is_net_down(&e);
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

    fn razer(&mut self, on: bool) {
        let r = self.sender.razer_mode(&self.sock, on);
        self.note(r, if on { "razer on" } else { "razer off" });
        if on {
            self.gate.reset();
            self.last_ms = None;
        }
    }
}

/// All devices of one sync session, plus the network bookkeeping that keeps their
/// sockets on the LAN interface across network changes.
pub struct Streams {
    pub devs: Vec<Stream>,
    keepalive_ms: u64,
    /// Datagrams sent (frames + keepalives), for the debug line.
    pub sent: u64,
    lan_if: Option<String>,
    ports: Ports,
    /// Razer mode is ON (streaming): a rebind sends razer ON on the new socket.
    pub live: bool,
    /// Re-select interfaces at this time (ns): netlink debounce or send-error back-off.
    reselect_at: Option<u64>,
    backoff: usize,
    verify: Option<Verify>,
}

impl Streams {
    /// Resolve each device's LAN interface and bind one socket per device to it
    /// (NETWORK.md: SO_BINDTODEVICE, never a tunnel). `use_layout`: honour a
    /// calibrated `layout` (screen); music renders in segment order and keeps
    /// reverse/offset.
    pub fn open(
        devices: &[Device],
        lan_if: Option<&str>,
        ports: Ports,
        threshold: u8,
        keepalive_ms: u32,
        use_layout: bool,
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
            let layout = d.layout.clone().filter(|l| use_layout && l.len() == n);
            devs.push(Stream {
                label: d.label(),
                sock,
                sender: Sender::new(SocketAddrV4::new(d.ip, d.control_port(ports)), n),
                gate: SendGate::new(threshold, u64::from(keepalive_ms)),
                // A calibrated layout already puts every zone on its segment.
                reverse: d.reverse && layout.is_none(),
                offset: if layout.is_some() {
                    0
                } else {
                    usize::from(d.offset) % n
                },
                layout,
                seg: [[0; 3]; MAX_SEGMENTS],
                last_ms: None,
                failing: false,
                iface: sel.iface,
                mac: d.mac.clone(),
                net_down: false,
                no_lan: false,
            });
        }
        emergency::install();
        for (i, s) in devs.iter().enumerate() {
            emergency::set(i, s.sock.as_raw_fd(), s.sender.dst);
        }
        Ok(Streams {
            devs,
            keepalive_ms: u64::from(keepalive_ms),
            sent: 0,
            lan_if: lan_if.map(str::to_string),
            ports,
            live: false,
            reselect_at: None,
            backoff: 0,
            verify: None,
        })
    }

    /// Segment count of device `i`.
    pub fn segments(&self, i: usize) -> usize {
        self.devs[i].n()
    }

    /// Razer mode ON/OFF on every device. ON also forgets the last sent frame so the
    /// next one goes out unconditionally.
    pub fn razer(&mut self, on: bool) {
        self.live = on;
        for s in &mut self.devs {
            s.razer(on);
        }
    }

    /// Razer OFF twice, 10 ms apart (UDP): stop, pause and output-lost alike.
    pub fn off(&mut self) {
        self.razer(false);
        std::thread::sleep(Duration::from_millis(10));
        self.razer(false);
    }

    /// Shutdown contract (SPEC.md): razer OFF ×2.
    pub fn shutdown(&mut self) {
        self.off();
    }

    /// Keepalive interval for every device (`calibrate --keepalive` steps).
    pub fn set_keepalive_ms(&mut self, ms: u64) {
        self.keepalive_ms = ms;
        for s in &mut self.devs {
            s.gate.keepalive_ms = ms;
        }
    }

    /// New colors (zone order, `colors.len() == segments`) for device `i`.
    /// Returns true if a datagram went out.
    pub fn push(&mut self, i: usize, colors: &[Rgb], now_ms: u64) -> bool {
        let s = &mut self.devs[i];
        let n = s.n();
        color::reorder(&colors[..n], s.reverse, s.offset, &mut s.seg[..n]);
        let r = s.maybe_send(now_ms);
        self.after_send(r, now_ms)
    }

    /// Resend to devices whose keepalive is due. A device that has no frame since
    /// razer ON is skipped: a keepalive only repeats what was really sent (rule 7).
    pub fn keepalive(&mut self, now_ms: u64) {
        for i in 0..self.devs.len() {
            if self.devs[i].last_ms.is_none() {
                continue;
            }
            let r = self.devs[i].maybe_send(now_ms);
            self.after_send(r, now_ms);
        }
    }

    /// The keepalive interval (ms; 0 = off).
    pub fn keepalive_ms(&self) -> u64 {
        self.keepalive_ms
    }

    fn after_send(&mut self, r: Sent, now_ms: u64) -> bool {
        match r {
            Sent::Skipped => false,
            Sent::Ok => {
                self.sent += 1;
                if self.backoff > 0 && !self.devs.iter().any(|s| s.net_down) {
                    self.backoff = 0;
                }
                true
            }
            Sent::Failed => {
                self.sent += 1;
                if self.reselect_at.is_none() && self.devs.iter().any(|s| s.net_down) {
                    let wait = RESELECT_BACKOFF[self.backoff.min(RESELECT_BACKOFF.len() - 1)];
                    self.backoff = (self.backoff + 1).min(RESELECT_BACKOFF.len() - 1);
                    self.reselect_at = Some(now_ms * MS + wait);
                }
                true
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

    // -------------------------------------------------------------- network changes

    /// Re-select interfaces at `at` (netlink debounce expired).
    pub fn request_reselect(&mut self, at: u64) {
        self.reselect_at = Some(self.reselect_at.map_or(at, |t| t.min(at)));
    }

    /// Earliest network deadline (ns): re-selection or a verification step.
    pub fn net_deadline(&self) -> Option<u64> {
        min_opt(self.reselect_at, self.verify.as_ref().map(Verify::deadline))
    }

    /// The verification socket while one runs, else -1 (poll ignores it).
    pub fn verify_fd(&self) -> RawFd {
        self.verify.as_ref().map_or(-1, Verify::fd)
    }

    /// Network timers due at `now`. True if a device was rebound or moved: the
    /// caller resends the current colors.
    pub fn on_net_timer(&mut self, now: u64) -> bool {
        let mut changed = false;
        if self.reselect_at.is_some_and(|t| t <= now) {
            self.reselect_at = None;
            match net::interfaces() {
                Ok(ifaces) => changed = self.reselect_with(&ifaces, now),
                Err(e) => warn!("getifaddrs: {e}"),
            }
        }
        if let Some(v) = &mut self.verify
            && v.deadline() <= now
            && !v.on_deadline(now)
        {
            self.verify = None;
        }
        changed
    }

    /// Re-run interface selection against `ifaces`; rebind every device whose
    /// interface (name, index or address) changed, or whose sends fail with
    /// "network gone". True if any socket was replaced.
    pub fn reselect_with(&mut self, ifaces: &[Iface], now: u64) -> bool {
        let mut changed = false;
        let mut verify: Vec<Pending> = Vec::new();
        for (i, s) in self.devs.iter_mut().enumerate() {
            let sel = match net::select_for(ifaces, s.ip(), self.lan_if.as_deref()) {
                Ok(sel) => sel,
                Err(e) => {
                    if !s.no_lan {
                        warn!("{}: {e}; waiting for the network to come back", s.label);
                        s.no_lan = true;
                    }
                    continue;
                }
            };
            let moved = sel.iface.name != s.iface.name
                || sel.iface.index != s.iface.index
                || sel.iface.addr != s.iface.addr;
            if !moved && !s.net_down && !s.no_lan {
                continue;
            }
            let sock = match net::bound_socket(&sel.iface, None).and_then(|k| {
                k.set_nonblocking(true)
                    .map_err(|e| Error::Io("set_nonblocking", e))?;
                Ok(k)
            }) {
                Ok(k) => k,
                Err(e) => {
                    warn!("{}: cannot rebind to {}: {e}", s.label, sel.iface);
                    continue;
                }
            };
            if moved || s.no_lan {
                warn!(
                    "{}: network changed; now via {} — {}",
                    s.label, sel.iface, sel.why
                );
            } else {
                debug!("{}: sends failed; new socket on {}", s.label, sel.iface);
            }
            emergency::set(i, sock.as_raw_fd(), s.sender.dst);
            s.sock = sock;
            s.no_lan = false;
            s.net_down = false;
            if moved && let Some(mac) = &s.mac {
                verify.push(Pending {
                    dev: i,
                    ip: s.ip(),
                    mac: mac.clone(),
                    port: s.sender.dst.port(),
                });
            }
            s.iface = sel.iface;
            if self.live {
                s.razer(true);
            }
            changed = true;
        }
        if !verify.is_empty() && self.verify.is_none() {
            // One interface per verification run (several LANs at once is rare).
            let iface = self.devs[verify[0].dev].iface.clone();
            verify.retain(|p| self.devs[p.dev].iface.name == iface.name);
            match Verify::start(&iface, self.ports, verify, now) {
                Ok(v) => self.verify = Some(v),
                Err(e) => debug!("device IP re-check skipped: {e}"),
            }
        }
        changed
    }

    /// The verification socket is readable. True if a device moved (resend).
    pub fn on_verify_readable(&mut self) -> bool {
        let Some(v) = &mut self.verify else {
            return false;
        };
        let mut moved = Vec::new();
        v.on_readable(&mut moved);
        if v.done() {
            self.verify = None;
        }
        for m in &moved {
            let live = self.live;
            let s = &mut self.devs[m.dev];
            warn!(
                "{}: the light now answers at {} (was {}); following it. A DHCP reservation for the light avoids this",
                s.label, m.to, m.from
            );
            s.sender.dst.set_ip(m.to);
            emergency::set(m.dev, s.sock.as_raw_fd(), s.sender.dst);
            if live {
                s.razer(true);
            }
            if let Some(mac) = &s.mac {
                let mut cache = crate::devices::load_cache();
                if let Some(c) = cache.iter_mut().find(|c| &c.mac == mac) {
                    c.ip = m.to;
                    if let Err(e) = crate::devices::save_cache(&cache) {
                        debug!("cannot update the device cache: {e}");
                    }
                }
            }
        }
        !moved.is_empty()
    }
}

impl Drop for Streams {
    fn drop(&mut self) {
        for (i, s) in self.devs.iter().enumerate() {
            emergency::clear(i, s.sock.as_raw_fd());
        }
    }
}

/// Razer OFF from a panic hook: `panic = "abort"` skips `shutdown()`, so the
/// devices' fds and addresses live in a static table the hook reads (D-048).
mod emergency {
    use std::net::SocketAddrV4;
    use std::sync::Once;
    use std::sync::atomic::{AtomicI32, AtomicU16, AtomicU32, Ordering};

    const MAX: usize = 16;
    static FD: [AtomicI32; MAX] = [const { AtomicI32::new(-1) }; MAX];
    static IP: [AtomicU32; MAX] = [const { AtomicU32::new(0) }; MAX];
    static PORT: [AtomicU16; MAX] = [const { AtomicU16::new(0) }; MAX];

    pub fn set(i: usize, fd: i32, dst: SocketAddrV4) {
        if i < MAX {
            IP[i].store(u32::from(*dst.ip()), Ordering::SeqCst);
            PORT[i].store(dst.port(), Ordering::SeqCst);
            FD[i].store(fd, Ordering::SeqCst);
        }
    }

    /// Forget slot `i` if it still holds `fd` (a newer Streams may own it).
    pub fn clear(i: usize, fd: i32) {
        if i < MAX {
            let _ = FD[i].compare_exchange(fd, -1, Ordering::SeqCst, Ordering::SeqCst);
        }
    }

    pub fn send_off() {
        send_off_slots(0..MAX);
    }

    pub fn send_off_slots(slots: std::ops::Range<usize>) {
        let mut m = [0u8; 64];
        let n = crate::proto::razer_msg(&crate::proto::razer_mode_frame(false), &mut m);
        for _ in 0..2 {
            for i in slots.clone() {
                let fd = FD[i].load(Ordering::SeqCst);
                if fd < 0 {
                    continue;
                }
                let addr = libc::sockaddr_in {
                    sin_family: libc::AF_INET as libc::sa_family_t,
                    sin_port: PORT[i].load(Ordering::SeqCst).to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: IP[i].load(Ordering::SeqCst).to_be(),
                    },
                    sin_zero: [0; 8],
                };
                // SAFETY: fd is a live UDP socket of a Streams (or a stale number:
                // EBADF/ENOTSOCK, harmless); buffer and address are valid.
                unsafe {
                    libc::sendto(
                        fd,
                        m.as_ptr().cast(),
                        n,
                        0,
                        (&raw const addr).cast(),
                        std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    );
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Chain a hook that sends razer OFF ×2 before the default panic report.
    pub fn install() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                send_off();
                prev(info);
            }));
        });
    }
}

/// The earlier of two optional deadlines.
pub fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

// ------------------------------------------------------------------ control plane

/// The fds every long-running mode polls besides its own source: signals, one
/// timerfd for every deadline, and the rtnetlink change listener.
pub struct Ctl {
    pub sig: Signals,
    pub timer: Timer,
    pub net: Option<NetWatch>,
}

impl Ctl {
    /// Blocks INT/TERM/HUP/USR1/USR2 for normal delivery first, so a signal that
    /// arrives during setup waits in the signalfd instead of killing the process.
    pub fn new() -> Result<Ctl, Error> {
        let sig = Signals::new()?;
        let timer = Timer::new()?;
        let net = match NetWatch::open() {
            Ok(w) => Some(w),
            Err(e) => {
                warn!("cannot watch for network changes ({e}); sync will not rebind on its own");
                None
            }
        };
        Ok(Ctl { sig, timer, net })
    }

    /// The netlink fd, or -1.
    pub fn net_fd(&self) -> RawFd {
        self.net.as_ref().map_or(-1, NetWatch::fd)
    }

    /// Earliest network deadline: netlink debounce or `streams`' own.
    pub fn net_deadline(&self, streams: &Streams) -> Option<u64> {
        min_opt(
            self.net.as_ref().and_then(|w| w.debounce.due()),
            streams.net_deadline(),
        )
    }

    /// The netlink fd is readable.
    pub fn on_netlink(&mut self, now: u64) {
        if let Some(w) = &mut self.net {
            w.on_readable(now);
        }
    }

    /// Run network work due at `now`. True if a device was rebound or moved: the
    /// caller resends the current colors.
    pub fn service_net(&mut self, streams: &mut Streams, now: u64) -> bool {
        if let Some(w) = &mut self.net
            && w.debounce.take(now)
        {
            debug!("network changed: re-selecting LAN interfaces");
            streams.request_reselect(now);
        }
        streams.on_net_timer(now)
    }
}

// ------------------------------------------------------------------ screen pipeline

/// Devices sharing one segment count (and layout) share one zone set and smoother.
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
            let layout = s.layout.as_deref();
            match groups
                .iter_mut()
                .find(|g| g.sampler.zone_count() == n && g.sampler.layout() == layout)
            {
                Some(g) => g.devs.push(i),
                None => groups.push(Group {
                    sampler: match layout {
                        Some(l) => Sampler::with_layout(l, depth),
                        None => Sampler::new(n, depth),
                    },
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

    /// After razer ON (resume, output back, rebind): resend the current colors now.
    pub fn resend(&mut self, now: u64) {
        for g in &self.groups {
            for &i in &g.devs {
                self.streams.push(i, &g.out, now / MS);
            }
        }
    }

    /// Next step or keepalive deadline (ns).
    pub fn deadline(&self) -> Option<u64> {
        min_opt(
            self.step_at,
            self.streams.keepalive_due_ms().map(|t| t * MS),
        )
    }
}

// ------------------------------------------------------------------ screen loop

const BACKOFF_FIRST: u64 = 100 * MS;
const BACKOFF_NEXT: u64 = 1000 * MS;

/// `gowl screen`: run until a stop signal. Sends razer ON first and the shutdown
/// sequence (razer OFF ×2) on every exit path.
pub fn run_screen(cap: &mut dyn Capture, pipe: &mut Pipeline, ctl: &mut Ctl) -> Result<(), Error> {
    pipe.streams.razer(true);
    let r = screen_loop(cap, pipe, ctl);
    pipe.streams.shutdown();
    r
}

fn screen_loop(cap: &mut dyn Capture, pipe: &mut Pipeline, ctl: &mut Ctl) -> Result<(), Error> {
    let interval = pipe.interval_ns;
    let mut next_capture = Some(now_ns());
    let mut last_request = 0u64;
    let mut fails = 0u32;
    let mut paused = false;
    let mut waiting = false; // output gone
    let mut report_at = now_ns() + 1000 * MS;
    let (mut frames0, mut sent0, mut allocs0) = (0u64, 0u64, ALLOCS.load(Ordering::Relaxed));
    let mut wakes = [0u32; 5]; // wayland, timer, signal, netlink, verify (debug line)
    let pfd = |fd: RawFd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let mut pfds = [
        pfd(cap.fd()),
        pfd(ctl.timer.fd()),
        pfd(ctl.sig.fd()),
        pfd(ctl.net_fd()),
        pfd(-1),
    ];
    cap.set_interrupt_fd(ctl.sig.fd());
    loop {
        if let Err(e) = cap.flush() {
            // A stop signal aborts a send stuck on a compositor that stopped reading.
            if ctl.sig.read() == Some(Sig::Stop) {
                debug!("stop signal while the compositor was not reading: razer off, exit");
                return Ok(());
            }
            return Err(e);
        }
        let live = !paused && !waiting;
        let cap_dl = next_capture.filter(|_| live);
        // Paused or output gone: razer is OFF, so no fade steps and no keepalives.
        let pipe_dl = if live { pipe.deadline() } else { None };
        let dl = min_opt(min_opt(cap_dl, pipe_dl), ctl.net_deadline(&pipe.streams));
        ctl.timer.arm(dl)?;
        pfds[4].fd = pipe.streams.verify_fd();
        // D-051: while a prepared frame waits for its slot, don't wake for its offers
        // (or anything else the compositor says); the timer reads them all at once.
        // Only while a capture deadline is armed (live, slot ahead): otherwise the fd is
        // polled, so an error/hang-up or output event is never left waiting.
        let held = cap_dl.is_some() && cap.held();
        pfds[0].fd = if held { -1 } else { cap.fd() };
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
            while let Some(s) = ctl.sig.read() {
                match s {
                    Sig::Stop => {
                        debug!("stop signal: razer off, exit");
                        return Ok(());
                    }
                    Sig::Pause if !paused => {
                        debug!("paused");
                        paused = true;
                        // Frees the 19.8 MB capture memfd too (niri's share of it).
                        cap.release()?;
                        next_capture = None;
                        pipe.streams.off();
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

        if pfds[3].revents != 0 {
            ctl.on_netlink(now);
        }
        if pfds[4].revents != 0 && pipe.streams.on_verify_readable() && !paused && !waiting {
            pipe.resend(now);
        }

        if pfds[0].revents != 0 || (held && pfds[1].revents != 0) {
            let st = cap.dispatch()?;
            {
                if st.lost {
                    warn!("output gone; lights off until it comes back");
                    waiting = true;
                    next_capture = None;
                    cap.release()?;
                    if !paused {
                        pipe.streams.off();
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
                    let live = !paused && !waiting;
                    let at = last_request + interval;
                    // D-051: negotiate the next frame now, so its offers arrive while
                    // this one is sampled; the copy waits for the slot (timer).
                    if live
                        && at > now
                        && cap.prepare()?
                        && let Err(e) = cap.flush()
                    {
                        if ctl.sig.read() == Some(Sig::Stop) {
                            debug!(
                                "stop signal while the compositor was not reading: razer off, exit"
                            );
                            return Ok(());
                        }
                        return Err(e);
                    }
                    if let Some(f) = cap.frame() {
                        pipe.on_frame(&f, now);
                    }
                    if live {
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
                // Offers for a prepared frame are read at the timer wakeup (the fd is
                // not polled while held), not here: a dispatch right after sampling
                // found nothing in practice and cost one EAGAIN recvmsg per frame.
            }
        }

        if pfds[1].revents != 0 {
            ctl.timer.drain();
            let live = !paused && !waiting;
            if live && next_capture.is_some_and(|t| t <= now) {
                next_capture = None;
                if cap.request_next()? {
                    last_request = now;
                }
            }
            if live {
                pipe.on_timer(now);
            }
            // Rebinds happen paused too (razer ON waits for resume); resend only live.
            if ctl.service_net(&mut pipe.streams, now) && live {
                pipe.resend(now);
            }
        }

        if now >= report_at && crate::log::enabled(crate::log::DEBUG) {
            let allocs = ALLOCS.load(Ordering::Relaxed);
            debug!(
                "last {:.1} s: {} frames, {} datagrams, wakeups wayland {} timer {} signal {} netlink {} verify {}{}",
                (now - report_at + 1000 * MS) as f64 / 1e9,
                pipe.frames - frames0,
                pipe.streams.sent - sent0,
                wakes[0],
                wakes[1],
                wakes[2],
                wakes[3],
                wakes[4],
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
            wakes = [0; 5];
            report_at = now + 1000 * MS;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::SegSource;
    use std::net::SocketAddr;

    fn razer_on() -> Vec<u8> {
        let mut m = [0u8; 64];
        let n = crate::proto::razer_msg(&crate::proto::razer_mode_frame(true), &mut m);
        m[..n].to_vec()
    }

    fn setup() -> (Streams, UdpSocket, Vec<Iface>) {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
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
            segments: 4,
            seg_source: SegSource::Default,
            reverse: false,
            offset: 0,
            layout: None,
            port: None,
        };
        let s = Streams::open(&[dev], None, ports, 3, 500, true).unwrap();
        (s, rx, net::interfaces().unwrap())
    }

    #[test]
    fn reselect_rebinds_only_on_change_and_resends_razer_on_when_live() {
        let (mut s, rx, ifaces) = setup();
        let mut buf = [0u8; 256];
        // Same interfaces: nothing happens, nothing is sent.
        assert!(!s.reselect_with(&ifaces, 0));
        assert!(rx.recv(&mut buf).is_err());
        // The loopback address changed (stand-in for a DHCP renumber / roam), not live.
        let mut moved = ifaces.clone();
        for i in moved.iter_mut().filter(|i| i.is_loopback()) {
            i.addr = Ipv4Addr::new(127, 0, 0, 5);
        }
        assert!(s.reselect_with(&moved, 0));
        assert!(rx.recv(&mut buf).is_err(), "paused: no razer ON yet");
        // Live: a rebind sends razer ON on the new socket, and the next push goes out.
        s.razer(true);
        let (n, _) = rx.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], &razer_on()[..]);
        assert!(s.reselect_with(&ifaces, 0));
        let (n, _) = rx.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], &razer_on()[..]);
        assert!(s.push(0, &[[9, 9, 9]; 4], 1));
        // No LAN interface at all: keep the old socket, log once, rebind when back.
        assert!(!s.reselect_with(&[], 0));
        assert!(s.devs[0].no_lan);
        assert!(s.reselect_with(&ifaces, 0));
        assert!(!s.devs[0].no_lan);
    }

    #[test]
    fn network_errors_schedule_a_backed_off_reselect() {
        let (mut s, _rx, _) = setup();
        assert_eq!(s.net_deadline(), None);
        s.devs[0].net_down = true;
        assert!(s.after_send(Sent::Failed, 1000));
        assert_eq!(s.net_deadline(), Some(1000 * MS + 250 * MS));
        s.reselect_at = None;
        s.after_send(Sent::Failed, 2000);
        assert_eq!(s.net_deadline(), Some(2000 * MS + 1000 * MS));
        s.reselect_at = None;
        s.after_send(Sent::Failed, 3000);
        s.reselect_at = None;
        s.after_send(Sent::Failed, 9000);
        assert_eq!(
            s.net_deadline(),
            Some(9000 * MS + 5000 * MS),
            "capped at 5 s"
        );
        // A send that works again resets the back-off.
        s.devs[0].net_down = false;
        s.after_send(Sent::Ok, 9500);
        assert_eq!(s.backoff, 0);
        // The netlink debounce asks for an earlier reselect: the earlier one wins.
        s.request_reselect(10 * MS);
        assert_eq!(s.net_deadline(), Some(10 * MS));
    }

    #[test]
    fn panic_path_sends_razer_off_twice_from_the_static_table() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let SocketAddr::V4(dst) = rx.local_addr().unwrap() else {
            unreachable!()
        };
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        emergency::set(15, tx.as_raw_fd(), dst);
        emergency::send_off_slots(15..16); // only our slot: other tests run in parallel
        emergency::clear(15, tx.as_raw_fd());
        let mut off = [0u8; 64];
        let n = crate::proto::razer_msg(&crate::proto::razer_mode_frame(false), &mut off);
        let mut buf = [0u8; 128];
        for _ in 0..2 {
            let k = rx.recv(&mut buf).unwrap();
            assert_eq!(&buf[..k], &off[..n]);
        }
    }
}
