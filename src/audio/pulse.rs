//! Minimal hand-written FFI to libpulse (served by `pipewire-pulse` on Fedora),
//! loaded with `dlopen("libpulse.so.0")` only when `gowl music` runs, so no other
//! mode ever maps it (AUDIO.md "Capture").
//!
//! We drive libpulse's *non-threaded* `pa_mainloop` from our own thread instead of
//! using `pa_simple` (DECISIONS D-030): `pa_simple` runs a second mainloop thread,
//! so every fragment woke two threads (measured 100 wakeups/s vs the ≤ 50 budget),
//! and its read waits on a condition variable that signals cannot interrupt.
//!
//! The mainloop's `poll()` is ours (`pa_mainloop_set_poll_func`, D-042): it watches
//! libpulse's fds *plus* the [`Hook`] fds (signalfd, timerfd, netlink, verify
//! socket) in one call, so music mode is one `poll()` over everything, like screen
//! mode. When one of ours fires, the read returns [`ReadError::Interrupted`] and the
//! caller handles it. No callbacks otherwise: states are polled after each
//! iteration and audio is taken with `pa_stream_peek`/`pa_stream_drop`.

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_int, c_ulong, c_void};

use crate::Error;

const LIB: &CStr = c"libpulse.so.0";
const PA_SAMPLE_FLOAT32LE: c_int = 5;
const PA_CONTEXT_NOAUTOSPAWN: c_int = 1;
const PA_CONTEXT_READY: c_int = 4;
const PA_CONTEXT_FAILED: c_int = 5;
const PA_CONTEXT_TERMINATED: c_int = 6;
const PA_STREAM_CREATING: c_int = 1;
const PA_ERR_TIMEOUT: c_int = 8;
const PA_STREAM_READY: c_int = 2;
const PA_STREAM_ADJUST_LATENCY: c_int = 0x2000;
/// Passive stream: don't keep the monitored sink awake (D-052).
const PA_STREAM_DONT_INHIBIT_AUTO_SUSPEND: c_int = 0x8000;

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
type PollFn = unsafe extern "C" fn(*mut libc::pollfd, c_ulong, c_int, *mut c_void) -> c_int;

/// Extra fds the mainloop's `poll()` watches for the caller.
pub const HOOK_FDS: usize = 5;
const NO_FD: libc::pollfd = libc::pollfd {
    fd: -1,
    events: libc::POLLIN,
    revents: 0,
};

/// The caller's fds, polled together with libpulse's (fd -1 = unused slot).
/// Interior mutability: the poll callback runs inside `pa_mainloop_iterate` on the
/// same thread while the caller holds a shared reference.
pub struct Hook {
    fds: Cell<[libc::pollfd; HOOK_FDS]>,
    /// Bit i set = slot i is polled. Slots outside the mask are left alone, so a
    /// level-triggered fd that is ready (an expired timerfd) can't make the
    /// mainloop spin or abort a connect that only cares about signals.
    active: Cell<u8>,
    fired: Cell<bool>,
}

/// Every slot polled.
pub const HOOK_ALL: u8 = (1 << HOOK_FDS) - 1;

impl Default for Hook {
    fn default() -> Self {
        Hook {
            fds: Cell::new([NO_FD; HOOK_FDS]),
            active: Cell::new(HOOK_ALL),
            fired: Cell::new(false),
        }
    }
}

impl Hook {
    pub fn set_fd(&self, i: usize, fd: c_int) {
        let mut f = self.fds.get();
        f[i].fd = fd;
        self.fds.set(f);
    }

    /// Poll only the slots in `mask` (bit i = slot i) until changed again.
    pub fn set_active(&self, mask: u8) {
        self.active.set(mask & HOOK_ALL);
    }

    /// One of our fds was ready during the last poll.
    pub fn fired(&self) -> bool {
        self.fired.get()
    }

    /// The ready events of each slot since the last `take`, clearing them.
    pub fn take(&self) -> [i16; HOOK_FDS] {
        let mut f = self.fds.get();
        let ev = f.map(|p| p.revents);
        for p in &mut f {
            p.revents = 0;
        }
        self.fds.set(f);
        self.fired.set(false);
        ev
    }

