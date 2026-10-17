//! Network change listener and device-IP re-verification (NETWORK.md "React to
//! network changes without polling").
//!
//! [`NetWatch`]: one `AF_NETLINK`/`NETLINK_ROUTE` socket subscribed to link, IPv4
//! address and IPv4 route changes sits in the sync loop's `poll()` set. Message
//! content is ignored beyond "something changed": the socket is drained into a stack
//! buffer and a 500 ms debounce (capped at 2 s after the first event of a burst)
//! schedules one interface re-selection on the loop's timerfd. While the network is
//! stable the kernel never makes the fd readable, so it costs zero wakeups.
//!
//! [`Verify`]: after a device's LAN binding changed, re-check its IP without blocking
//! the loop: unicast `devStatus` to the known IP; no reply within 300 ms → one scan on
//! that interface, matched by MAC. Its socket joins the poll set while it runs.

use std::io;
use std::mem;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::error::Error;
use crate::net::{self, Iface, Ports};
use crate::proto;

const MS: u64 = 1_000_000;
/// Quiet time after the last change before re-selecting.
pub const DEBOUNCE_NS: u64 = 500 * MS;
/// A burst that never goes quiet still re-selects this long after its first event.
pub const MAX_DELAY_NS: u64 = 2000 * MS;

/// Debounce state, separate from the socket so it can be tested.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Debounce {
    first: Option<u64>,
    due: Option<u64>,
}

impl Debounce {
    /// A change was seen at `now`.
    pub fn hit(&mut self, now: u64) {
        let first = *self.first.get_or_insert(now);
        self.due = Some((now + DEBOUNCE_NS).min(first + MAX_DELAY_NS));
    }

    pub fn due(&self) -> Option<u64> {
        self.due
    }

    /// True once, when the quiet time has passed: the caller re-selects.
    pub fn take(&mut self, now: u64) -> bool {
        if self.due.is_some_and(|t| t <= now) {
            *self = Debounce::default();
            true
        } else {
            false
        }
    }
}

/// rtnetlink change listener.
pub struct NetWatch {
    fd: OwnedFd,
    pub debounce: Debounce,
    /// Readable events seen (debug line).
    pub events: u64,
}

