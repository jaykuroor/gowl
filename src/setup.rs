//! `gowl setup` — first-run onboarding, interactive and in plain language (D-071).
//!
//! 1. LAN Control: what it is and where to switch it on.
//! 2. Find the light: the one already in the config, else `scan`; when nothing
//!    answers (Fedora's firewall drops the replies), the light's IP typed in or
//!    picked from the ARP table's Govee MAC prefixes. Every choice is confirmed by
//!    eye: the light blinks blue (razer frames, then OFF ×2 gives it back) and the
//!    human answers y/n. The firewall is never touched; the LAN-scoped runtime rule
//!    from docs/NETWORK.md is shown as optional text.
//! 3. Segments from the SKU table (or asked), then the `calibrate` walk and,
//!    optionally, `calibrate --keepalive` (run as child processes on this terminal).
//! 4. Offers: screen sync at login (the systemd unit), niri keys (`gowl keys`),
//!    notifications, a quickshell hint.
//! 5. Every config write is atomic and keeps every other byte (the byte-preserving
//!    setters); a summary and next steps at the end.
//!
//! Needs a terminal (stdin and stdout); without one it refuses and points at
//! `config.example`. Ctrl+C (signalfd) or end of input cancels at any prompt; a
//! blinking light is given back first.

use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{self, Config};
use crate::control::{self, Pidfile};
use crate::devices::{self, Cached, Device, SegSource};
use crate::error::Error;
use crate::keys;
use crate::net::{self, Ports};
use crate::proto::{self, MAX_SEGMENTS};
use crate::runtime::{Signals, Streams, now_ns};

fn cancelled() -> Error {
    Error::Runtime("setup cancelled; what was saved before stays saved".into())
}

/// Prompts on the terminal: line input polled together with the signalfd, so
/// Ctrl+C is an orderly cancel (and never leaves the light blinking).
struct Ui {
    sig: Signals,
    pending: Vec<u8>,
}

impl Ui {
    fn new() -> Result<Ui, Error> {
        Ok(Ui {
            sig: Signals::new()?,
            pending: Vec::with_capacity(256),
        })
    }

    /// A complete line from stdin, waiting at most `ms` (-1 = forever). Ok(None)
    /// on timeout; Err on Ctrl+C or end of input.
    fn read_line(&mut self, ms: i32) -> Result<Option<String>, Error> {
        let deadline = (ms >= 0).then(|| now_ns() + ms as u64 * 1_000_000);
        loop {
            if let Some(i) = self.pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=i).collect();
                return Ok(Some(String::from_utf8_lossy(&line).trim().to_string()));
            }
            let wait = match deadline {
                None => -1,
                Some(d) => {
                    let now = now_ns();
                    if now >= d {
                        return Ok(None);
                    }
                    ((d - now) / 1_000_000) as i32 + 1
                }
            };
            let mut p = [
                libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.sig.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: valid pollfd array.
            let r = unsafe { libc::poll(p.as_mut_ptr(), 2, wait) };
            if r < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(Error::Io("poll", e));
            }
            if p[1].revents != 0 && self.sig.read().is_some() {
                println!();
                return Err(cancelled());
            }
            if p[0].revents != 0 {
                let mut buf = [0u8; 256];
                // SAFETY: valid fd and buffer.
                let n =
                    unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    println!();
                    return Err(cancelled());
                }
                self.pending.extend_from_slice(&buf[..n as usize]);
            }
        }
    }

    fn ask(&mut self, prompt: &str) -> Result<String, Error> {
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        Ok(self.read_line(-1)?.unwrap_or_default())
    }

    /// A yes/no question; Enter takes `default`.
    fn yes(&mut self, q: &str, default: bool) -> Result<bool, Error> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        loop {
            let a = self.ask(&format!("{q} {hint} "))?;
            if a.is_empty() {
                return Ok(default);
            }
            match yes_no(&a) {
                Some(b) => return Ok(b),
                None => println!("   Please answer y or n."),
            }
        }
    }

    /// Signals that arrived while a child (calibrate) had the terminal belong to it.
    fn forget_signals(&mut self) {
        while self.sig.read().is_some() {}
    }
}

