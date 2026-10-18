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
    /// Link changes only count for these interface indices (`None` = all).
    links: Option<Vec<u32>>,
    /// `Streams::links_gen` the filter was built for (0 = none yet).
    pub links_gen: u64,
}

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
/// `ifinfomsg.ifi_index` offset in a link message (nlmsghdr 16 + family/pad/type 4).
const IFI_INDEX_OFF: usize = 20;

/// Is this datagram's content a change gowl cares about? Link messages only for the
/// interfaces in `links`; address and route messages always (a new LAN address on
/// another interface may be the new path).
pub fn relevant(buf: &[u8], links: Option<&[u32]>) -> bool {
    let Some(links) = links else { return true };
    let mut off = 0usize;
    while off + 16 <= buf.len() {
        let len = u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]) as usize;
        let ty = u16::from_ne_bytes([buf[off + 4], buf[off + 5]]);
        if len < 16 || off + len > buf.len() {
            return true; // malformed: don't risk missing a change
        }
        match ty {
            RTM_NEWLINK | RTM_DELLINK => {
                let i = off + IFI_INDEX_OFF;
                if len < IFI_INDEX_OFF + 4 {
                    return true;
                }
                let idx = u32::from_ne_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
                if links.contains(&idx) {
                    return true;
                }
            }
            _ => return true,
        }
        off += (len + 3) & !3;
    }
    false
}

/// Most interfaces the in-kernel filter lists (classic BPF jump offsets are u8);
/// more → no filter, every link change counts (in user space too).
pub const MAX_FILTER_LINKS: usize = 200;

