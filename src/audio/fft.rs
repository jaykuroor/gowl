//! Radix-2 FFT for the music analyser (AUDIO.md "Per window"). N = 1024 real samples
//! are packed into a 512-point complex FFT (`z[n] = x[2n] + i·x[2n+1]`) and split
//! into the real spectrum afterwards — half the butterflies of a 1024-point complex
//! FFT (measured: `bench_real_vs_complex`, DECISIONS D-031). Hann window, bit-reversal
//! and twiddles are tables built once; the window multiply is fused with the
//! bit-reversed load. Nothing here allocates after `new`.

use std::f64::consts::PI;

/// Samples per analysis window.
pub const N: usize = 1024;
const M: usize = N / 2;

/// In-place iterative radix-2 complex FFT of size `m` (power of two).
pub struct ComplexFft {
    m: usize,
    rev: Vec<u16>,
    tw_re: Vec<f32>,
    tw_im: Vec<f32>,
}

impl ComplexFft {
    pub fn new(m: usize) -> ComplexFft {
        assert!(m.is_power_of_two() && (2..=1 << 15).contains(&m));
        let bits = m.trailing_zeros();
        let rev = (0..m)
            .map(|i| ((i as u32).reverse_bits() >> (32 - bits)) as u16)
            .collect();
        // w^k = exp(-2πik/m), k < m/2.
        let (tw_re, tw_im) = (0..m / 2)
            .map(|k| {
                let a = 2.0 * PI * k as f64 / m as f64;
                (a.cos() as f32, -a.sin() as f32)
            })
            .unzip();
        ComplexFft {
            m,
            rev,
            tw_re,
            tw_im,
        }
    }

    /// Bit-reversed position of input index `i`.
    #[inline]
    pub fn rev(&self, i: usize) -> usize {
        self.rev[i] as usize
    }

