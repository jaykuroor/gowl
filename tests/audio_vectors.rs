//! Music golden vectors (tests/vectors/audio.json, render.json; CLAUDE.md rule 8):
//! bands within rel 1e-3 / abs 1e-6, AutoGain likewise, renderer ±1 per channel.

mod common;

use common::{Json, vector};
use gowl::audio::bands::{Analyzer, AutoGain};
use gowl::audio::fft::N;
use gowl::audio::render::{Palette, Reaction, Renderer};

fn signal(name: &str, n: usize, sr: f64) -> Vec<f32> {
    use std::f64::consts::PI;
    (0..n)
        .map(|i| {
            let t = i as f64 / sr;
            let s = |f: f64| (2.0 * PI * f * t).sin();
            (match name {
                "silence" => 0.0,
                "sine_100hz" => 0.5 * s(100.0),
                "sine_1khz" => 0.5 * s(1000.0),
                "sine_8khz" => 0.5 * s(8000.0),
                "mix" => 0.3 * s(60.0) + 0.2 * s(2000.0) + 0.1 * s(10000.0),
                o => panic!("unknown signal {o}"),
            }) as f32
        })
        .collect()
}

/// Asserts rel 1e-3 / abs 1e-6; returns the relative error for values ≥ 1e-4
/// (reporting only; tiny leakage bins are covered by the absolute floor).
fn close(got: f64, want: f64, what: &str) -> f64 {
    let d = (got - want).abs();
    let rel = d / want.abs().max(f64::MIN_POSITIVE);
    assert!(
        d <= 1e-6 || rel <= 1e-3,
        "{what}: got {got} want {want} (rel {rel:.2e})"
    );
    if want.abs() >= 1e-4 { rel } else { 0.0 }
}

#[test]
fn spectral_bands_n1024() {
    let v = vector("audio.json");
    let sr = v.get("sample_rate").f64();
    let mut a = Analyzer::new(sr as u32);
    let mut worst = 0f64;
    let mut cases = 0;
    for c in v.get("spectral_bands").arr() {
        if c.get("n").u64() != N as u64 {
            continue; // 1104 is the legacy window length; gowl uses radix-2 N = 1024
        }
        let name = c.get("signal").str();
        let x: [f32; N] = signal(name, N, sr).try_into().unwrap();
        let got = a.bands(&x);
        for (k, w) in c.get("bands").arr().iter().enumerate() {
            worst = worst.max(close(got[k], w.f64(), &format!("{name} band {k}")));
        }
        cases += 1;
    }
    assert_eq!(cases, 5);
    eprintln!("spectral_bands n=1024: worst relative error {worst:.2e}");
}

#[test]
fn auto_gain_sequence() {
    let v = vector("audio.json").get("auto_gain_fps60").clone();
    let mut g = AutoGain::new(60);
    let mut worst = 0f64;
    for (i, (inp, out)) in v.get("in").arr().iter().zip(v.get("out").arr()).enumerate() {
        let b: Vec<f64> = inp.arr().iter().map(Json::f64).collect();
        let got = g.process([b[0], b[1], b[2]]);
        for (k, w) in out.arr().iter().enumerate() {
            worst = worst.max(close(got[k], w.f64(), &format!("frame {i} band {k}")));
        }
    }
    eprintln!("auto_gain: worst relative error {worst:.2e}");
}

#[test]
fn renderer_matches_legacy() {
    let v = vector("render.json");
    let gain = v.get("gain").f64();
    let fps = v.get("fps").u64() as u32;
    let top = v.get("bands").arr();
    let parse_bands = |a: &[Json]| -> Vec<[f64; 3]> {
        a.iter()
            .map(|b| {
                let b = b.arr();
                [b[0].f64(), b[1].f64(), b[2].f64()]
            })
            .collect()
    };
    let top = parse_bands(top);
    let (mut worst, mut off_by_one, mut channels, mut cases) = (0u8, 0usize, 0usize, 0);
    for c in v.get("cases").arr() {
        let n = c.get("segments").u64() as usize;
        let palette = Palette::parse(c.get("palette").str()).unwrap();
        let bands = c
            .opt("bands")
            .map_or_else(|| top.clone(), |b| parse_bands(b.arr()));
        let frames = c.get("frames").arr();
        assert_eq!(frames.len(), bands.len());
        let reactions: Vec<Reaction> = match c.get("reaction") {
            Json::Str(s) => vec![Reaction::parse(s).unwrap(); bands.len()],
            Json::Arr(a) => a
                .iter()
                .map(|r| Reaction::parse(r.str()).unwrap())
                .collect(),
            o => panic!("bad reaction {o:?}"),
        };
        let mut r = Renderer::new(n, fps);
        for (i, (b, want)) in bands.iter().zip(frames).enumerate() {
            let got = r.render(*b, reactions[i], gain, palette);
            let want = want.rgb_list();
            assert_eq!(got.len(), want.len());
            for (s, (g, w)) in got.iter().zip(&want).enumerate() {
                for k in 0..3 {
                    let d = g[k].abs_diff(w[k]);
                    worst = worst.max(d);
                    off_by_one += usize::from(d == 1);
                    channels += 1;
                    assert!(
                        d <= 1,
                        "{:?}/{:?} n={n} frame {i} seg {s}: got {g:?} want {w:?}",
                        reactions[i],
                        palette
                    );
                }
            }
        }
        cases += 1;
    }
    eprintln!(
        "renderer: {cases} cases, {channels} channels, worst error {worst}, {off_by_one} off by one"
    );
}
