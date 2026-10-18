//! Minimal Wayland wire-protocol client (DECISIONS D-016): one Unix socket, a fixed
//! receive buffer, a fixed send buffer, fd passing for `wl_shm.create_pool`, and an
//! object-id allocator that reuses ids freed by `wl_display.delete_id`.
//! No libwayland, no proc-macro scanner, no per-message allocation.
//!
//! Only the handful of interfaces gowl uses are spoken (see `screencopy.rs`); message
//! layouts follow `/usr/share/wayland/wayland.xml` and `wlr-screencopy-unstable-v1.xml`.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::error::Error;

/// `wl_display` is always object 1.
pub const DISPLAY: u32 = 1;

const RBUF: usize = 16 * 1024;
const WBUF: usize = 4096;
const MAX_FDS: usize = 4;
/// Longest wait for a compositor that doesn't drain its socket.
const SEND_WAIT_MS: i32 = 2000;
/// Wayland caps one message at 4096 bytes.
const MAX_MSG: usize = 4096;

/// One request argument.
#[derive(Clone, Copy, Debug)]
pub enum Arg<'a> {
    Uint(u32),
    Int(i32),
    /// Object id (or a new_id we allocated).
    Obj(u32),
    /// String without the trailing NUL.
    Str(&'a [u8]),
    /// File descriptor, sent with SCM_RIGHTS. Must stay open until the next flush.
    Fd(RawFd),
}

impl Arg<'_> {
    fn wire_len(&self) -> usize {
        match self {
            Arg::Str(s) => 4 + (s.len() + 1).next_multiple_of(4),
            Arg::Fd(_) => 0,
            _ => 4,
        }
    }
}

/// A received event: object, opcode and where its body sits in the receive buffer.
/// Valid until the next `Conn::fill` (read the arguments right away).
#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub obj: u32,
    pub op: u16,
    off: usize,
    len: usize,
}

/// Sequential reader over an event body. Malformed bodies read as zeros / empty
/// strings instead of panicking (the compositor is trusted, but not that much).
pub struct Args<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Args<'a> {
    pub fn new(b: &'a [u8]) -> Args<'a> {
        Args { b, p: 0 }
    }

    pub fn uint(&mut self) -> u32 {
        let v = self
            .b
            .get(self.p..self.p + 4)
            .map_or(0, |w| u32::from_ne_bytes([w[0], w[1], w[2], w[3]]));
        self.p += 4;
        v
    }

    pub fn int(&mut self) -> i32 {
        self.uint() as i32
    }

    /// String bytes without the NUL terminator (empty for a null string).
    pub fn str(&mut self) -> &'a [u8] {
        let n = self.uint() as usize;
        let start = self.p;
        self.p += n.next_multiple_of(4);
        if n == 0 {
            return &[];
        }
        self.b.get(start..start + n - 1).unwrap_or(&[])
    }
}

/// The connection to the compositor.
pub struct Conn {
    fd: OwnedFd,
    rbuf: Box<[u8]>,
    rstart: usize,
    rend: usize,
    wbuf: Box<[u8]>,
    wlen: usize,
    wfds: [RawFd; MAX_FDS],
    nwfds: usize,
    next_id: u32,
    free_ids: Vec<u32>,
    /// Test compositor side: keep received fds for `take_fd` and hand `wl_display`
    /// messages (requests, there) to the caller. Clients close fds they get.
    server: bool,
    rfds: [RawFd; MAX_FDS],
    nrfds: usize,
    /// Polled alongside the socket while a send waits (the runtime's signalfd), so
    /// a compositor that stops reading can't swallow Ctrl+C. -1 = none.
    interrupt_fd: RawFd,
    /// The last `fill` emptied the socket: a short read without fds, or EAGAIN. A
    /// dispatch loop can stop there instead of paying one more recvmsg for EAGAIN.
    pub drained: bool,
}

