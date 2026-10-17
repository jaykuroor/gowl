//! Color math, fixed-point (ARCHITECTURE.md "Numbers & formats"): sRGB↔linear LUTs,
//! zone mapping (exact port of legacy `led_mapping.py`), gamma-correct zone averaging,
//! saturation, brightness, EMA smoothing, resampling, reverse/offset and the
//! send-or-skip decision. Float is used only at setup (mapping, Q16 conversion).
//! Every hot-path function writes into caller-provided buffers.

pub type Rgb = [u8; 3];

#[rustfmt::skip]
mod luts;
pub use luts::{LIN16, SRGB_THRESH_Q8};

/// Linear-light mean (×65535, Q8) → nearest sRGB8. Branchless 8-step binary search
/// over the 255 midpoint thresholds (`SRGB_THRESH_Q8[255]` is a `u32::MAX` sentinel).
#[inline]
pub fn linear_q8_to_srgb(v: u32) -> u8 {
    let mut i = 0usize;
    let mut step = 128;
    while step > 0 {
        if SRGB_THRESH_Q8[i + step - 1] <= v {
            i += step;
        }
        step >>= 1;
    }
    i as u8
}

/// `f` in Q16 (setup-time conversion of a config float).
pub fn q16(f: f64) -> u32 {
    (f.max(0.0) * 65536.0).round() as u32
}

// ------------------------------------------------------------------ zone mapping

pub const DEFAULT_ASPECT_RATIO: f64 = 16.0 / 9.0;
pub const DEFAULT_CAPTURE_DEPTH: f64 = 0.28;
pub const MIN_CAPTURE_DEPTH: f64 = 0.10;
pub const MAX_CAPTURE_DEPTH: f64 = 0.50;

/// Normalized rectangle, 0..1 of the output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

pub fn clamp_zone_count(n: usize) -> usize {
    n.clamp(1, 255)
}

pub fn sanitize_aspect_ratio(r: f64) -> f64 {
    if r.is_nan() || r <= 0.0 {
        return DEFAULT_ASPECT_RATIO;
    }
    r.clamp(0.25, 8.0)
}

pub fn clamp_capture_depth(d: f64) -> f64 {
    if d.is_nan() {
        return DEFAULT_CAPTURE_DEPTH;
    }
    d.clamp(MIN_CAPTURE_DEPTH, MAX_CAPTURE_DEPTH)
}

pub fn normalize_rect(r: Rect) -> Rect {
    let x = r.x.clamp(0.0, 0.999);
    let y = r.y.clamp(0.0, 0.999);
    let max_w = (1.0 - x).max(0.001);
    let max_h = (1.0 - y).max(0.001);
    Rect {
        x,
        y,
        w: max_w.min(r.w.max(0.001)),
        h: max_h.min(r.h.max(0.001)),
    }
}

/// Zones per edge `[top, left, bottom, right]` by largest remainder (legacy order,
/// stable on ties like Python's `sorted(reverse=True)`).
fn largest_remainder_counts(zone_count: usize, aspect: f64, depth: f64) -> [usize; 4] {
    match clamp_zone_count(zone_count) {
        1 => return [1, 0, 0, 0],
        2 => return [1, 0, 1, 0],
        3 => return [1, 1, 1, 0],
        _ => {}
    }
    let ratio = sanitize_aspect_ratio(aspect);
    let middle = (1.0 - 2.0 * clamp_capture_depth(depth)).max(0.05);
    let weights = [ratio, middle, ratio, middle];
    let mut counts = [1usize; 4];
    let remaining = clamp_zone_count(zone_count) - 4;
    if remaining == 0 {
        return counts;
    }
    let total: f64 = weights.iter().sum();
    let mut raw = [0f64; 4];
    let mut extras_sum = 0;
    for i in 0..4 {
        raw[i] = remaining as f64 * weights[i] / total;
        let extra = raw[i] as usize;
        counts[i] += extra;
        extras_sum += extra;
    }
    let leftover = remaining - extras_sum;
    let mut order = [0usize, 1, 2, 3];
    // Stable sort, descending remainder.
    order.sort_by(|&a, &b| {
        let (ra, rb) = (raw[a] - raw[a].trunc(), raw[b] - raw[b].trunc());
        rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
    });
    for &i in order.iter().take(leftover) {
        counts[i] += 1;
    }
    counts
}

