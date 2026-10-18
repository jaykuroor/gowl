//! `gowl music`: blocking read of 1024 samples from the default sink's monitor →
//! FFT → bands → AutoGain → render → EMA → brightness → reverse/offset → delta
//! check → send (AUDIO.md, ARCHITECTURE.md "Music mode"). The blocking read is the
//! pacing: no periodic timers, no sleeps. Renders are capped at min(music_fps, 40 Hz
//! device cap) with a deadline accumulator; windows in between are read and dropped.
//!
//! One `poll()`: libpulse's mainloop polls its own fds plus ours (signalfd, timerfd,
//! rtnetlink, verify socket) through `pulse::Hook` (D-042). SIGUSR1/SIGUSR2 pause and
//! resume (stream closed, razer OFF, zero wakeups while paused), SIGINT/SIGTERM/SIGHUP
//! stop (razer OFF ×2). While the sound server is unreachable the stream is retried
//! every second on the timerfd and the last frame is kept alive.

use super::bands::{Analyzer, AutoGain, SAMPLE_RATE};
use super::fft::N;
use super::pulse::{Capture, HOOK_ALL, Hook, Pulse, ReadError};
use super::render::{Palette, Reaction, Renderer};
use crate::Error;
use crate::color::{self, Rgb, Smoother};
use crate::config::{self, Config};
use crate::control::Pidfile;
use crate::devices;
use crate::net::Ports;
use crate::proto::MAX_SEGMENTS;
use crate::runtime::{Ctl, Sig, Streams, min_opt, now_ns};
use crate::state::{Mode, Problem, Writer};

/// The clock the music loop renders, sends and keeps alive on: CLOCK_MONOTONIC in
/// µs, the same clock as `now_ns()` (keepalive, fade, timerfd). Phase 5 rendered on
/// `Instant` since loop start, so every sent frame looked ~keepalive old and was
/// resent at once — every frame twice (review fix, D-064).
#[inline]
pub fn render_clock_us() -> u64 {
    now_ns() / 1000
}

/// Legacy `max_update_hz` for Govee LAN devices.
pub const DEVICE_CAP_HZ: u32 = 40;
const SOURCE: &std::ffi::CStr = c"@DEFAULT_MONITOR@";

const SEC: u64 = 1_000_000_000;
/// Deadlines below are rounded to this grid so windows arriving 47 times a second
/// don't re-arm the timerfd each time.
const GRID_NS: u64 = SEC / 2;
/// Black this long (+ up to one grid step) after the last window: audio stopped —
/// song paused, sink suspended or moved (D-054; the passive stream delivers nothing
/// then, so nothing would fade).
const BLACK_AFTER_NS: u64 = SEC + SEC / 2;
/// Re-resolve `@DEFAULT_MONITOR@` (restart the stream on the same connection) after
/// this long without sound (no windows, or only digital silence); doubles up to
/// `QUIET_REOPEN_MAX_NS` while it stays quiet, back to the start on sound (D-054).
const QUIET_REOPEN_NS: u64 = 30 * SEC;
const QUIET_REOPEN_MAX_NS: u64 = 300 * SEC;
/// Sound server unreachable: retry after 1 s, doubling to 30 s (D-054).
const RETRY_MAX_NS: u64 = 30 * SEC;
/// Sound server unreachable this long (e.g. the session ended under a
/// toggle-started `gowl music`): give up and exit (D-064).
pub const SERVER_GONE_EXIT_NS: u64 = 600 * SEC;

/// What the idle bookkeeping wants done at a timer wakeup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuietAction {
    None,
    /// Send black (silence) to every device.
    Blank,
    /// Restart the stream (follow a changed default sink).
    Restart,
}

/// Idle bookkeeping for the open stream (D-052, D-054): black ~1 s after the last
/// window, restart after a long time without sound (backing off). Pure; times in ns.
#[derive(Clone, Copy, Debug)]
pub struct Quiet {
    /// Last window, or the stream (re)open.
    heard_at: u64,
    /// Last window with sound (not all zeros), or the last (re)open / restart.
    sound_at: u64,
    /// Black was sent since the last window.
    blanked: bool,
    reopen_after: u64,
}

