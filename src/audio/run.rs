//! `gowl music`: blocking read of 1024 samples from the default sink's monitor →
//! FFT → bands → AutoGain → render → EMA → brightness → reverse/offset → delta
//! check → send (AUDIO.md, ARCHITECTURE.md "Music mode"). The blocking read is the
//! pacing: no timers, no sleeps (except a 1 s back-off while the sound server is
//! unreachable). Renders are capped at min(music_fps, 40 Hz device cap) with a
//! deadline accumulator; windows in between are read and dropped.
//!
//! Signals: SIGINT/SIGTERM/SIGHUP set an `AtomicBool` (sigaction without
//! SA_RESTART); the flag is checked after every read (≤ 21 ms). If the read ever
//! stalls, a 1 s `alarm` watchdog sends razer OFF from the handler and exits.

use std::ffi::c_int;
use std::net::{SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::bands::{Analyzer, AutoGain, SAMPLE_RATE};
use super::fft::N;
use super::pulse::{Pulse, Stream};
use super::render::{Palette, Reaction, Renderer};
use crate::Error;
use crate::color::{self, Rgb, SendGate, Smoother};
use crate::config::{self, Config};
use crate::devices::{self, Device};
use crate::net::{self, Ports, Sender};
use crate::proto::MAX_SEGMENTS;

/// Legacy `max_update_hz` for Govee LAN devices.
pub const DEVICE_CAP_HZ: u32 = 40;
/// Reopen the stream after this much continuous digital silence (default sink may
/// have changed; `@DEFAULT_MONITOR@` doesn't follow an open stream everywhere).
const SILENCE_REOPEN_WINDOWS: u32 = 30 * SAMPLE_RATE / N as u32;
const SOURCE: &std::ffi::CStr = c"@DEFAULT_MONITOR@";

// ------------------------------------------------------------------ pipeline

/// Music settings (config + CLI), resolved once.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub reaction: Reaction,
    pub palette: Palette,
    pub gain: f64,
    pub brightness: f64,
    pub smoothing: f64,
    pub threshold: u8,
    pub keepalive_ms: u64,
    pub fps: u32,
}

impl Settings {
    pub fn from_config(c: &Config) -> Result<Settings, Error> {
        Ok(Settings {
            reaction: parse_reaction(&c.reaction)?,
            palette: parse_palette(&c.palette)?,
            gain: c.music_gain,
            brightness: c.music_brightness,
            smoothing: c.music_smoothing,
            threshold: c.threshold,
            keepalive_ms: u64::from(c.keepalive_ms),
            fps: c.music_fps.clamp(1, 240),
        })
    }

    /// Render/send interval in µs: min(fps, device cap).
    pub fn interval_us(&self) -> u64 {
        1_000_000 / u64::from(self.fps.clamp(1, DEVICE_CAP_HZ))
    }
}

fn parse_reaction(s: &str) -> Result<Reaction, Error> {
    Reaction::parse(s).ok_or_else(|| {
        let names: Vec<&str> = Reaction::ALL.iter().map(|e| e.0).collect();
        Error::Usage(format!(
            "unknown reaction {s:?} (one of: {})",
            names.join(" ")
        ))
    })
}

fn parse_palette(s: &str) -> Result<Palette, Error> {
    Palette::parse(s).ok_or_else(|| {
        let names: Vec<&str> = Palette::ALL.iter().map(|e| e.0).collect();
        let extra = if s == "album_art" {
            " — album_art is not supported"
        } else {
            ""
        };
        Error::Usage(format!(
            "unknown palette {s:?}{extra} (one of: {})",
            names.join(" ")
        ))
    })
}

/// One streaming device: its renderer (segment count), smoother, send gate, socket.
pub struct Lane {
    pub label: String,
    sock: UdpSocket,
    sender: Sender,
    renderer: Renderer,
    smoother: Smoother,
    gate: SendGate,
    reverse: bool,
    offset: usize,
    smooth: [Rgb; MAX_SEGMENTS],
    out: [Rgb; MAX_SEGMENTS],
    pub sent: u64,
    last_err_ms: Option<u64>,
}