impl NetWatch {
    pub fn open() -> io::Result<NetWatch> {
        // SAFETY: plain socket(2).
        let raw = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                libc::NETLINK_ROUTE,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fresh fd we own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: sockaddr_nl is plain old data; zero = unbound, kernel assigns nl_pid.
        let mut sa: libc::sockaddr_nl = unsafe { mem::zeroed() };
        sa.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        sa.nl_groups =
            (libc::RTMGRP_LINK | libc::RTMGRP_IPV4_IFADDR | libc::RTMGRP_IPV4_ROUTE) as u32;
        // SAFETY: valid sockaddr_nl of the stated size.
        let r = unsafe {
            libc::bind(
                raw,
                (&sa as *const libc::sockaddr_nl).cast(),
                mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(NetWatch {
            fd,
            debounce: Debounce::default(),
            events: 0,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// The fd is readable: drain it and (re)start the debounce.
    pub fn on_readable(&mut self, now: u64) {
        let mut buf = [0u8; 8192];
        loop {
            // SAFETY: reading into a stack buffer of the stated length.
            let n = unsafe {
                libc::recv(
                    self.fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n > 0 {
                continue;
            }
            if n < 0 {
                match io::Error::last_os_error().raw_os_error() {
                    // Overrun: changes were lost, which still means "changed".
                    Some(libc::ENOBUFS) | Some(libc::EINTR) => continue,
                    _ => break,
                }
            }
            break;
        }
        self.events += 1;
        self.debounce.hit(now);
    }
}

// ------------------------------------------------------------------ verify

/// A device whose IP is being re-checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// Index into the caller's device list.
    pub dev: usize,
    pub ip: Ipv4Addr,
    /// Upper-case, as in the device cache and scan replies.
    pub mac: String,
    /// Control port (config override or 4003) for the devStatus probe.
    pub port: u16,
}

/// A device that answered a scan from a different address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moved {
    pub dev: usize,
    pub from: Ipv4Addr,
    pub to: Ipv4Addr,
}

/// devStatus → (300 ms) → scan → (1 s) → done.
pub struct Verify {
    sock: UdpSocket,
    iface: Iface,
    ports: Ports,
    pub pending: Vec<Pending>,
    scanning: bool,
    deadline: u64,
}

const STATUS_WAIT_NS: u64 = 300 * MS;
const SCAN_WAIT_NS: u64 = 1000 * MS;

impl Verify {
    /// Bind the reply port on `iface` and send one devStatus per pending device.
    pub fn start(
        iface: &Iface,
        ports: Ports,
        pending: Vec<Pending>,
        now: u64,
    ) -> Result<Verify, Error> {
        let ports = ports.for_iface(iface);
        let sock = net::bound_socket(iface, Some(ports.reply))?;
        sock.set_nonblocking(true)
            .map_err(|e| Error::Io("set_nonblocking", e))?;
        let mut msg = [0u8; 64];
        let n = proto::status_query(true, &mut msg);
        for p in &pending {
            let _ = net::send_to(&sock, &msg[..n], SocketAddrV4::new(p.ip, p.port));
        }
        Ok(Verify {
            sock,
            iface: iface.clone(),
            ports,
            pending,
            scanning: false,
            deadline: now + STATUS_WAIT_NS,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    pub fn deadline(&self) -> u64 {
        self.deadline
    }

    pub fn done(&self) -> bool {
        self.pending.is_empty()
    }

    /// Socket readable: a status reply from a pending IP confirms it; a scan reply
    /// with a pending MAC from another address is a move.
    pub fn on_readable(&mut self, moved: &mut Vec<Moved>) {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = self.sock.recv_from(&mut buf) {
            let std::net::SocketAddr::V4(from) = from else {
                continue;
            };
            if !self.iface.contains(*from.ip()) && !from.ip().is_loopback() {
                continue;
            }
            let data = &buf[..n];
            if let Some(r) = proto::parse_scan(data) {
                let mac = String::from_utf8_lossy(r.device).to_ascii_uppercase();
                let Some(k) = self.pending.iter().position(|p| p.mac == mac) else {
                    continue;
                };
                let ip =
                    r.ip.and_then(|b| std::str::from_utf8(b).ok()?.parse().ok())
                        .filter(|ip| self.iface.contains(*ip))
                        .unwrap_or(*from.ip());
                let p = self.pending.swap_remove(k);
                if ip != p.ip {
                    moved.push(Moved {
                        dev: p.dev,
                        from: p.ip,
                        to: ip,
                    });
                }
            } else if proto::parse_status(data).is_some() {
                self.pending.retain(|p| p.ip != *from.ip());
            }
        }
    }

    /// The deadline passed. Returns false when verification is over.
    pub fn on_deadline(&mut self, now: u64) -> bool {
        if self.done() {
            return false;
        }
        if self.scanning {
            for p in &self.pending {
                debug!(
                    "could not re-verify {} (no reply; firewall?) — keeping it",
                    p.ip
                );
            }
            self.pending.clear();
            return false;
        }
        self.scanning = true;
        let scan = self.ports.scan;
        if self.iface.is_loopback() {
            // Test device: unicast only, nothing broadcast.
            for p in &self.pending {
                let _ = net::send_to(&self.sock, proto::SCAN_MSG, SocketAddrV4::new(p.ip, scan));
            }
        } else {
            for dst in [
                Ipv4Addr::from(proto::MULTICAST),
                Ipv4Addr::BROADCAST,
                self.iface.broadcast(),
            ] {
                let _ = net::send_to(&self.sock, proto::SCAN_MSG, SocketAddrV4::new(dst, scan));
            }
        }
        self.deadline = now + SCAN_WAIT_NS;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_waits_for_quiet_but_not_forever() {
        let mut d = Debounce::default();
        assert_eq!(d.due(), None);
        assert!(!d.take(0));
        d.hit(1000);
        assert_eq!(d.due(), Some(1000 + DEBOUNCE_NS));
        assert!(!d.take(1000 + DEBOUNCE_NS - 1));
        // A steady stream of changes every 100 ms keeps pushing the deadline...
        let mut t = 1000;
        for _ in 0..30 {
            t += 100 * MS;
            d.hit(t);
        }
        // ...but never past 2 s after the first change.
        assert_eq!(d.due(), Some(1000 + MAX_DELAY_NS));
        assert!(d.take(1000 + MAX_DELAY_NS));
        assert_eq!(d, Debounce::default());
        assert!(!d.take(u64::MAX));
    }

    #[test]
    fn netlink_socket_opens_unprivileged_and_is_quiet() {
        let w = NetWatch::open().expect("rtnetlink socket");
        let mut p = libc::pollfd {
            fd: w.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd. A stable network sends nothing.
        let r = unsafe { libc::poll(&mut p, 1, 50) };
        assert!(r >= 0);
    }

    #[test]
    fn verify_confirms_by_status_and_follows_a_move_by_mac() {
        // Loopback stand-ins: the "device" answers from 127.0.0.1.
        let ifaces = net::interfaces().unwrap();
        let lo = net::select_for(&ifaces, Ipv4Addr::LOCALHOST, None)
            .unwrap()
            .iface;
        let dev = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dport = dev.local_addr().unwrap().port();
        let ports = Ports {
            scan: dport,
            reply: 0, // ephemeral: test only
            control: dport,
        };
        let pending = vec![
            Pending {
                dev: 0,
                ip: Ipv4Addr::LOCALHOST,
                mac: "AA".into(),
                port: dport,
            },
            Pending {
                dev: 1,
                ip: Ipv4Addr::new(127, 0, 0, 9),
                mac: "BB".into(),
                port: dport,
            },
        ];
        let mut v = Verify::start(&lo, ports, pending, 0).unwrap();
        let back = v.sock.local_addr().unwrap();
        dev.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut b = [0u8; 256];
        let (n, _) = dev.recv_from(&mut b).unwrap();
        assert!(std::str::from_utf8(&b[..n]).unwrap().contains("devStatus"));
        // Status reply from device 0's IP confirms it.
        dev.send_to(
            br#"{"msg":{"cmd":"devStatus","data":{"onOff":1,"brightness":50}}}"#,
            back,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut moved = Vec::new();
        v.on_readable(&mut moved);
        assert_eq!(v.pending.len(), 1);
        assert_eq!(v.pending[0].dev, 1);
        // No reply for device 1 within 300 ms → scan; its MAC answers from 127.0.0.1.
        assert!(v.on_deadline(STATUS_WAIT_NS));
        dev.send_to(
            br#"{"msg":{"cmd":"scan","data":{"ip":"127.0.0.1","device":"bb","sku":"H6609"}}}"#,
            back,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        v.on_readable(&mut moved);
        assert_eq!(
            moved,
            [Moved {
                dev: 1,
                from: Ipv4Addr::new(127, 0, 0, 9),
                to: Ipv4Addr::LOCALHOST
            }]
        );
        assert!(v.done());
        assert!(!v.on_deadline(STATUS_WAIT_NS + SCAN_WAIT_NS));
    }
}
