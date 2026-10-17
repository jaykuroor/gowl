//! One error type for the whole binary; each variant maps to an exit code (SPEC.md).

use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Error {
    /// Bad command line (exit 2).
    Usage(String),
    /// No device could be resolved (exit 3).
    NoDevice(String),
    /// Screen capture unavailable (exit 4).
    Capture(String),
    /// No LAN interface reaches the device (exit 5).
    NoLanIf(String),
    /// I/O failure with a short context (exit 1).
    Io(&'static str, io::Error),
    /// Anything else at runtime (exit 1).
    Runtime(String),
}

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) => 2,
            Error::NoDevice(_) => 3,
            Error::Capture(_) => 4,
            Error::NoLanIf(_) => 5,
            Error::Io(..) | Error::Runtime(_) => 1,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(m) => write!(f, "{m} (see gowl --help)"),
            Error::NoDevice(m) | Error::Capture(m) | Error::NoLanIf(m) | Error::Runtime(m) => {
                f.write_str(m)
            }
            Error::Io(ctx, e) => write!(f, "{ctx}: {e}"),
        }
    }
}

impl std::error::Error for Error {}
