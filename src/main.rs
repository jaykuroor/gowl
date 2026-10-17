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
  gowl screen [--output NAME] [--fps N] [--depth 0.10-0.50] [DEV...]
                                       sync the lights to the screen edges (Ctrl+C stops)
  gowl music [--reaction R] [--palette P] [--fps N] [DEV...]
                                       sync to what you hear (default sink monitor)
      reactions: flow pulse energy_fill band_split center_burst wave bass_bounce chase twinkle auto
      palettes:  rgb auto rainbow aurora sunset ocean neon jewel fire spectrum warm cool mono
  gowl calibrate | pause | resume | stop | toggle
                                       not yet implemented
  gowl --version | --help

DEV = alias | IP | MAC | index from `gowl list`; default: $GOWL_DEVICE, config device=,
or the only cached device. Config: $XDG_CONFIG_HOME/gowl/config. GOWL_LOG=debug for verbose.
exit: 0 ok, 1 runtime error, 2 usage, 3 no device, 4 capture unavailable, 5 no LAN interface";

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
            e.exit_code()
        }
    };
    std::process::exit(code);
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
        "screen" => cmd_screen(rest),
        "music" => gowl::audio::cmd_music(rest),
        "calibrate" | "pause" | "resume" | "stop" | "toggle" => {
            Err(usage(format!("{cmd}: not yet implemented")))
        }
        other => Err(usage(format!("unknown command {other:?}"))),
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
    net::send_to(&sock, msg, SocketAddrV4::new(d.ip, c.ports.control)).map_err(|e| {
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
    match net::status(&lan, c.ports, d.ip, c.ports.control, 1000)? {
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
            d.ip, c.ports.reply
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
    doctor::run(dev.as_ref(), c.cfg.lan_if.as_deref(), c.ports)
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

/// `gowl screen [--output NAME] [--fps N] [--depth D] [DEV...]`.
fn cmd_screen(rest: &[String]) -> Result<(), Error> {
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
                    .filter(|f| (1..=240).contains(f))
                    .ok_or_else(|| usage("screen: --fps expects 1-240"))?;
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
    let devices = if specs.is_empty() {
        vec![resolve(&c, None)?]
    } else {
        specs
            .iter()
            .map(|s| resolve(&c, Some(s)))
            .collect::<Result<Vec<_>, _>>()?
    };
    let streams = gowl::runtime::Streams::open(
        &devices,
        c.cfg.lan_if.as_deref(),
        c.ports,
        c.cfg.threshold,
        c.cfg.keepalive_ms,
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
    gowl::runtime::run_screen(cap.as_mut(), &mut pipe)
}