    /// The active slots as pollfds (inactive ones as fd -1, which poll ignores).
    fn polled(&self) -> [libc::pollfd; HOOK_FDS] {
        let mut f = self.fds.get();
        let mask = self.active.get();
        for (i, p) in f.iter_mut().enumerate() {
            if mask & (1 << i) == 0 {
                p.fd = -1;
            }
            p.revents = 0;
        }
        f
    }

    /// Record the revents of a poll over `polled()`; true if any slot fired.
    fn record(&self, got: &[libc::pollfd]) -> c_int {
        let mut f = self.fds.get();
        let mut hits = 0;
        for (p, g) in f.iter_mut().zip(got) {
            if g.fd >= 0 && g.revents != 0 {
                p.revents = g.revents;
                hits += 1;
            }
        }
        if hits > 0 {
            self.fds.set(f);
            self.fired.set(true);
        }
        hits
    }

    /// Block on our active fds alone (no stream open). EINTR = nothing ready.
    pub fn wait(&self, timeout_ms: c_int) -> std::io::Result<()> {
        let mut f = self.polled();
        // SAFETY: valid pollfd array of HOOK_FDS entries.
        let r = unsafe { libc::poll(f.as_mut_ptr(), HOOK_FDS as libc::nfds_t, timeout_ms) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            return if e.kind() == std::io::ErrorKind::Interrupted {
                Ok(())
            } else {
                Err(e)
            };
        }
        self.record(&f);
        Ok(())
    }
}

/// `pa_poll_func`: poll libpulse's fds and the hook's active ones in one call.
/// Returns the number of *libpulse* fds with events (its dispatch counts down
/// from that).
unsafe extern "C" fn hook_poll(
    ufds: *mut libc::pollfd,
    nfds: c_ulong,
    timeout: c_int,
    ud: *mut c_void,
) -> c_int {
    // SAFETY: `ud` is the `&Hook` registered with this mainloop; the Capture that
    // owns the mainloop borrows it, so it outlives every call.
    let hook = unsafe { &*(ud as *const Hook) };
    let theirs: &mut [libc::pollfd] = if nfds == 0 || ufds.is_null() {
        &mut []
    } else {
        // SAFETY: libpulse passes `nfds` valid pollfds.
        unsafe { std::slice::from_raw_parts_mut(ufds, nfds as usize) }
    };
    poll_both(hook, theirs, timeout)
}

/// The body of [`hook_poll`], on safe slices (tested directly).
fn poll_both(hook: &Hook, theirs: &mut [libc::pollfd], timeout: c_int) -> c_int {
    const MAX: usize = 32;
    let n = theirs.len();
    if n + HOOK_FDS > MAX {
        // Never seen (libpulse uses a handful of fds); stay correct without ours.
        // SAFETY: libpulse's own array, `n` entries.
        return unsafe { libc::poll(theirs.as_mut_ptr(), n as libc::nfds_t, timeout) };
    }
    let mut all = [NO_FD; MAX];
    all[..HOOK_FDS].copy_from_slice(&hook.polled());
    all[HOOK_FDS..HOOK_FDS + n].copy_from_slice(theirs);
    // SAFETY: `all` holds HOOK_FDS + n valid entries.
    let r = unsafe { libc::poll(all.as_mut_ptr(), (HOOK_FDS + n) as libc::nfds_t, timeout) };
    if r < 0 {
        return r;
    }
    for (t, a) in theirs.iter_mut().zip(&all[HOOK_FDS..]) {
        t.revents = a.revents;
    }
    r - hook.record(&all[..HOOK_FDS])
}

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
    mainloop_set_poll_func = c"pa_mainloop_set_poll_func": fn(P, PollFn, *mut c_void);
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
    /// One of the hook's fds is ready (signal, timer, network): handle it, read again.
    Interrupted,
    /// Stream or connection failed; reopen.
    Failed(String),
    /// A passive stream that never started (sink suspended the whole time) hit
    /// libpulse's request timeout; reopen quietly (D-052).
    Expired,
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
    /// The hook's fds are polled while waiting, so a signal aborts the connect.
    /// The stream is passive (D-052): it doesn't keep the sink from suspending, and
    /// on a suspended sink it stays CREATING until something plays, so this returns
    /// once the context is up and the stream is requested; `read` waits for data.
    pub fn open<'a>(
        &'a self,
        source: &CStr,
        rate: u32,
        frag_bytes: u32,
        hook: &'a Hook,
    ) -> Result<Capture<'a>, ReadError> {
        let f = &self.f;
        // SAFETY: plain constructor; NULL on OOM.
        let m = unsafe { (f.mainloop_new)() };
        if m.is_null() {
            return Err(ReadError::Failed("pa_mainloop_new failed".into()));
        }
        // SAFETY: live mainloop; `hook` outlives it (the Capture borrows it).
        unsafe {
            (f.mainloop_set_poll_func)(m, hook_poll, (hook as *const Hook).cast_mut().cast());
        }
        let mut cap = Capture {
            p: self,
            hook,
            m,
            c: std::ptr::null_mut(),
            s: std::ptr::null_mut(),
            off: 0,
            filled: 0,
            iterations: 0,
            started: false,
            rate: 0,
            frag_bytes: 0,
        };
        // SAFETY: `m` is live; the API pointer lives as long as `m`.
        cap.c = unsafe { (f.context_new)((f.mainloop_get_api)(m), c"gowl".as_ptr()) };
        if cap.c.is_null() {
            return Err(ReadError::Failed("pa_context_new failed".into()));
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
            return Err(ReadError::Failed(cap.context_error()));
        }
        loop {
            // SAFETY: live context.
            match unsafe { (f.context_get_state)(cap.c) } {
                PA_CONTEXT_READY => break,
                PA_CONTEXT_FAILED | PA_CONTEXT_TERMINATED => {
                    return Err(ReadError::Failed(cap.context_error()));
                }
                _ => cap.iterate()?,
            }
        }
        cap.rate = rate;
        cap.frag_bytes = frag_bytes;
        cap.connect_stream(source)?;
        Ok(cap)
    }
}