impl Lane {
    /// `sock` must already be bound to the LAN interface (net::bound_socket).
    pub fn new(
        label: String,
        sock: UdpSocket,
        dst: SocketAddrV4,
        segments: usize,
        reverse: bool,
        offset: usize,
        s: &Settings,
    ) -> Lane {
        let sender = Sender::new(dst, segments);
        let n = sender.segments();
        Lane {
            label,
            sock,
            sender,
            renderer: Renderer::new(n, s.fps),
            smoother: Smoother::new(s.smoothing, n),
            gate: SendGate::new(s.threshold, s.keepalive_ms),
            reverse,
            offset: offset % n,
            smooth: [[0; 3]; MAX_SEGMENTS],
            out: [[0; 3]; MAX_SEGMENTS],
            sent: 0,
            last_err_ms: None,
        }
    }

    pub fn razer(&self, on: bool) -> std::io::Result<()> {
        self.sender.razer_mode(&self.sock, on)
    }

    fn fd(&self) -> c_int {
        use std::os::fd::AsRawFd;
        self.sock.as_raw_fd()
    }
}

/// Everything between "1024 samples" and "datagram sent". No allocation per tick.
pub struct Music {
    analyzer: Analyzer,
    autogain: AutoGain,
    settings: Settings,
    bri_q16: u32,
    pub lanes: Vec<Lane>,
    /// Last bands after AutoGain (debug output).
    pub last_bands: [f64; 3],
}

impl Music {
    pub fn new(settings: Settings, lanes: Vec<Lane>) -> Music {
        Music {
            analyzer: Analyzer::new(SAMPLE_RATE),
            autogain: AutoGain::new(settings.fps),
            settings,
            bri_q16: color::q16(settings.brightness.clamp(0.0, 1.0)),
            lanes,
            last_bands: [0.0; 3],
        }
    }

    /// One render tick; returns how many datagrams were sent.
    #[inline]
    pub fn tick(&mut self, samples: &[f32; N], now_ms: u64) -> usize {
        let s = self.settings;
        let bands = self.autogain.process(self.analyzer.bands(samples));
        self.last_bands = bands;
        let mut sent = 0;
        for l in &mut self.lanes {
            let n = l.sender.segments();
            let frame = l.renderer.render(bands, s.reaction, s.gain, s.palette);
            l.smoother.update(frame, &mut l.smooth[..n]);
            color::scale(&mut l.smooth[..n], self.bri_q16);
            color::reorder(&l.smooth[..n], l.reverse, l.offset, &mut l.out[..n]);
            if !l.gate.needs_send(&l.out[..n], now_ms) {
                continue;
            }
            match l.sender.send_colors(&l.sock, &l.out[..n]) {
                Ok(()) => {
                    l.gate.mark_sent(&l.out[..n], now_ms);
                    l.sent += 1;
                    sent += 1;
                }
                Err(e) => {
                    if l.last_err_ms
                        .is_none_or(|t| now_ms.saturating_sub(t) >= 5000)
                    {
                        warn!(
                            "send to {} failed: {e} (retrying; see `gowl doctor`)",
                            l.label
                        );
                        l.last_err_ms = Some(now_ms);
                    }
                }
            }
        }
        sent
    }

    /// Razer ON again after an outage, and force the next frame out.
    fn restart_stream(&mut self) {
        for l in &mut self.lanes {
            let _ = l.razer(true);
            l.gate.reset();
        }
    }
}

// ------------------------------------------------------------------ signals

static STOP: AtomicBool = AtomicBool::new(false);

const MAX_EMERG: usize = 8;
static EMERG_N: AtomicUsize = AtomicUsize::new(0);
static EMERG_FD: [AtomicI32; MAX_EMERG] = [const { AtomicI32::new(-1) }; MAX_EMERG];
static EMERG_IP: [AtomicU32; MAX_EMERG] = [const { AtomicU32::new(0) }; MAX_EMERG];
static EMERG_PORT: [AtomicU16; MAX_EMERG] = [const { AtomicU16::new(0) }; MAX_EMERG];

/// `razer` OFF datagram (constant; checked against proto in a test).
pub const RAZER_OFF_MSG: &[u8] = br#"{"msg":{"cmd":"razer","data":{"pt":"uwABsQAL"}}}"#;

extern "C" fn on_stop(_: c_int) {
    STOP.store(true, Ordering::SeqCst);
    // SAFETY: alarm is async-signal-safe.
    unsafe { libc::alarm(1) };
}