/// "y"/"yes" → true, "n"/"no" → false, "" → None (the caller's default).
fn yes_no(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

/// The light setup settled on.
#[derive(Clone, Debug)]
struct Light {
    ip: Ipv4Addr,
    mac: Option<String>,
    sku: Option<String>,
    /// Its `[device …]` section, if the config has one.
    section: Option<String>,
}

fn isatty(fd: i32) -> bool {
    // SAFETY: isatty only inspects the fd.
    unsafe { libc::isatty(fd) == 1 }
}

/// `gowl setup`.
pub fn cmd_setup(rest: &[String]) -> Result<(), Error> {
    if !rest.is_empty() {
        return Err(Error::Usage("setup takes no arguments".into()));
    }
    if !isatty(libc::STDIN_FILENO) || !isatty(libc::STDOUT_FILENO) {
        return Err(Error::Usage(
            "gowl setup asks questions, so it needs a terminal. Without one: copy config.example to ~/.config/gowl/config and set `device` and an `[device …]` section with the light's `ip` by hand".into(),
        ));
    }
    let mut ui = Ui::new()?;
    let path = config::config_path().ok_or_else(|| {
        Error::Runtime("no config path: HOME and XDG_CONFIG_HOME are unset".into())
    })?;
    println!(
        "gowl setup — a few questions, then your lights follow the screen or the music.\n\
         Answers in [brackets] are what Enter picks. Ctrl+C stops at any point.\n"
    );
    if let Some(s) = control::session() {
        println!(
            "{} is running; setup needs the light to itself for a moment.",
            s.title()
        );
        if !ui.yes("Stop it now?", true)? {
            return Err(cancelled());
        }
        if !s.stop(2000)? {
            return Err(Error::Runtime(
                "the running sync didn't stop; try `gowl stop`".into(),
            ));
        }
    }
    println!(
        "1. LAN Control\n   gowl talks to the light directly over your home network. That needs \"LAN Control\":\n   \
         Govee Home app → your light → settings (gear) → LAN Control → on. The light and this\n   \
         computer must be on the same network.\n"
    );
    ui.ask("   Press Enter once LAN Control is on. ")?;

    // 2. Find the light.
    println!("\n2. Your light");
    let ports = Ports::from_env();
    let text = config::read_text(&path)?;
    let cfg = config::parse(&text, &mut |_, _| {});
    let mut cache = devices::load_cache();
    let light = find_light(&mut ui, &cfg, &mut cache, ports)?;
    let alias = save_light(&mut ui, &path, &light)?;

    // 3. Segments.
    let text = config::read_text(&path)?;
    let cfg = config::parse(&text, &mut |_, _| {});
    segments(&mut ui, &path, &cfg, &alias, &light)?;

    // 4. Offers.
    println!("\n4. Extras");
    let autostart = offer_autostart(&mut ui)?;
    let keys_state = offer_keys(&mut ui)?;
    let cfg = config::parse(&config::read_text(&path)?, &mut |_, _| {});
    let notify = ui.yes(
        "   Show a short notification when sync turns on, off or switches?",
        cfg.notify,
    )?;
    if notify != cfg.notify {
        let text = config::read_text(&path)?;
        let new = config::set_global_key(&text, "notify", if notify { "on" } else { "off" });
        config::write_atomic(&path, &new)?;
    }
    quickshell_hint();

    // 5. Summary.
    let cfg = config::load_quiet();
    let d = devices::resolve(Some(&alias), &cfg, &devices::load_cache())?;
    println!("\nDone. Saved in {}:", path.display());
    println!(
        "  light      {} — {} segments{}",
        d.label(),
        d.segments,
        if d.layout.is_some() {
            ", calibrated layout"
        } else {
            ""
        }
    );
    println!(
        "  default    {}",
        if cfg.device.as_deref() == Some(alias.as_str()) {
            "yes (commands without a light name use it)"
        } else {
            "no"
        }
    );
    println!("  keepalive  {} ms", cfg.keepalive_ms);
    println!("  notify     {}", if cfg.notify { "on" } else { "off" });
    println!("  at login   {autostart}");
    println!("  niri keys  {keys_state}");
    println!(
        "\nNext: Mod+M toggles screen sync, Mod+Shift+M music sync (or `gowl toggle screen|music`).\n\
         `gowl screen` / `gowl music` run in a terminal; `gowl doctor` explains network trouble."
    );
    Ok(())
}

/// Step 2: the config's light, a scanned one, or one typed in / picked from ARP.
fn find_light(
    ui: &mut Ui,
    cfg: &Config,
    cache: &mut Vec<Cached>,
    ports: Ports,
) -> Result<Light, Error> {
    if let Ok(d) = devices::resolve(None, cfg, cache) {
        println!(
            "   Your config already has a light: {}{}.",
            d.label(),
            d.sku.as_deref().map_or(String::new(), |s| format!(", {s}"))
        );
        if ui.yes("   Use it?", true)? {
            let l = Light {
                ip: d.ip,
                mac: d.mac.clone(),
                sku: d.sku.clone(),
                section: d.alias.clone(),
            };
            if !ui.yes("   Blink it once to check gowl reaches it?", true)?
                || blink(ui, &l, cfg, ports)?
            {
                return Ok(l);
            }
            println!("   Then let's look for it.");
        }
    }

    println!("   Looking for Govee lights on your network…");
    let before = net::icmp_unreach_sent();
    let found = devices::scan_targets(cfg, cache)
        .and_then(|t| net::scan(&t, ports, 1500, false))
        .unwrap_or_else(|e| {
            println!("   (scan failed: {e})");
            Vec::new()
        });
    if !found.is_empty() {
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
        devices::merge(cache, &new);
        let _ = devices::save_cache(cache);
        for (i, f) in found.iter().enumerate() {
            println!("   {}. {} at {} ({})", i + 1, or_q(&f.sku), f.ip, f.mac);
        }
        loop {
            let pick = if found.len() == 1 {
                if ui.yes("   Use this one?", true)? {
                    1
                } else {
                    0
                }
            } else {
                ui.ask(&format!(
                    "   Which one? [1-{}, Enter = none of these] ",
                    found.len()
                ))?
                .parse::<usize>()
                .unwrap_or(0)
            };
            let Some(f) = pick.checked_sub(1).and_then(|i| found.get(i)) else {
                break;
            };
            let l = Light {
                ip: f.ip,
                mac: Some(f.mac.clone()),
                sku: (!f.sku.is_empty()).then(|| f.sku.clone()),
                section: devices::section_for(cfg, f.ip, Some(&f.mac)).map(|s| s.alias.clone()),
            };
            if blink(ui, &l, cfg, ports)? {
                return Ok(l);
            }
            if found.len() == 1 {
                break;
            }
        }
    } else {
        let rejected = net::rejected_since(before);
        if rejected > 0 {
            println!(
                "   No answer got through: the light probably answered, but this computer's firewall\n   \
                 turned the replies away. That's fine — sync, on/off and colors only send, they need no\n   \
                 replies. If you want `gowl scan` and `gowl status` to work too, this lets replies in\n   \
                 from your home network only, until the next reboot (optional; gowl never changes it):"
            );
            if let Some(rule) = firewall_rule_for(cfg) {
                println!("     {rule}");
                println!(
                    "   (`gowl doctor` explains more; repeat with --permanent once it works.)"
                );
            }
        } else {
            println!(
                "   No light answered. Check that it's powered, on the same network, and that LAN Control\n   \
                 is on (it can take a minute after switching it on)."
            );
        }
    }

    // Typed in, or picked from the ARP table.
    loop {
        let cands: Vec<devices::Neighbour> = std::fs::read_to_string("/proc/net/arp")
            .map(|t| devices::parse_arp(&t))
            .unwrap_or_default()
            .into_iter()
            .filter(|n| n.govee)
            .collect();
        if cands.is_empty() {
            println!(
                "   Type the light's IP address. Your router's list of connected devices shows it; a\n   \
                 fixed address for the light (a DHCP reservation in the router) keeps it from changing."
            );
        } else {
            println!("   This computer has seen these Govee devices on the network:");
            for (i, n) in cands.iter().enumerate() {
                println!("   {}. {} ({}, via {})", i + 1, n.ip, n.mac, n.iface);
            }
            println!("   Pick one by number, or type the light's IP address.");
        }
        let a = ui.ask("   Light: ")?;
        let ip = match a.parse::<usize>() {
            Ok(i) if i >= 1 && i <= cands.len() => cands[i - 1].ip,
            _ => match a.parse::<Ipv4Addr>() {
                Ok(ip) => ip,
                Err(_) => {
                    println!(
                        "   \"{a}\" isn't a number from the list or an IP address like 192.168.1.50."
                    );
                    continue;
                }
            },
        };
        let mut l = Light {
            ip,
            mac: cands.iter().find(|n| n.ip == ip).map(|n| n.mac.clone()),
            sku: None,
            section: None,
        };
        l.section = devices::section_for(cfg, ip, l.mac.as_deref()).map(|s| s.alias.clone());
        if blink(ui, &l, cfg, ports)? {
            // Talking to it filled the ARP table.
            l.mac = l.mac.or_else(|| devices::arp_mac(ip));
            return Ok(l);
        }
        println!(
            "   Not that one, then. Check the address, that the light is on and that LAN Control is on."
        );
    }
}

fn or_q(s: &str) -> &str {
    if s.is_empty() { "Govee light" } else { s }
}

/// The optional firewall rule for the interface that reaches the LAN (the config's
/// light, else the first LAN interface).
fn firewall_rule_for(cfg: &Config) -> Option<String> {
    let ifaces = net::interfaces().ok()?;
    let lan = net::discovery_ifaces(&ifaces, cfg.lan_if.as_deref())
        .into_iter()
        .find(|i| !i.is_loopback())?;
    Some(crate::doctor::firewall_rule(&lan, proto::REPLY_PORT))
}

/// Blink `l` blue, ask whether it did; the light goes back to normal afterwards
/// (razer OFF ×2). Holds the pidfile meanwhile, so no sync can start in between.
fn blink(ui: &mut Ui, l: &Light, cfg: &Config, ports: Ports) -> Result<bool, Error> {
    let ifaces = net::interfaces().map_err(|e| Error::Io("getifaddrs", e))?;
    let sel = match net::select_for(&ifaces, l.ip, cfg.lan_if.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            println!("   {e}");
            return Ok(false);
        }
    };
    let section = l
        .section
        .as_deref()
        .and_then(|a| cfg.devices.iter().find(|d| d.alias == a));
    let n = section
        .and_then(|s| s.segments)
        .or_else(|| l.sku.as_deref().and_then(devices::sku_segments))
        .unwrap_or(devices::DEFAULT_SEGMENTS);
    let dev = Device {
        ip: l.ip,
        mac: l.mac.clone(),
        sku: l.sku.clone(),
        alias: None,
        segments: n,
        seg_source: SegSource::Default,
        reverse: false,
        offset: 0,
        layout: None,
        port: section.and_then(|s| s.port),
    };
    let pidfile = Pidfile::acquire()?;
    // Switch it on first (a light that is off shows nothing), then stream.
    let sock = net::bound_socket(&sel.iface, None)?;
    let mut m = [0u8; 64];
    let k = proto::turn(true, &mut m);
    let _ = net::send_to(
        &sock,
        &m[..k],
        SocketAddrV4::new(l.ip, dev.control_port(ports)),
    );
    drop(sock);
    let mut streams = Streams::open(&[dev], cfg.lan_if.as_deref(), ports, 0, 0, false)?;
    streams.razer(true);
    print!(
        "   The light at {} should be blinking blue now. Is it? [y/n] ",
        l.ip
    );
    let _ = std::io::stdout().flush();
    let blue = [[0u8, 40, 255]; MAX_SEGMENTS];
    let black = [[0u8; 3]; MAX_SEGMENTS];
    let n = usize::from(n).min(MAX_SEGMENTS);
    let mut on = true;
    let answer = loop {
        let frame = if on { &blue } else { &black };
        streams.push_exact(0, &frame[..n], now_ns() / 1_000_000);
        on = !on;
        // Each half second: the other frame (it doubles as the keepalive).
        match ui.read_line(500) {
            Ok(None) => {}
            Ok(Some(a)) => match yes_no(&a) {
                Some(b) => break Ok(b),
                None => {
                    print!("   y or n? ");
                    let _ = std::io::stdout().flush();
                }
            },
            Err(e) => break Err(e),
        }
    };
    streams.shutdown();
    drop(pidfile);
    answer
}

