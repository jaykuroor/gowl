//! `gowl calibrate [DEV]` — find where each segment of the light sits.
//!
//! Lights one segment red at a time (the rest dim white); the human types its
//! position as a numpad cell (`color::layout_rects`). At the end the light previews
//! the result in edge colors, and `y` writes `segments` and `layout` into the
//! device's config section, leaving the rest of the file byte-for-byte as it was.
//!
//! One `poll()` over stdin, a timerfd (keepalive, so the light stays in stream
//! mode while the human looks) and the signalfd. Razer OFF on every exit path.

use std::io::Write as _;
use std::path::PathBuf;

use crate::color::Rgb;
use crate::config::{self, Config};
use crate::devices::{self, Device};
use crate::error::Error;
use crate::net::Ports;
use crate::proto::MAX_SEGMENTS;
use crate::runtime::{self, Sig, Signals, Streams, Timer};

const RED: Rgb = [255, 0, 0];
const DIM: Rgb = [24, 24, 24];

const LEGEND: &str = "\
One segment at a time turns red (the rest stay dim white). For each, type where it
sits around your screen, seen from your chair, using the numpad as a map, then Enter:

    7 8 9     7 top-left corner   8 top          9 top-right corner
    4 5 6     4 left side         5 behind / middle (follows the whole screen)
    1 2 3     1 bottom-left       2 bottom       3 bottom-right     6 right side
              0 can't see it (stays off)

b = back one   ? = this help   q = quit without saving (Ctrl+C too)
If one red patch looks like two pieces, or part of the light never turns red,
the segment count is off: n = the light has one more segment, p = one fewer.";

/// Preview color of a placed cell.
fn cell_color(c: Option<u8>) -> Rgb {
    match c {
        Some(8) => [0, 0, 255],
        Some(2) => [0, 255, 0],
        Some(4) => [255, 0, 0],
        Some(6) => [255, 200, 0],
        Some(7 | 9 | 1 | 3) => [255, 255, 255],
        Some(5) => [60, 60, 60],
        _ => [0, 0, 0],
    }
}

/// What the loop must do after a line of input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Send the new frame and print the prompt.
    Show,
    /// Segment count changed: reopen the stream with this many segments.
    Resize(usize),
    Save,
    Quit,
    Help,
    Bad,
}

/// The walk's state, separate from I/O so it can be tested.
#[derive(Debug)]
struct Walk {
    cells: Vec<Option<u8>>,
    at: usize,
    review: bool,
}

impl Walk {
    fn new(n: usize) -> Walk {
        Walk {
            cells: vec![None; n.clamp(1, MAX_SEGMENTS)],
            at: 0,
            review: false,
        }
    }

    fn n(&self) -> usize {
        self.cells.len()
    }

    fn input(&mut self, line: &str) -> Step {
        let l = line.trim().to_ascii_lowercase();
        match l.as_str() {
            "" => Step::Show,
            "q" | "quit" => Step::Quit,
            "?" | "h" | "help" => Step::Help,
            "b" | "back" => {
                if self.review {
                    self.review = false;
                    self.at = self.n() - 1;
                } else {
                    self.at = self.at.saturating_sub(1);
                }
                Step::Show
            }
            "n" if self.n() < MAX_SEGMENTS => {
                self.cells.push(None);
                if self.review {
                    self.review = false;
                    self.at = self.n() - 1;
                }
                Step::Resize(self.n())
            }
            "p" if self.n() > 1 => {
                self.cells.pop();
                self.at = self.at.min(self.n() - 1);
                self.review = self.review && self.cells.iter().all(Option::is_some);
                Step::Resize(self.n())
            }
            "y" | "yes" if self.review => Step::Save,
            "r" if self.review => {
                self.cells.fill(None);
                self.at = 0;
                self.review = false;
                Step::Show
            }
            d if !self.review && d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => {
                self.cells[self.at] = Some(d.as_bytes()[0] - b'0');
                match (self.at + 1..self.n()).find(|&i| self.cells[i].is_none()) {
                    Some(i) => self.at = i,
                    None if self.cells.iter().all(Option::is_some) => self.review = true,
                    None => self.at = (self.at + 1) % self.n(),
                }
                Step::Show
            }
            _ => Step::Bad,
        }
    }