/// One normalized edge rectangle per zone, in legacy order: top right→left, left
/// top→bottom, bottom left→right, right bottom→top.
pub fn generate_screen_mapping(zone_count: usize, aspect: f64, depth: f64) -> Vec<Rect> {
    let zone_count = clamp_zone_count(zone_count);
    let depth = clamp_capture_depth(depth);
    if zone_count == 1 {
        return vec![Rect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        }];
    }
    let [top, left, bottom, right] =
        largest_remainder_counts(zone_count, sanitize_aspect_ratio(aspect), depth);
    let middle_h = (1.0 - 2.0 * depth).max(0.001);
    let mut m = Vec::with_capacity(zone_count);
    for i in 0..top {
        let width = 1.0 / top as f64;
        m.push(Rect {
            x: 1.0 - (i + 1) as f64 * width,
            y: 0.0,
            w: width,
            h: depth,
        });
    }
    for i in 0..left {
        let height = middle_h / left as f64;
        m.push(Rect {
            x: 0.0,
            y: depth + i as f64 * height,
            w: depth,
            h: height,
        });
    }
    for i in 0..bottom {
        let width = 1.0 / bottom as f64;
        m.push(Rect {
            x: i as f64 * width,
            y: 1.0 - depth,
            w: width,
            h: depth,
        });
    }
    for i in 0..right {
        let height = middle_h / right as f64;
        m.push(Rect {
            x: 1.0 - depth,
            y: 1.0 - depth - (i + 1) as f64 * height,
            w: depth,
            h: height,
        });
    }
    m.into_iter().map(normalize_rect).collect()
}

// ------------------------------------------------------------------ zone averaging

/// Legacy cap on samples per axis per zone.
pub const MAX_SAMPLES_PER_AXIS: u32 = 24;

/// Anything that yields an sRGB8 pixel at (x, y). Phase 2 implements it for the
/// mapped SHM buffer (XRGB8888 etc.); tests use [`Rgb8Frame`].
pub trait PixelSource {
    fn rgb(&self, x: u32, y: u32) -> Rgb;
}

/// Packed RGB8 rows.
pub struct Rgb8Frame<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub stride: usize,
}

impl PixelSource for Rgb8Frame<'_> {
    #[inline]
    fn rgb(&self, x: u32, y: u32) -> Rgb {
        let o = y as usize * self.stride + x as usize * 3;
        [self.data[o], self.data[o + 1], self.data[o + 2]]
    }
}

/// Precomputed sample grid of one zone: `x0, x0+sx, … < x1` × `y0, y0+sy, … < y1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Zone {
    pub x0: u32,
    pub x1: u32,
    pub sx: u32,
    pub y0: u32,
    pub y1: u32,
    pub sy: u32,
    pub count: u32,
}

/// Pixel bounds `(x1, y1, x2, y2)` of a rect, exact port of legacy `_zone_bounds`.
pub fn zone_bounds(r: Rect, width: u32, height: u32) -> (u32, u32, u32, u32) {
    if width == 0 || height == 0 {
        return (0, 0, 0, 0); // empty frame: empty zone (avoids clamp(0, -1) panic)
    }
    let r = normalize_rect(r);
    let (w, h) = (f64::from(width), f64::from(height));
    let x1 = ((r.x * w) as i64).clamp(0, i64::from(width) - 1);
    let y1 = ((r.y * h) as i64).clamp(0, i64::from(height) - 1);
    let x2 = (((r.x + r.w) * w) as i64).max(x1 + 1).min(i64::from(width));
    let y2 = (((r.y + r.h) * h) as i64)
        .max(y1 + 1)
        .min(i64::from(height));
    (x1 as u32, y1 as u32, x2 as u32, y2 as u32)
}