/// Watchdog: the read did not return within 1 s of a stop signal. Only
/// async-signal-safe calls: sendto + _exit.
extern "C" fn on_alarm(_: c_int) {
    let n = EMERG_N.load(Ordering::SeqCst).min(MAX_EMERG);
    for i in 0..n {
        let fd = EMERG_FD[i].load(Ordering::SeqCst);
        let addr = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: EMERG_PORT[i].load(Ordering::SeqCst).to_be(),
            sin_addr: libc::in_addr {
                s_addr: EMERG_IP[i].load(Ordering::SeqCst).to_be(),
            },
            sin_zero: [0; 8],
        };
        // SAFETY: fd is a live UDP socket (or -1 → EBADF); buffer and address are valid.
        unsafe {
            libc::sendto(
                fd,
                RAZER_OFF_MSG.as_ptr().cast(),
                RAZER_OFF_MSG.len(),
                0,
                (&raw const addr).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            );
        }
    }
    // SAFETY: _exit is async-signal-safe.
    unsafe { libc::_exit(0) };
}

fn install(sig: c_int, handler: extern "C" fn(c_int)) -> Result<(), Error> {
    // SAFETY: zeroed sigaction is valid; the handler only touches atomics and
    // async-signal-safe functions. No SA_RESTART, so blocking syscalls return EINTR.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as usize;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
            return Err(Error::Io("sigaction", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

fn install_signals(lanes: &[Lane]) -> Result<(), Error> {
    for (i, l) in lanes.iter().take(MAX_EMERG).enumerate() {
        EMERG_FD[i].store(l.fd(), Ordering::SeqCst);
        EMERG_IP[i].store(u32::from(*l.sender.dst.ip()), Ordering::SeqCst);
        EMERG_PORT[i].store(l.sender.dst.port(), Ordering::SeqCst);
    }
    EMERG_N.store(lanes.len().min(MAX_EMERG), Ordering::SeqCst);
    install(libc::SIGALRM, on_alarm)?;
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        install(sig, on_stop)?;
    }
    Ok(())
}

fn stopping() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Sleep up to `d`; returns early on a signal (nanosleep → EINTR).
fn backoff(d: Duration) {
    let ts = libc::timespec {
        tv_sec: d.as_secs() as libc::time_t,
        tv_nsec: d.subsec_nanos() as libc::c_long,
    };
    // SAFETY: valid timespec; remaining time not needed.
    unsafe { libc::nanosleep(&ts, std::ptr::null_mut()) };
}

// ------------------------------------------------------------------ command

/// `gowl music [--reaction R] [--palette P] [--fps N] [DEV...]`
pub fn cmd_music(args: &[String]) -> Result<(), Error> {
    let cfg = config::load();
    let mut s = Settings::from_config(&cfg)?;
    let mut specs: Vec<&str> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |what: &str| {
            it.next()
                .map(String::as_str)
                .ok_or_else(|| Error::Usage(format!("music: {a} expects {what}")))
        };
        match a.as_str() {
            "--reaction" => s.reaction = parse_reaction(val("a reaction")?)?,
            "--palette" => s.palette = parse_palette(val("a palette")?)?,
            "--fps" => {
                s.fps = val("1-240")?
                    .parse::<u32>()
                    .ok()
                    .filter(|f| (1..=240).contains(f))
                    .ok_or_else(|| Error::Usage("music: --fps expects 1-240".into()))?;
            }
            o if o.starts_with("--") => {
                return Err(Error::Usage(format!("music: unknown option {o}")));
            }
            dev => specs.push(dev),
        }
    }

    let cache = devices::load_cache();
    let ports = Ports::from_env();
    let mut devs: Vec<Device> = Vec::new();
    if specs.is_empty() {
        devs.push(devices::resolve(None, &cfg, &cache)?);
    }
    for sp in specs {
        let d = devices::resolve(Some(sp), &cfg, &cache)?;
        if !devs.iter().any(|x| x.ip == d.ip) {
            devs.push(d);
        }
    }
    let ifaces = net::interfaces().map_err(|e| Error::Io("getifaddrs", e))?;
    let mut lanes = Vec::with_capacity(devs.len());
    for d in &devs {
        let sel = net::select_for(&ifaces, d.ip, cfg.lan_if.as_deref())?;
        debug!(
            "music → {} ({} segments, {:?}) via {} — {}",
            d.label(),
            d.segments,
            d.seg_source,
            sel.iface,
            sel.why
        );
        let sock = net::bound_socket(&sel.iface, None)?;
        lanes.push(Lane::new(
            d.label(),
            sock,
            SocketAddrV4::new(d.ip, ports.control),
            usize::from(d.segments),
            d.reverse,
            usize::from(d.offset),
            &s,
        ));
    }
    debug!(
        "music: reaction {} palette {} fps {} (renders ≤ {} Hz) gain {} brightness {} smoothing {}",
        s.reaction.name(),
        s.palette.name(),
        s.fps,
        1_000_000 / s.interval_us(),
        s.gain,
        s.brightness,
        s.smoothing
    );

    let pulse = Pulse::load()?;
    install_signals(&lanes)?;
    let mut music = Music::new(s, lanes);
    for l in &music.lanes {
        l.razer(true).map_err(|e| {
            if net::is_blocked(&e) {
                Error::Runtime(format!(
                    "sending to {} was refused ({e}): a VPN kill switch blocks LAN traffic; allow LAN access in the VPN (see `gowl doctor`)",
                    l.label
                ))
            } else {
                Error::Io("razer on", e)
            }
        })?;
    }
    let r = stream_loop(&pulse, &mut music);
    // Shutdown contract: razer OFF twice, 10 ms apart.
    for round in 0..2 {
        if round == 1 {
            std::thread::sleep(Duration::from_millis(10));
        }
        for l in &music.lanes {
            if let Err(e) = l.razer(false) {
                debug!("razer off to {}: {e}", l.label);
            }
        }
    }
    r
}

fn open_stream(pulse: &Pulse) -> Option<Stream<'_>> {
    let mut warned = false;
    while !stopping() {
        match pulse.open(SOURCE, SAMPLE_RATE, (N * 4) as u32) {
            Ok(st) => {
                if warned {
                    warn!("audio capture is back");
                }
                return Some(st);
            }
            Err(e) => {
                if !warned {
                    warn!(
                        "cannot open {}: {e}; retrying every second",
                        SOURCE.to_string_lossy()
                    );
                    warned = true;
                }
                backoff(Duration::from_secs(1));
            }
        }
    }
    None
}