impl Quiet {
    pub fn new(now: u64) -> Quiet {
        Quiet {
            heard_at: now,
            sound_at: now,
            blanked: false,
            reopen_after: QUIET_REOPEN_NS,
        }
    }

    /// Stream opened or restarted: black if nothing arrives; keeps the back-off.
    pub fn opened(&mut self, now: u64) {
        self.heard_at = now;
        self.sound_at = now;
        self.blanked = false;
    }

    /// A full window arrived (`silent` = all zeros).
    #[inline]
    pub fn heard(&mut self, now: u64, silent: bool) {
        self.heard_at = now;
        self.blanked = false;
        if !silent {
            self.sound_at = now;
            self.reopen_after = QUIET_REOPEN_NS;
        }
    }

    pub fn blanked(&self) -> bool {
        self.blanked
    }

    fn grid(t: u64) -> u64 {
        t - t % GRID_NS
    }

    /// When to send black, if not done yet: 1.5–2.0 s after the last window.
    pub fn black_due(&self) -> Option<u64> {
        (!self.blanked).then(|| Quiet::grid(self.heard_at) + GRID_NS + BLACK_AFTER_NS)
    }

    /// When to restart the stream for lack of sound.
    pub fn restart_due(&self) -> u64 {
        Quiet::grid(self.sound_at) + GRID_NS + self.reopen_after
    }

    pub fn deadline(&self) -> u64 {
        self.black_due()
            .map_or(self.restart_due(), |b| b.min(self.restart_due()))
    }

    /// Timer wakeup at `now`.
    pub fn on_timer(&mut self, now: u64) -> QuietAction {
        if now >= self.restart_due() {
            self.reopen_after = (self.reopen_after * 2).min(QUIET_REOPEN_MAX_NS);
            self.sound_at = now;
            return QuietAction::Restart;
        }
        if self.black_due().is_some_and(|t| now >= t) {
            self.blanked = true;
            return QuietAction::Blank;
        }
        QuietAction::None
    }
}

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
    /// Fade to black over this long once audio stops (0 = instant black).
    pub fade_ms: u32,
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
            fade_ms: c.music_fade_ms.min(10_000),
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

/// Per-device render state: renderer (segment count) and smoother.
struct Voice {
    renderer: Renderer,
    smoother: Smoother,
    /// Last output (after brightness): what the light shows.
    smooth: [Rgb; MAX_SEGMENTS],
    /// The output when a fade started (D-063).
    from: [Rgb; MAX_SEGMENTS],
}

/// Fade timer steps: 30 ms ≈ 33 Hz, under the 40 Hz device cap.
pub const FADE_STEP_NS: u64 = 30_000_000;

/// A running fade to black (D-063): times in ns, level in Q16.
#[derive(Clone, Copy, Debug)]
struct Fade {
    start: u64,
    next: u64,
    level_q16: u32,
}

/// Ease-out level (1 − p)² in Q16 for `elapsed` of `dur` (µs, dur ≤ 10 s, so the
/// square fits u64): fast at first, gently into black.
pub fn fade_level_q16(elapsed_us: u64, dur_us: u64) -> u32 {
    if dur_us == 0 || elapsed_us >= dur_us {
        return 0;
    }
    let r = dur_us - elapsed_us;
    ((r * r * 65536) / (dur_us * dur_us)) as u32
}

/// Everything between "1024 samples" and "datagram sent". No allocation per tick.
/// Sending (reorder, gate, socket, rebind) is the shared `runtime::Streams`.
pub struct Music {
    analyzer: Analyzer,
    autogain: AutoGain,
    settings: Settings,
    bri_q16: u32,
    pub streams: Streams,
    voices: Vec<Voice>,
    /// Last bands after AutoGain (debug output).
    pub last_bands: [f64; 3],
    fade: Option<Fade>,
}

