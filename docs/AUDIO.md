# AUDIO — music sync

Legacy reference: `legacy/lumisync/sync/music.py` (loop) and
`legacy/lumisync/sync/audio.py` (bands, AutoGain, `MusicPatternRenderer`).
Golden numbers: `tests/vectors/audio.json`.

## Capture
- `libpulse-simple` (served by `pipewire-pulse` on Fedora), loaded with `dlopen("libpulse-simple.so.0")`
  only when `gowl music` runs. Hand-declare the ~4 symbols needed:
  `pa_simple_new`, `pa_simple_read`, `pa_simple_free`, `pa_strerror`.
- Source `"@DEFAULT_MONITOR@"` (the default sink's monitor = what you hear).
- Sample spec: `PA_SAMPLE_FLOAT32LE`, 48000 Hz, **1 channel** (server downmixes — cheaper
  than us doing it). `pa_buffer_attr.fragsize = 1024 * 4` bytes, others `u32::MAX`.
- Stream name `"gowl music"` so it's identifiable in `pavucontrol`/`wpctl status`.
- If the default sink changes, `@DEFAULT_MONITOR@` doesn't follow an open stream: on
  read error, reopen; additionally reopen every 30 s of continuous silence (cheap check).

Alternative considered: native libpipewire. Rejected for v1 (bindgen/clang build dep,
larger API surface). Phase 5 may benchmark it if pulse overhead shows up in profiles.

## Per window (N = 1024 samples ≈ 21.3 ms → ~47 windows/s)
Legacy used 1104 samples (0.023 s); 1024 enables radix-2 FFT. Vectors exist for both.
1. Multiply by precomputed Hann window (`f32[1024]`, `0.5 − 0.5·cos(2πn/(N−1))` — numpy
   `hanning` is the symmetric form; match it).
2. Real FFT, radix-2, in place, precomputed twiddles + bit-reversal table. Hand-written
   (~80 lines). Magnitudes for bins 0..N/2.
3. Scale magnitudes by `2 / Σwindow`.
4. Band energy = RMS of magnitudes whose bin frequency `f = k·sr/N` is in `[low, high)`;
   bin index ranges precomputed once. Bass 20–250, mid 250–4000, treble 4000–min(16000, sr/2).
5. AutoGain (if enabled): fast-attack/slow-release envelope of `bass+mid+treble`,
   `scale = 0.10 / max(env, 0.10/40)`; α from `1 − exp(−dt/τ)`, τ attack 0.15 s / release
   1.0 s, dt = 1/fps; silence → zeros and envelope held. Match `audio.json` sequence.
   (Legacy also cancels the Windows master-volume fader — Windows-only, drop it.)
6. Render: port `MusicPatternRenderer.render(bands, reaction, gain=1.7, palette)`.
7. EMA smoothing α 0.6 → brightness × 0.85 → per-device reverse/offset → delta check → send.

No sleeps: the blocking read *is* the pacing. If `music_fps` < window rate, skip sends
(not reads) to honour the cap. Device cap 40 Hz still applies per device.

## Reactions / palettes to port (v1)
Required: reactions `flow`, `pulse`, `energy_fill`, `band_split`, `center_burst`;
palettes `rgb`, `rainbow`, `fire`, `ocean`, `aurora`, `sunset`, `neon`, `jewel`, `warm`,
`cool`, `mono`, `spectrum`.
Optional (after budgets are met): `wave`, `bass_bounce`, `chase`, `twinkle`, `auto`
palette, `auto` reaction (Auto Director).
Dropped: `album_art` (needs MPRIS + image decoding).

Port rules: read the legacy method, write a Rust equivalent using `f32` (this path is
~47 Hz × ≤255 segments — float is fine here), no allocation per frame (render into a
preallocated `[Rgb; 255]`). Any randomness (twinkle) uses a tiny xorshift seeded once.
For each ported reaction, add a test feeding a fixed band sequence and comparing to
legacy output produced by extending `tools/gen_vectors.py` (add the cases there, re-run).
