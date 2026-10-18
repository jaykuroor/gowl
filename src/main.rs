//! gowl CLI: hand-written argument parsing and dispatch (SPEC.md "CLI").

use std::net::{Ipv4Addr, SocketAddrV4};

use gowl::color::Rgb;
use gowl::config::{self, Config};
use gowl::devices::{self, Cached, Device};
use gowl::net::{self, Ports, ScanTarget};
use gowl::{Error, debug, doctor, proto, warn};

const HELP: &str = "\
gowl — Govee LAN lights on niri

usage:
  gowl scan [--sweep] [--timeout MS]   discover devices; update the device cache
  gowl list                            print cached devices (no network)
  gowl on|off [DEV]
  gowl brightness 0-100 [DEV]
  gowl color RRGGBB | R,G,B [DEV]
  gowl temp KELVIN [DEV]               2000-9000
  gowl status [DEV]
  gowl doctor [DEV]                    network diagnostics (LAN interface, VPN, firewall)
  gowl outputs                         list Wayland outputs (name, mode, scale, transform)
  gowl screen [--output NAME] [--fps 1-40] [--depth 0.10-0.50] [DEV...]
                                       sync the lights to the screen edges (Ctrl+C stops)
  gowl music [--reaction R] [--palette P] [--fps N] [DEV...]
                                       sync to what you hear (default sink monitor)
      reactions: flow pulse energy_fill band_split center_burst wave bass_bounce chase twinkle auto
      palettes:  rgb auto rainbow aurora sunset ocean neon jewel fire spectrum warm cool mono
  gowl calibrate [DEV]                 find where each segment sits; saves layout= to the config
  gowl calibrate --keepalive [DEV]     find the slowest keepalive the light tolerates; saves keepalive_ms=
  gowl pause | resume                  pause the running sync (lights back to normal) / resume it
  gowl stop                            stop the running sync (lights back to normal)
  gowl toggle [screen|music] [ARGS]    that mode running → stop; paused → resume; the other mode
                                       running → switch to this one; nothing running → start it
  gowl state                           print the sync's state as one JSON line
  gowl watch                           print the state line now and on every change (for bars)
  gowl --version | --help

DEV = alias | IP | MAC | index from `gowl list`; default: $GOWL_DEVICE, config device=,
or the only cached device; screen and music take several. One sync at a time
($XDG_RUNTIME_DIR/gowl.pid). Config: $XDG_CONFIG_HOME/gowl/config. GOWL_LOG=debug for verbose.
toggle/pause/resume/stop show a desktop notification (notify-send; config notify = off to silence).
niri keys: Mod+M { spawn \"gowl\" \"toggle\" \"screen\"; }  Mod+Shift+M { spawn \"gowl\" \"toggle\" \"music\"; }
exit: 0 ok, 1 runtime error (or nothing running), 2 usage, 3 no device, 4 capture unavailable,
5 no LAN interface";

#[cfg(feature = "count-alloc")]
mod count_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::Ordering;

    pub struct Counting;

    // SAFETY: forwards unchanged to `System`; the counter has no effect on memory.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            gowl::runtime::ALLOCS.fetch_add(1, Ordering::Relaxed);
            unsafe { System.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            unsafe { System.dealloc(p, l) }
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
            gowl::runtime::ALLOCS.fetch_add(1, Ordering::Relaxed);
            unsafe { System.realloc(p, l, n) }
        }
    }

    #[global_allocator]
    static A: Counting = Counting;
}

fn main() {
    gowl::log::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(&args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("gowl: {e}");
            notify_failure(&args, &e);
            e.exit_code()
        }
    };
    std::process::exit(code);
}

