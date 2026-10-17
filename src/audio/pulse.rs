//! Minimal hand-written FFI to `libpulse-simple` (served by `pipewire-pulse` on
//! Fedora), loaded with `dlopen` only when `gowl music` runs, so no other mode ever
//! maps it (AUDIO.md "Capture"). Four symbols: `pa_simple_new`, `pa_simple_read`,
//! `pa_simple_free`, `pa_strerror` (the last lives in libpulse, which
//! libpulse-simple depends on; `dlsym` on the handle searches its dependencies).

use std::ffi::{CStr, c_char, c_int, c_void};

use crate::Error;

const LIB: &CStr = c"libpulse-simple.so.0";
const PA_STREAM_RECORD: c_int = 2;
const PA_SAMPLE_FLOAT32LE: c_int = 5;

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

type NewFn = unsafe extern "C" fn(
    server: *const c_char,
    name: *const c_char,
    dir: c_int,
    dev: *const c_char,
    stream_name: *const c_char,
    ss: *const SampleSpec,
    map: *const c_void,
    attr: *const BufferAttr,
    error: *mut c_int,
) -> *mut c_void;
type ReadFn = unsafe extern "C" fn(
    s: *mut c_void,
    data: *mut c_void,
    bytes: usize,
    error: *mut c_int,
) -> c_int;
type FreeFn = unsafe extern "C" fn(s: *mut c_void);
type StrerrorFn = unsafe extern "C" fn(error: c_int) -> *const c_char;

/// The loaded library (never unloaded: libpulse does not like `dlclose`).
pub struct Pulse {
    new: NewFn,
    read: ReadFn,
    free: FreeFn,
    strerror: StrerrorFn,
}

fn dlerror() -> String {
    // SAFETY: dlerror returns NULL or a NUL-terminated thread-local string.
    let p = unsafe { libc::dlerror() };
    if p.is_null() {
        "unknown error".into()
    } else {
        // SAFETY: non-null, NUL-terminated (see above); copied before the next dl* call.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
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
        let sym = |name: &CStr| -> Result<*mut c_void, Error> {
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
        let (n, r, f, e) = (
            sym(c"pa_simple_new")?,
            sym(c"pa_simple_read")?,
            sym(c"pa_simple_free")?,
            sym(c"pa_strerror")?,
        );
        // SAFETY: the symbols come from libpulse(-simple) and have exactly these C
        // signatures (pulse/simple.h, pulse/error.h); the library is never unloaded.
        unsafe {
            Ok(Pulse {
                new: std::mem::transmute::<*mut c_void, NewFn>(n),
                read: std::mem::transmute::<*mut c_void, ReadFn>(r),
                free: std::mem::transmute::<*mut c_void, FreeFn>(f),
                strerror: std::mem::transmute::<*mut c_void, StrerrorFn>(e),
            })
        }
    }

    pub fn strerror(&self, code: c_int) -> String {
        // SAFETY: pa_strerror returns NULL or a static NUL-terminated string.
        let p = unsafe { (self.strerror)(code) };
        if p.is_null() {
            format!("pulse error {code}")
        } else {
            // SAFETY: see above.
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    }

    /// Record stream on `source` (e.g. `@DEFAULT_MONITOR@`): float32 LE, mono,
    /// `rate` Hz, fragments of `frag_bytes`.
    pub fn open(&self, source: &CStr, rate: u32, frag_bytes: u32) -> Result<Stream<'_>, String> {
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
        let mut err: c_int = 0;
        // SAFETY: all pointers are valid for the call; NULL server = default; NULL
        // channel map = default for 1 channel. Returns NULL on failure with `err` set.
        let s = unsafe {
            (self.new)(
                std::ptr::null(),
                c"gowl".as_ptr(),
                PA_STREAM_RECORD,
                source.as_ptr(),
                c"gowl music".as_ptr(),
                &ss,
                std::ptr::null(),
                &attr,
                &mut err,
            )
        };
        if s.is_null() {
            return Err(self.strerror(err));
        }
        Ok(Stream { pulse: self, s })
    }
}

/// An open record stream; freed on drop.
pub struct Stream<'a> {
    pulse: &'a Pulse,
    s: *mut c_void,
}

impl Stream<'_> {
    /// Blocking read of exactly `buf.len()` samples. Allocation-free on our side.
    #[inline]
    pub fn read(&mut self, buf: &mut [f32]) -> Result<(), String> {
        let mut err: c_int = 0;
        // SAFETY: `self.s` is a live stream; `buf` is valid for `size_of_val(buf)` bytes.
        let r = unsafe {
            (self.pulse.read)(
                self.s,
                buf.as_mut_ptr().cast(),
                std::mem::size_of_val(buf),
                &mut err,
            )
        };
        if r < 0 {
            Err(self.pulse.strerror(err))
        } else {
            Ok(())
        }
    }
}

impl Drop for Stream<'_> {
    fn drop(&mut self) {
        // SAFETY: `self.s` came from pa_simple_new and is freed exactly once.
        unsafe { (self.pulse.free)(self.s) }
    }
}