fn valid_alias(a: &str) -> bool {
    !a.is_empty()
        && a.len() <= 32
        && a.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Save the light: its existing section updated (only keys that change), or a new
/// `[device ALIAS]` section; `device = ALIAS` if there is no default yet (or the
/// human wants this one). Returns the alias.
fn save_light(ui: &mut Ui, path: &Path, l: &Light) -> Result<String, Error> {
    let text = config::read_text(path)?;
    let cfg = config::parse(&text, &mut |_, _| {});
    let existing = l
        .section
        .as_deref()
        .and_then(|a| cfg.devices.iter().find(|d| d.alias == a));
    let alias = match existing {
        Some(s) => s.alias.clone(),
        None => {
            let base = l
                .sku
                .as_deref()
                .filter(|s| valid_alias(s))
                .map_or_else(|| "light".to_string(), str::to_ascii_lowercase);
            let mut def = base.clone();
            let mut i = 2;
            while cfg.devices.iter().any(|d| d.alias == def) {
                def = format!("{base}-{i}");
                i += 1;
            }
            loop {
                let a = ui.ask(&format!(
                    "   A short name for this light (letters, digits, - _ .) [{def}]: "
                ))?;
                let a = if a.is_empty() { def.clone() } else { a };
                if !valid_alias(&a) {
                    println!("   Letters, digits, '-', '_' or '.' only, please.");
                } else if cfg.devices.iter().any(|d| d.alias == a) {
                    println!("   \"{a}\" is taken by another light in your config.");
                } else {
                    break a;
                }
            }
        }
    };
    let mut keys: Vec<(&str, String)> = Vec::new();
    if existing.is_none_or(|s| s.ip != Some(l.ip)) {
        keys.push(("ip", l.ip.to_string()));
    }
    if let Some(m) = &l.mac
        && existing.is_none_or(|s| s.mac.is_none())
    {
        keys.push(("mac", m.clone()));
    }
    if let Some(s) = &l.sku
        && existing.is_none_or(|e| e.sku.is_none())
    {
        keys.push(("sku", s.clone()));
    }
    let mut new = match existing {
        Some(_) => config::set_device_keys(&text, &alias, &keys).unwrap_or_else(|| text.clone()),
        None => config::append_device_section(&text, &alias, &keys),
    };
    let make_default = match cfg.device.as_deref() {
        None => true,
        Some(d) if d == alias => false,
        Some(d) => ui.yes(
            &format!("   Make \"{alias}\" the default light (instead of \"{d}\")?"),
            true,
        )?,
    };
    if make_default {
        new = config::set_global_key(&new, "device", &alias);
    }
    if new != text {
        config::write_atomic(path, &new)?;
        println!("   Saved \"{alias}\" ({}) to {}.", l.ip, path.display());
    }
    Ok(alias)
}

/// Step 3: segment count, the calibrate walk, the keepalive walk.
fn segments(ui: &mut Ui, path: &Path, cfg: &Config, alias: &str, l: &Light) -> Result<(), Error> {
    println!(
        "\n3. Segments\n   gowl colors the light in segments, one per part of your screen's edge."
    );
    let section = cfg.devices.iter().find(|d| d.alias == alias);
    let walk = if let Some(layout) = section.and_then(|s| s.layout.as_ref()) {
        let cells: Vec<String> = layout.iter().map(u8::to_string).collect();
        println!(
            "   Already calibrated: {} segments, layout = {}.",
            layout.len(),
            cells.join(" ")
        );
        ui.yes("   Calibrate again?", false)?
    } else {
        let table = l.sku.as_deref().and_then(devices::sku_segments);
        match (section.and_then(|s| s.segments), table) {
            (Some(n), _) => println!("   Your config says {n} segments."),
            (None, Some(n)) => println!(
                "   A {} has {n} segments.",
                l.sku.as_deref().unwrap_or("light")
            ),
            (None, None) => {
                let n = loop {
                    let a = ui.ask(&format!(
                        "   How many segments does the light have? (10 is usual; calibrating shows if it's off) [{}]: ",
                        devices::DEFAULT_SEGMENTS
                    ))?;
                    if a.is_empty() {
                        break devices::DEFAULT_SEGMENTS;
                    }
                    match a.parse::<u8>() {
                        Ok(n) if (1..=MAX_SEGMENTS as u8).contains(&n) => break n,
                        _ => println!("   A number from 1 to {MAX_SEGMENTS}, please."),
                    }
                };
                let text = config::read_text(path)?;
                if let Some(new) =
                    config::set_device_keys(&text, alias, &[("segments", n.to_string())])
                {
                    config::write_atomic(path, &new)?;
                }
            }
        }
        println!(
            "   Calibrating lights one segment at a time; you type where each one is, so the colors\n   \
             land in the right places (about two minutes)."
        );
        ui.yes("   Calibrate now?", true)?
    };
    if walk {
        run_child(ui, &["calibrate", alias]);
    }
    let measured = cfg.keepalive_ms != Config::default().keepalive_ms;
    let q = if measured {
        format!(
            "   keepalive_ms = {} is set already. Measure how long the light holds a frame again?",
            cfg.keepalive_ms
        )
    } else {
        "   Measure how long the light holds a frame (fewer packets on a still screen; about a minute)?"
            .to_string()
    };
    if ui.yes(&q, false)? {
        run_child(ui, &["calibrate", "--keepalive", alias]);
    }
    Ok(())
}

/// Run `gowl ARGS` on this terminal and wait for it.
fn run_child(ui: &mut Ui, args: &[&str]) {
    println!();
    let r = keys::own_exe().and_then(|exe| {
        Command::new(exe)
            .args(args)
            .status()
            .map_err(|e| Error::Io("run gowl", e))
    });
    ui.forget_signals();
    match r {
        Ok(s) if s.success() => {}
        Ok(s) => println!(
            "   (gowl {} ended with status {}; you can run it again later.)",
            args.join(" "),
            s.code().unwrap_or(-1)
        ),
        Err(e) => println!("   ({e})"),
    }
}

fn user_unit_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .map(|c| c.join("systemd/user"))
}