/// A failed toggle/pause/resume/stop, or a sync that couldn't start or died, shows
/// one notification (D-059), so a keybind is never silent. A usage error typed
/// into `gowl screen|music` in a terminal is not notified (the terminal shows it).
fn notify_failure(args: &[String], e: &Error) {
    let cmd = args.first().map_or("", String::as_str);
    let wanted = match cmd {
        "toggle" | "pause" | "resume" | "stop" => true,
        "screen" | "music" => !matches!(e, Error::Usage(_)),
        _ => false,
    };
    if !wanted {
        return;
    }
    let text = e.to_string();
    let (summary, body) = match e {
        Error::NoDevice(_) => (
            "No light configured — run gowl setup".to_string(),
            Some(text),
        ),
        Error::NoLanIf(_) => (
            "Light unreachable: no LAN interface for it".into(),
            Some(text),
        ),
        Error::Capture(_) => ("Screen capture unavailable".into(), Some(text)),
        Error::Usage(_) => ("gowl: bad command".into(), Some(text)),
        Error::Io(..) => ("gowl stopped on an error".into(), Some(text)),
        Error::Runtime(m) => {
            let mut c = m.chars();
            let first = c.next().map(|f| f.to_uppercase().collect::<String>());
            (first.unwrap_or_default() + c.as_str(), None)
        }
    };
    gowl::notify::send(config::load_quiet().notify, &summary, body.as_deref());
}

fn usage(msg: impl Into<String>) -> Error {
    Error::Usage(msg.into())
}

/// At most `max` positional args, no flags.
fn positionals<'a>(cmd: &str, rest: &'a [String], max: usize) -> Result<&'a [String], Error> {
    if let Some(f) = rest.iter().find(|a| a.starts_with("--")) {
        return Err(usage(format!("{cmd}: unknown option {f}")));
    }
    if rest.len() > max {
        return Err(usage(format!("{cmd}: too many arguments")));
    }
    Ok(rest)
}

