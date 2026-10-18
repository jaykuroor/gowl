//! One error type for the whole binary; each variant maps to an exit code (SPEC.md):
//! 0 ok, 1 runtime error, 2 usage or a bad config value, 3 no light configured,
//! 4 screen capture unavailable, 5 no LAN interface reaches the light, 6 another
//! gowl session holds the light. 2, 3 and 6 never fix themselves, so the systemd
//! unit doesn't restart on them (`RestartPreventExitStatus=2 3 6`, D-065).

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
    /// Another gowl session holds the light — or keys took it over from the
    /// systemd unit (exit 6). Never fixes itself by retrying, so the unit's
    /// `RestartPreventExitStatus` lists it (D-065).
    AlreadyRunning(String),
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
            Error::AlreadyRunning(_) => 6,
            Error::Io(..) | Error::Runtime(_) => 1,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(m) => write!(f, "{m} (see gowl --help)"),
            Error::NoDevice(m)
            | Error::Capture(m)
            | Error::NoLanIf(m)
            | Error::AlreadyRunning(m)
            | Error::Runtime(m) => f.write_str(m),
            Error::Io(ctx, e) => write!(f, "{ctx}: {e}"),
        }
    }
}

impl std::error::Error for Error {}
