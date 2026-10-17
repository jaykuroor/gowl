//! `ext-image-copy-capture-v1` backend — a seam only. niri 26.04 on the target
//! machine does not advertise it (DECISIONS D-006); when it does, implement the
//! session/frame cycle from CAPTURE_NIRI.md here, reusing `shm.rs` and `sample.rs`.
//! TODO (tracked in PROGRESS.md): implement once niri ships ext-image-copy-capture.

use super::Capture;
use super::screencopy::Globals;
use crate::error::Error;

/// Always fails (exit 4): either not advertised or not implemented yet.
pub fn open(g: &Globals) -> Result<Box<dyn Capture>, Error> {
    Err(Error::Capture(if g.imagecopy {
        "ext-image-copy-capture is advertised but gowl does not implement it yet; unset GOWL_CAPTURE to use wlr-screencopy".into()
    } else {
        "the compositor does not advertise ext-image-copy-capture; unset GOWL_CAPTURE to use wlr-screencopy".into()
    }))
}