fn run(args: &[String]) -> Result<(), Error> {
    let Some(cmd) = args.first() else {
        println!("{HELP}");
        return Err(usage("missing command"));
    };
    let rest = &args[1..];
    match cmd.as_str() {
        "-h" | "--help" | "help" => {
            println!("{HELP}");
            Ok(())
        }
        "-V" | "--version" | "version" => {
            println!("gowl {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "scan" => cmd_scan(rest),
        "list" => cmd_list(positionals(cmd, rest, 0)?),
        "on" | "off" => {
            let r = positionals(cmd, rest, 1)?;
            let mut m = [0u8; 64];
            let n = proto::turn(cmd == "on", &mut m);
            send_one(r.first(), &m[..n])
        }
        "brightness" => {
            let r = positionals(cmd, rest, 2)?;
            let v = r
                .first()
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|&v| v <= 100)
                .ok_or_else(|| usage("brightness: expected a value 0-100"))?;
            let mut m = [0u8; 64];
            let n = proto::brightness(v, &mut m);
            send_one(r.get(1), &m[..n])
        }
        "color" | "colour" => {
            let r = positionals(cmd, rest, 2)?;
            let c = r
                .first()
                .and_then(|v| parse_color(v))
                .ok_or_else(|| usage("color: expected RRGGBB or R,G,B"))?;
            let mut m = [0u8; 128];
            let n = proto::colorwc(c, 0, &mut m);
            send_one(r.get(1), &m[..n])
        }
        "temp" => {
            let r = positionals(cmd, rest, 2)?;
            let k = r
                .first()
                .and_then(|v| v.trim_end_matches(['K', 'k']).parse::<u32>().ok())
                .filter(|k| (2000..=9000).contains(k))
                .ok_or_else(|| usage("temp: expected KELVIN 2000-9000"))?;
            let mut m = [0u8; 128];
            let n = proto::colorwc(proto::kelvin_to_rgb(k), k, &mut m);
            send_one(r.get(1), &m[..n])
        }
        "status" => cmd_status(positionals(cmd, rest, 1)?),
        "doctor" => cmd_doctor(positionals(cmd, rest, 1)?),
        "outputs" => {
            positionals(cmd, rest, 0)?;
            println!("{}", gowl::capture::list_outputs()?);
            Ok(())
        }
        "screen" => cmd_screen(rest, None),
        "music" => gowl::audio::cmd_music(rest, None),
        "calibrate" => gowl::calibrate::cmd_calibrate(rest),
        "pause" | "resume" => {
            positionals(cmd, rest, 0)?;
            gowl::control::cmd_pause(cmd == "resume")
        }
        "stop" => {
            positionals(cmd, rest, 0)?;
            gowl::control::cmd_stop()
        }
        "toggle" => cmd_toggle(rest),
        "state" => {
            positionals(cmd, rest, 0)?;
            gowl::state::cmd_state()
        }
        "watch" => {
            positionals(cmd, rest, 0)?;
            gowl::state::cmd_watch()
        }
        other => Err(usage(format!("unknown command {other:?}"))),
    }
}

/// `gowl toggle [screen|music] [ARGS...]` (D-059), what Mod+M / Mod+Shift+M run:
/// - that mode running → stop it ("Screen sync off"); paused → resume it;
/// - the other mode running or paused → stop it (wait for the exit: its razer OFF
///   goes out), then run this one ("Switched from screen to music sync");
/// - calibration running → refuse (it is interactive);
/// - nothing running → run this mode in the foreground ("Screen sync on"), which
///   is what niri's `spawn` keeps alive.
///
/// The "on"/"switched" notification goes out once setup worked (light, LAN
/// interface, capture); a setup error is notified instead (main).
fn cmd_toggle(rest: &[String]) -> Result<(), Error> {
    use gowl::state::Mode;
    let (want, args) = match rest.first().map(String::as_str) {
        Some("music") => (Mode::Music, &rest[1..]),
        Some("screen") => (Mode::Screen, &rest[1..]),
        _ => (Mode::Screen, rest),
    };
    let notify = config::load_quiet().notify;
    let announce = match gowl::control::session() {
        None => format!("{} on", want.title()),
        // The other mode, if it is one; None = the same mode, or a gowl that
        // doesn't say which (toggle it).
        Some(s) => match s.info.mode.filter(|&m| m != want) {
            Some(Mode::Calibrate) => {
                return Err(Error::Runtime(
                    "calibration is running; finish it (or `gowl stop`) first".into(),
                ));
            }
            None => {
                if s.info.paused {
                    s.pause(true)?;
                    gowl::notify::send(notify, &format!("{} resumed", s.title()), None);
                } else if s.stop(1000)? {
                    gowl::notify::send(notify, &format!("{} off", s.title()), None);
                } else {
                    return Err(Error::Runtime(format!(
                        "{} got the stop signal but has not exited after 1 s",
                        s.title()
                    )));
                }
                return Ok(());
            }
            Some(other) => {
                if !s.stop(2000)? {
                    return Err(Error::Runtime(format!(
                        "can't switch: {} has not stopped after 2 s",
                        other.title().to_lowercase()
                    )));
                }
                format!("Switched from {} to {} sync", other.name(), want.name())
            }
        },
    };
    match want {
        Mode::Music => gowl::audio::cmd_music(args, Some(&announce)),
        _ => cmd_screen(args, Some(&announce)),
    }
}

/// `RRGGBB`, `#RRGGBB` or `R,G,B`.
fn parse_color(s: &str) -> Option<Rgb> {
    if s.contains(',') {
        let mut it = s.split(',').map(|p| p.trim().parse::<u8>());
        let c = [it.next()?.ok()?, it.next()?.ok()?, it.next()?.ok()?];
        return it.next().is_none().then_some(c);
    }
    let h = s.strip_prefix('#').unwrap_or(s);
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let ch = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([ch(0)?, ch(2)?, ch(4)?])
}

struct Ctx {
    cfg: Config,
    cache: Vec<Cached>,
    ports: Ports,
}

fn ctx() -> Ctx {
    Ctx {
        cfg: config::load(),
        cache: devices::load_cache(),
        ports: Ports::from_env(),
    }
}

fn resolve(c: &Ctx, spec: Option<&String>) -> Result<Device, Error> {
    let d = devices::resolve(spec.map(String::as_str), &c.cfg, &c.cache)?;
    debug!(
        "device {} mac {} sku {} segments {} ({:?})",
        d.label(),
        d.mac.as_deref().unwrap_or("?"),
        d.sku.as_deref().unwrap_or("?"),
        d.segments,
        d.seg_source
    );
    Ok(d)
}

fn lan_for(c: &Ctx, d: &Device) -> Result<net::Iface, Error> {
    let ifaces = net::interfaces().map_err(|e| Error::Io("getifaddrs", e))?;
    let s = net::select_for(&ifaces, d.ip, c.cfg.lan_if.as_deref())?;
    debug!("lan interface {} — {}", s.iface, s.why);
    Ok(s.iface)
}

/// One-shot: resolve, bind to the LAN interface, send one datagram (ephemeral port).
fn send_one(spec: Option<&String>, msg: &[u8]) -> Result<(), Error> {
    let c = ctx();
    let d = resolve(&c, spec)?;
    let lan = lan_for(&c, &d)?;
    let sock = net::bound_socket(&lan, None)?;
    net::send_to(&sock, msg, SocketAddrV4::new(d.ip, d.control_port(c.ports))).map_err(|e| {
        if net::is_blocked(&e) {
            Error::Runtime(format!(
                "sending to {} was refused ({e}): a VPN kill switch blocks LAN traffic; allow LAN access in the VPN (see `gowl doctor`)",
                d.ip
            ))
        } else {
            Error::Io("send", e)
        }
    })
}

fn cmd_status(r: &[String]) -> Result<(), Error> {
    let c = ctx();
    let d = resolve(&c, r.first())?;
    let lan = lan_for(&c, &d)?;
    let icmp_before = net::icmp_unreach_sent();
    let ports = c.ports.for_ip(d.ip);
    match net::status(&lan, ports, d.ip, d.control_port(c.ports), 1000)? {
        Some(rep) => {
            let s = rep.status;
            let power = match s.on {
                Some(true) => "on",
                Some(false) => "off",
                None => "?",
            };
            let mut line = format!("{} power {power}", d.label());
            if let Some(b) = s.brightness {
                line += &format!(" brightness {b}");
            }
            if let Some([r, g, b]) = s.color {
                line += &format!(" color {r:02x}{g:02x}{b:02x}");
            }
            if let Some(k) = s.kelvin.filter(|&k| k > 0) {
                line += &format!(" temp {k}K");
            }
            println!("{line}");
            debug!(
                "status via {} from {} in {} ms",
                rep.query, rep.from, rep.elapsed_ms
            );
            Ok(())
        }
        None if net::rejected_since(icmp_before) > 0 => Err(Error::Runtime(format!(
            "{} likely answered, but the firewall rejected the reply (inbound UDP {} blocked) — run `gowl doctor`",
            d.ip, ports.reply
        ))),
        None => Err(Error::Runtime(format!(
            "no status reply from {} (light offline or LAN Control off) — run `gowl doctor`",
            d.ip
        ))),
    }
}

fn cmd_doctor(r: &[String]) -> Result<(), Error> {
    let c = ctx();
    let dev = match resolve(&c, r.first()) {
        Ok(d) => Some(d),
        Err(Error::NoDevice(m)) if r.is_empty() => {
            debug!("{m}");
            None
        }
        Err(e) => return Err(e),
    };
    let ports = dev
        .as_ref()
        .map_or(net::Ports::default(), |d| c.ports.for_ip(d.ip));
    doctor::run(dev.as_ref(), c.cfg.lan_if.as_deref(), ports)
}

fn print_table(devs: &[Cached], cfg: &Config) {
    println!(
        "{:>2}  {:<23}  {:<15}  {:<6}  {:>4}  alias",
        "#", "mac", "ip", "sku", "segs"
    );
    for (i, d) in devs.iter().enumerate() {
        let sec = cfg
            .devices
            .iter()
            .find(|s| s.mac.as_deref() == Some(d.mac.as_str()) || s.ip == Some(d.ip));
        let segs = sec
            .and_then(|s| s.segments)
            .or_else(|| devices::sku_segments(&d.sku))
            .map_or_else(
                || format!("{}?", devices::DEFAULT_SEGMENTS),
                |n| n.to_string(),
            );
        println!(
            "{:>2}  {:<23}  {:<15}  {:<6}  {:>4}  {}",
            i + 1,
            d.mac,
            d.ip,
            d.sku,
            segs,
            sec.map_or("", |s| s.alias.as_str())
        );
    }
    if devs.iter().any(|d| devices::sku_segments(&d.sku).is_none()) {
        println!(
            "(\"10?\" = SKU not in the built-in table; 10 assumed — set segments= or run `gowl calibrate`)"
        );
    }
}

fn cmd_list(_: &[String]) -> Result<(), Error> {
    let c = ctx();
    if c.cache.is_empty() {
        return Err(Error::NoDevice("no cached devices; run `gowl scan`".into()));
    }
    print_table(&c.cache, &c.cfg);
    Ok(())
}

fn cmd_scan(rest: &[String]) -> Result<(), Error> {
    let mut sweep = false;
    let mut timeout = 1000u64;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sweep" => sweep = true,
            "--timeout" => {
                timeout = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|&t| (1..=60_000).contains(&t))
                    .ok_or_else(|| usage("scan: --timeout expects milliseconds 1-60000"))?;
            }
            o => return Err(usage(format!("scan: unexpected argument {o:?}"))),
        }
    }
    let c = ctx();
    let ifaces = net::interfaces().map_err(|e| Error::Io("getifaddrs", e))?;
    let lan_if = c.cfg.lan_if.as_deref();
    let mut targets: Vec<ScanTarget> = net::discovery_ifaces(&ifaces, lan_if)
        .into_iter()
        .map(|iface| ScanTarget {
            iface,
            discover: true,
            unicast: Vec::new(),
        })
        .collect();
    // Also probe known IPs by unicast (one datagram each): replies to a unicast probe
    // pass conntrack, so known devices are found even when multicast replies are dropped.
    let mut known: Vec<Ipv4Addr> = c.cache.iter().map(|d| d.ip).collect();
    known.extend(c.cfg.devices.iter().filter_map(|d| d.ip));
    if let Ok(d) = devices::resolve(None, &c.cfg, &c.cache) {
        known.push(d.ip);
    }
    known.sort();
    known.dedup();
    for ip in known {
        let Ok(sel) = net::select_for(&ifaces, ip, lan_if) else {
            debug!("known device {ip} is on no LAN interface; skipped");
            continue;
        };
        match targets.iter_mut().find(|t| t.iface.name == sel.iface.name) {
            Some(t) => t.unicast.push(ip),
            None => targets.push(ScanTarget {
                iface: sel.iface,
                discover: false,
                unicast: vec![ip],
            }),
        }
    }
    if targets.is_empty() {
        return Err(Error::NoLanIf(
            "no LAN interface to scan on (VPN interfaces are never used); set lan_if= in the config".into(),
        ));
    }
    for t in &targets {
        debug!(
            "scan on {}: {}{} unicast",
            t.iface,
            if t.discover {
                "multicast+broadcast, "
            } else {
                ""
            },
            t.unicast.len()
        );
    }
    let icmp_before = net::icmp_unreach_sent();
    let found = net::scan(&targets, c.ports, timeout, sweep)?;
    if found.is_empty() {
        let rejected = net::rejected_since(icmp_before);
        if rejected > 0 {
            eprintln!(
                "gowl: {rejected} repl{} likely reached this machine but the firewall rejected them (inbound UDP {} blocked; `gowl doctor` prints a LAN-scoped rule). A known IP works without replies: GOWL_DEVICE=IP or ip= in the config.",
                if rejected == 1 { "y" } else { "ies" },
                c.ports.reply
            );
        } else {
            eprintln!(
                "gowl: no devices answered. Check LAN Control in the Govee Home app. A known IP works without replies: GOWL_DEVICE=IP or ip= in the config."
            );
        }
        return Err(Error::NoDevice("scan found no devices".into()));
    }
    let new: Vec<Cached> = found
        .iter()
        .map(|f| Cached {
            mac: f.mac.clone(),
            ip: f.ip,
            sku: if f.sku.is_empty() {
                "?".into()
            } else {
                f.sku.clone()
            },
        })
        .collect();
    for f in &found {
        debug!(
            "found {} {} {} (reply from {} on {})",
            f.mac, f.ip, f.sku, f.from, f.iface
        );
    }
    let mut cache = c.cache.clone();
    devices::merge(&mut cache, &new);
    match devices::save_cache(&cache) {
        Ok(p) => debug!("device cache written: {}", p.display()),
        Err(e) => warn!("cannot write device cache: {e}"),
    }
    print_table(&new, &c.cfg);
    Ok(())
}