/// Classic BPF for the netlink socket: drop link messages (RTM_NEWLINK/DELLINK)
/// for interfaces not in `links`, accept everything else. Dropped in the kernel, so
/// e.g. an idle Wi-Fi's MAC randomisation never wakes gowl. Loads are big-endian
/// while netlink is host-endian, hence the byte-swapped constants.
/// `links.len() <= MAX_FILTER_LINKS`.
pub fn link_filter(links: &[u32]) -> Vec<libc::sock_filter> {
    const LD_H_ABS: u16 = 0x28;
    const LD_W_ABS: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const RET_K: u16 = 0x06;
    let ins = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    let be16 = |v: u16| u32::from(u16::from_be_bytes(v.to_ne_bytes()));
    let be32 = |v: u32| u32::from_be_bytes(v.to_ne_bytes());
    assert!(links.len() <= MAX_FILTER_LINKS);
    let n = links.len();
    let mut p = Vec::with_capacity(n + 6);
    p.push(ins(LD_H_ABS, 0, 0, 4)); // 0: nlmsg_type
    p.push(ins(JEQ_K, 1, 0, be16(RTM_NEWLINK))); // 1: → 3
    p.push(ins(JEQ_K, 0, (n + 2) as u8, be16(RTM_DELLINK))); // 2: → 3, else accept
    p.push(ins(LD_W_ABS, 0, 0, IFI_INDEX_OFF as u32)); // 3: ifi_index
    for (i, &l) in links.iter().enumerate() {
        p.push(ins(JEQ_K, (n - i) as u8, 0, be32(l))); // → accept
    }
    p.push(ins(RET_K, 0, 0, 0)); // drop
    p.push(ins(RET_K, 0, 0, 0xffff_ffff)); // accept
    p
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
            links: None,
            links_gen: 0,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Only link changes of these interfaces count (`None` = all): an in-kernel
    /// filter plus the same check here (D-056). Rebind / setup only.
    pub fn set_links(&mut self, links: Option<&[u32]>) {
        // One set for both checks; too many to list → accept every link change.
        let links = links.filter(|l| l.len() <= MAX_FILTER_LINKS);
        let r = match links {
            Some(l) => {
                let prog = link_filter(l);
                let fp = libc::sock_fprog {
                    len: prog.len() as u16,
                    filter: prog.as_ptr().cast_mut(),
                };
                // SAFETY: SO_ATTACH_FILTER copies the program; `prog` outlives the call.
                unsafe {
                    libc::setsockopt(
                        self.fd(),
                        libc::SOL_SOCKET,
                        libc::SO_ATTACH_FILTER,
                        (&fp as *const libc::sock_fprog).cast(),
                        mem::size_of::<libc::sock_fprog>() as libc::socklen_t,
                    )
                }
            }
            // SAFETY: plain setsockopt; ENOENT if no filter was attached (ignored).
            None => unsafe {
                libc::setsockopt(
                    self.fd(),
                    libc::SOL_SOCKET,
                    libc::SO_DETACH_FILTER,
                    std::ptr::null(),
                    0,
                );
                0
            },
        };
        if r != 0 {
            debug!(
                "netlink link filter not attached: {}",
                io::Error::last_os_error()
            );
        }
        debug!("netlink: link changes count for interfaces {links:?}");
        self.links = links.map(<[u32]>::to_vec);
    }

    /// The fd is readable: drain it and (re)start the debounce if any message is a
    /// change gowl cares about (see [`relevant`]).
    pub fn on_readable(&mut self, now: u64) {
        let mut buf = [0u8; 8192];
        let mut hit = false;
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
                hit |= relevant(&buf[..n as usize], self.links.as_deref());
                continue;
            }
            if n < 0 {
                match io::Error::last_os_error().raw_os_error() {
                    // Overrun: changes were lost, which still means "changed".
                    Some(libc::ENOBUFS) => {
                        hit = true;
                        continue;
                    }
                    Some(libc::EINTR) => continue,
                    _ => break,
                }
            }
            break;
        }
        if hit {
            self.events += 1;
            self.debounce.hit(now);
        }
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
                let Some(k) = self
                    .pending
                    .iter()
                    .position(|p| crate::devices::mac_matches(&mac, &p.mac))
                else {
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

    /// One rtnetlink message: nlmsghdr + 16-byte body (ifinfomsg / ifaddrmsg-ish).
    fn nlmsg(ty: u16, index: u32) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&32u32.to_ne_bytes()); // len
        m.extend_from_slice(&ty.to_ne_bytes());
        m.extend_from_slice(&0u16.to_ne_bytes()); // flags
        m.extend_from_slice(&[0; 8]); // seq, pid
        m.extend_from_slice(&[2, 0, 1, 0]); // family, pad, type
        m.extend_from_slice(&index.to_ne_bytes());
        m.extend_from_slice(&[0; 8]); // flags, change
        m
    }

    #[test]
    fn only_link_changes_of_bound_interfaces_count() {
        const NEWADDR: u16 = 20;
        const NEWROUTE: u16 = 24;
        let bound = [2u32, 7];
        assert!(relevant(&nlmsg(RTM_NEWLINK, 2), Some(&bound)));
        assert!(relevant(&nlmsg(RTM_DELLINK, 7), Some(&bound)));
        assert!(
            !relevant(&nlmsg(RTM_NEWLINK, 3), Some(&bound)),
            "idle Wi-Fi"
        );
        assert!(!relevant(&nlmsg(RTM_DELLINK, 3), Some(&bound)));
        assert!(
            relevant(&nlmsg(NEWADDR, 3), Some(&bound)),
            "address: always"
        );
        assert!(relevant(&nlmsg(NEWROUTE, 3), Some(&bound)), "route: always");
        assert!(relevant(&nlmsg(RTM_NEWLINK, 3), None), "no LAN: everything");
        // Two messages in one datagram: one relevant is enough.
        let mut two = nlmsg(RTM_NEWLINK, 3);
        two.extend(nlmsg(RTM_NEWLINK, 7));
        assert!(relevant(&two, Some(&bound)));
        let _ = relevant(&[1, 2, 3], Some(&bound)); // short: no panic
        assert!(relevant(&[0xff; 20], Some(&bound)), "malformed: counts");
    }

    /// The BPF program does in the kernel what `relevant` does here: run it on a
    /// unix datagram socket (socket filters apply there too) with crafted messages.
    #[test]
    fn kernel_filter_drops_foreign_link_changes() {
        let mut sv = [0; 2];
        // SAFETY: socketpair fills two fds we then own.
        let r = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
                sv.as_mut_ptr(),
            )
        };
        assert_eq!(r, 0);
        // SAFETY: fresh fds.
        let (a, b) = unsafe { (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1])) };
        let prog = link_filter(&[2, 7]);
        let fp = libc::sock_fprog {
            len: prog.len() as u16,
            filter: prog.as_ptr().cast_mut(),
        };
        // SAFETY: valid program for the call.
        let r = unsafe {
            libc::setsockopt(
                b.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ATTACH_FILTER,
                (&fp as *const libc::sock_fprog).cast(),
                mem::size_of::<libc::sock_fprog>() as libc::socklen_t,
            )
        };
        assert_eq!(r, 0, "{}", io::Error::last_os_error());
        let pass = |m: Vec<u8>| -> bool {
            // SAFETY: plain send/recv on our sockets and buffers.
            unsafe {
                libc::send(a.as_raw_fd(), m.as_ptr().cast(), m.len(), 0);
                let mut buf = [0u8; 64];
                libc::recv(b.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) > 0
            }
        };
        assert!(pass(nlmsg(RTM_NEWLINK, 2)));
        assert!(pass(nlmsg(RTM_NEWLINK, 7)));
        assert!(!pass(nlmsg(RTM_NEWLINK, 3)), "foreign link change dropped");
        assert!(!pass(nlmsg(RTM_DELLINK, 3)));
        assert!(pass(nlmsg(20, 3)), "address change passes");
        assert!(pass(nlmsg(24, 3)), "route change passes");
    }

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
                // As `gowl setup` stores it from the ARP table: the 6-byte Wi-Fi MAC;
                // the light's scan reply names its 8-byte device id (D-073).
                mac: "60:74:f4:d9:72:f0".into(),
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
            br#"{"msg":{"cmd":"scan","data":{"ip":"127.0.0.1","device":"1A:2B:60:74:F4:D9:72:F0","sku":"H6609"}}}"#,
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
