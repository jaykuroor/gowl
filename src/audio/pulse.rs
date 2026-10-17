//! Minimal hand-written FFI to libpulse (served by `pipewire-pulse` on Fedora),
//! loaded with `dlopen("libpulse.so.0")` only when `gowl music` runs, so no other
//! mode ever maps it (AUDIO.md "Capture").
//!
//! We drive libpulse's *non-threaded* `pa_mainloop` from our own thread instead of
//! using `pa_simple` (DECISIONS D-030): `pa_simple` runs a second mainloop thread,
//! so every fragment woke two threads (measured 100 wakeups/s vs the ≤ 50 budget),
//! and its read waits on a condition variable that signals cannot interrupt. Here a
//! signal makes `poll()` return EINTR, `pa_mainloop_iterate` returns, and the caller
//! sees its stop flag. No callbacks: states are polled after each iteration and
//! audio is taken with `pa_stream_peek`/`pa_stream_drop`.

use std::ffi::{CStr, c_char, c_int, c_void};

use crate::Error;

const LIB: &CStr = c"libpulse.so.0";
const PA_SAMPLE_FLOAT32LE: c_int = 5;
const PA_CONTEXT_NOAUTOSPAWN: c_int = 1;
const PA_CONTEXT_READY: c_int = 4;
const PA_CONTEXT_FAILED: c_int = 5;
const PA_CONTEXT_TERMINATED: c_int = 6;
const PA_STREAM_READY: c_int = 2;
const PA_STREAM_FAILED: c_int = 3;
const PA_STREAM_TERMINATED: c_int = 4;
const PA_STREAM_ADJUST_LATENCY: c_int = 0x2000;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type P = *mut c_void;

/// Declares the function table and its loader together, so each C signature is
/// written once and every `transmute` names its exact target type.
macro_rules! pulse_fns {
    ($($field:ident = $sym:literal : fn($($a:ty),*) $(-> $r:ty)?;)*) => {
        /// Function table (pulse/mainloop.h, context.h, stream.h, error.h).
        struct Fns { $($field: unsafe extern "C" fn($($a),*) $(-> $r)?,)* }

        impl Fns {
            fn load(sym: &dyn Fn(&CStr) -> Result<P, Error>) -> Result<Fns, Error> {
                Ok(Fns { $(
                    // SAFETY: the symbol is the libpulse function of that name, whose C
                    // signature is exactly this type; the library is never unloaded.
                    $field: unsafe {
                        std::mem::transmute::<P, unsafe extern "C" fn($($a),*) $(-> $r)?>(sym($sym)?)
                    },
                )* })
            }
        }
    };
}

pulse_fns! {
    mainloop_new = c"pa_mainloop_new": fn() -> P;
    mainloop_get_api = c"pa_mainloop_get_api": fn(P) -> P;
    mainloop_iterate = c"pa_mainloop_iterate": fn(P, c_int, *mut c_int) -> c_int;
    mainloop_free = c"pa_mainloop_free": fn(P);
    context_new = c"pa_context_new": fn(P, *const c_char) -> P;
    context_connect = c"pa_context_connect": fn(P, *const c_char, c_int, *const c_void) -> c_int;
    context_get_state = c"pa_context_get_state": fn(P) -> c_int;
    context_errno = c"pa_context_errno": fn(P) -> c_int;
    context_disconnect = c"pa_context_disconnect": fn(P);
    context_unref = c"pa_context_unref": fn(P);
    stream_new = c"pa_stream_new": fn(P, *const c_char, *const SampleSpec, *const c_void) -> P;
    stream_connect_record = c"pa_stream_connect_record": fn(P, *const c_char, *const BufferAttr, c_int) -> c_int;
    stream_get_state = c"pa_stream_get_state": fn(P) -> c_int;
    stream_peek = c"pa_stream_peek": fn(P, *mut *const c_void, *mut usize) -> c_int;
    stream_drop = c"pa_stream_drop": fn(P) -> c_int;
    stream_disconnect = c"pa_stream_disconnect": fn(P) -> c_int;
    stream_unref = c"pa_stream_unref": fn(P);
    strerror = c"pa_strerror": fn(c_int) -> *const c_char;
}

/// The loaded library (never unloaded: libpulse does not like `dlclose`).
pub struct Pulse {
    f: Fns,
}