    /// Segment colors for the current state.
    fn frame(&self, out: &mut [Rgb]) {
        for (i, (o, &c)) in out.iter_mut().zip(&self.cells).enumerate() {
            *o = if self.review {
                cell_color(c)
            } else if i == self.at {
                RED
            } else {
                DIM
            };
        }
    }

    fn layout(&self) -> Vec<u8> {
        self.cells.iter().map(|c| c.unwrap_or(0)).collect()
    }

    fn prompt(&self) -> String {
        if self.review {
            let l: Vec<String> = self.layout().iter().map(u8::to_string).collect();
            format!(
                "\nlayout = {}\nThe light now previews it: top blue, bottom green, left red, right yellow,\n\
                 corners white, whole-screen dim white, off dark.\n\
                 y = save   b = back to the last segment   r = start over   q = quit without saving\n> ",
                l.join(" ")
            )
        } else {
            let now = self.cells[self.at].map_or(String::new(), |c| format!(" (now {c})"));
            format!(
                "segment {}/{} is red{now} — position? ",
                self.at + 1,
                self.n()
            )
        }
    }
}

/// `gowl calibrate [DEV]`.
pub fn cmd_calibrate(rest: &[String]) -> Result<(), Error> {
    if let Some(f) = rest.iter().find(|a| a.starts_with("--")) {
        return Err(Error::Usage(format!("calibrate: unknown option {f}")));
    }
    if rest.len() > 1 {
        return Err(Error::Usage("calibrate: too many arguments".into()));
    }
    let cfg = config::load();
    let cache = devices::load_cache();
    let dev = devices::resolve(rest.first().map(String::as_str), &cfg, &cache)?;
    let ports = Ports::from_env();
    let keepalive = if cfg.keepalive_ms == 0 {
        500
    } else {
        cfg.keepalive_ms
    };
    let open = |n: usize| -> Result<Streams, Error> {
        // Raw segment order while calibrating: no layout, reverse or offset.
        let d = Device {
            segments: n as u8,
            reverse: false,
            offset: 0,
            layout: None,
            ..dev.clone()
        };
        Streams::open(&[d], cfg.lan_if.as_deref(), ports, 0, keepalive)
    };
    let mut walk = Walk::new(usize::from(dev.segments));
    let sig = Signals::new()?;
    let mut timer = Timer::new()?;
    let mut streams = open(walk.n())?;
    println!(
        "gowl calibrate — {}{}, {} segments\n{LEGEND}\n",
        dev.label(),
        dev.sku
            .as_deref()
            .map_or(String::new(), |s| format!(" {s}")),
        walk.n()
    );
    streams.razer(true);
    let r = walk_loop(&mut walk, &mut streams, &open, &sig, &mut timer);
    streams.shutdown();
    match r? {
        true => {
            let layout = walk.layout();
            let path = save(&dev, &cfg, &layout)?;
            println!("saved to {}", path.display());
            if dev.reverse || dev.offset != 0 {
                println!("note: reverse/offset in that section are ignored now that layout is set");
            }
            println!("restart `gowl screen` to use it");
        }
        false => println!("not saved"),
    }
    Ok(())
}