impl Zone {
    pub fn new(r: Rect, width: u32, height: u32) -> Zone {
        let (x0, y0, x1, y1) = zone_bounds(r, width, height);
        let sx = ((x1 - x0) / MAX_SAMPLES_PER_AXIS).max(1);
        let sy = ((y1 - y0) / MAX_SAMPLES_PER_AXIS).max(1);
        let count = (x1 - x0).div_ceil(sx) * (y1 - y0).div_ceil(sy);
        Zone {
            x0,
            x1,
            sx,
            y0,
            y1,
            sy,
            count,
        }
    }

    /// Gamma-correct mean (linear-light average, legacy default).
    #[inline]
    pub fn average_linear<P: PixelSource>(&self, src: &P) -> Rgb {
        let mut s = [0u32; 3];
        for y in (self.y0..self.y1).step_by(self.sy as usize) {
            for x in (self.x0..self.x1).step_by(self.sx as usize) {
                let p = src.rgb(x, y);
                s[0] += u32::from(LIN16[p[0] as usize]);
                s[1] += u32::from(LIN16[p[1] as usize]);
                s[2] += u32::from(LIN16[p[2] as usize]);
            }
        }
        let c = u64::from(self.count);
        s.map(|v| linear_q8_to_srgb(((u64::from(v) << 8) / c) as u32))
    }

    /// Plain sRGB mean, rounded half-to-even like `np.rint`.
    #[inline]
    pub fn average_srgb<P: PixelSource>(&self, src: &P) -> Rgb {
        let mut s = [0u32; 3];
        for y in (self.y0..self.y1).step_by(self.sy as usize) {
            for x in (self.x0..self.x1).step_by(self.sx as usize) {
                let p = src.rgb(x, y);
                s[0] += u32::from(p[0]);
                s[1] += u32::from(p[1]);
                s[2] += u32::from(p[2]);
            }
        }
        let c = self.count;
        s.map(|v| {
            let (q, r) = (v / c, v % c);
            let up = 2 * r > c || (2 * r == c && q & 1 == 1);
            (q + u32::from(up)).min(255) as u8
        })
    }
}

/// Zones for one output size; rebuilt only when size or zone count changes.
pub fn build_zones(rects: &[Rect], width: u32, height: u32) -> Vec<Zone> {
    rects.iter().map(|&r| Zone::new(r, width, height)).collect()
}

/// Average every zone into `out` (`out.len() == zones.len()`).
pub fn average_zones<P: PixelSource>(zones: &[Zone], src: &P, gamma: bool, out: &mut [Rgb]) {
    for (z, o) in zones.iter().zip(out.iter_mut()) {
        *o = if gamma {
            z.average_linear(src)
        } else {
            z.average_srgb(src)
        };
    }
}

// ------------------------------------------------------------------ per-color ops

// Rec.709 luma weights in Q16 (sum = 65536).
const LUMA_R: i64 = 13933;
const LUMA_G: i64 = 46871;
const LUMA_B: i64 = 4732;

/// Scale saturation around Rec.709 luma; `sat_q16 == 65536` is a no-op.
#[inline]
pub fn saturate(colors: &mut [Rgb], sat_q16: u32) {
    if sat_q16 == 65536 {
        return;
    }
    let s = i64::from(sat_q16);
    for c in colors {
        let luma = LUMA_R * i64::from(c[0]) + LUMA_G * i64::from(c[1]) + LUMA_B * i64::from(c[2]);
        for ch in c.iter_mut() {
            let d = (i64::from(*ch) << 16) - luma;
            let v = luma + ((d * s) >> 16);
            *ch = ((v + 32768) >> 16).clamp(0, 255) as u8;
        }
    }
}

