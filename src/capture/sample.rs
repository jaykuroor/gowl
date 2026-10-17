//! Zone sampling straight from the mapped SHM buffer (CAPTURE_NIRI.md "Geometry"
//! and "Damage → dirty zones").
//!
//! Setup (per output size/format/transform/zone-count change): zones come from
//! `color::generate_screen_mapping` in logical (as-seen) space; each zone's legacy
//! sample grid (`color::Zone`, ≤ 24 per axis) is mapped into buffer coordinates by
//! inverting the output transform and `y_invert`, and stored as flat byte offsets.
//! Per frame: damage rects mark zones dirty; only dirty zones are re-summed (LIN16
//! linear light, like `Zone::average_linear`); clean zones keep their last mean.
//! The inner loop is monomorphized per pixel format and allocates nothing.

use super::{DamageRect, Format, Frame};
use crate::color::{self, LIN16, Rgb, linear_q8_to_srgb};

/// Decode one little-endian 32-bit pixel to sRGB8.
pub trait PixelFormat {
    fn rgb(v: u32) -> Rgb;
}

/// XRGB8888 / ARGB8888: bytes B G R X.
pub struct Xrgb8888;
/// XBGR8888 / ABGR8888: bytes R G B X.
pub struct Xbgr8888;
/// XRGB2101010: x2 r10 g10 b10.
pub struct Xrgb2101010;
/// XBGR2101010: x2 b10 g10 r10.
pub struct Xbgr2101010;

impl PixelFormat for Xrgb8888 {
    #[inline(always)]
    fn rgb(v: u32) -> Rgb {
        [(v >> 16) as u8, (v >> 8) as u8, v as u8]
    }
}
impl PixelFormat for Xbgr8888 {
    #[inline(always)]
    fn rgb(v: u32) -> Rgb {
        [v as u8, (v >> 8) as u8, (v >> 16) as u8]
    }
}
impl PixelFormat for Xrgb2101010 {
    #[inline(always)]
    fn rgb(v: u32) -> Rgb {
        [
            ((v >> 22) & 0xff) as u8,
            ((v >> 12) & 0xff) as u8,
            ((v >> 2) & 0xff) as u8,
        ]
    }
}
impl PixelFormat for Xbgr2101010 {
    #[inline(always)]
    fn rgb(v: u32) -> Rgb {
        [
            ((v >> 2) & 0xff) as u8,
            ((v >> 12) & 0xff) as u8,
            ((v >> 22) & 0xff) as u8,
        ]
    }
}

/// Everything the offset tables depend on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: Format,
    pub transform: u32,
    pub y_invert: bool,
}

impl Geometry {
    pub fn of(f: &Frame<'_>) -> Geometry {
        Geometry {
            width: f.width,
            height: f.height,
            stride: f.stride,
            format: f.format,
            transform: f.transform & 7,
            y_invert: f.y_invert,
        }
    }

    /// Size as seen on screen (90°/270° swap the axes).
    pub fn logical(&self) -> (u32, u32) {
        if self.transform & 1 != 0 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    /// Logical (as-seen) pixel → buffer pixel. The table is the inverse of the
    /// buffer → logical mapping of wlroots `wlr_box_transform` / smithay
    /// `Transform::transform_point_in` (pixel-index form): FLIPPED_90 is the pure
    /// transpose, FLIPPED_270 the anti-transpose. `y_invert` (rows stored bottom-up)
    /// is applied last, to the buffer row.
    #[inline]
    pub fn to_buffer(&self, lx: u32, ly: u32) -> (u32, u32) {
        let (w, h) = (self.width, self.height);
        let (bx, by) = match self.transform & 7 {
            0 => (lx, ly),
            1 => (ly, h - 1 - lx),
            2 => (w - 1 - lx, h - 1 - ly),
            3 => (w - 1 - ly, lx),
            4 => (w - 1 - lx, ly),
            5 => (ly, lx),
            6 => (lx, h - 1 - ly),
            _ => (w - 1 - ly, h - 1 - lx),
        };
        let by = if self.y_invert { h - 1 - by } else { by };
        (bx, by)
    }
}

#[derive(Clone, Copy, Debug)]
struct ZoneTab {
    start: u32,
    end: u32,
    /// Buffer-space bounds, exclusive max: x0 y0 x1 y1.
    bbox: [u32; 4],
}

/// Per-zone sample offsets and cached means for one zone count.
pub struct Sampler {
    n: usize,
    depth: f64,
    /// Calibrated layout (one cell per zone) instead of the legacy edge walk.
    layout: Option<Vec<u8>>,
    geo: Option<Geometry>,
    offsets: Vec<u32>,
    zones: Vec<ZoneTab>,
    means: Vec<Rgb>,
    dirty: [u64; 4],
}

impl Sampler {
    pub fn new(zone_count: usize, depth: f64) -> Sampler {
        let n = color::clamp_zone_count(zone_count);
        Sampler {
            n,
            depth: color::clamp_capture_depth(depth),
            layout: None,
            geo: None,
            offsets: Vec::new(),
            zones: Vec::with_capacity(n),
            means: vec![[0; 3]; n],
            dirty: [u64::MAX; 4],
        }
    }