impl Music {
    pub fn new(settings: Settings, streams: Streams) -> Music {
        let voices = (0..streams.devs.len())
            .map(|i| {
                let n = streams.segments(i);
                Voice {
                    renderer: Renderer::new(n, settings.fps),
                    smoother: Smoother::new(settings.smoothing, n),
                    smooth: [[0; 3]; MAX_SEGMENTS],
                    from: [[0; 3]; MAX_SEGMENTS],
                }
            })
            .collect();
        Music {
            analyzer: Analyzer::new(SAMPLE_RATE),
            autogain: AutoGain::new(settings.fps),
            settings,
            bri_q16: color::q16(settings.brightness.clamp(0.0, 1.0)),
            streams,
            voices,
            last_bands: [0.0; 3],
            fade: None,
        }
    }

    /// One render tick; returns how many datagrams were sent.
    #[inline]
    pub fn tick(&mut self, samples: &[f32; N], now_ms: u64) -> usize {
        let s = self.settings;
        let bands = self.autogain.process(self.analyzer.bands(samples));
        self.last_bands = bands;
        let mut sent = 0;
        for (i, v) in self.voices.iter_mut().enumerate() {
            let n = self.streams.segments(i);
            let frame = v.renderer.render(bands, s.reaction, s.gain, s.palette);
            v.smoother.update(frame, &mut v.smooth[..n]);
            color::scale(&mut v.smooth[..n], self.bri_q16);
            sent += usize::from(self.streams.push(i, &v.smooth[..n], now_ms));
        }
        sent
    }

    /// Black on every device (what silence renders to); smoothing restarts from
    /// black, like the light. The gate skips devices already showing black.
    pub fn blank(&mut self, now_ms: u64) {
        self.fade = None;
        let black = [[0u8; 3]; MAX_SEGMENTS];
        for (i, v) in self.voices.iter_mut().enumerate() {
            let n = self.streams.segments(i);
            v.smoother.reset(n);
            v.smooth[..n].fill([0; 3]);
            self.streams.push_exact(i, &black[..n], now_ms);
        }
    }

    /// Audio stopped (D-063): fade the last output to black over `fade_ms` with an
    /// ease-out, on timer steps (`fade_due`); `fade_ms = 0` = black now (D-054).
    pub fn fade_start(&mut self, now: u64) {
        let dark = self.voices.iter().enumerate().all(|(i, v)| {
            v.smooth[..self.streams.segments(i)]
                .iter()
                .all(|c| *c == [0; 3])
        });
        if self.settings.fade_ms == 0 || dark {
            // Nothing to fade (silence already rendered black): no 33 steps.
            self.blank(now / 1_000_000);
            return;
        }
        for v in &mut self.voices {
            v.from = v.smooth;
        }
        self.fade = Some(Fade {
            start: now,
            next: now + FADE_STEP_NS,
            level_q16: 65536,
        });
    }

    pub fn fading(&self) -> bool {
        self.fade.is_some()
    }

    /// Next fade step (ns), while fading.
    pub fn fade_due(&self) -> Option<u64> {
        self.fade.map(|f| f.next)
    }

    /// One fade step at `now`: the start output × level, through the normal gate;
    /// the last step sends exact black and ends the fade (true).
    pub fn fade_step(&mut self, now: u64) -> bool {
        let Some(f) = self.fade.as_mut() else {
            return true;
        };
        let el_us = now.saturating_sub(f.start) / 1000;
        let dur_us = u64::from(self.settings.fade_ms) * 1000;
        let level = fade_level_q16(el_us, dur_us);
        if level == 0 {
            self.blank(now / 1_000_000);
            return true;
        }
        f.level_q16 = level;
        f.next = now + FADE_STEP_NS;
        for (i, v) in self.voices.iter_mut().enumerate() {
            let n = self.streams.segments(i);
            v.smooth[..n].copy_from_slice(&v.from[..n]);
            color::scale(&mut v.smooth[..n], level);
            self.streams.push(i, &v.smooth[..n], now / 1_000_000);
        }
        false
    }

