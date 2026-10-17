//! Device cache (`$XDG_STATE_HOME/gowl/devices`, one `mac ip sku` per line), DEV
//! resolution (alias | IP | MAC | index) and the built-in SKU → segment table.

use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::path::PathBuf;

use crate::config::{Config, DeviceCfg};
use crate::error::Error;

/// Built-in SKU table (legacy `sku_catalog.BUILTIN`).
const SKU_SEGMENTS: &[(&str, u8)] = &[
    ("H619A", 10),
    ("H619B", 10),
    ("H619C", 10),
    ("H619D", 10),
    ("H619E", 10),
    ("H619Z", 10),
    ("H6199", 10),
    ("H6672", 14),
];

pub const DEFAULT_SEGMENTS: u8 = 10;

pub fn sku_segments(sku: &str) -> Option<u8> {
    let sku = sku.trim();
    SKU_SEGMENTS
        .iter()
        .find(|(s, _)| s.eq_ignore_ascii_case(sku))
        .map(|&(_, n)| n)
}

/// One cached device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cached {
    pub mac: String,
    pub ip: Ipv4Addr,
    pub sku: String,
}

pub fn cache_path() -> Option<PathBuf> {
    crate::config::state_dir().map(|d| d.join("devices"))
}

pub fn parse_cache(text: &str) -> Vec<Cached> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let mac = w.next()?.to_ascii_uppercase();
            let ip = w.next()?.parse().ok()?;
            let sku = w.next().unwrap_or("?").to_string();
            Some(Cached { mac, ip, sku })
        })
        .collect()
}

pub fn load_cache() -> Vec<Cached> {
    cache_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| parse_cache(&t))
        .unwrap_or_default()
}

/// Atomic write: temp file in the same directory, then `rename`.
pub fn save_cache(devices: &[Cached]) -> io::Result<PathBuf> {
    let path = cache_path().ok_or_else(|| io::Error::other("no XDG_STATE_HOME or HOME"))?;
    let dir = path.parent().expect("cache path has a parent");
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".devices.{}.tmp", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        for d in devices {
            writeln!(f, "{} {} {}", d.mac, d.ip, d.sku)?;
        }
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Update the cache with scan results: same MAC → new IP/SKU; new MAC → appended.
/// Devices not seen this time are kept (a scan can miss a device; the firewall
/// may drop replies).
pub fn merge(cache: &mut Vec<Cached>, found: &[Cached]) {
    for f in found {
        match cache.iter_mut().find(|c| c.mac == f.mac) {
            Some(c) => *c = f.clone(),
            None => cache.push(f.clone()),
        }
    }
}

/// Where a segment count came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegSource {
    Config,
    SkuTable,
    Default,
}

/// A fully resolved target device.
#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub ip: Ipv4Addr,
    pub mac: Option<String>,
    pub sku: Option<String>,
    pub alias: Option<String>,
    pub segments: u8,
    pub seg_source: SegSource,
    pub reverse: bool,
    pub offset: u16,
    /// Calibrated segment positions (`gowl calibrate`); replaces reverse/offset.
    pub layout: Option<Vec<u8>>,
}

impl Device {
    /// Short human label: alias, else IP.
    pub fn label(&self) -> String {
        match &self.alias {
            Some(a) => format!("{a} ({})", self.ip),
            None => self.ip.to_string(),
        }
    }
}

/// Govee ids are 8 bytes; accept the 6-byte Wi-Fi MAC a user sees in the app too.
fn mac_matches(id: &str, given: &str) -> bool {
    let (id, given) = (id.to_ascii_uppercase(), given.to_ascii_uppercase());
    id == given || (given.len() >= 17 && id.ends_with(&format!(":{given}")))
}