/// Screen sync at login: the installed unit, enabled or disabled with systemctl.
fn offer_autostart(ui: &mut Ui) -> Result<String, Error> {
    let Some(dir) = user_unit_dir() else {
        return Ok("not set up".into());
    };
    if !dir.join("gowl.service").exists() {
        println!(
            "   Screen sync at login: the service isn't installed — `make install` from gowl's source adds it."
        );
        return Ok("not installed".into());
    }
    let enabled =
        std::fs::symlink_metadata(dir.join("graphical-session.target.wants/gowl.service")).is_ok();
    let want = ui.yes(
        "   Start screen sync automatically when you log in?",
        enabled,
    )?;
    if want == enabled {
        return Ok(if enabled { "on" } else { "off" }.into());
    }
    let verb = if want { "enable" } else { "disable" };
    match Command::new("systemctl")
        .args(["--user", verb, "gowl.service"])
        .status()
    {
        Ok(s) if s.success() => {
            if want {
                println!(
                    "   Done: screen sync starts at your next login (now: `systemctl --user start gowl`)."
                );
            }
            Ok(if want { "on" } else { "off" }.into())
        }
        _ => {
            println!("   That didn't work; run `systemctl --user {verb} gowl` yourself.");
            Ok(if enabled { "on" } else { "off" }.into())
        }
    }
}

