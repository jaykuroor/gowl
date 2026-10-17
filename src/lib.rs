//! gowl — Govee LAN lights on niri: one-shot control, screen and music sync.
//!
//! One crate, one binary (`src/main.rs`); this library target only exists so the
//! integration tests in `tests/` can reach the modules.

#[macro_use]
pub mod log;
pub mod audio;
pub mod color;
pub mod config;
pub mod devices;
pub mod doctor;
pub mod error;
pub mod net;
pub mod proto;

pub use error::Error;