    /// Audio is back mid-fade: stop fading at once; the smoother carries on from
    /// the dimmed output instead of jumping back to the old one.
    pub fn cancel_fade(&mut self) {
        if let Some(f) = self.fade.take() {
            for v in &mut self.voices {
                v.smoother.scale(f.level_q16);
            }
        }
    }

    /// Paused mid-fade: forget it (razer is OFF; smoothing restarts from black).
    pub fn drop_fade(&mut self) {
        if self.fade.take().is_some() {
            for (i, v) in self.voices.iter_mut().enumerate() {
                let n = self.streams.segments(i);
                v.smoother.reset(n);
                v.smooth[..n].fill([0; 3]);
            }
        }
    }
}

// ------------------------------------------------------------------ command

/// `gowl music [--reaction R] [--palette P] [--fps N] [DEV...]`. `announce`: the
/// notification to show once setup worked (toggle).
pub fn cmd_music(args: &[String], announce: Option<&str>) -> Result<(), Error> {
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
    let devs = devices::resolve_many(&specs, &cfg, &cache)?;
    // Signals blocked before anything streams; then claim the pidfile.
    let mut ctl = Ctl::new()?;
    let pidfile = Pidfile::acquire()?;
    let streams = Streams::open(
        &devs,
        cfg.lan_if.as_deref(),
        ports,
        s.threshold,
        u32::try_from(s.keepalive_ms).unwrap_or(u32::MAX),
        false,
    )?;
    for d in &devs {
        debug!(
            "music → {} ({} segments, {:?})",
            d.label(),
            d.segments,
            d.seg_source
        );
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
    let hz = 1_000_000 / s.interval_us();
    let extra = vec![
        ("reaction", s.reaction.name().to_string()),
        ("palette", s.palette.name().to_string()),
    ];
    let mut music = Music::new(s, streams);
    // From the first razer ON on, every exit path below runs the OFF rounds.
    music.streams.razer(true);
    let mut status = Writer::start(pidfile, Mode::Music, hz as u32, extra, &music.streams);
    if let Some(a) = announce {
        crate::notify::send(cfg.notify, a, None);
    }
    let r = run_loop(&pulse, &mut music, &mut ctl, &mut status);
    music.streams.shutdown();
    r
}

const MS: u64 = 1_000_000;
// Hook slots.
pub const H_SIG: usize = 0;
pub const H_TIMER: usize = 1;
pub const H_NET: usize = 2;
pub const H_VERIFY: usize = 3;

/// Open the monitor stream with only the signal slot of `hook` polled. A ready
/// timerfd or netlink fd must not abort a half-made connection (they are
/// level-triggered, so the loop sees them right after the open returns);
/// `Interrupted` therefore always means "a signal is pending" (D-042, review fix).
pub fn open_capture<'a>(
    pulse: &'a Pulse,
    hook: &'a Hook,
    source: &std::ffi::CStr,
) -> Result<Capture<'a>, ReadError> {
    hook.set_active(1 << H_SIG);
    let r = pulse.open(source, SAMPLE_RATE, (N * 4) as u32, hook);
    hook.set_active(HOOK_ALL);
    r
}

