//! Music sync (AUDIO.md): libpulse capture via `dlopen` (non-threaded mainloop), radix-2 FFT, band
//! energies + AutoGain, reactions × palettes, and the `gowl music` loop.

pub mod bands;
pub mod fft;
pub mod pulse;
pub mod render;
pub mod run;

pub use run::cmd_music;