fn sock_path() -> Result<Vec<u8>, Error> {
    let name = std::env::var_os("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into());
    let name = name.as_encoded_bytes();
    if name.first() == Some(&b'/') {
        return Ok(name.to_vec());
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .ok_or_else(|| {
            Error::Capture("XDG_RUNTIME_DIR is not set: not in a Wayland session?".into())
        })?;
    let mut p = dir.as_encoded_bytes().to_vec();
    p.push(b'/');
    p.extend_from_slice(name);
    Ok(p)
}

impl Conn {
    /// Connect to `$WAYLAND_DISPLAY` (relative to `$XDG_RUNTIME_DIR`, or absolute).
    pub fn connect() -> Result<Conn, Error> {
        let path = sock_path()?;
        let shown = String::from_utf8_lossy(&path).into_owned();
        // SAFETY: plain socket(2).
        let raw = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if raw < 0 {
            return Err(Error::Io("wayland socket", io::Error::last_os_error()));
        }
        // SAFETY: raw is a fresh fd we own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: sockaddr_un is plain old data; zeroed is a valid value.
        let mut sa: libc::sockaddr_un = unsafe { mem::zeroed() };
        sa.sun_family = libc::AF_UNIX as libc::sa_family_t;
        if path.len() >= sa.sun_path.len() {
            return Err(Error::Capture(format!(
                "Wayland socket path too long: {shown}"
            )));
        }
        for (d, s) in sa.sun_path.iter_mut().zip(&path) {
            *d = *s as libc::c_char;
        }
        // SAFETY: sa is a valid sockaddr_un.
        let r = unsafe {
            libc::connect(
                raw,
                (&sa as *const libc::sockaddr_un).cast(),
                mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        if r != 0 {
            let e = io::Error::last_os_error();
            return Err(Error::Capture(format!(
                "cannot connect to the Wayland compositor at {shown}: {e}"
            )));
        }
        Ok(Conn::from_fd(fd))
    }

    /// Wrap an already-connected stream socket (tests use a socketpair).
    pub fn from_fd(fd: OwnedFd) -> Conn {
        Conn {
            fd,
            rbuf: vec![0u8; RBUF].into_boxed_slice(),
            rstart: 0,
            rend: 0,
            wbuf: vec![0u8; WBUF].into_boxed_slice(),
            wlen: 0,
            wfds: [-1; MAX_FDS],
            nwfds: 0,
            next_id: 2,
            free_ids: Vec::with_capacity(64),
            server: false,
            rfds: [-1; MAX_FDS],
            nrfds: 0,
            interrupt_fd: -1,
            drained: false,
        }
    }

    /// Server side of a test socketpair: keep fds that arrive (see `take_fd`) and
    /// allocate ids from the server range.
    pub fn test_server(fd: OwnedFd) -> Conn {
        let mut c = Conn::from_fd(fd);
        c.server = true;
        c.next_id = 0xff00_0000;
        c
    }

    /// Oldest fd received and not yet taken (only with `test_server`).
    pub fn take_fd(&mut self) -> Option<OwnedFd> {
        if self.nrfds == 0 {
            return None;
        }
        let fd = self.rfds[0];
        self.rfds.copy_within(1..self.nrfds, 0);
        self.nrfds -= 1;
        // SAFETY: the fd arrived via SCM_RIGHTS and is owned by us until now.
        Some(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// A fresh client object id (reusing ids the server has deleted).
    pub fn new_id(&mut self) -> u32 {
        if let Some(id) = self.free_ids.pop() {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Queue one request; flushes first if the send buffer would overflow.
    pub fn request(&mut self, obj: u32, op: u16, args: &[Arg<'_>]) -> Result<(), Error> {
        let size = 8 + args.iter().map(Arg::wire_len).sum::<usize>();
        let nfds = args.iter().filter(|a| matches!(a, Arg::Fd(_))).count();
        debug_assert!(size <= MAX_MSG);
        if self.wlen + size > WBUF || self.nwfds + nfds > MAX_FDS {
            self.flush()?;
        }
        let mut p = self.wlen;
        put(&mut self.wbuf, &mut p, obj);
        put(
            &mut self.wbuf,
            &mut p,
            ((size as u32) << 16) | u32::from(op),
        );
        for a in args {
            match *a {
                Arg::Uint(v) | Arg::Obj(v) => put(&mut self.wbuf, &mut p, v),
                Arg::Int(v) => put(&mut self.wbuf, &mut p, v as u32),
                Arg::Str(s) => {
                    put(&mut self.wbuf, &mut p, s.len() as u32 + 1);
                    let padded = (s.len() + 1).next_multiple_of(4);
                    self.wbuf[p..p + s.len()].copy_from_slice(s);
                    self.wbuf[p + s.len()..p + padded].fill(0);
                    p += padded;
                }
                Arg::Fd(fd) => {
                    self.wfds[self.nwfds] = fd;
                    self.nwfds += 1;
                }
            }
        }
        self.wlen = p;
        Ok(())
    }

    /// Send everything queued (one `sendmsg`, fds attached). Blocks only if the
    /// compositor's socket buffer is full.
    pub fn flush(&mut self) -> Result<(), Error> {
        let mut sent = 0usize;
        while sent < self.wlen {
            let mut iov = libc::iovec {
                iov_base: self.wbuf[sent..].as_mut_ptr().cast(),
                iov_len: self.wlen - sent,
            };
            // Room for MAX_FDS ints, 8-byte aligned.
            let mut cbuf = [0u64; 4];
            // SAFETY: msghdr is plain old data.
            let mut msg: libc::msghdr = unsafe { mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            if self.nwfds > 0 {
                let data_len = self.nwfds * mem::size_of::<RawFd>();
                // SAFETY: CMSG_SPACE is a pure computation.
                let space = unsafe { libc::CMSG_SPACE(data_len as u32) } as usize;
                debug_assert!(space <= mem::size_of_val(&cbuf));
                msg.msg_control = cbuf.as_mut_ptr().cast();
                msg.msg_controllen = space;
                // SAFETY: msg_control points at `space` bytes we own; CMSG_FIRSTHDR is
                // non-null because msg_controllen >= sizeof(cmsghdr).
                unsafe {
                    let c = libc::CMSG_FIRSTHDR(&msg);
                    (*c).cmsg_level = libc::SOL_SOCKET;
                    (*c).cmsg_type = libc::SCM_RIGHTS;
                    (*c).cmsg_len = libc::CMSG_LEN(data_len as u32) as usize;
                    std::ptr::copy_nonoverlapping(
                        self.wfds.as_ptr(),
                        libc::CMSG_DATA(c).cast::<RawFd>(),
                        self.nwfds,
                    );
                }
            }
            // SAFETY: msg points at valid iovec/control buffers for the call.
            let n = unsafe {
                libc::sendmsg(
                    self.fd.as_raw_fd(),
                    &msg,
                    libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                match e.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => {
                        self.wait_writable()?;
                        continue;
                    }
                    _ => return Err(Error::Capture(format!("Wayland connection lost: {e}"))),
                }
            }
            self.nwfds = 0; // fds go with the first chunk
            sent += n as usize;
        }
        self.wlen = 0;
        Ok(())
    }

    /// Read what the socket has. `Ok(false)` = nothing available right now.
    /// Invalidates earlier `Event`s (the buffer is compacted).
    pub fn fill(&mut self) -> Result<bool, Error> {
        if self.rstart > 0 {
            self.rbuf.copy_within(self.rstart..self.rend, 0);
            self.rend -= self.rstart;
            self.rstart = 0;
        }
        if self.rend == self.rbuf.len() {
            return Err(Error::Capture("Wayland receive buffer overflow".into()));
        }
        loop {
            let mut iov = libc::iovec {
                iov_base: self.rbuf[self.rend..].as_mut_ptr().cast(),
                iov_len: self.rbuf.len() - self.rend,
            };
            let mut cbuf = [0u64; 8];
            // SAFETY: msghdr is plain old data.
            let mut msg: libc::msghdr = unsafe { mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = mem::size_of_val(&cbuf);
            // SAFETY: valid buffers for the call.
            let n = unsafe {
                libc::recvmsg(
                    self.fd.as_raw_fd(),
                    &mut msg,
                    libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                return match e.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => {
                        self.drained = true;
                        Ok(false)
                    }
                    _ => Err(Error::Capture(format!("Wayland connection lost: {e}"))),
                };
            }
            if n == 0 {
                return Err(Error::Capture(
                    "the Wayland compositor closed the connection".into(),
                ));
            }
            // Fewer bytes than room and no fds: the kernel queue was empty (a stream
            // socket returns everything queued, up to the buffer).
            self.drained = (n as usize) < iov.iov_len && msg.msg_controllen == 0;
            self.take_received_fds(&msg);
            self.rend += n as usize;
            return Ok(true);
        }
    }

    /// None of the events gowl handles carry fds; close any that arrive anyway
    /// (a test server keeps them).
    fn take_received_fds(&mut self, msg: &libc::msghdr) {
        // SAFETY: walking the control buffer recvmsg filled, with the libc CMSG macros.
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(c).cast::<RawFd>();
                    let n = ((*c).cmsg_len - libc::CMSG_LEN(0) as usize) / mem::size_of::<RawFd>();
                    for i in 0..n {
                        let fd = data.add(i).read_unaligned();
                        if self.server && self.nrfds < MAX_FDS {
                            self.rfds[self.nrfds] = fd;
                            self.nrfds += 1;
                        } else {
                            libc::close(fd);
                        }
                    }
                }
                c = libc::CMSG_NXTHDR(msg, c);
            }
        }
    }

    /// Next complete event, or `None` if more bytes are needed. `wl_display` events
    /// are handled here: `delete_id` frees the id, `error` becomes an `Err`.
    pub fn next_event(&mut self) -> Result<Option<Event>, Error> {
        loop {
            let avail = self.rend - self.rstart;
            if avail < 8 {
                return Ok(None);
            }
            let h = &self.rbuf[self.rstart..self.rstart + 8];
            let obj = u32::from_ne_bytes([h[0], h[1], h[2], h[3]]);
            let w = u32::from_ne_bytes([h[4], h[5], h[6], h[7]]);
            let size = (w >> 16) as usize;
            if !(8..=MAX_MSG).contains(&size) || !size.is_multiple_of(4) {
                return Err(Error::Capture(format!(
                    "malformed Wayland message (size {size})"
                )));
            }
            if avail < size {
                return Ok(None);
            }
            let ev = Event {
                obj,
                op: (w & 0xffff) as u16,
                off: self.rstart + 8,
                len: size - 8,
            };
            self.rstart += size;
            if obj != DISPLAY || self.server {
                return Ok(Some(ev));
            }
            let mut a = self.args(&ev);
            match ev.op {
                0 => {
                    let (o, code) = (a.uint(), a.uint());
                    let m = String::from_utf8_lossy(a.str()).into_owned();
                    return Err(Error::Capture(format!(
                        "Wayland protocol error on object {o}, code {code}: {m}"
                    )));
                }
                1 => {
                    let id = a.uint();
                    // Capacity is reserved; ids are only freed after we allocated them.
                    if self.free_ids.len() < self.free_ids.capacity() {
                        self.free_ids.push(id);
                    }
                }
                _ => {}
            }
        }
    }

    /// Argument reader for `ev` (valid until the next `fill`).
    pub fn args(&self, ev: &Event) -> Args<'_> {
        Args::new(&self.rbuf[ev.off..ev.off + ev.len])
    }

    /// `wl_display.sync`: returns the callback id whose `done` (opcode 0) marks the
    /// point where the compositor has handled everything sent before it.
    pub fn sync(&mut self) -> Result<u32, Error> {
        let cb = self.new_id();
        self.request(DISPLAY, 0, &[Arg::Obj(cb)])?;
        self.flush()?;
        Ok(cb)
    }

    /// Also watch `fd` (readable = give up) whenever a send has to wait.
    pub fn set_interrupt_fd(&mut self, fd: RawFd) {
        self.interrupt_fd = fd;
    }

    /// The compositor's socket buffer is full: wait at most `SEND_WAIT_MS` for it
    /// to drain, and not at all once the interrupt fd (signals) is readable.
    fn wait_writable(&self) -> Result<(), Error> {
        let mut p = [
            libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            },
            libc::pollfd {
                fd: self.interrupt_fd, // poll ignores negative fds
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: valid pollfd array.
            let r = unsafe { libc::poll(p.as_mut_ptr(), 2, SEND_WAIT_MS) };
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(Error::Io("poll", e));
            }
            if r == 0 {
                return Err(Error::Capture(format!(
                    "the Wayland compositor stopped reading requests for {SEND_WAIT_MS} ms"
                )));
            }
            if p[1].revents != 0 {
                return Err(Error::Capture(
                    "interrupted while waiting for the Wayland compositor".into(),
                ));
            }
            return Ok(());
        }
    }

    /// Block until the socket is readable (setup roundtrips only).
    pub fn wait_readable(&self) -> Result<(), Error> {
        wait_fd(self.fd.as_raw_fd(), libc::POLLIN)
    }
}

#[inline]
fn put(b: &mut [u8], p: &mut usize, v: u32) {
    b[*p..*p + 4].copy_from_slice(&v.to_ne_bytes());
    *p += 4;
}

/// Block until `fd` is ready for `events` (setup roundtrips only).
fn wait_fd(fd: RawFd, events: libc::c_short) -> Result<(), Error> {
    let mut p = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        // SAFETY: one valid pollfd.
        let r = unsafe { libc::poll(&mut p, 1, -1) };
        if r >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(Error::Io("poll", e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (OwnedFd, OwnedFd) {
        let mut sv = [0; 2];
        // SAFETY: socketpair fills two fds we then own.
        let r = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
                sv.as_mut_ptr(),
            )
        };
        assert_eq!(r, 0);
        // SAFETY: fresh fds from socketpair.
        unsafe { (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1])) }
    }

    #[test]
    fn stuck_compositor_cannot_swallow_a_signal() {
        let (a, _server_never_reads) = pair();
        let mut c = Conn::from_fd(a);
        let mut p = [0; 2];
        // SAFETY: pipe fills two fds; we make the read end readable.
        unsafe {
            assert_eq!(libc::pipe(p.as_mut_ptr()), 0);
            assert_eq!(libc::write(p[1], b"x".as_ptr().cast(), 1), 1);
        }
        c.set_interrupt_fd(p[0]);
        let t = std::time::Instant::now();
        let big = [b'x'; 1000];
        let err = loop {
            if let Err(e) = c.request(5, 0, &[Arg::Str(&big)]) {
                break e;
            }
            assert!(t.elapsed().as_secs() < 5, "never blocked");
        };
        assert!(err.to_string().contains("interrupted"), "{err}");
        assert!(t.elapsed().as_millis() < 1000);
        // SAFETY: closing the pipe we created.
        unsafe {
            libc::close(p[0]);
            libc::close(p[1]);
        }
    }

    #[test]
    fn request_encoding_event_parsing_and_fds() {
        let (a, b) = pair();
        let mut c = Conn::from_fd(a);
        let mut s = Conn::test_server(b);
        c.request(7, 3, &[Arg::Uint(5), Arg::Str(b"wl_shm"), Arg::Int(-2)])
            .unwrap();
        // An fd rides along with the next flush.
        // SAFETY: memfd_create with a static name.
        let m = unsafe { libc::memfd_create(c"t".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(m >= 0);
        c.request(9, 0, &[Arg::Obj(10), Arg::Fd(m), Arg::Int(4096)])
            .unwrap();
        c.flush().unwrap();
        // SAFETY: closing our copy after sending.
        unsafe { libc::close(m) };
        assert!(s.fill().unwrap());
        let ev = s.next_event().unwrap().unwrap();
        assert_eq!((ev.obj, ev.op), (7, 3));
        let mut r = s.args(&ev);
        assert_eq!(r.uint(), 5);
        assert_eq!(r.str(), b"wl_shm");
        assert_eq!(r.int(), -2);
        let ev = s.next_event().unwrap().unwrap();
        assert_eq!((ev.obj, ev.op), (9, 0));
        let mut r = s.args(&ev);
        assert_eq!((r.uint(), r.int()), (10, 4096)); // the fd is out of band
        assert!(s.take_fd().is_some());
        assert!(s.next_event().unwrap().is_none());
        // delete_id from the "server" frees the id for reuse; error becomes Err.
        let id = c.new_id();
        s.request(DISPLAY, 1, &[Arg::Uint(id)]).unwrap();
        s.flush().unwrap();
        assert!(c.fill().unwrap());
        assert!(c.next_event().unwrap().is_none());
        assert_eq!(c.new_id(), id);
        s.request(DISPLAY, 0, &[Arg::Obj(3), Arg::Uint(1), Arg::Str(b"boom")])
            .unwrap();
        s.flush().unwrap();
        assert!(c.fill().unwrap());
        let e = c.next_event().unwrap_err().to_string();
        assert!(e.contains("boom"), "{e}");
    }
}