    /// Butterflies only: `re`/`im` must already be in bit-reversed order.
    #[inline]
    pub fn butterflies(&self, re: &mut [f32], im: &mut [f32]) {
        let m = self.m;
        let (re, im) = (&mut re[..m], &mut im[..m]);
        // len = 2: twiddle is 1.
        for a in (0..m).step_by(2) {
            let (tr, ti) = (re[a + 1], im[a + 1]);
            re[a + 1] = re[a] - tr;
            im[a + 1] = im[a] - ti;
            re[a] += tr;
            im[a] += ti;
        }
        let mut len = 4;
        while len <= m {
            let half = len / 2;
            let step = m / len;
            for start in (0..m).step_by(len) {
                for k in 0..half {
                    let (wr, wi) = (self.tw_re[k * step], self.tw_im[k * step]);
                    let (a, b) = (start + k, start + k + half);
                    let tr = re[b] * wr - im[b] * wi;
                    let ti = re[b] * wi + im[b] * wr;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            len <<= 1;
        }
    }

    /// Full transform of natural-order input (permutes in place first).
    pub fn transform(&self, re: &mut [f32], im: &mut [f32]) {
        for i in 0..self.m {
            let j = self.rev(i);
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        self.butterflies(re, im);
    }
}

/// Windowed real FFT of [`N`] samples. Call [`RealFft::analyze`], then read bins
/// `0..=N/2` with [`RealFft::bin`] / [`RealFft::power`].
pub struct RealFft {
    cfft: ComplexFft,
    window: [f32; N],
    window_sum: f64,
    // exp(-2πik/N), k < M — the split twiddles.
    post_re: [f32; M],
    post_im: [f32; M],
    re: [f32; M],
    im: [f32; M],
}

impl Default for RealFft {
    fn default() -> Self {
        Self::new()
    }
}

impl RealFft {
    pub fn new() -> RealFft {
        let mut window = [0f32; N];
        // numpy.hanning (symmetric): 0.5 − 0.5·cos(2πn/(N−1)), computed in f64, stored f32.
        for (n, w) in window.iter_mut().enumerate() {
            *w = (0.5 - 0.5 * (2.0 * PI * n as f64 / (N - 1) as f64).cos()) as f32;
        }
        let window_sum = window.iter().map(|&w| f64::from(w)).sum();
        let mut post_re = [0f32; M];
        let mut post_im = [0f32; M];
        for k in 0..M {
            let a = 2.0 * PI * k as f64 / N as f64;
            post_re[k] = a.cos() as f32;
            post_im[k] = -a.sin() as f32;
        }
        RealFft {
            cfft: ComplexFft::new(M),
            window,
            window_sum,
            post_re,
            post_im,
            re: [0.0; M],
            im: [0.0; M],
        }
    }

    /// Σ window (for the legacy `2 / Σwindow` magnitude scale).
    pub fn window_sum(&self) -> f64 {
        self.window_sum
    }

    /// Window `samples` (Hann) and transform them.
    #[inline]
    pub fn analyze(&mut self, samples: &[f32; N]) {
        for n in 0..M {
            let j = self.cfft.rev(n);
            self.re[j] = samples[2 * n] * self.window[2 * n];
            self.im[j] = samples[2 * n + 1] * self.window[2 * n + 1];
        }
        self.cfft.butterflies(&mut self.re, &mut self.im);
    }

    /// Transform `samples` as-is (no window) — for tests.
    pub fn analyze_raw(&mut self, samples: &[f32; N]) {
        for n in 0..M {
            let j = self.cfft.rev(n);
            self.re[j] = samples[2 * n];
            self.im[j] = samples[2 * n + 1];
        }
        self.cfft.butterflies(&mut self.re, &mut self.im);
    }

    /// Complex bin `k` in `0..=N/2` of the last analysed window.
    #[inline]
    pub fn bin(&self, k: usize) -> (f32, f32) {
        debug_assert!(k <= M);
        if k == 0 || k == M {
            let (r, i) = (self.re[0], self.im[0]);
            return (if k == 0 { r + i } else { r - i }, 0.0);
        }
        let (zr, zi) = (self.re[k], self.im[k]);
        let (cr, ci) = (self.re[M - k], self.im[M - k]);
        // E = (Z[k] + conj Z[M−k]) / 2,  O = (Z[k] − conj Z[M−k]) / 2i
        let (er, ei) = (0.5 * (zr + cr), 0.5 * (zi - ci));
        let (or, oi) = (0.5 * (zi + ci), -0.5 * (zr - cr));
        let (wr, wi) = (self.post_re[k], self.post_im[k]);
        (er + wr * or - wi * oi, ei + wr * oi + wi * or)
    }

    /// |X[k]|².
    #[inline]
    pub fn power(&self, k: usize) -> f32 {
        let (r, i) = self.bin(k);
        r * r + i * i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift32 → [-1, 1).
    fn noise(seed: &mut u32) -> f32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        (*seed as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32
    }

    fn naive_dft(x: &[f32], k: usize) -> (f64, f64) {
        let n = x.len();
        let (mut r, mut i) = (0f64, 0f64);
        for (t, &v) in x.iter().enumerate() {
            let a = -2.0 * PI * ((k * t) % n) as f64 / n as f64;
            r += f64::from(v) * a.cos();
            i += f64::from(v) * a.sin();
        }
        (r, i)
    }

    #[test]
    fn real_fft_matches_naive_dft() {
        let mut seed = 0x1234_5678;
        let mut f = RealFft::new();
        let mut worst = 0f64;
        for _ in 0..3 {
            let mut x = [0f32; N];
            x.iter_mut().for_each(|v| *v = noise(&mut seed));
            f.analyze_raw(&x);
            for k in 0..=N / 2 {
                let (gr, gi) = f.bin(k);
                let (wr, wi) = naive_dft(&x, k);
                worst = worst
                    .max((f64::from(gr) - wr).abs())
                    .max((f64::from(gi) - wi).abs());
            }
        }
        eprintln!("real FFT vs naive DFT: max abs error {worst:.3e} (inputs in [-1,1))");
        assert!(worst < 1e-4, "max error {worst}");
    }

    #[test]
    fn complex_fft_matches_naive_dft() {
        let mut seed = 99;
        let c = ComplexFft::new(64);
        let mut re: Vec<f32> = (0..64).map(|_| noise(&mut seed)).collect();
        let mut im = vec![0f32; 64];
        let x = re.clone();
        c.transform(&mut re, &mut im);
        for k in 0..64 {
            let (wr, wi) = naive_dft(&x, k);
            assert!((f64::from(re[k]) - wr).abs() < 1e-4);
            assert!((f64::from(im[k]) - wi).abs() < 1e-4);
        }
    }

    #[test]
    fn hann_matches_numpy_symmetric() {
        let f = RealFft::new();
        assert_eq!(f.window[0], 0.0);
        assert_eq!(f.window[N - 1], 0.0);
        assert!((f.window[N / 2] - 0.99999_76).abs() < 1e-6);
        assert!((f.window_sum() - 511.5).abs() < 1e-3);
    }

    /// `cargo test --release -- --ignored bench_real_vs_complex --nocapture`
    #[test]
    #[ignore]
    fn bench_real_vs_complex() {
        use std::hint::black_box;
        use std::time::Instant;
        let mut seed = 7;
        let mut x = [0f32; N];
        x.iter_mut().for_each(|v| *v = noise(&mut seed));
        let iters = 20_000;

        let mut f = RealFft::new();
        let t = Instant::now();
        let mut acc = 0f32;
        for _ in 0..iters {
            f.analyze(black_box(&x));
            for k in 1..342 {
                acc += f.power(k);
            }
        }
        let real_ns = t.elapsed().as_nanos() as f64 / iters as f64;
        black_box(acc);

        let c = ComplexFft::new(N);
        let mut re = vec![0f32; N];
        let mut im = vec![0f32; N];
        let t = Instant::now();
        for _ in 0..iters {
            for n in 0..N {
                re[c.rev(n)] = black_box(x[n]) * f.window[n];
                im[n] = 0.0;
            }
            c.butterflies(&mut re, &mut im);
            for k in 1..342 {
                acc += re[k] * re[k] + im[k] * im[k];
            }
        }
        let cplx_ns = t.elapsed().as_nanos() as f64 / iters as f64;
        black_box(acc);
        eprintln!(
            "window+FFT+342 bins: packed real {real_ns:.0} ns, plain complex-1024 {cplx_ns:.0} ns"
        );
    }
}
