//! Logging: `eprintln!` gated by a level read once from `GOWL_LOG`. No timestamps
//! (journald adds them), one line per message.

use std::sync::atomic::{AtomicU8, Ordering};

pub const WARN: u8 = 1;
pub const DEBUG: u8 = 2;

static LEVEL: AtomicU8 = AtomicU8::new(WARN);

/// Read `GOWL_LOG` once at startup (`debug` → verbose, `quiet`/`error` → errors only).
pub fn init() {
    let level = match std::env::var("GOWL_LOG").as_deref() {
        Ok("debug") | Ok("trace") => DEBUG,
        Ok("quiet") | Ok("error") | Ok("off") => 0,
        _ => WARN,
    };
    LEVEL.store(level, Ordering::Relaxed);
}

#[inline]
pub fn enabled(level: u8) -> bool {
    LEVEL.load(Ordering::Relaxed) >= level
}

/// Verbose diagnostics, printed only with `GOWL_LOG=debug`.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::log::enabled($crate::log::DEBUG) {
            eprintln!("gowl: {}", format_args!($($arg)*));
        }
    };
}

/// Warnings (default level).
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        if $crate::log::enabled($crate::log::WARN) {
            eprintln!("gowl: warning: {}", format_args!($($arg)*));
        }
    };
}
