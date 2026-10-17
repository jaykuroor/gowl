//! `key = value` config with `[device ALIAS]` sections (SPEC.md "Config"), XDG paths
//! and env overrides. Parsed once at startup; unknown keys warn, never fail.

use std::net::Ipv4Addr;
use std::path::PathBuf;

/// Per-device section `[device ALIAS]`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceCfg {
    pub alias: String,
    pub mac: Option<String>,
    pub ip: Option<Ipv4Addr>,
    pub sku: Option<String>,
    pub segments: Option<u8>,
    pub reverse: bool,
    pub offset: u16,
    /// Calibrated position of each segment (numpad cells, `color::layout_rects`).
    pub layout: Option<Vec<u8>>,
}

/// Defaults are the legacy values (`legacy/lumisync/config/options.py`), except
/// `fps` (30, see DECISIONS D-004).
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub device: Option<String>,
    pub output: Option<String>,
    pub fps: u32,
    pub depth: f64,
    pub saturation: f64,
    pub screen_brightness: f64,
    pub smoothing: f64,
    pub threshold: u8,
    pub keepalive_ms: u32,
    pub reaction: String,
    pub palette: String,
    pub music_fps: u32,
    pub music_gain: f64,
    pub music_brightness: f64,
    pub music_smoothing: f64,
    pub lan_if: Option<String>,
    pub devices: Vec<DeviceCfg>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            device: None,
            output: None,
            fps: 30,
            depth: 0.28,
            saturation: 1.15,
            screen_brightness: 0.75,
            smoothing: 0.55,
            threshold: 3,
            keepalive_ms: 500,
            reaction: "flow".into(),
            palette: "rgb".into(),
            music_fps: 60,
            music_gain: 1.7,
            music_brightness: 0.85,
            music_smoothing: 0.6,
            lan_if: None,
            devices: Vec::new(),
        }
    }
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn home_join(rel: &str) -> Option<PathBuf> {
    env_path("HOME").map(|h| h.join(rel))
}

/// `$GOWL_CONFIG`, else `$XDG_CONFIG_HOME/gowl/config`, else `~/.config/gowl/config`.
pub fn config_path() -> Option<PathBuf> {
    env_path("GOWL_CONFIG").or_else(|| {
        env_path("XDG_CONFIG_HOME")
            .or_else(|| home_join(".config"))
            .map(|d| d.join("gowl/config"))
    })
}

/// `$XDG_STATE_HOME/gowl`, else `~/.local/state/gowl`.
pub fn state_dir() -> Option<PathBuf> {
    env_path("XDG_STATE_HOME")
        .or_else(|| home_join(".local/state"))
        .map(|d| d.join("gowl"))
}

/// `$XDG_RUNTIME_DIR` (pidfile lives here).
pub fn runtime_dir() -> Option<PathBuf> {
    env_path("XDG_RUNTIME_DIR")
}

/// Load the config file; a missing file means defaults.
pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text, &mut |line, msg| {
            warn!("{}:{line}: {msg}", path.display())
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => {
            warn!("cannot read {}: {e}; using defaults", path.display());
            Config::default()
        }
    }
}

fn num<T: std::str::FromStr>(v: &str) -> Result<T, String> {
    v.parse().map_err(|_| format!("bad number {v:?}"))
}

fn float_in(v: &str, lo: f64, hi: f64) -> Result<f64, String> {
    let f: f64 = num(v)?;
    if f.is_finite() && (lo..=hi).contains(&f) {
        Ok(f)
    } else {
        Err(format!("{v} out of range {lo}-{hi}"))
    }
}

fn int_in<T>(v: &str, lo: T, hi: T) -> Result<T, String>
where
    T: std::str::FromStr + PartialOrd + std::fmt::Display + Copy,
{
    let n: T = num(v)?;
    if n >= lo && n <= hi {
        Ok(n)
    } else {
        Err(format!("{v} out of range {lo}-{hi}"))
    }
}

fn boolean(v: &str) -> Result<bool, String> {
    match v {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("bad boolean {v:?}")),
    }
}