/// An open record stream with its own mainloop and connection; torn down on drop.
pub struct Capture<'a> {
    p: &'a Pulse,
    hook: &'a Hook,
    m: P,
    c: P,
    s: P,
    /// Bytes of the currently peeked fragment already consumed.
    off: usize,
    /// Samples already in the caller's window.
    filled: usize,
    /// Mainloop iterations (≈ wakeups) since the last `take_iterations` (debug stat).
    iterations: u32,
    /// The stream has been READY at least once.
    started: bool,
    rate: u32,
    frag_bytes: u32,
}

impl Capture<'_> {
    /// Create the record stream on the open context (passive, D-052).
    fn connect_stream(&mut self, source: &CStr) -> Result<(), ReadError> {
        let f = &self.p.f;
        let ss = SampleSpec {
            format: PA_SAMPLE_FLOAT32LE,
            rate: self.rate,
            channels: 1,
        };
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: self.frag_bytes,
        };
        // SAFETY: live context; `ss` valid for the call (copied); NULL map = default.
        self.s = unsafe { (f.stream_new)(self.c, c"gowl music".as_ptr(), &ss, std::ptr::null()) };
        if self.s.is_null() {
            return Err(ReadError::Failed(self.context_error()));
        }
        // SAFETY: live stream; `source` and `attr` valid for the call (copied).
        if unsafe {
            (f.stream_connect_record)(
                self.s,
                source.as_ptr(),
                &attr,
                PA_STREAM_ADJUST_LATENCY | PA_STREAM_DONT_INHIBIT_AUTO_SUSPEND,
            )
        } < 0
        {
            return Err(ReadError::Failed(self.context_error()));
        }
        Ok(())
    }

    /// Drop the record stream and request a new one on the same connection (no new
    /// pulse connection): after libpulse expired a never-started passive stream, or
    /// to re-resolve `@DEFAULT_MONITOR@` after a long quiet spell (D-054).
    pub fn restart(&mut self, source: &CStr) -> Result<(), ReadError> {
        let f = &self.p.f;
        if !self.s.is_null() {
            // SAFETY: the stream came from stream_new and is released exactly once.
            unsafe {
                (f.stream_disconnect)(self.s);
                (f.stream_unref)(self.s);
            }
            self.s = std::ptr::null_mut();
        }
        // SAFETY: live context.
        if unsafe { (f.context_get_state)(self.c) } != PA_CONTEXT_READY {
            return Err(ReadError::Failed(self.context_error()));
        }
        self.off = 0;
        self.filled = 0;
        self.started = false;
        self.connect_stream(source)
    }

    /// Mainloop iterations since the last call.
    pub fn take_iterations(&mut self) -> u32 {
        std::mem::take(&mut self.iterations)
    }

    /// The stream left READY/CREATING (or the context died): why, as a ReadError.
    fn broken(&self) -> ReadError {
        // SAFETY: live context.
        let (cs, errno) = unsafe {
            (
                (self.p.f.context_get_state)(self.c),
                (self.p.f.context_errno)(self.c),
            )
        };
        if !self.started && cs == PA_CONTEXT_READY && errno == PA_ERR_TIMEOUT {
            return ReadError::Expired;
        }
        ReadError::Failed(self.context_error())
    }

    fn context_error(&self) -> String {
        // SAFETY: live context.
        self.p.strerror(unsafe { (self.p.f.context_errno)(self.c) })
    }

    /// One blocking mainloop iteration. Err(Interrupted) if a hook fd is ready.
    #[inline]
    fn iterate(&mut self) -> Result<(), ReadError> {
        if self.hook.fired() {
            return Err(ReadError::Interrupted);
        }
        self.iterations += 1;
        // SAFETY: live mainloop; blocks in poll() until I/O or a signal (EINTR → 0).
        let r = unsafe { (self.p.f.mainloop_iterate)(self.m, 1, std::ptr::null_mut()) };
        if r < 0 {
            return Err(ReadError::Failed("pulse mainloop quit".into()));
        }
        if self.hook.fired() {
            return Err(ReadError::Interrupted);
        }
        Ok(())
    }

    /// Fill `buf` with the next `buf.len()` samples, blocking. Allocation-free.
    #[inline]
    pub fn read(&mut self, buf: &mut [f32]) -> Result<(), ReadError> {
        let f = &self.p.f;
        loop {
            // SAFETY: live stream.
            match unsafe { (f.stream_get_state)(self.s) } {
                PA_STREAM_CREATING => {
                    // Passive stream on a suspended sink (D-052): nothing to peek yet.
                    self.iterate()?;
                    continue;
                }
                PA_STREAM_READY => self.started = true,
                _ => return Err(self.broken()),
            }
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
                return Err(self.broken());
            }
            self.iterate()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pipe_with_data() -> (c_int, c_int) {
        let mut fds = [0; 2];
        // SAFETY: valid 2-int array; then 1 byte written so the read end is readable.
        unsafe {
            assert_eq!(libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC), 0);
            assert_eq!(libc::write(fds[1], b"x".as_ptr().cast(), 1), 1);
        }
        (fds[0], fds[1])
    }

    #[test]
    fn inactive_slots_neither_fire_nor_wake() {
        let (ready, w1) = pipe_with_data(); // stands in for an expired timerfd
        let mut quiet = [0; 2];
        // SAFETY: valid array.
        unsafe { libc::pipe2(quiet.as_mut_ptr(), libc::O_CLOEXEC) };
        let hook = Hook::default();
        hook.set_fd(0, quiet[0]); // "signalfd": nothing pending
        hook.set_fd(1, ready);
        // Signal-only (what an open uses): the ready timer is ignored, poll times out.
        hook.set_active(1 << 0);
        let mut theirs: [libc::pollfd; 0] = [];
        let t = std::time::Instant::now();
        assert_eq!(poll_both(&hook, &mut theirs, 50), 0);
        assert!(!hook.fired(), "a ready timer must not interrupt an open");
        assert!(t.elapsed() >= std::time::Duration::from_millis(45));
        // All slots: the timer fires and is reported, not counted as libpulse's.
        hook.set_active(HOOK_ALL);
        let (lp, w2) = pipe_with_data();
        let mut theirs = [libc::pollfd {
            fd: lp,
            events: libc::POLLIN,
            revents: 0,
        }];
        assert_eq!(poll_both(&hook, &mut theirs, 50), 1);
        assert!(hook.fired());
        assert_ne!(theirs[0].revents, 0);
        let ev = hook.take();
        assert_ne!(ev[1], 0);
        assert_eq!(ev[0], 0);
        assert!(!hook.fired());
        // SAFETY: closing fds we created.
        unsafe {
            for fd in [ready, w1, quiet[0], quiet[1], lp, w2] {
                libc::close(fd);
            }
        }
    }
}