fn dlerror() -> String {
    // SAFETY: dlerror returns NULL or a NUL-terminated thread-local string.
    let p = unsafe { libc::dlerror() };
    if p.is_null() {
        "unknown error".into()
    } else {
        // SAFETY: non-null, NUL-terminated; copied before the next dl* call.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// Why a read returned without a window.
#[derive(Debug)]
pub enum ReadError {
    /// A signal interrupted the wait (caller checks its stop flag).
    Interrupted,
    /// Stream or connection failed; reopen.
    Failed(String),
}

impl Pulse {
    pub fn load() -> Result<Pulse, Error> {
        // SAFETY: valid NUL-terminated name; flags are plain constants.
        let h = unsafe { libc::dlopen(LIB.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if h.is_null() {
            return Err(Error::Runtime(format!(
                "cannot load {}: {} (install pulseaudio-libs; PipeWire serves it via pipewire-pulseaudio)",
                LIB.to_string_lossy(),
                dlerror()
            )));
        }
        let sym = |name: &CStr| -> Result<P, Error> {
            // SAFETY: `h` is a live handle from dlopen; `name` is NUL-terminated.
            let p = unsafe { libc::dlsym(h, name.as_ptr()) };
            if p.is_null() {
                Err(Error::Runtime(format!(
                    "{} lacks {}: {}",
                    LIB.to_string_lossy(),
                    name.to_string_lossy(),
                    dlerror()
                )))
            } else {
                Ok(p)
            }
        };
        let f = Fns::load(&sym)?;
        Ok(Pulse { f })
    }

    fn strerror(&self, code: c_int) -> String {
        // SAFETY: pa_strerror returns NULL or a static NUL-terminated string.
        let p = unsafe { (self.f.strerror)(code) };
        if p.is_null() {
            format!("pulse error {code}")
        } else {
            // SAFETY: see above.
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    }

    /// Connect and start a record stream on `source` (e.g. `@DEFAULT_MONITOR@`):
    /// float32 LE, mono (server downmixes), `rate` Hz, fragments of `frag_bytes`.
    /// `stop` is polled while waiting so a signal aborts the connect.
    pub fn open(
        &self,
        source: &CStr,
        rate: u32,
        frag_bytes: u32,
        stop: &dyn Fn() -> bool,
    ) -> Result<Capture<'_>, String> {
        let f = &self.f;
        // SAFETY: plain constructor; NULL on OOM.
        let m = unsafe { (f.mainloop_new)() };
        if m.is_null() {
            return Err("pa_mainloop_new failed".into());
        }
        let mut cap = Capture {
            p: self,
            m,
            c: std::ptr::null_mut(),
            s: std::ptr::null_mut(),
            off: 0,
            filled: 0,
            iterations: 0,
        };
        // SAFETY: `m` is live; the API pointer lives as long as `m`.
        cap.c = unsafe { (f.context_new)((f.mainloop_get_api)(m), c"gowl".as_ptr()) };
        if cap.c.is_null() {
            return Err("pa_context_new failed".into());
        }
        // SAFETY: live context; NULL server = default; no spawn API.
        if unsafe {
            (f.context_connect)(
                cap.c,
                std::ptr::null(),
                PA_CONTEXT_NOAUTOSPAWN,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(cap.context_error());
        }
        loop {
            // SAFETY: live context.
            match unsafe { (f.context_get_state)(cap.c) } {
                PA_CONTEXT_READY => break,
                PA_CONTEXT_FAILED | PA_CONTEXT_TERMINATED => return Err(cap.context_error()),
                _ => cap.iterate(stop).map_err(|_| "interrupted".to_string())?,
            }
        }
        let ss = SampleSpec {
            format: PA_SAMPLE_FLOAT32LE,
            rate,
            channels: 1,
        };
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: frag_bytes,
        };
        // SAFETY: live context; `ss` valid for the call (copied); NULL map = default.
        cap.s = unsafe { (f.stream_new)(cap.c, c"gowl music".as_ptr(), &ss, std::ptr::null()) };
        if cap.s.is_null() {
            return Err(cap.context_error());
        }
        // SAFETY: live stream; `source` and `attr` valid for the call (copied).
        if unsafe {
            (f.stream_connect_record)(cap.s, source.as_ptr(), &attr, PA_STREAM_ADJUST_LATENCY)
        } < 0
        {
            return Err(cap.context_error());
        }
        loop {
            // SAFETY: live stream.
            match unsafe { (f.stream_get_state)(cap.s) } {
                PA_STREAM_READY => return Ok(cap),
                PA_STREAM_FAILED | PA_STREAM_TERMINATED => return Err(cap.context_error()),
                _ => cap.iterate(stop).map_err(|_| "interrupted".to_string())?,
            }
        }
    }
}

/// An open record stream with its own mainloop and connection; torn down on drop.
pub struct Capture<'a> {
    p: &'a Pulse,
    m: P,
    c: P,
    s: P,
    /// Bytes of the currently peeked fragment already consumed.
    off: usize,
    /// Samples already in the caller's window.
    filled: usize,
    /// Mainloop iterations (≈ wakeups) since the last `take_iterations` (debug stat).
    iterations: u32,
}

impl Capture<'_> {
    /// Mainloop iterations since the last call.
    pub fn take_iterations(&mut self) -> u32 {
        std::mem::take(&mut self.iterations)
    }

    fn context_error(&self) -> String {
        // SAFETY: live context.
        self.p.strerror(unsafe { (self.p.f.context_errno)(self.c) })
    }

    /// One blocking mainloop iteration. Err if a signal arrived (stop flag set).
    #[inline]
    fn iterate(&mut self, stop: &dyn Fn() -> bool) -> Result<(), ReadError> {
        if stop() {
            return Err(ReadError::Interrupted);
        }
        self.iterations += 1;
        // SAFETY: live mainloop; blocks in poll() until I/O or a signal (EINTR → 0).
        let r = unsafe { (self.p.f.mainloop_iterate)(self.m, 1, std::ptr::null_mut()) };
        if r < 0 {
            return Err(ReadError::Failed("pulse mainloop quit".into()));
        }
        if stop() {
            return Err(ReadError::Interrupted);
        }
        Ok(())
    }

    /// Fill `buf` with the next `buf.len()` samples, blocking. Allocation-free.
    #[inline]
    pub fn read(&mut self, buf: &mut [f32], stop: &dyn Fn() -> bool) -> Result<(), ReadError> {
        let f = &self.p.f;
        loop {
            while self.filled < buf.len() {
                let mut data: *const c_void = std::ptr::null();
                let mut nbytes = 0usize;
                // SAFETY: live stream; out-pointers valid.
                if unsafe { (f.stream_peek)(self.s, &mut data, &mut nbytes) } < 0 {
                    return Err(ReadError::Failed(self.context_error()));
                }
                if nbytes == 0 {
                    break; // nothing buffered: wait
                }
                if data.is_null() {
                    // A hole (dropped data): skip it.
                    // SAFETY: a fragment is peeked.
                    unsafe { (f.stream_drop)(self.s) };
                    self.off = 0;
                    continue;
                }
                let avail = (nbytes - self.off) / 4;
                let take = avail.min(buf.len() - self.filled);
                // SAFETY: `data` points to `nbytes` readable bytes until stream_drop;
                // `off + 4·take <= nbytes` and `filled + take <= buf.len()`. Copied as
                // bytes, so no alignment is assumed; any bit pattern is a valid f32.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        data.cast::<u8>().add(self.off),
                        buf.as_mut_ptr().add(self.filled).cast::<u8>(),
                        take * 4,
                    );
                }
                self.filled += take;
                self.off += take * 4;
                if self.off + 4 > nbytes {
                    // SAFETY: a fragment is peeked.
                    unsafe { (f.stream_drop)(self.s) };
                    self.off = 0;
                }
            }
            if self.filled == buf.len() {
                self.filled = 0;
                return Ok(());
            }
            // SAFETY: live stream / context.
            let (ss, cs) = unsafe { ((f.stream_get_state)(self.s), (f.context_get_state)(self.c)) };
            if ss != PA_STREAM_READY || cs != PA_CONTEXT_READY {
                return Err(ReadError::Failed(self.context_error()));
            }
            self.iterate(stop)?;
        }
    }
}

impl Drop for Capture<'_> {
    fn drop(&mut self) {
        let f = &self.p.f;
        // SAFETY: each object came from its constructor and is released exactly once,
        // stream before context before mainloop.
        unsafe {
            if !self.s.is_null() {
                (f.stream_disconnect)(self.s);
                (f.stream_unref)(self.s);
            }
            if !self.c.is_null() {
                (f.context_disconnect)(self.c);
                (f.context_unref)(self.c);
            }
            (f.mainloop_free)(self.m);
        }
    }
}