/// `gowl screen [--output NAME] [--fps N] [--depth D] [DEV...]`. `announce`: the
/// notification to show once setup worked (toggle).
fn cmd_screen(rest: &[String], announce: Option<&str>) -> Result<(), Error> {
    let c = ctx();
    let mut output = c.cfg.output.clone();
    let mut fps = c.cfg.fps;
    let mut depth = c.cfg.depth;
    let mut specs: Vec<&String> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--output" => {
                output = Some(
                    it.next()
                        .ok_or_else(|| usage("screen: --output expects a name"))?
                        .clone(),
                );
            }
            "--fps" => {
                fps = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|f| (1..=gowl::runtime::DEVICE_MAX_HZ).contains(f))
                    .ok_or_else(|| usage("screen: --fps expects 1-40 (40 Hz is the device cap)"))?;
            }
            "--depth" => {
                depth = it
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                    .filter(|d| (0.10..=0.50).contains(d))
                    .ok_or_else(|| usage("screen: --depth expects 0.10-0.50"))?;
            }
            o if o.starts_with("--") => return Err(usage(format!("screen: unknown option {o}"))),
            _ => specs.push(a),
        }
    }
    let specs: Vec<&str> = specs.iter().map(|s| s.as_str()).collect();
    let devices = devices::resolve_many(&specs, &c.cfg, &c.cache)?;
    for d in &devices {
        debug!(
            "device {} mac {} sku {} segments {} ({:?})",
            d.label(),
            d.mac.as_deref().unwrap_or("?"),
            d.sku.as_deref().unwrap_or("?"),
            d.segments,
            d.seg_source
        );
    }
    // Signals blocked before anything streams; then claim the pidfile.
    let mut ctl = gowl::runtime::Ctl::new()?;
    let pidfile = gowl::control::Pidfile::acquire()?;
    let streams = gowl::runtime::Streams::open(
        &devices,
        c.cfg.lan_if.as_deref(),
        c.ports,
        c.cfg.threshold,
        c.cfg.keepalive_ms,
        true,
    )?;
    let mut cap = gowl::capture::open(output.as_deref())?;
    let mut pipe = gowl::runtime::Pipeline::new(streams, &c.cfg, depth, fps);
    debug!(
        "screen sync: {} device(s), output {}, {} fps (cap {}), depth {depth}",
        devices.len(),
        cap.output_name().unwrap_or("?"),
        fps,
        gowl::runtime::DEVICE_MAX_HZ
    );
    pipe.streams.razer(true);
    let mut st = gowl::state::Writer::start(
        pidfile,
        gowl::state::Mode::Screen,
        fps,
        Vec::new(),
        &pipe.streams,
    );
    if let Some(a) = announce {
        gowl::notify::send(c.cfg.notify, a, None);
    }
    gowl::runtime::run_screen(cap.as_mut(), &mut pipe, &mut ctl, &mut st)
}