/// Multiply every channel by `k_q16 / 65536`, truncating (legacy `int(c * k)`).
#[inline]
pub fn scale(colors: &mut [Rgb], k_q16: u32) {
    if k_q16 == 65536 {
        return;
    }
    for c in colors {
        for ch in c.iter_mut() {
            *ch = ((u32::from(*ch) * k_q16) >> 16).min(255) as u8;
        }
    }
}

/// Per-zone temporal EMA. State is value << 8 (u16). Like legacy
/// `ColorSmoother(alpha, count)`, the state starts black, so the first frames fade
/// in; a zone-count change snaps to the target.
pub struct Smoother {
    alpha_q16: i64,
    state: Vec<[u16; 3]>,
}

impl Smoother {
    pub fn new(alpha: f64, count: usize) -> Smoother {
        let mut state = Vec::with_capacity(crate::proto::MAX_SEGMENTS.max(count));
        state.resize(count, [0u16; 3]);
        Smoother {
            alpha_q16: i64::from(q16(alpha.clamp(0.0, 1.0))),
            state,
        }
    }

    /// Back to black with `count` zones (legacy `reset(count)`).
    pub fn reset(&mut self, count: usize) {
        self.state.clear();
        self.state.resize(count, [0u16; 3]);
    }

    /// `out.len() == target.len()`. Allocation-free once `target.len() <= 255`.
    #[inline]
    pub fn update(&mut self, target: &[Rgb], out: &mut [Rgb]) {
        if self.state.len() != target.len() {
            self.state.clear();
            self.state
                .extend(target.iter().map(|t| t.map(|v| u16::from(v) << 8)));
        } else {
            for (s, t) in self.state.iter_mut().zip(target) {
                for (sc, &tc) in s.iter_mut().zip(t) {
                    let d = (i64::from(tc) << 8) - i64::from(*sc);
                    *sc = (i64::from(*sc) + ((d * self.alpha_q16 + 32768) >> 16)) as u16;
                }
            }
        }
        for (o, s) in out.iter_mut().zip(&self.state) {
            *o = s.map(|v| ((u32::from(v) + 128) >> 8).min(255) as u8);
        }
    }
}

/// Spread `src` over `out.len()` samples around one loop (legacy
/// `resample_colors_to_count`, integer lerp with truncation).
pub fn resample(src: &[Rgb], out: &mut [Rgb]) {
    let (n, m) = (src.len(), out.len());
    if n == 0 {
        out.fill([0; 3]);
        return;
    }
    if n == m {
        out.copy_from_slice(src);
        return;
    }
    for (i, o) in out.iter_mut().enumerate() {
        let p = i * n;
        let left = (p / m) % n;
        let right = (left + 1) % n;
        let rem = (p % m) as u32;
        let m32 = m as u32;
        for ch in 0..3 {
            let (a, b) = (u32::from(src[left][ch]), u32::from(src[right][ch]));
            o[ch] = if b >= a {
                a + (b - a) * rem / m32
            } else {
                a - ((a - b) * rem).div_ceil(m32)
            } as u8;
        }
    }
}

/// Map zones to physical segments: zone `i` → segment `(r(i) + offset) % n`, where
/// `r(i) = n-1-i` if `reverse` else `i`. `src.len() == out.len()`.
#[inline]
pub fn reorder(src: &[Rgb], reverse: bool, offset: usize, out: &mut [Rgb]) {
    let n = out.len();
    if n == 0 {
        return;
    }
    for (i, c) in src.iter().enumerate() {
        let r = if reverse { n - 1 - i } else { i };
        out[(r + offset) % n] = *c;
    }
}