fn stream_loop(pulse: &Pulse, music: &mut Music) -> Result<(), Error> {
    let Some(mut stream) = open_stream(pulse) else {
        return Ok(());
    };
    debug!(
        "capturing {} (float32 mono {} Hz)",
        SOURCE.to_string_lossy(),
        SAMPLE_RATE
    );
    let mut samples = [0f32; N];
    let interval = music.settings.interval_us();
    let start = Instant::now();
    let mut next_us = 0u64;
    let mut silent = 0u32;
    let verbose = crate::log::enabled(crate::log::DEBUG);
    let (mut windows, mut renders, mut sends, mut stat_at) = (0u32, 0u32, 0u32, 0u64);

    while !stopping() {
        if let Err(e) = stream.read(&mut samples) {
            if stopping() {
                break;
            }
            warn!("audio read failed: {e}; reopening");
            drop(stream);
            match open_stream(pulse) {
                Some(st) => stream = st,
                None => break,
            }
            music.restart_stream();
            silent = 0;
            continue;
        }
        if stopping() {
            break;
        }
        windows += 1;
        if samples.iter().all(|&v| v == 0.0) {
            silent += 1;
            if silent >= SILENCE_REOPEN_WINDOWS {
                debug!(
                    "30 s of silence: reopening the monitor stream (default sink may have changed)"
                );
                drop(stream);
                match open_stream(pulse) {
                    Some(st) => stream = st,
                    None => break,
                }
                silent = 0;
            }
        } else {
            silent = 0;
        }

        let now_us = start.elapsed().as_micros() as u64;
        if now_us < next_us {
            continue;
        }
        // Deadline accumulator: average rate = 1/interval, no burst after a stall.
        next_us = if now_us - next_us >= interval {
            now_us + interval
        } else {
            next_us + interval
        };
        renders += 1;
        sends += music.tick(&samples, now_us / 1000) as u32;

        if verbose && now_us / 1000 >= stat_at + 1000 {
            let b = music.last_bands;
            debug!(
                "music: {windows} windows, {renders} renders, {sends} datagrams in the last second; bands {:.3} {:.3} {:.3}",
                b[0], b[1], b[2]
            );
            (windows, renders, sends, stat_at) = (0, 0, 0, now_us / 1000);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn razer_off_constant_matches_proto() {
        let mut m = [0u8; 64];
        let n = crate::proto::razer_msg(&crate::proto::razer_mode_frame(false), &mut m);
        assert_eq!(&m[..n], RAZER_OFF_MSG);
    }

    #[test]
    fn interval_caps_at_device_rate() {
        let mut s = Settings::from_config(&Config::default()).unwrap();
        assert_eq!(s.interval_us(), 25_000);
        s.fps = 20;
        assert_eq!(s.interval_us(), 50_000);
    }
}
