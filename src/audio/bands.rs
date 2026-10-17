//! Band energies and AutoGain (port of legacy `spectral_bands` and `AutoGain`).
//! Band = RMS of the scaled magnitudes whose bin frequency `k·sr/N` lies in
//! `[low, high)`; bin ranges are precomputed. RMS needs only Σ|X|², so no per-bin
//! square root. AutoGain is scalar f64 (same cost as f32 here, closer to legacy).

use super::fft::{N, RealFft};

pub const SAMPLE_RATE: u32 = 48_000;
pub const BASS: (f64, f64) = (20.0, 250.0);
pub const MID: (f64, f64) = (250.0, 4000.0);
pub const TREBLE: (f64, f64) = (4000.0, 16000.0);

pub type Bands = [f64; 3];

/// Window → `[bass, mid, treble]` (legacy `spectral_bands`, N = 1024).
pub struct Analyzer {
    fft: RealFft,
    /// Bin index ranges `k0..k1` per band.
    ranges: [(usize, usize); 3],
    scale: f64,
}

/// Bins `k` (of `0..=n/2`) with `low <= k·sr/n < high`, as numpy `rfftfreq` computes them.
pub fn bin_range(n: usize, sr: u32, low: f64, high: f64) -> (usize, usize) {
    let val = 1.0 / (n as f64 * (1.0 / f64::from(sr.max(1))));
    let inside = |k: usize| {
        let f = k as f64 * val;
        f >= low && f < high
    };
    let k0 = (0..=n / 2).find(|&k| inside(k));
    match k0 {
        Some(k0) => {
            let k1 = (k0..=n / 2).find(|&k| !inside(k)).unwrap_or(n / 2 + 1);
            (k0, k1)
        }
        None => (0, 0),
    }
}

impl Analyzer {
    pub fn new(sample_rate: u32) -> Analyzer {
        let fft = RealFft::new();
        let nyquist = f64::from(sample_rate) / 2.0;
        let ranges = [
            bin_range(N, sample_rate, BASS.0, BASS.1),
            bin_range(N, sample_rate, MID.0, MID.1),
            bin_range(N, sample_rate, TREBLE.0, TREBLE.1.min(nyquist)),
        ];
        let scale = 2.0 / fft.window_sum().max(1e-9);
        Analyzer { fft, ranges, scale }
    }

    pub fn ranges(&self) -> [(usize, usize); 3] {
        self.ranges
    }

    /// Raw (un-normalised) band energies of one window.
    #[inline]
    pub fn bands(&mut self, samples: &[f32; N]) -> Bands {
        self.fft.analyze(samples);
        let mut out = [0f64; 3];
        for (o, &(k0, k1)) in out.iter_mut().zip(&self.ranges) {
            if k1 <= k0 {
                continue;
            }
            let mut p = 0f64;
            for k in k0..k1 {
                p += f64::from(self.fft.power(k));
            }
            *o = self.scale * (p / (k1 - k0) as f64).sqrt();
        }
        out
    }
}

/// Loudness normaliser: fast-attack / slow-release envelope of `bass+mid+treble`,
/// output scaled so the envelope maps to `TARGET` (legacy `AutoGain`).
pub struct AutoGain {
    attack: f64,
    release: f64,
    floor: f64,
    envelope: f64,
}

impl AutoGain {
    pub const TARGET: f64 = 0.10;
    pub const MAX_BOOST: f64 = 40.0;

    /// `fps` sets the per-call dt (legacy passes the configured music fps).
    pub fn new(fps: u32) -> AutoGain {
        let dt = 1.0 / f64::from(fps.max(1));
        AutoGain {
            attack: 1.0 - (-dt / 0.15f64.max(1e-3)).exp(),
            release: 1.0 - (-dt / 1.0f64.max(1e-3)).exp(),
            floor: Self::TARGET / Self::MAX_BOOST,
            envelope: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.envelope = 0.0;
    }

    /// Silence passes as zeros and holds the envelope.
    #[inline]
    pub fn process(&mut self, b: Bands) -> Bands {
        let b = b.map(|v| v.max(0.0));
        let total = b[0] + b[1] + b[2];
        if total <= 1e-9 {
            return [0.0; 3];
        }
        if self.envelope < self.floor {
            self.envelope = total;
        } else {
            let a = if total > self.envelope {
                self.attack
            } else {
                self.release
            };
            self.envelope += (total - self.envelope) * a;
        }
        let s = Self::TARGET / self.envelope.max(self.floor);
        b.map(|v| v * s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_ranges_1024_at_48k() {
        let a = Analyzer::new(48_000);
        // 46.875 Hz per bin: bass 1..=5, mid 6..=85, treble 86..=341.
        assert_eq!(a.ranges(), [(1, 6), (6, 86), (86, 342)]);
    }
}