    /// Zones placed by a calibrated layout (`layout.len()` zones, see
    /// `color::layout_rects`).
    pub fn with_layout(layout: &[u8], depth: f64) -> Sampler {
        let mut s = Sampler::new(layout.len(), depth);
        s.layout = Some(layout[..s.n].to_vec());
        s
    }

    pub fn zone_count(&self) -> usize {
        self.n
    }

    pub fn layout(&self) -> Option<&[u8]> {
        self.layout.as_deref()
    }

    /// Rebuild the tables if the geometry changed (allocates; setup only).
    /// Returns true if rebuilt (all zones dirty).
    pub fn ensure(&mut self, geo: Geometry) -> bool {
        if self.geo == Some(geo) {
            return false;
        }
        let (lw, lh) = geo.logical();
        let rects: Vec<Option<color::Rect>> = match &self.layout {
            Some(l) => color::layout_rects(l, self.depth),
            None => {
                color::generate_screen_mapping(self.n, f64::from(lw) / f64::from(lh), self.depth)
                    .into_iter()
                    .map(Some)
                    .collect()
            }
        };
        self.offsets.clear();
        self.zones.clear();
        for r in rects {
            let start = self.offsets.len() as u32;
            let Some(r) = r else {
                // Off segment: no samples (mean stays black), never damaged.
                self.zones.push(ZoneTab {
                    start,
                    end: start,
                    bbox: [0; 4],
                });
                continue;
            };
            let z = color::Zone::new(r, lw, lh);
            for y in (z.y0..z.y1).step_by(z.sy as usize) {
                for x in (z.x0..z.x1).step_by(z.sx as usize) {
                    let (bx, by) = geo.to_buffer(x, y);
                    self.offsets.push(by * geo.stride + bx * 4);
                }
            }
            let (ax, ay) = geo.to_buffer(z.x0, z.y0);
            let (bx, by) = geo.to_buffer(z.x1 - 1, z.y1 - 1);
            self.zones.push(ZoneTab {
                start,
                end: self.offsets.len() as u32,
                bbox: [ax.min(bx), ay.min(by), ax.max(bx) + 1, ay.max(by) + 1],
            });
        }
        self.geo = Some(geo);
        self.dirty = [u64::MAX; 4];
        debug!(
            "zone tables: {} zones{}, {} samples, logical {lw}x{lh}, transform {}{}",
            self.n,
            if self.layout.is_some() {
                " (calibrated layout)"
            } else {
                ""
            },
            self.offsets.len(),
            super::transform_name(geo.transform),
            if geo.y_invert { ", y-inverted" } else { "" }
        );
        true
    }