fn looks_like_mac(s: &str) -> bool {
    s.contains(':')
        && s.split(':')
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Build a `Device` from what we know, filling segments by precedence
/// config → SKU table → 10.
fn finish(ip: Ipv4Addr, mac: Option<String>, cfg: Option<&DeviceCfg>, cache: &[Cached]) -> Device {
    let cached = cache
        .iter()
        .find(|c| mac.as_deref().is_some_and(|m| mac_matches(&c.mac, m)))
        .or_else(|| cache.iter().find(|c| c.ip == ip));
    let mac = mac.or_else(|| cached.map(|c| c.mac.clone()));
    let sku = cfg
        .and_then(|d| d.sku.clone())
        .or_else(|| cached.map(|c| c.sku.clone()).filter(|s| s != "?"));
    let mut layout = cfg.and_then(|d| d.layout.clone());
    let from_layout = layout.as_ref().map(|l| l.len() as u8);
    let (segments, seg_source) = match (
        cfg.and_then(|d| d.segments).or(from_layout),
        sku.as_deref().and_then(sku_segments),
    ) {
        (Some(n), _) => (n, SegSource::Config),
        (None, Some(n)) => (n, SegSource::SkuTable),
        (None, None) => (DEFAULT_SEGMENTS, SegSource::Default),
    };
    if from_layout.is_some_and(|l| l != segments) {
        warn!(
            "layout has {} cells but segments = {segments}; layout ignored (re-run `gowl calibrate`)",
            from_layout.unwrap_or(0)
        );
        layout = None;
    }
    Device {
        ip,
        mac,
        sku,
        alias: cfg.map(|d| d.alias.clone()),
        segments,
        seg_source,
        reverse: cfg.is_some_and(|d| d.reverse),
        offset: cfg.map_or(0, |d| d.offset),
        layout,
    }
}

fn section_for<'a>(cfg: &'a Config, ip: Ipv4Addr, mac: Option<&str>) -> Option<&'a DeviceCfg> {
    cfg.devices.iter().find(|d| {
        d.ip == Some(ip) || matches!((d.mac.as_deref(), mac), (Some(a), Some(b)) if mac_matches(a, b) || mac_matches(b, a))
    })
}

/// Resolve one DEV spec. `None` → `$GOWL_DEVICE` → config `device=` → the only cached
/// device → error.
pub fn resolve(spec: Option<&str>, cfg: &Config, cache: &[Cached]) -> Result<Device, Error> {
    let env = std::env::var("GOWL_DEVICE")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let spec = spec
        .map(str::to_string)
        .or(env)
        .or_else(|| cfg.device.clone());
    let Some(spec) = spec else {
        return match cache {
            [only] => Ok(finish(
                only.ip,
                Some(only.mac.clone()),
                section_for(cfg, only.ip, Some(&only.mac)),
                cache,
            )),
            [] => Err(Error::NoDevice(
                "no device: pass DEV, set device= in the config or GOWL_DEVICE, or run `gowl scan`"
                    .into(),
            )),
            _ => Err(Error::NoDevice(format!(
                "{} cached devices: pass DEV (alias, IP, MAC or index from `gowl list`) or set device= in the config",
                cache.len()
            ))),
        };
    };
    let spec = spec.trim();

    // Alias.
    if let Some(d) = cfg.devices.iter().find(|d| d.alias == spec) {
        let mac = d.mac.clone();
        let ip = d.ip.or_else(|| {
            mac.as_deref()
                .and_then(|m| cache.iter().find(|c| mac_matches(&c.mac, m)))
                .map(|c| c.ip)
        });
        return match ip {
            Some(ip) => Ok(finish(ip, mac, Some(d), cache)),
            None => Err(Error::NoDevice(format!(
                "device {spec:?} has no ip= and its mac is not in the cache; run `gowl scan`"
            ))),
        };
    }
    // IP literal.
    if let Ok(ip) = spec.parse::<Ipv4Addr>() {
        let mac = cache.iter().find(|c| c.ip == ip).map(|c| c.mac.clone());
        return Ok(finish(
            ip,
            mac.clone(),
            section_for(cfg, ip, mac.as_deref()),
            cache,
        ));
    }
    // MAC / device id.
    if looks_like_mac(spec) {
        if let Some(c) = cache.iter().find(|c| mac_matches(&c.mac, spec)) {
            return Ok(finish(
                c.ip,
                Some(c.mac.clone()),
                section_for(cfg, c.ip, Some(&c.mac)),
                cache,
            ));
        }
        return Err(Error::NoDevice(format!(
            "MAC {spec} is not in the device cache; run `gowl scan`"
        )));
    }
    // Index from `gowl list` (1-based).
    if let Ok(i) = spec.parse::<usize>() {
        return match i.checked_sub(1).and_then(|i| cache.get(i)) {
            Some(c) => Ok(finish(
                c.ip,
                Some(c.mac.clone()),
                section_for(cfg, c.ip, Some(&c.mac)),
                cache,
            )),
            None => Err(Error::NoDevice(format!(
                "no device #{i} (cache has {}); see `gowl list`",
                cache.len()
            ))),
        };
    }
    Err(Error::NoDevice(format!(
        "unknown device {spec:?}: not an alias, IPv4 address, MAC or index"
    )))
}