/// niri keys via `gowl keys add`.
fn offer_keys(ui: &mut Ui) -> Result<String, Error> {
    let paths = keys::Paths::from_env()?;
    if !paths.niri.exists() {
        return Ok("no niri config".into());
    }
    if keys::included(&paths) {
        println!("   niri keys: Mod+M (screen) and Mod+Shift+M (music) are set up.");
        return Ok("Mod+M / Mod+Shift+M".into());
    }
    let taken = keys::conflicts(&paths, &paths.niri);
    if !taken.is_empty() {
        for (f, n, k) in &taken {
            println!("   niri keys: {k} is already bound in {}:{n}.", f.display());
        }
        println!("   (`gowl keys` explains how to pick other keys.)");
        return Ok("not added (keys taken)".into());
    }
    if !ui.yes(
        "   Add niri keys: Mod+M toggles screen sync, Mod+Shift+M music sync?",
        true,
    )? {
        return Ok("not added".into());
    }
    match keys::add(&paths, &keys::own_exe()?) {
        Ok(keys::Added::Added(bak)) => {
            println!(
                "   Added (niri config backed up to {}). They work right away.",
                bak.display()
            );
            Ok("Mod+M / Mod+Shift+M".into())
        }
        Ok(keys::Added::Present) => Ok("Mod+M / Mod+Shift+M".into()),
        Err(e) => {
            println!("   Not added: {e}");
            Ok("not added".into())
        }
    }
}