/// Ok(true) = save, Ok(false) = quit.
fn walk_loop(
    walk: &mut Walk,
    streams: &mut Streams,
    open: &dyn Fn(usize) -> Result<Streams, Error>,
    sig: &Signals,
    timer: &mut Timer,
) -> Result<bool, Error> {
    let mut seg = [[0u8; 3]; MAX_SEGMENTS];
    let mut pending: Vec<u8> = Vec::with_capacity(256);
    let mut show = |walk: &Walk, streams: &mut Streams| {
        let n = walk.n();
        walk.frame(&mut seg[..n]);
        streams.push(0, &seg[..n], runtime::now_ns() / 1_000_000);
        print!("{}", walk.prompt());
        let _ = std::io::stdout().flush();
    };
    show(walk, streams);
    let mut pfds = [
        libc::pollfd {
            fd: libc::STDIN_FILENO,
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
    loop {
        timer.arm(streams.keepalive_due_ms().map(|ms| ms * 1_000_000))?;
        // SAFETY: valid pollfd array of the stated length.
        let r = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, -1) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::Io("poll", e));
        }
        if pfds[2].revents != 0 && sig.read() == Some(Sig::Stop) {
            println!();
            return Ok(false);
        }
        if pfds[1].revents != 0 {
            timer.drain();
            streams.keepalive(runtime::now_ns() / 1_000_000);
        }
        if pfds[0].revents == 0 {
            continue;
        }
        let mut buf = [0u8; 256];
        // SAFETY: reading into a stack buffer of the stated length.
        let got = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if got < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::Io("read stdin", e));
        }
        if got == 0 {
            println!();
            return Ok(false); // EOF
        }
        pending.extend_from_slice(&buf[..got as usize]);
        while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=nl).collect();
            match walk.input(&String::from_utf8_lossy(&line)) {
                Step::Show => show(walk, streams),
                Step::Resize(n) => {
                    *streams = open(n)?;
                    streams.razer(true);
                    println!("now {n} segments");
                    show(walk, streams);
                }
                Step::Save => return Ok(true),
                Step::Quit => return Ok(false),
                Step::Help => {
                    println!("{LEGEND}");
                    show(walk, streams);
                }
                Step::Bad => {
                    print!(
                        "type a digit 0-9{} (? for help): ",
                        if walk.review {
                            ", or y / b / r / q"
                        } else {
                            ""
                        }
                    );
                    let _ = std::io::stdout().flush();
                }
            }
        }
    }
}

/// A section alias for a new device: its SKU (lowercase) or `light`, made unique.
fn new_alias(dev: &Device, cfg: &Config) -> String {
    let base = dev
        .sku
        .as_deref()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map_or_else(|| "light".to_string(), str::to_ascii_lowercase);
    let taken = |a: &str| cfg.devices.iter().any(|d| d.alias == a);
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|i| format!("{base}-{i}"))
        .find(|a| !taken(a))
        .unwrap_or(base)
}

/// The config text with `segments` and `layout` set for `dev`: in its existing
/// section, else in a new `[device ALIAS]` section appended at the end (a new
/// file also gets `device = ALIAS`, so DEV can be left out afterwards).
fn updated_config(text: &str, dev: &Device, cfg: &Config, layout: &[u8]) -> String {
    let cells: Vec<String> = layout.iter().map(u8::to_string).collect();
    let keys = [
        ("segments", layout.len().to_string()),
        (
            "layout",
            format!(
                "{}   # where each segment sits (numpad cells), from gowl calibrate",
                cells.join(" ")
            ),
        ),
    ];
    if let Some(out) = dev
        .alias
        .as_deref()
        .and_then(|a| config::set_device_keys(text, a, &keys))
    {
        return out;
    }
    let alias = new_alias(dev, cfg);
    let mut out = text.to_string();
    if out.is_empty() {
        out += &format!("# gowl config (see config.example for every key)\ndevice = {alias}\n");
    } else if !out.ends_with('\n') {
        out.push('\n');
    }
    out += &format!("\n[device {alias}]\n");
    if let Some(m) = &dev.mac {
        out += &format!("mac = {m}\n");
    }
    out += &format!("ip = {}\n", dev.ip);
    if let Some(s) = &dev.sku {
        out += &format!("sku = {s}\n");
    }
    for (k, v) in keys {
        out += &format!("{k} = {v}\n");
    }
    out
}