/// The music loop. States: streaming (blocked in the mainloop's poll, which also
/// watches our fds), stream down (retry every second, keepalive the last frame) and
/// paused (stream closed, razer OFF, timer disarmed: zero wakeups).
fn run_loop(
    pulse: &Pulse,
    music: &mut Music,
    ctl: &mut Ctl,
    status: &mut Writer,
) -> Result<(), Error> {
    let hook = Hook::default();
    hook.set_fd(H_SIG, ctl.sig.fd());
    hook.set_fd(H_TIMER, ctl.timer.fd());
    hook.set_fd(H_NET, ctl.net_fd());
    let mut stream: Option<Capture<'_>> = None;
    let mut paused = false;
    let mut retry_at: Option<u64> = Some(0);
    let mut warned = false; // "cannot open" logged, waiting for it to come back
    let mut failed = false; // the stream broke: razer ON again when it's back
    let mut samples = [0f32; N];
    let interval = music.settings.interval_us();
    let mut next_us = 0u64;
    let mut quiet = Quiet::new(now_ns());
    let mut retry_ns = SEC; // next back-off while the sound server is unreachable
    let mut down_since: Option<u64> = None; // sound server unreachable since
    let verbose = crate::log::enabled(crate::log::DEBUG);
    let (mut windows, mut renders, mut sends, mut stat_at) = (0u32, 0u32, 0u32, 0u64);

    loop {
        hook.set_fd(H_VERIFY, music.streams.verify_fd());
        let opening = stream.is_none() && !paused && retry_at.is_some_and(|t| t <= now_ns());
        // The timer carries network work always. Stream down (not paused, not
        // opening right now): also the reopen retry and the keepalive of the last
        // rendered frame. Stream open: a stall guard a quarter interval after a
        // missed keepalive — windows normally send the keepalive first, so it only
        // fires if the monitor stops delivering (sink suspended, device gone).
        let mut dl = ctl.net_deadline(&music.streams);
        // Keepalives only while the light is in stream mode (not after server loss).
        let ka = music
            .streams
            .keepalive_due_ms()
            .filter(|_| music.streams.live);
        if !paused {
            dl = min_opt(dl, music.fade_due());
        }
        if stream.is_some() {
            let grace = music.streams.keepalive_ms() / 4;
            dl = min_opt(dl, ka.map(|t| (t + grace) * MS));
            dl = min_opt(dl, Some(quiet.deadline()));
        } else if !paused && !opening {
            dl = min_opt(dl, retry_at);
            dl = min_opt(dl, ka.map(|t| t * MS));
        }
        ctl.timer.arm(dl)?;

        if opening {
            retry_at = None;
            match open_capture(pulse, &hook, SOURCE) {
                Ok(st) => {
                    down_since = None;
                    if warned {
                        warn!("audio capture is back");
                        warned = false;
                        status.set_problem(Problem::None, &music.streams);
                    }
                    if failed {
                        // The light may have left stream mode during the outage.
                        music.streams.razer(true);
                        failed = false;
                    }
                    debug!(
                        "capturing {} (float32 mono {} Hz)",
                        SOURCE.to_string_lossy(),
                        SAMPLE_RATE
                    );
                    stream = Some(st);
                    quiet.opened(now_ns());
                    retry_ns = SEC;
                    continue;
                }
                // A signal is pending: handle it below, then retry (unless paused/stopped).
                Err(ReadError::Interrupted | ReadError::Expired) => retry_at = Some(now_ns()),
                Err(ReadError::Failed(e)) => {
                    if !warned {
                        warn!(
                            "cannot open {}: {e}; retrying (1 s, backing off to 30 s)",
                            SOURCE.to_string_lossy()
                        );
                        warned = true;
                        status.set_problem(Problem::NoAudio, &music.streams);
                    }
                    // The sound server is gone (logout, PipeWire restart): give the
                    // light back now instead of keeping it on the last frame (D-064);
                    // razer ON again when the stream reopens (`failed`).
                    if music.streams.live {
                        music.drop_fade();
                        music.streams.off();
                    }
                    failed = true;
                    let now = now_ns();
                    let since = *down_since.get_or_insert(now);
                    if now - since >= SERVER_GONE_EXIT_NS {
                        return Err(Error::Runtime(
                            "the sound server has been unreachable for 10 minutes; music sync stopped"
                                .into(),
                        ));
                    }
                    retry_at = Some(now + retry_ns);
                    retry_ns = (retry_ns * 2).min(RETRY_MAX_NS);
                    continue;
                }
            }
        }

        if let Some(st) = stream.as_mut() {
            match st.read(&mut samples) {
                Ok(()) => {}
                Err(ReadError::Interrupted) => {}
                Err(ReadError::Expired) => {
                    // libpulse's ~30 s request timeout; same connection, new stream.
                    debug!("passive stream never started (sink suspended): new stream");
                    if let Err(e) = st.restart(SOURCE) {
                        warn!("audio stream restart failed: {e:?}; reopening");
                        stream = None;
                        failed = true;
                        retry_at = Some(now_ns());
                    }
                    continue;
                }
                Err(ReadError::Failed(e)) => {
                    warn!("audio read failed: {e}; reopening");
                    stream = None;
                    failed = true;
                    retry_at = Some(now_ns());
                    continue;
                }
            }
            if !hook.fired() {
                // A full window.
                windows += 1;
                quiet.heard(now_ns(), samples.iter().all(|&v| v == 0.0));
                if music.fading() {
                    debug!("audio is back: fade cancelled");
                    music.cancel_fade();
                }
                // One clock for everything (CLOCK_MONOTONIC): renders, keepalive, fade.
                let now_us = render_clock_us();
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
                status.sync(&music.streams);
                if verbose && now_us / 1000 >= stat_at + 1000 {
                    let b = music.last_bands;
                    debug!(
                        "music: {} wakeups, {windows} windows, {renders} renders, {sends} datagrams in the last second; bands {:.3} {:.3} {:.3}",
                        st.take_iterations(),
                        b[0],
                        b[1],
                        b[2]
                    );
                    (windows, renders, sends, stat_at) = (0, 0, 0, now_us / 1000);
                }
                continue;
            }
        } else if !hook.fired() {
            hook.wait(-1).map_err(|e| Error::Io("poll", e))?;
        }

        // One of our fds is ready.
        let now = now_ns();
        let ev = hook.take();
        if ev[H_SIG] != 0 {
            while let Some(sig) = ctl.sig.read() {
                match sig {
                    Sig::Stop => {
                        debug!("stop signal: razer off, exit");
                        return Ok(());
                    }
                    Sig::Pause if !paused => {
                        debug!("paused: stream closed, razer off");
                        paused = true;
                        stream = None;
                        music.drop_fade();
                        music.streams.off();
                        status.set_paused(true, &music.streams);
                    }
                    Sig::Resume if paused => {
                        debug!("resumed");
                        paused = false;
                        music.streams.razer(true);
                        failed = false; // razer ON just went out
                        retry_at = Some(now);
                        status.set_paused(false, &music.streams);
                    }
                    _ => {}
                }
            }
        }
        if ev[H_NET] != 0 {
            ctl.on_netlink(now);
        }
        // A moved or rebound device got razer ON (gate reset): while audio flows the
        // next render sends to it; while quiet and already black, send black now so
        // the light isn't left in stream mode without a frame.
        let mut rebound = false;
        if ev[H_VERIFY] != 0 {
            rebound |= music.streams.on_verify_readable();
        }
        if ev[H_TIMER] != 0 {
            ctl.timer.drain();
            rebound |= ctl.service_net(&mut music.streams, now);
            if !paused && music.fade_due().is_some_and(|t| t <= now) && music.fade_step(now) {
                debug!("faded to black");
            }
            if !paused && let Some(st) = stream.as_mut() {
                match quiet.on_timer(now) {
                    QuietAction::Restart => {
                        debug!(
                            "no sound for a while: new stream (the default sink may have changed)"
                        );
                        quiet.opened(now);
                        if let Err(e) = st.restart(SOURCE) {
                            warn!("audio stream restart failed: {e:?}; reopening");
                            stream = None;
                            failed = true;
                            retry_at = Some(now);
                        }
                    }
                    QuietAction::Blank => {
                        debug!("no audio for 1.5 s: fading to black until something plays");
                        music.fade_start(now);
                    }
                    QuietAction::None => {}
                }
            }
            if !paused && music.streams.live {
                // Only frames that were actually rendered are kept alive.
                music.streams.keepalive(now / MS);
            }
        }
        // (Mid-fade, the next fade step sends to it.)
        if rebound && !paused && stream.is_some() && quiet.blanked() && !music.fading() {
            music.blank(now / MS);
        }
        status.sync(&music.streams);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_caps_at_device_rate() {
        let mut s = Settings::from_config(&Config::default()).unwrap();
        assert_eq!(s.interval_us(), 25_000);
        s.fps = 20;
        assert_eq!(s.interval_us(), 50_000);
    }
}