/// Parse config text. `warn(line_no, message)` is called for every problem.
pub fn parse(text: &str, warn: &mut dyn FnMut(usize, String)) -> Config {
    let mut c = Config::default();
    let mut section: Option<usize> = None; // index into c.devices
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(inner) = line.strip_prefix('[') {
            let Some(inner) = inner.strip_suffix(']') else {
                warn(n, format!("bad section header {line:?}"));
                continue;
            };
            let mut words = inner.split_whitespace();
            match (words.next(), words.next(), words.next()) {
                (Some("device"), Some(alias), None) => {
                    c.devices.retain(|d| d.alias != alias);
                    c.devices.push(DeviceCfg {
                        alias: alias.to_string(),
                        ..DeviceCfg::default()
                    });
                    section = Some(c.devices.len() - 1);
                }
                _ => {
                    warn(
                        n,
                        format!("unknown section {line:?} (expected [device ALIAS])"),
                    );
                    section = None;
                }
            }
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            warn(n, format!("expected key = value, got {line:?}"));
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        let res = match section {
            Some(idx) => device_key(&mut c.devices[idx], k, v),
            None => global_key(&mut c, k, v),
        };
        if let Err(msg) = res {
            warn(n, format!("{k}: {msg}"));
        }
    }
    c
}

fn global_key(c: &mut Config, k: &str, v: &str) -> Result<(), String> {
    match k {
        "device" => c.device = Some(v.to_string()),
        "output" => c.output = Some(v.to_string()),
        "fps" => c.fps = int_in(v, 1, 40)?, // 40 Hz device cap (D-020)
        "depth" => c.depth = float_in(v, 0.10, 0.50)?,
        "saturation" => c.saturation = float_in(v, 0.0, 4.0)?,
        "screen_brightness" => c.screen_brightness = float_in(v, 0.0, 1.0)?,
        "smoothing" => c.smoothing = float_in(v, 0.0, 1.0)?,
        "threshold" => c.threshold = int_in(v, 0, 255)?,
        "keepalive_ms" => c.keepalive_ms = int_in(v, 0, 60_000)?,
        "reaction" => c.reaction = v.to_string(),
        "palette" => c.palette = v.to_string(),
        "music_fps" => c.music_fps = int_in(v, 1, 240)?,
        "music_gain" => c.music_gain = float_in(v, 0.0, 100.0)?,
        "music_brightness" => c.music_brightness = float_in(v, 0.0, 1.0)?,
        "music_smoothing" => c.music_smoothing = float_in(v, 0.0, 1.0)?,
        "lan_if" => c.lan_if = Some(v.to_string()),
        _ => return Err("unknown key (ignored)".into()),
    }
    Ok(())
}

fn device_key(d: &mut DeviceCfg, k: &str, v: &str) -> Result<(), String> {
    match k {
        "mac" => d.mac = Some(v.to_ascii_uppercase()),
        "ip" => d.ip = Some(v.parse().map_err(|_| format!("bad IPv4 address {v:?}"))?),
        "sku" => d.sku = Some(v.to_ascii_uppercase()),
        "segments" => d.segments = Some(int_in(v, 1, 255)?),
        "reverse" => d.reverse = boolean(v)?,
        "offset" => d.offset = int_in(v, 0, 254)?,
        "layout" => d.layout = Some(parse_layout(v)?),
        _ => return Err("unknown key (ignored)".into()),
    }
    Ok(())
}

/// `layout = 4 4 8 8 9 …`: one numpad cell (0-9) per segment, spaces or commas.
pub fn parse_layout(v: &str) -> Result<Vec<u8>, String> {
    let cells = v
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
        .map(|w| match w.parse::<u8>() {
            Ok(c) if c <= crate::color::LAYOUT_MAX_CELL => Ok(c),
            _ => Err(format!("bad cell {w:?} (expected 0-9)")),
        })
        .collect::<Result<Vec<u8>, String>>()?;
    if cells.is_empty() || cells.len() > crate::proto::MAX_SEGMENTS {
        return Err(format!("expected 1-{} cells", crate::proto::MAX_SEGMENTS));
    }
    Ok(cells)
}

/// What `parse` makes of a line that starts with `[`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Header<'a> {
    /// `[device ALIAS]`: keys below belong to ALIAS.
    Device(&'a str),
    /// A well-formed header that isn't a device section: keys below are global.
    Other,
    /// No closing `]`: ignored, the current section continues.
    Malformed,
}

