//! One SHM buffer for screencopy: memfd → mmap (read-only, once) → `wl_shm_pool` →
//! `wl_buffer`. Reallocated only when the frame's format/size/stride changes.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::wire::{Arg, Conn};
use crate::error::Error;

/// Opcodes (wayland.xml).
const SHM_CREATE_POOL: u16 = 0;
const POOL_CREATE_BUFFER: u16 = 0;
const POOL_DESTROY: u16 = 1;
const BUFFER_DESTROY: u16 = 0;

/// A mapped SHM buffer the compositor copies frames into.
pub struct ShmBuffer {
    ptr: *mut u8,
    len: usize,
    pool: u32,
    /// `wl_buffer` object id.
    pub buffer: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl ShmBuffer {
    /// Create memfd, map it and register pool + buffer with the compositor (flushes,
    /// so the fd can be closed right after).
    pub fn new(
        conn: &mut Conn,
        shm: u32,
        format: u32,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<ShmBuffer, Error> {
        let len = stride as usize * height as usize;
        if len == 0 || len > i32::MAX as usize || (stride as usize) < width as usize * 4 {
            return Err(Error::Capture(format!(
                "compositor offered an unusable buffer {width}x{height} stride {stride}"
            )));
        }
        // SAFETY: memfd_create with a static NUL-terminated name.
        let raw = unsafe {
            libc::memfd_create(
                c"gowl-shm".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if raw < 0 {
            return Err(Error::Io("memfd_create", io::Error::last_os_error()));
        }
        // SAFETY: fresh fd we own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: plain syscalls on our fd.
        unsafe {
            if libc::ftruncate(raw, len as libc::off_t) != 0 {
                return Err(Error::Io("ftruncate", io::Error::last_os_error()));
            }
            // The compositor can rely on the size (no SIGBUS from a shrink).
            libc::fcntl(
                raw,
                libc::F_ADD_SEALS,
                libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL,
            );
        }
        // SAFETY: mapping `len` bytes of our memfd, shared, read-only for us.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                raw,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(Error::Io("mmap", io::Error::last_os_error()));
        }
        let pool = conn.new_id();
        let buffer = conn.new_id();
        let me = ShmBuffer {
            ptr: ptr.cast(),
            len,
            pool,
            buffer,
            format,
            width,
            height,
            stride,
        };
        conn.request(
            shm,
            SHM_CREATE_POOL,
            &[
                Arg::Obj(pool),
                Arg::Fd(fd.as_raw_fd()),
                Arg::Int(len as i32),
            ],
        )?;
        conn.request(
            pool,
            POOL_CREATE_BUFFER,
            &[
                Arg::Obj(buffer),
                Arg::Int(0),
                Arg::Int(width as i32),
                Arg::Int(height as i32),
                Arg::Int(stride as i32),
                Arg::Uint(format),
            ],
        )?;
        conn.flush()?; // the fd must be sent before it is closed (drop below)
        drop(fd);
        Ok(me)
    }

    /// The mapped pixels. Only read after `ready`, while no copy is pending.
    pub fn data(&self) -> &[u8] {
        // SAFETY: ptr/len describe a live read-only mapping owned by self; the
        // compositor writes only between copy request and `ready`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn matches(&self, format: u32, width: u32, height: u32, stride: u32) -> bool {
        (self.format, self.width, self.height, self.stride) == (format, width, height, stride)
    }

    /// Tell the compositor to drop buffer and pool; unmap ours. Only call while no
    /// copy into this buffer is pending.
    pub fn destroy(self, conn: &mut Conn) -> Result<(), Error> {
        conn.request(self.buffer, BUFFER_DESTROY, &[])?;
        conn.request(self.pool, POOL_DESTROY, &[])?;
        Ok(()) // Drop unmaps
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        // SAFETY: unmapping the mapping created in `new`, exactly once.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}