/// How to use the quickshell service and tile, if the human uses quickshell.
fn quickshell_hint() {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let uses_qs = home
        .as_ref()
        .is_some_and(|h| h.join(".config/quickshell").is_dir());
    if !uses_qs {
        return;
    }
    let share = keys::own_exe()
        .ok()
        .and_then(|e| {
            Path::new(&e)
                .parent()?
                .parent()
                .map(|p| p.join("share/gowl/quickshell"))
        })
        .filter(|p| p.is_dir());
    let from = share.map_or_else(
        || "contrib/quickshell".to_string(),
        |p| p.display().to_string(),
    );
    println!(
        "   quickshell: a status service and a tile are in {from}/. Copy them into your shell's\n   \
         singletons directory, e.g.\n     cp {from}/Gowl.qml {from}/GowlTile.qml ~/.config/quickshell/<your shell>/services/\n   \
         then use `GowlTile {{}}` in a panel (README, \"quickshell\"). gowl never edits your shell."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert_eq!(yes_no("Y"), Some(true));
        assert_eq!(yes_no(" no "), Some(false));
        assert_eq!(yes_no(""), None);
        assert!(
            valid_alias("h6609")
                && valid_alias("desk-2")
                && !valid_alias("a b")
                && !valid_alias("")
        );
    }
}