fn header(line: &str) -> Option<Header<'_>> {
    let l = line.split('#').next().unwrap_or("").trim();
    let inner = l.strip_prefix('[')?;
    let Some(inner) = inner.strip_suffix(']') else {
        return Some(Header::Malformed);
    };
    let mut w = inner.split_whitespace();
    Some(match (w.next(), w.next(), w.next()) {
        (Some("device"), Some(alias), None) => Header::Device(alias),
        _ => Header::Other,
    })
}

/// The key of a `key = value` line (comment stripped), if it is one.
fn line_key(line: &str) -> Option<&str> {
    let l = line.split('#').next()?.trim();
    if l.starts_with('[') {
        return None;
    }
    l.split_once('=').map(|(k, _)| k.trim())
}

/// For each line, the device section it belongs to (`None` = global), exactly as
/// `parse` sees it.
fn sections(lines: &[String]) -> Vec<Option<&str>> {
    let mut cur: Option<&str> = None;
    lines
        .iter()
        .map(|l| {
            match header(l) {
                Some(Header::Device(a)) => cur = Some(a),
                Some(Header::Other) => cur = None,
                Some(Header::Malformed) | None => {}
            }
            cur
        })
        .collect()
}

/// The file's line ending: CRLF if any line uses it.
fn eol_of(lines: &[String]) -> &'static str {
    if lines.iter().any(|l| l.ends_with("\r\n")) {
        "\r\n"
    } else {
        "\n"
    }
}

/// `key = value` with the line ending of the line it replaces (or the file's).
fn key_line(key: &str, value: &str, old: Option<&str>, eol: &str) -> String {
    let e = match old {
        Some(o) if o.ends_with("\r\n") => "\r\n",
        Some(o) if o.ends_with('\n') => "\n",
        _ => eol,
    };
    format!("{key} = {value}{e}")
}

/// Insert `new` at `at`, giving an unterminated previous line its line ending.
fn insert_line(lines: &mut Vec<String>, at: usize, new: String, eol: &str) {
    if let Some(prev) = lines.get_mut(at.wrapping_sub(1))
        && !prev.ends_with('\n')
    {
        prev.push_str(eol);
    }
    lines.insert(at, new);
}

/// Set `keys` in the `[device ALIAS]` section of config `text`, leaving every
/// other byte as it was: an existing `key =` line is replaced in place, a missing
/// one goes after the section's last non-blank line. The last header for ALIAS
/// wins, like `parse`; the section ends at the next well-formed header (a
/// malformed one doesn't end it, as in `parse`). `None` if there is no such section.
pub fn set_device_keys(text: &str, alias: &str, keys: &[(&str, String)]) -> Option<String> {
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
    let eol = eol_of(&lines);
    let head = lines
        .iter()
        .rposition(|l| header(l) == Some(Header::Device(alias)))?;
    let end = lines[head + 1..]
        .iter()
        .position(|l| matches!(header(l), Some(Header::Device(_) | Header::Other)))
        .map_or(lines.len(), |p| head + 1 + p);
    let mut insert_at = lines[head + 1..end]
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map_or(head + 1, |p| head + 2 + p);
    for (k, v) in keys {
        match (head + 1..end)
            .rev()
            .find(|&i| line_key(&lines[i]) == Some(k))
        {
            Some(i) => lines[i] = key_line(k, v, Some(&lines[i]), eol),
            None => {
                insert_line(&mut lines, insert_at, key_line(k, v, None, eol), eol);
                insert_at += 1;
            }
        }
    }
    Some(lines.concat())
}

/// Set the global `key = value` in config `text`, leaving every other byte as it
/// was. "Global" is what `parse` treats as global: lines before the first device
/// section and lines after a non-device section header. The last such `key =`
/// line (the one `parse` keeps) is replaced in place; if there is none, the line
/// goes after the last non-blank line before the first `[device …]` section (or
/// at the end of a file without one). Line endings (LF/CRLF) are kept.
pub fn set_global_key(text: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
    let eol = eol_of(&lines);
    let sec = sections(&lines);
    let found = (0..lines.len())
        .rev()
        .find(|&i| sec[i].is_none() && line_key(&lines[i]) == Some(key));
    match found {
        Some(i) => lines[i] = key_line(key, value, Some(&lines[i]), eol),
        None => {
            let end = lines
                .iter()
                .position(|l| matches!(header(l), Some(Header::Device(_))))
                .unwrap_or(lines.len());
            let at = lines[..end]
                .iter()
                .rposition(|l| !l.trim().is_empty())
                .map_or(0, |p| p + 1);
            insert_line(&mut lines, at, key_line(key, value, None, eol), eol);
        }
    }
    lines.concat()
}

