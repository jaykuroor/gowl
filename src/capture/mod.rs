//! Screen capture on niri (docs/CAPTURE_NIRI.md). One backend today —
//! `zwlr_screencopy_manager_v1` over a hand-written wire client (`wire.rs`, D-016) —
//! behind `trait Capture`, so `ext-image-copy-capture` can slot in when niri ships it.

pub mod imagecopy;
pub mod sample;
pub mod screencopy;
pub mod shm;
pub mod wire;

use std::os::fd::RawFd;

use crate::error::Error;

// wl_shm format codes: 0 and 1 are special, the rest are DRM fourccs.
pub const WL_ARGB8888: u32 = 0;
pub const WL_XRGB8888: u32 = 1;
const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}
pub const WL_XBGR8888: u32 = fourcc(b"XB24");
pub const WL_ABGR8888: u32 = fourcc(b"AB24");
pub const WL_XRGB2101010: u32 = fourcc(b"XR30");
pub const WL_ARGB2101010: u32 = fourcc(b"AR30");
pub const WL_XBGR2101010: u32 = fourcc(b"XB30");
pub const WL_ABGR2101010: u32 = fourcc(b"AB30");

/// Pixel layouts gowl can read (alpha variants read the same; alpha is ignored).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Xrgb8888,
    Xbgr8888,
    Xrgb2101010,
    Xbgr2101010,
}

impl Format {
    pub fn from_wl(code: u32) -> Option<Format> {
        match code {
            WL_XRGB8888 | WL_ARGB8888 => Some(Format::Xrgb8888),
            WL_XBGR8888 | WL_ABGR8888 => Some(Format::Xbgr8888),
            WL_XRGB2101010 | WL_ARGB2101010 => Some(Format::Xrgb2101010),
            WL_XBGR2101010 | WL_ABGR2101010 => Some(Format::Xbgr2101010),
            _ => None,
        }
    }

    /// Preference when several are offered (CAPTURE_NIRI.md table order; X before A).
    pub fn rank(code: u32) -> Option<u8> {
        Some(match code {
            WL_XRGB8888 => 0,
            WL_ARGB8888 => 1,
            WL_XBGR8888 => 2,
            WL_ABGR8888 => 3,
            WL_XRGB2101010 => 4,
            WL_ARGB2101010 => 5,
            WL_XBGR2101010 => 6,
            WL_ABGR2101010 => 7,
            _ => return None,
        })
    }
}

/// Human name of a wl_shm format code (`XR24`, `AB30`, …).
pub fn format_name(code: u32) -> String {
    match code {
        WL_ARGB8888 => "AR24".into(),
        WL_XRGB8888 => "XR24".into(),
        c => {
            let b = c.to_le_bytes();
            if b.iter().all(|x| x.is_ascii_graphic()) {
                String::from_utf8_lossy(&b).into_owned()
            } else {
                format!("{c:#x}")
            }
        }
    }
}

/// `wl_output.transform` value (0-7) as niri spells it.
pub fn transform_name(t: u32) -> &'static str {
    match t {
        0 => "normal",
        1 => "90",
        2 => "180",
        3 => "270",
        4 => "flipped",
        5 => "flipped-90",
        6 => "flipped-180",
        7 => "flipped-270",
        _ => "?",
    }
}

/// Damaged rectangle in buffer coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Most damage rects kept per frame; more than this means "everything".
pub const MAX_DAMAGE: usize = 32;

/// A captured frame, valid until the next `request_next`.
pub struct Frame<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: Format,
    /// `wl_output.transform` of the captured output (0-7).
    pub transform: u32,
    pub y_invert: bool,
    pub damage: &'a [DamageRect],
    /// Treat the whole frame as damaged (first frame into a new buffer, too many rects…).
    pub full_damage: bool,
}

/// What one `dispatch` call saw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// A frame is ready (`Capture::frame`).
    pub ready: bool,
    /// The in-flight frame failed; retry later.
    pub failed: bool,
    /// The captured output went away.
    pub lost: bool,
    /// The wanted output is (back) and can be captured.
    pub found: bool,
}

/// One capture backend. Single frame in flight; the caller paces requests.
pub trait Capture {
    /// Fd to poll for readability.
    fn fd(&self) -> RawFd;
    /// Send queued requests (call before blocking in poll).
    fn flush(&mut self) -> Result<(), Error>;
    /// Read and handle everything the compositor sent.
    fn dispatch(&mut self) -> Result<Status, Error>;
    /// Ask for the next frame. `Ok(false)` if one is in flight or no output is selected.
    fn request_next(&mut self) -> Result<bool, Error>;
    /// Abandon the in-flight frame (pause). No-op if none.
    fn cancel(&mut self) -> Result<(), Error>;
    /// The last ready frame.
    fn frame(&self) -> Option<Frame<'_>>;
    /// Name of the captured output, if any.
    fn output_name(&self) -> Option<&str>;
}

/// Open the backend: `GOWL_CAPTURE=screencopy|imagecopy` forces one; otherwise
/// image-copy when niri advertises it (not implemented yet → error), else screencopy.
pub fn open(output: Option<&str>) -> Result<Box<dyn Capture>, Error> {
    let g = screencopy::Globals::connect()?;
    let forced = std::env::var("GOWL_CAPTURE").ok();
    match forced.as_deref() {
        Some("imagecopy") => return imagecopy::open(&g),
        Some("screencopy") | None | Some("") => {}
        Some(o) => {
            return Err(Error::Usage(format!(
                "GOWL_CAPTURE={o}: expected screencopy or imagecopy"
            )));
        }
    }
    if g.screencopy.is_none() {
        if g.imagecopy {
            return imagecopy::open(&g);
        }
        return Err(Error::Capture(
            "the compositor offers neither wlr-screencopy nor ext-image-copy-capture".into(),
        ));
    }
    if forced.is_none() && g.imagecopy {
        debug!("ext-image-copy-capture advertised but not implemented yet; using screencopy");
    }
    Ok(Box::new(screencopy::Screencopy::new(g, output)?))
}

/// `gowl outputs`: one line per output plus a capture summary.
pub fn list_outputs() -> Result<String, Error> {
    let g = screencopy::Globals::connect()?;
    let mut s = String::new();
    if g.outputs.is_empty() {
        s += "no outputs\n";
    }
    for o in &g.outputs {
        s += &format!(
            "{:<10} {}x{} @ {}.{:03} Hz  scale {}  transform {}",
            if o.name.is_empty() { "?" } else { &o.name },
            o.width,
            o.height,
            o.refresh_mhz / 1000,
            o.refresh_mhz % 1000,
            o.scale,
            transform_name(o.transform),
        );
        if !o.description.is_empty() {
            s += &format!("  ({})", o.description);
        }
        s.push('\n');
    }
    let fmts: Vec<String> = g.shm_formats.iter().map(|&f| format_name(f)).collect();
    s += &format!(
        "capture: wlr-screencopy {}; ext-image-copy-capture {}; shm formats {}",
        g.screencopy
            .map_or("absent".to_string(), |(_, v)| format!("v{v}")),
        if g.imagecopy {
            "advertised (not used yet)"
        } else {
            "absent"
        },
        if fmts.is_empty() {
            "none".to_string()
        } else {
            fmts.join(" ")
        }
    );
    Ok(s)
}