/// True if any channel of any segment moved by more than `threshold` (or the shape
/// changed / there is no previous frame / `threshold == 0`).
#[inline]
pub fn colors_changed(prev: Option<&[Rgb]>, cur: &[Rgb], threshold: u8) -> bool {
    let Some(prev) = prev else { return true };
    if prev.len() != cur.len() || threshold == 0 {
        return true;
    }
    prev.iter()
        .zip(cur)
        .any(|(p, c)| (0..3).any(|k| p[k].abs_diff(c[k]) > threshold))
}

/// Delta threshold + keepalive (legacy `frame_needs_send`). Holds the last sent frame.
pub struct SendGate {
    last: [Rgb; crate::proto::MAX_SEGMENTS],
    last_len: usize,
    last_at_ms: Option<u64>,
    pub threshold: u8,
    pub keepalive_ms: u64,
}

impl SendGate {
    pub fn new(threshold: u8, keepalive_ms: u64) -> SendGate {
        SendGate {
            last: [[0; 3]; crate::proto::MAX_SEGMENTS],
            last_len: 0,
            last_at_ms: None,
            threshold,
            keepalive_ms,
        }
    }

    pub fn last_sent(&self) -> Option<&[Rgb]> {
        self.last_at_ms.map(|_| &self.last[..self.last_len])
    }

    #[inline]
    pub fn needs_send(&self, cur: &[Rgb], now_ms: u64) -> bool {
        if colors_changed(self.last_sent(), cur, self.threshold) {
            return true;
        }
        match self.last_at_ms {
            None => true,
            Some(t) => self.keepalive_ms > 0 && now_ms.saturating_sub(t) >= self.keepalive_ms,
        }
    }

    #[inline]
    pub fn mark_sent(&mut self, cur: &[Rgb], now_ms: u64) {
        let n = cur.len().min(self.last.len());
        self.last[..n].copy_from_slice(&cur[..n]);
        self.last_len = n;
        self.last_at_ms = Some(now_ms);
    }

    /// Forget the last frame (e.g. after razer mode was re-entered).
    pub fn reset(&mut self) {
        self.last_len = 0;
        self.last_at_ms = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srgb_to_linear(s: f64) -> f64 {
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }

    #[test]
    fn luts_match_formula() {
        for (i, &v) in LIN16.iter().enumerate() {
            let want = (srgb_to_linear(i as f64 / 255.0) * 65535.0).round() as u16;
            assert_eq!(v, want, "LIN16[{i}]");
        }
        for (k, &v) in SRGB_THRESH_Q8[..255].iter().enumerate() {
            let want = (srgb_to_linear((k as f64 + 0.5) / 255.0) * 65535.0 * 256.0).round() as u32;
            assert_eq!(v, want, "THRESH[{k}]");
        }
        assert_eq!(SRGB_THRESH_Q8[255], u32::MAX);
    }

    #[test]
    fn lut_roundtrip_is_identity() {
        for (i, &v) in LIN16.iter().enumerate() {
            assert_eq!(linear_q8_to_srgb(u32::from(v) << 8), i as u8);
        }
    }

    #[test]
    fn reorder_reverse_offset() {
        let src = [[0; 3], [1; 3], [2; 3], [3; 3]];
        let mut out = [[9; 3]; 4];
        reorder(&src, false, 1, &mut out);
        assert_eq!(out, [[3; 3], [0; 3], [1; 3], [2; 3]]);
        reorder(&src, true, 0, &mut out);
        assert_eq!(out, [[3; 3], [2; 3], [1; 3], [0; 3]]);
    }

    #[test]
    fn legacy_resample_cases() {
        let c: Vec<Rgb> = (0..10).map(|i| [i; 3]).collect();
        let mut out = [[0; 3]; 4];
        resample(&c, &mut out);
        assert_eq!(out, [[0; 3], [2; 3], [5; 3], [7; 3]]);
        resample(&c[..2], &mut out);
        assert_eq!(out, [[0; 3], [0; 3], [1; 3], [0; 3]]);
    }
}