    /// Mark zones touched by `damage` (buffer coordinates) dirty. More than
    /// `MAX_DAMAGE` rects (the capture backend then sets `full`) or one rect over
    /// half the output → everything.
    pub fn mark(&mut self, damage: &[DamageRect], full: bool) {
        let Some(geo) = self.geo else {
            self.dirty = [u64::MAX; 4];
            return;
        };
        let half = u64::from(geo.width) * u64::from(geo.height) / 2;
        if full
            || damage
                .iter()
                .any(|d| u64::from(d.w) * u64::from(d.h) > half)
        {
            self.dirty = [u64::MAX; 4];
            return;
        }
        for (i, z) in self.zones.iter().enumerate() {
            let [x0, y0, x1, y1] = z.bbox;
            let hit = damage.iter().any(|d| {
                d.x < x1 && d.x.saturating_add(d.w) > x0 && d.y < y1 && d.y.saturating_add(d.h) > y0
            });
            if hit {
                self.dirty[i >> 6] |= 1 << (i & 63);
            }
        }
    }

    pub fn any_dirty(&self) -> bool {
        self.dirty.iter().any(|&w| w != 0)
    }

    /// Re-average the dirty zones from `data`. Returns true if any zone was dirty.
    pub fn sample(&mut self, data: &[u8]) -> bool {
        let Some(geo) = self.geo else { return false };
        if !self.any_dirty() {
            return false;
        }
        // Tables were built for this geometry; refuse a short buffer instead of panicking.
        if (data.len() as u64) < u64::from(geo.stride) * u64::from(geo.height) {
            return false;
        }
        match geo.format {
            Format::Xrgb8888 => self.run::<Xrgb8888>(data),
            Format::Xbgr8888 => self.run::<Xbgr8888>(data),
            Format::Xrgb2101010 => self.run::<Xrgb2101010>(data),
            Format::Xbgr2101010 => self.run::<Xbgr2101010>(data),
        }
        self.dirty = [0; 4];
        true
    }

    fn run<F: PixelFormat>(&mut self, data: &[u8]) {
        for (i, z) in self.zones.iter().enumerate() {
            if self.dirty[i >> 6] & (1 << (i & 63)) == 0 {
                continue;
            }
            let offs = &self.offsets[z.start as usize..z.end as usize];
            let mut s = [0u32; 3];
            for &o in offs {
                let o = o as usize;
                let p = F::rgb(u32::from_le_bytes([
                    data[o],
                    data[o + 1],
                    data[o + 2],
                    data[o + 3],
                ]));
                s[0] += u32::from(LIN16[p[0] as usize]);
                s[1] += u32::from(LIN16[p[1] as usize]);
                s[2] += u32::from(LIN16[p[2] as usize]);
            }
            let c = offs.len().max(1) as u64;
            self.means[i] = s.map(|v| linear_q8_to_srgb(((u64::from(v) << 8) / c) as u32));
        }
    }

