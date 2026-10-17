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
        "fps" => c.fps = int_in(v, 1, 240)?,
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
        _ => return Err("unknown key (ignored)".into()),
    }
    Ok(())
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
}
