//! Reactions × palettes: port of legacy `MusicPatternRenderer` and `bands_to_color`
//! (`legacy/lumisync/sync/audio.py`). Scalar f64 like the Python floats; the spots
//! where legacy computes in numpy float32 (`bands_to_color`, `_spectral_complexity`)
//! are mirrored in f32, and Python's `round()` is `round_ties_even`. Renders into a
//! fixed `[Rgb; 255]`; no allocation per frame (the Auto Director's inner renderer
//! is boxed once, on first use, and reset in place afterwards).

use std::f64::consts::PI;

use crate::color::Rgb;
use crate::proto::MAX_SEGMENTS;

const BLACK: Rgb = [0, 0, 0];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reaction {
    Flow,
    Pulse,
    EnergyFill,
    BandSplit,
    CenterBurst,
    Wave,
    BassBounce,
    Chase,
    Twinkle,
    Auto,
}

impl Reaction {
    pub const ALL: [(&'static str, Reaction); 10] = [
        ("flow", Reaction::Flow),
        ("pulse", Reaction::Pulse),
        ("energy_fill", Reaction::EnergyFill),
        ("band_split", Reaction::BandSplit),
        ("center_burst", Reaction::CenterBurst),
        ("wave", Reaction::Wave),
        ("bass_bounce", Reaction::BassBounce),
        ("chase", Reaction::Chase),
        ("twinkle", Reaction::Twinkle),
        ("auto", Reaction::Auto),
    ];

    pub fn parse(s: &str) -> Option<Reaction> {
        Self::ALL.iter().find(|(n, _)| *n == s).map(|&(_, r)| r)
    }

    pub fn name(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(_, r)| *r == self)
            .map_or("?", |e| e.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Palette {
    Rgb,
    Auto,
    Rainbow,
    Aurora,
    Sunset,
    Ocean,
    Neon,
    Jewel,
    Fire,
    Spectrum,
    Warm,
    Cool,
    Mono,
}

impl Palette {
    pub const ALL: [(&'static str, Palette); 13] = [
        ("rgb", Palette::Rgb),
        ("auto", Palette::Auto),
        ("rainbow", Palette::Rainbow),
        ("aurora", Palette::Aurora),
        ("sunset", Palette::Sunset),
        ("ocean", Palette::Ocean),
        ("neon", Palette::Neon),
        ("jewel", Palette::Jewel),
        ("fire", Palette::Fire),
        ("spectrum", Palette::Spectrum),
        ("warm", Palette::Warm),
        ("cool", Palette::Cool),
        ("mono", Palette::Mono),
    ];

    pub fn parse(s: &str) -> Option<Palette> {
        Self::ALL.iter().find(|(n, _)| *n == s).map(|&(_, p)| p)
    }

    pub fn name(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(_, p)| *p == self)
            .map_or("?", |e| e.0)
    }

    /// Palettes whose hue offset rotates with the music.
    fn rotating(self) -> bool {
        matches!(self, Palette::Rainbow | Palette::Auto)
    }

    fn stops(self) -> Option<&'static [Rgb; 4]> {
        Some(match self {
            Palette::Aurora => &AURORA,
            Palette::Sunset => &SUNSET,
            Palette::Ocean => &OCEAN,
            Palette::Neon => &NEON,
            Palette::Jewel => &JEWEL,
            Palette::Fire => &FIRE,
            _ => return None,
        })
    }
}

const AURORA: [Rgb; 4] = [
    [16, 185, 129],
    [34, 211, 238],
    [139, 92, 246],
    [244, 114, 182],
];
const SUNSET: [Rgb; 4] = [
    [126, 34, 104],
    [236, 72, 153],
    [249, 115, 22],
    [253, 224, 71],
];
const OCEAN: [Rgb; 4] = [[7, 72, 116], [14, 165, 233], [45, 212, 191], [99, 102, 241]];
const NEON: [Rgb; 4] = [
    [236, 72, 153],
    [34, 211, 238],
    [168, 85, 247],
    [249, 115, 22],
];
const JEWEL: [Rgb; 4] = [[5, 150, 105], [37, 99, 235], [147, 51, 234], [225, 29, 72]];
const FIRE: [Rgb; 4] = [[153, 27, 27], [239, 68, 68], [249, 115, 22], [250, 204, 21]];

// ------------------------------------------------------------------ color helpers

#[inline]
fn clamp01(x: f64) -> f64 {
    x.clamp(0.0, 1.0)
}

/// `int(round(x))` clamped to a channel.
#[inline]
fn ch(x: f64) -> u8 {
    x.round_ties_even().clamp(0.0, 255.0) as u8
}

/// `colorsys.hsv_to_rgb(hue % 1, s, v)` → rounded sRGB8 (legacy `_hsv`).
fn hsv(hue: f64, s: f64, v: f64) -> Rgb {
    let h = hue.rem_euclid(1.0);
    let (r, g, b) = if s == 0.0 {
        (v, v, v)
    } else {
        let i = (h * 6.0) as i64;
        let f = h * 6.0 - i as f64;
        let p = v * (1.0 - s);
        let q = v * (1.0 - s * f);
        let t = v * (1.0 - s * (1.0 - f));
        match i % 6 {
            0 => (v, t, p),
            1 => (q, v, p),
            2 => (p, v, t),
            3 => (p, q, v),
            4 => (t, p, v),
            _ => (v, p, q),
        }
    };
    [ch(r * 255.0), ch(g * 255.0), ch(b * 255.0)]
}

fn mix(a: Rgb, b: Rgb, amount: f64) -> Rgb {
    let t = clamp01(amount);
    let m = |x: u8, y: u8| {
        let (x, y) = (f64::from(x), f64::from(y));
        ch(x + (y - x) * t)
    };
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

fn gradient(stops: &[Rgb; 4], position: f64, level: f64) -> Rgb {
    if level <= 0.0 {
        return BLACK;
    }
    let scaled = clamp01(position) * 3.0;
    let index = (scaled as usize).min(2);
    let c = mix(stops[index], stops[index + 1], scaled - index as f64);
    let l = clamp01(level);
    c.map(|v| ch(f64::from(v) * l))
}

/// `round(c * amount)` per channel (legacy `_scale`).
#[inline]
fn scale(c: Rgb, amount: f64) -> Rgb {
    let a = clamp01(amount);
    c.map(|v| ch(f64::from(v) * a))
}

#[inline]
fn brightest(a: Rgb, b: Rgb) -> Rgb {
    [a[0].max(b[0]), a[1].max(b[1]), a[2].max(b[2])]
}

/// Normalised 3-band entropy (legacy `_spectral_complexity`, numpy float32 path).
fn complexity(b: [f32; 3], total: f64) -> f64 {
    if total <= 1e-9 {
        return 0.0;
    }
    let t = total as f32;
    let mut sum = 0f32;
    let mut active = 0;
    for v in b {
        let s = v / t;
        if s > 1e-9 {
            sum += s * s.ln();
            active += 1;
        }
    }
    if active <= 1 {
        return 0.0;
    }
    f64::from(-sum) / 3f64.ln()
}

/// One color from band energies under `palette` (legacy `bands_to_color`).
pub fn bands_to_color(
    bass: f64,
    mid: f64,
    treble: f64,
    gain: f64,
    palette: Palette,
    hue_offset: f64,
) -> Rgb {
    let b = [bass as f32, mid as f32, treble as f32];
    let total32 = b[0] + b[1] + b[2];
    let total = f64::from(total32);
    if total <= 1e-9 {
        return BLACK;
    }
    let level = (total.sqrt() * gain).min(1.0);
    if palette == Palette::Mono {
        let v = ch(level * 255.0);
        return [v, v, v];
    }
    let centroid = f64::from((b[1] * 0.5 + b[2] * 1.0) / total32);
    match palette {
        Palette::Spectrum => return hsv(0.66 * centroid, 1.0, level),
        Palette::Warm => return hsv(0.02 + 0.12 * centroid, 1.0, level),
        Palette::Cool => return hsv(0.5 + 0.16 * centroid, 1.0, level),
        Palette::Rainbow => return hsv(hue_offset + centroid * 0.42, 0.96, level),
        Palette::Auto => {
            let cx = complexity(b, total);
            let share = |v: f32| f64::from(v / total32);
            let stops = if cx > 0.82 {
                &NEON
            } else if share(b[0]) > 0.52 {
                &FIRE
            } else if share(b[2]) > 0.44 {
                &OCEAN
            } else if share(b[1]) > 0.50 {
                &JEWEL
            } else {
                &AURORA
            };
            return gradient(stops, (centroid * 0.62 + hue_offset).rem_euclid(1.0), level);
        }
        _ => {}
    }
    if let Some(stops) = palette.stops() {
        return gradient(stops, centroid, level);
    }
    // rgb: bass → red, mid → green, treble → blue (float32 like numpy).
    let k = level as f32;
    b.map(|v| {
        (v / total32 * k * 255.0)
            .round_ties_even()
            .clamp(0.0, 255.0) as u8
    })
}

// ------------------------------------------------------------------ renderer

/// Auto Director decision categories, in legacy order.
const AUTO_CHOICES: [&[Reaction]; 6] = [
    &[Reaction::Pulse, Reaction::CenterBurst],
    &[
        Reaction::BassBounce,
        Reaction::EnergyFill,
        Reaction::CenterBurst,
    ],
    &[Reaction::Twinkle, Reaction::Wave, Reaction::Chase],
    &[Reaction::BandSplit, Reaction::Wave, Reaction::EnergyFill],
    &[Reaction::Chase, Reaction::Pulse, Reaction::EnergyFill],
    &[Reaction::Flow, Reaction::Wave, Reaction::EnergyFill],
];

#[derive(Clone, Copy)]
struct AutoState {
    reaction: Reaction,
    hold_s: f64,
    cycle: [u32; 6],
    ready: bool,
    level: f64,
    bass_share: f64,
    treble_share: f64,
    complexity: f64,
    transient_peak: f64,
    fade: f64,
    fade_from: [Rgb; MAX_SEGMENTS],
    /// The inner renderer must be rebuilt before its next use (legacy sets it to None).
    inner_stale: bool,
}

impl AutoState {
    const fn new() -> AutoState {
        AutoState {
            reaction: Reaction::Flow,
            hold_s: 0.0,
            cycle: [0; 6],
            ready: false,
            level: 0.0,
            bass_share: 0.0,
            treble_share: 0.0,
            complexity: 0.0,
            transient_peak: 0.0,
            fade: 0.0,
            fade_from: [BLACK; MAX_SEGMENTS],
            inner_stale: true,
        }
    }
}

/// Turns band energies into one frame of segment colors (legacy `MusicPatternRenderer`).
pub struct Renderer {
    n: usize,
    fps: u32,
    frame_dt: f64,
    frame: [Rgb; MAX_SEGMENTS],
    twinkles: [Rgb; MAX_SEGMENTS],
    reaction: Option<Reaction>,
    phase: f64,
    chase_pos: f64,
    chase_dir: f64,
    prev_level: f64,
    fill_level: f64,
    fill_color: Rgb,
    palette_phase: f64,
    auto: AutoState,
    inner: Option<Box<Renderer>>,
}

impl Renderer {
    /// `segments` is clamped to 1..=255; `fps` only sets the Auto Director's clock.
    pub fn new(segments: usize, fps: u32) -> Renderer {
        let fps = fps.max(1);
        Renderer {
            n: segments.clamp(1, MAX_SEGMENTS),
            fps,
            frame_dt: 1.0 / f64::from(fps),
            frame: [BLACK; MAX_SEGMENTS],
            twinkles: [BLACK; MAX_SEGMENTS],
            reaction: None,
            phase: 0.0,
            chase_pos: 0.0,
            chase_dir: 1.0,
            prev_level: 0.0,
            fill_level: 0.0,
            fill_color: BLACK,
            palette_phase: 0.0,
            auto: AutoState::new(),
            inner: None,
        }
    }

    pub fn segments(&self) -> usize {
        self.n
    }

    /// Back to a freshly constructed renderer, keeping the boxed inner renderer.
    fn renew(&mut self) {
        let inner = self.inner.take();
        *self = Renderer::new(self.n, self.fps);
        self.inner = inner;
    }

    fn reset(&mut self, reaction: Reaction) {
        let n = self.n;
        self.frame[..n].fill(BLACK);
        self.twinkles[..n].fill(BLACK);
        self.reaction = Some(reaction);
        self.phase = 0.0;
        self.chase_pos = 0.0;
        self.chase_dir = 1.0;
        // prev_level is kept on purpose (legacy: avoids a phantom transient).
        self.fill_level = 0.0;
        self.fill_color = BLACK;
        self.auto = AutoState::new();
    }

    #[inline]
    fn level(total: f64, gain: f64) -> f64 {
        if total <= 1e-9 {
            0.0
        } else {
            (total.sqrt() * gain).min(1.0)
        }
    }

    /// Render one frame; returns the `segments()` colors.
    pub fn render(
        &mut self,
        bands: [f64; 3],
        reaction: Reaction,
        gain: f64,
        palette: Palette,
    ) -> &[Rgb] {
        if self.reaction != Some(reaction) {
            self.reset(reaction);
        }
        let [bass, mid, treble] = bands.map(|v| v.max(0.0));
        let total = bass + mid + treble;
        let level = Self::level(total, gain);
        let transient = ((level - self.prev_level).max(0.0) * 4.0).min(1.0);

        if reaction == Reaction::Auto {
            self.auto_frame([bass, mid, treble], level, transient, gain, palette);
            self.prev_level = level;
            return &self.frame[..self.n];
        }

        let bass_share = if total > 1e-9 { bass / total } else { 0.0 };
        let beat = (transient + level * (0.35 + 0.65 * bass_share)).min(1.0);
        if total > 1e-9 && palette.rotating() {
            self.palette_phase = (self.palette_phase + 0.0015 + 0.0040 * level).rem_euclid(1.0);
        }
        let po = self.palette_phase;
        let source = bands_to_color(bass, mid, treble, gain, palette, po);
        let n = self.n;

        match reaction {
            Reaction::Pulse => {
                let c = scale(source, 0.48 + 0.52 * beat);
                self.frame[..n].fill(c);
            }
            Reaction::EnergyFill => self.energy_fill(bass, mid, treble, source, level, transient),
            Reaction::BandSplit => {
                if n == 1 {
                    self.frame[0] = source;
                } else {
                    let bc = [
                        bands_to_color(bass, 0.0, 0.0, gain, palette, po),
                        bands_to_color(0.0, mid, 0.0, gain, palette, po),
                        bands_to_color(0.0, 0.0, treble, gain, palette, po),
                    ];
                    if n == 2 {
                        self.frame[0] = bc[0];
                        self.frame[1] = bc[2];
                    } else {
                        for (i, f) in self.frame[..n].iter_mut().enumerate() {
                            *f = bc[((i * 3) / n).min(2)];
                        }
                    }
                }
            }
            Reaction::CenterBurst => {
                let burst = scale(source, 0.22 + 0.78 * beat);
                let (lc, rc) = ((n - 1) / 2, n / 2);
                for i in 0..lc {
                    self.frame[i] = self.frame[i + 1];
                }
                for i in (rc + 1..n).rev() {
                    self.frame[i] = self.frame[i - 1];
                }
                self.frame[lc] = burst;
                self.frame[rc] = burst;
            }
            Reaction::Wave => {
                if source == BLACK {
                    self.frame[..n].fill(BLACK);
                } else {
                    for (i, f) in self.frame[..n].iter_mut().enumerate() {
                        let pos = i as f64 / n as f64;
                        let w = 0.5 + 0.5 * (pos * 2.0 * PI - self.phase).sin();
                        *f = scale(source, 0.16 + 0.84 * w);
                    }
                    self.phase = (self.phase + 0.18 + 0.22 * level).rem_euclid(2.0 * PI);
                }
            }
            Reaction::BassBounce => {
                if total <= 1e-9 || source == BLACK {
                    self.frame[..n].fill(BLACK);
                } else {
                    let fill = level * (bass / total).sqrt();
                    let half = n.div_ceil(2);
                    let active = ((fill * half as f64).ceil() as usize).min(half);
                    for (i, f) in self.frame[..n].iter_mut().enumerate() {
                        *f = if i < active || i >= n - active {
                            source
                        } else {
                            BLACK
                        };
                    }
                }
            }
            Reaction::Chase => {
                for f in &mut self.frame[..n] {
                    *f = scale(*f, 0.58);
                }
                if source != BLACK {
                    let head = (self.chase_pos.round_ties_even().max(0.0) as usize).min(n - 1);
                    self.frame[head] = brightest(self.frame[head], source);
                    if n > 1 {
                        let step = 0.55 + 0.45 * level;
                        let mut next = self.chase_pos + self.chase_dir * step;
                        if next >= (n - 1) as f64 {
                            next = (n - 1) as f64;
                            self.chase_dir = -1.0;
                        } else if next <= 0.0 {
                            next = 0.0;
                            self.chase_dir = 1.0;
                        }
                        self.chase_pos = next;
                    }
                }
            }
            Reaction::Twinkle => {
                self.twinkle(bass, mid, treble, source, gain, palette, level, transient)
            }
            Reaction::Flow | Reaction::Auto => {
                self.frame.copy_within(1..n, 0);
                self.frame[n - 1] = source;
            }
        }
        self.prev_level = level;
        &self.frame[..self.n]
    }

    fn energy_fill(
        &mut self,
        bass: f64,
        mid: f64,
        treble: f64,
        source: Rgb,
        level: f64,
        transient: f64,
    ) {
        let n = self.n;
        let total = bass + mid + treble;
        let cx = complexity([bass as f32, mid as f32, treble as f32], total);
        let activity = (level * (0.62 + 0.38 * cx) + transient * 0.10).min(1.0);
        let target = activity * n as f64;
        let response = if target > self.fill_level { 0.68 } else { 0.22 };
        self.fill_level += (target - self.fill_level) * response;
        if target <= 1e-9 && self.fill_level < 0.02 {
            self.fill_level = 0.0;
        }
        self.fill_color = if source != BLACK {
            source
        } else {
            scale(self.fill_color, 0.72)
        };
        let full = (self.fill_level as usize).min(n);
        let partial = self.fill_level - full as f64;
        self.frame[..n].fill(BLACK);
        self.frame[..full].fill(self.fill_color);
        if full < n && partial > 1e-9 {
            self.frame[full] = scale(self.fill_color, partial);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn twinkle(
        &mut self,
        bass: f64,
        mid: f64,
        treble: f64,
        source: Rgb,
        gain: f64,
        palette: Palette,
        level: f64,
        transient: f64,
    ) {
        let n = self.n;
        for t in &mut self.twinkles[..n] {
            *t = scale(*t, 0.62);
        }
        let total = bass + mid + treble;
        if total <= 1e-9 || source == BLACK {
            self.frame[..n].copy_from_slice(&self.twinkles[..n]);
            return;
        }
        let treble_share = treble / total;
        let sparkle = bands_to_color(
            bass * 0.15,
            mid * 0.45,
            treble * 1.25,
            gain,
            palette,
            self.palette_phase,
        );
        let count = ((level + transient).min(1.0) * (1.0 + treble_share * (n / 3).max(1) as f64))
            .round_ties_even()
            .max(1.0) as usize;
        let step = self.phase as usize;
        for offset in 0..count.min(n) {
            let i = (step * 7 + offset * 3) % n;
            self.twinkles[i] = brightest(self.twinkles[i], sparkle);
        }
        self.phase += 1.0;
        let ambient = scale(source, 0.12);
        for (f, t) in self.frame[..n].iter_mut().zip(&self.twinkles[..n]) {
            *f = brightest(ambient, *t);
        }
    }

    fn auto_frame(
        &mut self,
        bands: [f64; 3],
        level: f64,
        transient: f64,
        gain: f64,
        palette: Palette,
    ) {
        let n = self.n;
        let total = bands[0] + bands[1] + bands[2];
        let a = &mut self.auto;
        if total > 1e-9 {
            // Smoothed features (legacy `_update_auto_features`, shares in float32).
            let b = bands.map(|v| v as f32);
            let t = total as f32;
            let bass_share = f64::from(b[0] / t);
            let treble_share = f64::from(b[2] / t);
            let cx = complexity(b, total);
            if !a.ready {
                a.level = level;
                a.bass_share = bass_share;
                a.treble_share = treble_share;
                a.complexity = cx;
                a.ready = true;
            } else {
                a.level += (level - a.level) * 0.25;
                a.bass_share += (bass_share - a.bass_share) * 0.25;
                a.treble_share += (treble_share - a.treble_share) * 0.25;
                a.complexity += (cx - a.complexity) * 0.25;
            }
            a.transient_peak = transient.max(a.transient_peak * 0.85);
        }
        if total > 1e-9 && a.hold_s <= 0.0 {
            let previous = a.reaction;
            let key = if a.transient_peak > 0.55 {
                0
            } else if a.bass_share > 0.55 {
                1
            } else if a.treble_share > 0.45 {
                2
            } else if a.complexity > 0.78 {
                3
            } else if a.level > 0.65 {
                4
            } else {
                5
            };
            let choices = AUTO_CHOICES[key];
            a.reaction = choices[a.cycle[key] as usize % choices.len()];
            a.cycle[key] += 1;
            a.hold_s = 3.0 + 2.0 * a.level;
            if a.reaction != previous && self.frame[..n].iter().any(|&c| c != BLACK) {
                a.fade_from[..n].copy_from_slice(&self.frame[..n]);
                a.fade = 1.0;
            }
        }
        if total > 1e-9 {
            a.hold_s = (a.hold_s - self.frame_dt).max(0.0);
        }
        let inner = self
            .inner
            .get_or_insert_with(|| Box::new(Renderer::new(n, self.fps)));
        if a.inner_stale {
            inner.renew();
            a.inner_stale = false;
        }
        let fresh = inner.render(bands, a.reaction, gain, palette);
        self.frame[..n].copy_from_slice(fresh);
        if a.fade > 0.0 {
            for (f, p) in self.frame[..n].iter_mut().zip(&a.fade_from[..n]) {
                *f = mix(*f, *p, a.fade);
            }
            a.fade = (a.fade - self.frame_dt / 0.12).max(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_roundtrip() {
        for (n, r) in Reaction::ALL {
            assert_eq!(Reaction::parse(n), Some(r));
            assert_eq!(r.name(), n);
        }
        for (n, p) in Palette::ALL {
            assert_eq!(Palette::parse(n), Some(p));
            assert_eq!(p.name(), n);
        }
        assert_eq!(Palette::parse("album_art"), None);
    }

    #[test]
    fn silence_is_black() {
        let mut r = Renderer::new(10, 60);
        for (_, re) in Reaction::ALL {
            for (_, p) in Palette::ALL {
                let f = r.render([0.0; 3], re, 1.7, p);
                assert!(f.iter().all(|&c| c == BLACK), "{re:?} {p:?}");
            }
        }
    }
}