    /// Latest per-zone means (sRGB8), legacy zone order.
    pub fn colors(&self) -> &[Rgb] {
        &self.means
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(w: u32, h: u32, transform: u32, y_invert: bool) -> Geometry {
        Geometry {
            width: w,
            height: h,
            stride: w * 4,
            format: Format::Xrgb8888,
            transform,
            y_invert,
        }
    }

    #[test]
    fn to_buffer_is_a_bijection_for_every_transform() {
        let (w, h) = (7u32, 4u32);
        for t in 0..8 {
            for yi in [false, true] {
                let g = geo(w, h, t, yi);
                let (lw, lh) = g.logical();
                let mut seen = vec![false; (w * h) as usize];
                for ly in 0..lh {
                    for lx in 0..lw {
                        let (bx, by) = g.to_buffer(lx, ly);
                        assert!(bx < w && by < h, "t{t} yi{yi}");
                        let k = (by * w + bx) as usize;
                        assert!(!seen[k]);
                        seen[k] = true;
                    }
                }
            }
        }
    }

    /// Buffer pixel → logical pixel per wlroots/smithay (buffer size w×h), written
    /// out independently of `to_buffer`.
    fn reference_to_logical(t: u32, w: u32, h: u32, bx: u32, by: u32) -> (u32, u32) {
        match t {
            0 => (bx, by),
            1 => (h - 1 - by, bx),
            2 => (w - 1 - bx, h - 1 - by),
            3 => (by, w - 1 - bx),
            4 => (w - 1 - bx, by),
            5 => (by, bx),
            6 => (bx, h - 1 - by),
            _ => (h - 1 - by, w - 1 - bx),
        }
    }

    #[test]
    fn transforms_match_wlroots_smithay_tables() {
        let (w, h) = (7u32, 4u32);
        // Buffer pixel holding logical (1, 0), per transform — fixed values.
        let want_1_0 = [
            (1, 0),
            (0, 2),
            (5, 3),
            (6, 1),
            (5, 0),
            (0, 1),
            (1, 3),
            (6, 2),
        ];
        // Buffer pixel holding logical (0, 0).
        let want_0_0 = [
            (0, 0),
            (0, 3),
            (6, 3),
            (6, 0),
            (6, 0),
            (0, 0),
            (0, 3),
            (6, 3),
        ];
        for t in 0..8u32 {
            let g = geo(w, h, t, false);
            assert_eq!(g.to_buffer(1, 0), want_1_0[t as usize], "transform {t}");
            assert_eq!(g.to_buffer(0, 0), want_0_0[t as usize], "transform {t}");
            let (lw, lh) = g.logical();
            for ly in 0..lh {
                for lx in 0..lw {
                    let (bx, by) = g.to_buffer(lx, ly);
                    assert_eq!(reference_to_logical(t, w, h, bx, by), (lx, ly), "t{t}");
                }
            }
            // y_invert flips the stored row only.
            let gi = geo(w, h, t, true);
            let (bx, by) = g.to_buffer(1, 0);
            assert_eq!(gi.to_buffer(1, 0), (bx, h - 1 - by), "t{t} y_invert");
        }
    }

    #[test]
    fn center_damage_marks_nothing_at_depth_010() {
        let mut s = Sampler::new(10, 0.10);
        s.ensure(geo(1920, 1080, 0, false));
        s.dirty = [0; 4];
        // Edge zones reach 10% in: 192 px horizontally, 108 px vertically.
        s.mark(
            &[
                DamageRect {
                    x: 200,
                    y: 120,
                    w: 1300,
                    h: 700,
                },
                DamageRect {
                    x: 900,
                    y: 500,
                    w: 10,
                    h: 10,
                },
            ],
            false,
        );
        assert!(!s.any_dirty());
        // A rect over the top edge dirties top zones only.
        s.mark(
            &[DamageRect {
                x: 0,
                y: 0,
                w: 50,
                h: 50,
            }],
            false,
        );
        assert!(s.any_dirty());
        assert_ne!(s.dirty[0], (1 << 10) - 1);
        // Over half the output → everything.
        s.dirty = [0; 4];
        s.mark(
            &[DamageRect {
                x: 200,
                y: 120,
                w: 1520,
                h: 840,
            }],
            false,
        );
        assert_eq!(s.dirty, [u64::MAX; 4]);
    }

    #[test]
    fn layout_zones_follow_the_calibration() {
        // 40x20 XRGB: left half red, right half blue.
        let (w, h) = (40u32, 20u32);
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let o = ((y * w + x) * 4) as usize;
                data[o..o + 4].copy_from_slice(
                    &if x < w / 2 { 0x00ff_0000u32 } else { 0xffu32 }.to_le_bytes(),
                );
            }
        }
        // left, right, off, whole screen
        let mut s = Sampler::with_layout(&[4, 6, 0, 5], 0.2);
        assert_eq!((s.zone_count(), s.layout()), (4, Some(&[4u8, 6, 0, 5][..])));
        s.ensure(geo(w, h, 0, false));
        assert!(s.sample(&data));
        let c = s.colors();
        assert_eq!((c[0], c[1], c[2]), ([255, 0, 0], [0, 0, 255], [0, 0, 0]));
        assert!(c[3][0] > 150 && c[3][2] > 150 && c[3][1] == 0, "{:?}", c[3]);
        // The off zone is never dirtied by damage.
        s.dirty = [0; 4];
        s.mark(
            &[DamageRect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            }],
            false,
        );
        assert_eq!(s.dirty[0] & 0b0100, 0);
    }
}