/// Several DEVs for a sync mode (SPEC.md): none → the default device; the same
/// light named twice (by alias and IP, say) is streamed to once.
pub fn resolve_many(specs: &[&str], cfg: &Config, cache: &[Cached]) -> Result<Vec<Device>, Error> {
    if specs.is_empty() {
        return Ok(vec![resolve(None, cfg, cache)?]);
    }
    let mut out: Vec<Device> = Vec::with_capacity(specs.len());
    for s in specs {
        let d = resolve(Some(s), cfg, cache)?;
        if out.iter().any(|x| x.ip == d.ip) {
            warn!("{s} is {} again; streaming to it once", d.ip);
        } else {
            out.push(d);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> Vec<Cached> {
        parse_cache(
            "1F:80:60:74:F4:D9:72:F0 192.168.1.3 H6609\nAA:BB:CC:DD:EE:FF:00:11 192.168.1.50 H619C\n",
        )
    }

    // These tests never set GOWL_DEVICE; resolve() only reads it when spec is None.
    #[test]
    fn resolve_forms() {
        let mut cfg = Config::default();
        cfg.devices.push(DeviceCfg {
            alias: "desk".into(),
            mac: Some("AA:BB:CC:DD:EE:FF:00:11".into()),
            segments: Some(12),
            reverse: true,
            ..DeviceCfg::default()
        });
        let c = cache();
        let d = resolve(Some("desk"), &cfg, &c).unwrap();
        assert_eq!(
            (d.ip, d.segments, d.seg_source, d.reverse),
            (Ipv4Addr::new(192, 168, 1, 50), 12, SegSource::Config, true)
        );
        let d = resolve(Some("2"), &cfg, &c).unwrap();
        assert_eq!(d.alias.as_deref(), Some("desk"));
        let d = resolve(Some("60:74:f4:d9:72:f0"), &cfg, &c).unwrap();
        assert_eq!(
            (d.ip, d.segments, d.seg_source),
            (Ipv4Addr::new(192, 168, 1, 3), 10, SegSource::Default)
        );
        assert_eq!(d.sku.as_deref(), Some("H6609"));
        let d = resolve(Some("192.168.1.50"), &cfg, &c).unwrap();
        assert_eq!((d.segments, d.seg_source), (12, SegSource::Config));
        let d = resolve(Some("10.0.0.9"), &Config::default(), &c).unwrap();
        assert_eq!((d.mac, d.segments), (None, 10));
        assert!(matches!(
            resolve(Some("3"), &cfg, &c),
            Err(Error::NoDevice(_))
        ));
        assert!(matches!(
            resolve(Some("lamp"), &cfg, &c),
            Err(Error::NoDevice(_))
        ));
    }

    #[test]
    fn sku_table() {
        assert_eq!(sku_segments("h619c"), Some(10));
        assert_eq!(sku_segments("H6672"), Some(14));
        assert_eq!(sku_segments("H6609"), None);
    }

    #[test]
    fn merge_keeps_unseen() {
        let mut c = cache();
        merge(
            &mut c,
            &[Cached {
                mac: "1F:80:60:74:F4:D9:72:F0".into(),
                ip: Ipv4Addr::new(192, 168, 1, 9),
                sku: "H6609".into(),
            }],
        );
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].ip, Ipv4Addr::new(192, 168, 1, 9));
    }
}