/// The config file's text; a missing file is empty.
pub fn read_text(path: &std::path::Path) -> Result<String, crate::Error> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(crate::Error::Io("read config", e)),
    }
}

/// Write the config through a sibling file + rename: a crash never leaves a
/// half-written config. A symlinked config stays a symlink (the target is
/// written), and the file keeps its permissions.
pub fn write_atomic(path: &std::path::Path, text: &str) -> Result<(), crate::Error> {
    use crate::Error;
    let target = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => match std::fs::canonicalize(path) {
            Ok(t) => t,
            // Dangling link: write where it points.
            Err(_) => {
                let l = std::fs::read_link(path).map_err(|e| Error::Io("read config link", e))?;
                match path.parent() {
                    Some(d) if l.is_relative() => d.join(l),
                    _ => l,
                }
            }
        },
        _ => path.to_path_buf(),
    };
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::Io("create config dir", e))?;
    }
    let perms = std::fs::metadata(&target).ok().map(|m| m.permissions());
    let tmp = target.with_extension("gowl-tmp");
    std::fs::write(&tmp, text).map_err(|e| Error::Io("write config", e))?;
    if let Some(p) = perms {
        std::fs::set_permissions(&tmp, p).map_err(|e| Error::Io("chmod config", e))?;
    }
    std::fs::rename(&tmp, &target).map_err(|e| Error::Io("rename config", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses_cleanly() {
        let text = include_str!("../config.example");
        let mut warnings = Vec::new();
        let c = parse(text, &mut |n, m| warnings.push(format!("{n}: {m}")));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(c.device.as_deref(), Some("desk"));
        assert_eq!(c.fps, 30);
        assert_eq!(c.keepalive_ms, 500);
        let d = &c.devices[0];
        assert_eq!(d.alias, "desk");
        assert_eq!(d.ip, Some(Ipv4Addr::new(192, 168, 1, 50)));
        assert_eq!(d.segments, Some(10));
        assert_eq!(d.mac.as_deref(), Some("AA:BB:CC:DD:EE:FF:00:11"));
    }

    #[test]
    fn bad_values_warn_and_keep_defaults() {
        let mut n = 0;
        let c = parse(
            "fps = fast\nnope = 1\ndepth=0.9 # too deep\n[device x]\nreverse = maybe\n[bogus]\n",
            &mut |_, _| n += 1,
        );
        assert_eq!(n, 5);
        assert_eq!(c.fps, 30);
        assert_eq!(c.depth, 0.28);
        assert!(!c.devices[0].reverse);
    }

    #[test]
    fn layout_parses() {
        let c = parse(
            "[device a]\nlayout = 4 4,8 8 9 6 6 2 2 1 # from calibrate\n",
            &mut |_, m| panic!("{m}"),
        );
        assert_eq!(
            c.devices[0].layout,
            Some(vec![4, 4, 8, 8, 9, 6, 6, 2, 2, 1])
        );
        assert!(parse_layout("1 x").is_err());
        assert!(parse_layout("10").is_err());
        assert!(parse_layout("  ").is_err());
    }

    #[test]
    fn set_device_keys_preserves_everything_else() {
        let text = "fps = 30 # mine\n\n[device desk]\nmac = AA\nsegments = 8   # old\n\n# trailing note\n[device other]\nsegments = 4\n";
        let out = set_device_keys(
            text,
            "desk",
            &[("segments", "10".into()), ("layout", "4 8 6 2".into())],
        )
        .unwrap();
        assert_eq!(
            out,
            "fps = 30 # mine\n\n[device desk]\nmac = AA\nsegments = 10\n\n# trailing note\nlayout = 4 8 6 2\n[device other]\nsegments = 4\n"
        );
        // No trailing newline, section at the end of the file.
        let out = set_device_keys("[device x]\nip = 1.2.3.4", "x", &[("layout", "5".into())]);
        assert_eq!(
            out.as_deref(),
            Some("[device x]\nip = 1.2.3.4\nlayout = 5\n")
        );
        assert_eq!(set_device_keys(text, "nope", &[]), None);
    }

    #[test]
    fn set_global_key_preserves_everything_else() {
        let k = |t: &str| set_global_key(t, "keepalive_ms", "2100");
        // Replaced in place (the last global occurrence wins, like parse); comments,
        // unknown keys and sections untouched; a section's own key never matched.
        let text = "# mine\nkeepalive_ms = 500\nfps = 30\nkeepalive_ms=700 # later\nwhatever = 1\n\n[device a]\nkeepalive_ms = 9\n";
        assert_eq!(
            k(text),
            "# mine\nkeepalive_ms = 500\nfps = 30\nkeepalive_ms = 2100\nwhatever = 1\n\n[device a]\nkeepalive_ms = 9\n"
        );
        // Missing: after the last global line, before the blank line + section.
        assert_eq!(
            k("fps = 30\n\n[device a]\nip = 1.2.3.4\n"),
            "fps = 30\nkeepalive_ms = 2100\n\n[device a]\nip = 1.2.3.4\n"
        );
        // A commented-out key is not a key.
        assert_eq!(
            k("# keepalive_ms = 1\n"),
            "# keepalive_ms = 1\nkeepalive_ms = 2100\n"
        );
        // File starting with a section, file without newline, empty file.
        assert_eq!(k("[device a]\n"), "keepalive_ms = 2100\n[device a]\n");
        assert_eq!(k("fps = 30"), "fps = 30\nkeepalive_ms = 2100\n");
        assert_eq!(k(""), "keepalive_ms = 2100\n");
        let c = parse(&k(text), &mut |_, _| {}); // unknown keys only warn
        assert_eq!(c.keepalive_ms, 2100);
        assert_eq!(c.devices.len(), 1);
    }

    #[test]
    fn rewrites_follow_parse_sections_and_keep_crlf() {
        let k = |t: &str| set_global_key(t, "keepalive_ms", "2100");
        // Keys after an unknown section are global for parse, so they are found.
        let text = "fps = 30\n[device a]\nip = 1.2.3.4\n[extras]\nkeepalive_ms = 9\n";
        let out = k(text);
        assert_eq!(
            out,
            "fps = 30\n[device a]\nip = 1.2.3.4\n[extras]\nkeepalive_ms = 2100\n"
        );
        assert_eq!(parse(&out, &mut |_, _| {}).keepalive_ms, 2100);
        // A malformed header doesn't end the device section: its keys stay the device's.
        let text = "[device a]\n[oops\nkeepalive_ms = 9\n";
        let out = k(text);
        assert_eq!(
            out,
            "keepalive_ms = 2100\n[device a]\n[oops\nkeepalive_ms = 9\n"
        );
        assert_eq!(parse(&out, &mut |_, _| {}).keepalive_ms, 2100);
        let d = set_device_keys(text, "a", &[("segments", "4".into())]).unwrap();
        assert_eq!(d, "[device a]\n[oops\nkeepalive_ms = 9\nsegments = 4\n");
        // CRLF: replaced and inserted lines keep CRLF, the rest is untouched.
        let text = "fps = 30\r\nkeepalive_ms = 5\r\n\r\n[device a]\r\nip = 1.2.3.4\r\n";
        assert_eq!(
            k(text),
            "fps = 30\r\nkeepalive_ms = 2100\r\n\r\n[device a]\r\nip = 1.2.3.4\r\n"
        );
        assert_eq!(
            k("fps = 30\r\n[device a]\r\n"),
            "fps = 30\r\nkeepalive_ms = 2100\r\n[device a]\r\n"
        );
        assert_eq!(
            set_device_keys("[device a]\r\nip = 1.2.3.4", "a", &[("layout", "5".into())]),
            Some("[device a]\r\nip = 1.2.3.4\r\nlayout = 5\r\n".into())
        );
    }

    #[test]
    fn write_atomic_keeps_symlinks_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("gowl-cfg-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("dotfiles")).unwrap();
        let real = d.join("dotfiles/gowl.conf");
        std::fs::write(&real, "fps = 30\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = d.join("config");
        std::os::unix::fs::symlink("dotfiles/gowl.conf", &link).unwrap();
        write_atomic(&link, "fps = 20\n").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "fps = 20\n");
        let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // A new file is created normally.
        write_atomic(&d.join("new/config"), "x = 1\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("new/config")).unwrap(),
            "x = 1\n"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