fn save(dev: &Device, cfg: &Config, layout: &[u8]) -> Result<PathBuf, Error> {
    let path = config::config_path().ok_or_else(|| {
        Error::Runtime("no config path: HOME and XDG_CONFIG_HOME are unset".into())
    })?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::Io("read config", e)),
    };
    let out = updated_config(&text, dev, cfg, layout);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::Io("create config dir", e))?;
    }
    // Write a sibling file, then rename: a crash never leaves a half-written config.
    let tmp = path.with_extension("gowl-tmp");
    std::fs::write(&tmp, out).map_err(|e| Error::Io("write config", e))?;
    std::fs::rename(&tmp, &path).map_err(|e| Error::Io("rename config", e))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::SegSource;
    use std::net::Ipv4Addr;

    #[test]
    fn walk_places_every_segment_then_reviews() {
        let mut w = Walk::new(3);
        let mut f = [[9u8; 3]; 3];
        w.frame(&mut f);
        assert_eq!(f, [RED, DIM, DIM]);
        assert_eq!(w.input("4\n"), Step::Show);
        assert_eq!(w.input(" 8 "), Step::Show);
        w.frame(&mut f);
        assert_eq!(f, [DIM, DIM, RED]);
        assert_eq!(w.input("b"), Step::Show);
        assert_eq!(w.at, 1);
        assert_eq!(w.input("9"), Step::Show); // re-place segment 2
        assert!(!w.review);
        assert_eq!(w.input("x"), Step::Bad);
        assert_eq!(w.input("y"), Step::Bad); // nothing to save yet
        assert_eq!(w.input("6"), Step::Show);
        assert!(w.review);
        w.frame(&mut f);
        assert_eq!(
            f,
            [
                cell_color(Some(4)),
                cell_color(Some(9)),
                cell_color(Some(6))
            ]
        );
        assert_eq!(w.layout(), vec![4, 9, 6]);
        assert_eq!(w.input("Y"), Step::Save);
    }

    #[test]
    fn walk_resizes_and_restarts() {
        let mut w = Walk::new(2);
        w.input("4");
        w.input("6");
        assert!(w.review);
        assert_eq!(w.input("n"), Step::Resize(3));
        assert!(!w.review && w.at == 2);
        assert_eq!(w.input("2"), Step::Show);
        assert!(w.review);
        assert_eq!(w.input("p"), Step::Resize(2));
        assert!(
            w.review,
            "dropping the last segment keeps a complete layout"
        );
        assert_eq!(w.input("r"), Step::Show);
        assert_eq!((w.at, w.review, w.layout()), (0, false, vec![0, 0]));
        let mut one = Walk::new(1);
        assert_eq!(one.input("p"), Step::Bad);
        assert_eq!(one.input("q"), Step::Quit);
    }

    fn dev(alias: Option<&str>) -> Device {
        Device {
            ip: Ipv4Addr::new(192, 168, 1, 3),
            mac: None,
            sku: Some("H6609".into()),
            alias: alias.map(str::to_string),
            segments: 10,
            seg_source: SegSource::Default,
            reverse: false,
            offset: 0,
            layout: None,
        }
    }

    #[test]
    fn saved_layout_round_trips_through_the_config() {
        let layout = [4, 4, 8, 8, 9, 6, 6, 2, 2, 1];
        // New file: a section keyed by IP plus device = alias.
        let out = updated_config("", &dev(None), &Config::default(), &layout);
        let c = config::parse(&out, &mut |_, m| panic!("{m}"));
        assert_eq!(c.device.as_deref(), Some("h6609"));
        let d = &c.devices[0];
        assert_eq!(
            (d.alias.as_str(), d.ip),
            ("h6609", Some(Ipv4Addr::new(192, 168, 1, 3)))
        );
        assert_eq!(
            (d.segments, d.layout.as_deref()),
            (Some(10), Some(&layout[..]))
        );
        let cache = [];
        let r = devices::resolve(Some("192.168.1.3"), &c, &cache).unwrap();
        assert_eq!(r.layout.as_deref(), Some(&layout[..]));
        // Existing section: updated in place, nothing else touched.
        let text = "fps = 30\n[device desk]\nip = 192.168.1.3\nsegments = 8\n";
        let cfg = config::parse(text, &mut |_, m| panic!("{m}"));
        let out = updated_config(text, &dev(Some("desk")), &cfg, &layout);
        assert!(out.starts_with(
            "fps = 30\n[device desk]\nip = 192.168.1.3\nsegments = 10\nlayout = 4 4 8"
        ));
        // Existing file, no section: appended, alias made unique.
        let text = "fps = 30\n[device h6609]\nip = 10.0.0.1";
        let cfg = config::parse(text, &mut |_, m| panic!("{m}"));
        let out = updated_config(text, &dev(None), &cfg, &layout);
        assert!(out.starts_with("fps = 30\n[device h6609]\nip = 10.0.0.1\n\n[device h6609-2]\n"));
        assert!(!out.contains("device = "));
    }
}
