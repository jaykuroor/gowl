//! UDP to Govee devices over the LAN interface only (docs/NETWORK.md).
//!
//! Every socket here is `SO_BINDTODEVICE`-bound to the interface whose subnet holds
//! the light, so VPN policy tables, exit nodes and tunnel default routes cannot pull
//! gowl's traffic into a tunnel. Port 4002 is bound only during `scan`/`status`.
//! No DNS, nothing outside the LAN.

use std::ffi::CStr;
use std::io;
use std::mem;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use crate::color::Rgb;
use crate::error::Error;
use crate::proto::{self, MAX_FRAME, MSG_MAX};

// ------------------------------------------------------------------ ports

/// Destination/listen ports. `GOWL_PORT_BASE=N` shifts them to N+1/N+2/N+3 — a test
/// hook matching `tools/fake_govee.py --port-base N`; real devices use 4001-4003.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ports {
    pub scan: u16,
    pub reply: u16,
    pub control: u16,
}

impl Default for Ports {
    fn default() -> Self {
        Ports {
            scan: proto::SCAN_PORT,
            reply: proto::REPLY_PORT,
            control: proto::CONTROL_PORT,
        }
    }
}

impl Ports {
    pub fn from_env() -> Ports {
        match std::env::var("GOWL_PORT_BASE")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
        {
            Some(b) if b <= 65532 => Ports {
                scan: b + 1,
                reply: b + 2,
                control: b + 3,
            },
            _ => Ports::default(),
        }
    }
}

// ------------------------------------------------------------------ interfaces

const ARPHRD_ETHER: u16 = 1;
const VPN_NAME_PREFIXES: &[&str] = &["tailscale", "wg", "tun", "proton", "pvpn", "ipv6leak"];

/// One IPv4 address on one interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Iface {
    pub name: String,
    pub index: u32,
    pub addr: Ipv4Addr,
    pub prefix: u8,
    pub flags: u32,
    /// ARPHRD_* link type from the AF_PACKET entry, if any.
    pub hw_type: Option<u16>,
}

impl Iface {
    pub fn mask(&self) -> u32 {
        if self.prefix == 0 {
            0
        } else {
            u32::MAX << (32 - u32::from(self.prefix.min(32)))
        }
    }
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        (u32::from(ip) ^ u32::from(self.addr)) & self.mask() == 0
    }
    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.addr) | !self.mask())
    }
    pub fn is_up(&self) -> bool {
        self.flags & libc::IFF_UP as u32 != 0
    }
    pub fn is_loopback(&self) -> bool {
        self.flags & libc::IFF_LOOPBACK as u32 != 0
    }
    pub fn is_p2p(&self) -> bool {
        self.flags & libc::IFF_POINTOPOINT as u32 != 0
    }
    pub fn is_ether(&self) -> bool {
        self.hw_type == Some(ARPHRD_ETHER)
    }
    /// Tailscale's CGNAT range 100.64.0.0/10.
    pub fn is_cgnat(&self) -> bool {
        let o = self.addr.octets();
        o[0] == 100 && (o[1] & 0xC0) == 64
    }
    pub fn has_vpn_name(&self) -> bool {
        VPN_NAME_PREFIXES.iter().any(|p| self.name.starts_with(p))
    }
    /// Looks like a VPN/tunnel: point-to-point, VPN-ish name or CGNAT address.
    pub fn is_tunnel_like(&self) -> bool {
        self.is_p2p() || self.has_vpn_name() || self.is_cgnat()
    }
    fn kind(&self) -> &'static str {
        if self.is_loopback() {
            "loopback"
        } else if self.is_ether() {
            "ethernet/wi-fi"
        } else if self.is_p2p() {
            "point-to-point"
        } else {
            "other"
        }
    }
}

impl std::fmt::Display for Iface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}/{}", self.name, self.addr, self.prefix)
    }
}

/// All IPv4 interface addresses (`getifaddrs`).
pub fn interfaces() -> io::Result<Vec<Iface>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list we free below.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut out: Vec<Iface> = Vec::new();
    let mut hw: Vec<(String, u16)> = Vec::new();
    let mut p = head;
    while !p.is_null() {
        // SAFETY: p is a valid node of the list returned by getifaddrs.
        let ifa = unsafe { &*p };
        p = ifa.ifa_next;
        if ifa.ifa_addr.is_null() || ifa.ifa_name.is_null() {
            continue;
        }
        // SAFETY: ifa_name is a NUL-terminated string owned by the list.
        let cname = unsafe { CStr::from_ptr(ifa.ifa_name) };
        let name = cname.to_string_lossy().into_owned();
        // SAFETY: ifa_addr is non-null and points at a sockaddr of `sa_family`.
        let family = i32::from(unsafe { (*ifa.ifa_addr).sa_family });
        if family == libc::AF_INET {
            // SAFETY: AF_INET → sockaddr_in.
            let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
            let prefix = if ifa.ifa_netmask.is_null() {
                32
            } else {
                // SAFETY: the netmask of an AF_INET entry is a sockaddr_in.
                let m = unsafe { &*(ifa.ifa_netmask as *const libc::sockaddr_in) };
                u32::from_be(m.sin_addr.s_addr).count_ones() as u8
            };
            // SAFETY: cname is a valid C string.
            let index = unsafe { libc::if_nametoindex(cname.as_ptr()) };
            out.push(Iface {
                name,
                index,
                addr: Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)),
                prefix,
                flags: ifa.ifa_flags,
                hw_type: None,
            });
        } else if family == libc::AF_PACKET {
            // SAFETY: AF_PACKET → sockaddr_ll.
            let sll = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_ll) };
            hw.push((name, sll.sll_hatype));
        }
    }
    // SAFETY: head came from getifaddrs and is freed exactly once.
    unsafe { libc::freeifaddrs(head) };
    for i in &mut out {
        i.hw_type = hw.iter().find(|(n, _)| *n == i.name).map(|&(_, t)| t);
    }
    Ok(out)
}

/// The chosen interface plus a one-line reason (for `doctor` and debug logs).
#[derive(Clone, Debug)]
pub struct Selected {
    pub iface: Iface,
    pub why: String,
}

fn better(a: &Iface, b: &Iface) -> std::cmp::Ordering {
    // Ethernet/Wi-Fi first, then lowest ifindex.
    (!a.is_ether(), a.index).cmp(&(!b.is_ether(), b.index))
}

/// Pick the interface for talking to `ip` (NETWORK.md strategy step 1): config
/// `lan_if` wins; loopback IPs use `lo`; else an UP, non-loopback, non-tunnel
/// interface whose subnet contains the IP. If only a point-to-point/TUN link that
/// is not VPN-named contains it, use that (with a warning in `why`).
pub fn select_for(ifaces: &[Iface], ip: Ipv4Addr, lan_if: Option<&str>) -> Result<Selected, Error> {
    if let Some(name) = lan_if {
        let named: Vec<&Iface> = ifaces.iter().filter(|i| i.name == name).collect();
        if named.is_empty() {
            return Err(Error::NoLanIf(format!(
                "lan_if = {name} from the config has no IPv4 address (is it up?)"
            )));
        }
        // Forcing an interface never lets traffic leave the LAN (CLAUDE.md rule 7):
        // the device must be on that interface's subnet.
        return match named.iter().find(|i| i.contains(ip)) {
            Some(i) => Ok(Selected {
                iface: (*i).clone(),
                why: format!("lan_if = {name} in config"),
            }),
            None => Err(Error::NoLanIf(format!(
                "{ip} is not on the subnet of lan_if = {name}; gowl only talks to devices on the LAN"
            ))),
        };
    }
    if ip.is_loopback()
        && let Some(lo) = ifaces.iter().find(|i| i.is_loopback() && i.contains(ip))
    {
        return Ok(Selected {
            iface: lo.clone(),
            why: "device address is loopback (test device)".into(),
        });
    }
    let mut cands: Vec<&Iface> = ifaces
        .iter()
        .filter(|i| i.is_up() && !i.is_loopback() && !i.is_cgnat() && i.contains(ip))
        .collect();
    cands.sort_by(|a, b| better(a, b));
    if let Some(i) = cands.iter().find(|i| !i.is_tunnel_like()) {
        return Ok(Selected {
            iface: (*i).clone(),
            why: format!(
                "subnet {}/{} contains {ip} ({})",
                Ipv4Addr::from(u32::from(i.addr) & i.mask()),
                i.prefix,
                i.kind()
            ),
        });
    }
    if let Some(i) = cands.iter().find(|i| !i.has_vpn_name() && i.prefix < 32) {
        return Ok(Selected {
            iface: (*i).clone(),
            why: format!(
                "subnet {}/{} contains {ip} (point-to-point link: the only non-VPN interface on that subnet)",
                Ipv4Addr::from(u32::from(i.addr) & i.mask()),
                i.prefix
            ),
        });
    }
    Err(Error::NoLanIf(format!(
        "no LAN interface has {ip} in its subnet (VPN interfaces are never used); set lan_if= in the config to force one"
    )))
}

/// Interfaces to run discovery on when no device is known (NETWORK.md step 5):
/// every UP, non-loopback, non-tunnel interface with a real subnet; if there is
/// none, non-VPN-named point-to-point links.
pub fn discovery_ifaces(ifaces: &[Iface], lan_if: Option<&str>) -> Vec<Iface> {
    if let Some(name) = lan_if {
        return ifaces
            .iter()
            .filter(|i| i.name == name)
            .take(1)
            .cloned()
            .collect();
    }
    let base = |i: &&Iface| i.is_up() && !i.is_loopback() && !i.is_cgnat() && i.prefix < 31;
    let mut v: Vec<Iface> = ifaces
        .iter()
        .filter(base)
        .filter(|i| !i.is_tunnel_like())
        .cloned()
        .collect();
    if v.is_empty() {
        v = ifaces
            .iter()
            .filter(base)
            .filter(|i| !i.has_vpn_name())
            .cloned()
            .collect();
    }
    v.sort_by(better);
    v.dedup_by(|a, b| a.name == b.name);
    v
}

/// Names of interfaces that look like VPN tunnels.
pub fn tunnel_names(ifaces: &[Iface]) -> Vec<String> {
    let mut v: Vec<String> = ifaces
        .iter()
        .filter(|i| i.is_up() && !i.is_loopback() && i.is_tunnel_like())
        .map(|i| i.name.clone())
        .collect();
    v.dedup();
    v
}

// ------------------------------------------------------------------ sockets

fn setopt<T>(fd: RawFd, level: i32, name: i32, val: &T, what: &'static str) -> Result<(), Error> {
    // SAFETY: val points at a T of the given size for the duration of the call.
    let r = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            (val as *const T).cast(),
            mem::size_of::<T>() as libc::socklen_t,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(Error::Io(what, io::Error::last_os_error()))
    }
}

/// UDP socket bound to `iface` (`SO_BINDTODEVICE`), with broadcast allowed and
/// multicast pinned to the interface (TTL 1, no loop). With `bind_port`, also
/// `SO_REUSEADDR|SO_REUSEPORT` and bound to `0.0.0.0:port`; else ephemeral.
pub fn bound_socket(iface: &Iface, bind_port: Option<u16>) -> Result<UdpSocket, Error> {
    // SAFETY: plain socket(2) call.
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(Error::Io("socket", io::Error::last_os_error()));
    }
    // SAFETY: raw is a fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let name = iface.name.as_bytes();
    // SAFETY: name bytes are valid for name.len().
    let r = unsafe {
        libc::setsockopt(
            raw,
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            name.as_ptr().cast(),
            name.len() as libc::socklen_t,
        )
    };
    if r != 0 {
        let e = io::Error::last_os_error();
        return Err(Error::Runtime(format!(
            "cannot bind socket to {} (SO_BINDTODEVICE): {e}; needs Linux >= 5.7",
            iface.name
        )));
    }
    let one: libc::c_int = 1;
    let zero: libc::c_int = 0;
    setopt(
        raw,
        libc::SOL_SOCKET,
        libc::SO_BROADCAST,
        &one,
        "SO_BROADCAST",
    )?;
    let mreqn = libc::ip_mreqn {
        imr_multiaddr: libc::in_addr { s_addr: 0 },
        imr_address: libc::in_addr {
            s_addr: u32::from(iface.addr).to_be(),
        },
        imr_ifindex: iface.index as libc::c_int,
    };
    setopt(
        raw,
        libc::IPPROTO_IP,
        libc::IP_MULTICAST_IF,
        &mreqn,
        "IP_MULTICAST_IF",
    )?;
    setopt(
        raw,
        libc::IPPROTO_IP,
        libc::IP_MULTICAST_TTL,
        &one,
        "IP_MULTICAST_TTL",
    )?;
    setopt(
        raw,
        libc::IPPROTO_IP,
        libc::IP_MULTICAST_LOOP,
        &zero,
        "IP_MULTICAST_LOOP",
    )?;
    if let Some(port) = bind_port {
        setopt(
            raw,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &one,
            "SO_REUSEADDR",
        )?;
        setopt(
            raw,
            libc::SOL_SOCKET,
            libc::SO_REUSEPORT,
            &one,
            "SO_REUSEPORT",
        )?;
        let sin = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr { s_addr: 0 },
            sin_zero: [0; 8],
        };
        // SAFETY: sin is a valid sockaddr_in.
        let r = unsafe {
            libc::bind(
                raw,
                (&sin as *const libc::sockaddr_in).cast(),
                mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if r != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EADDRINUSE) {
                return Err(Error::Runtime(format!(
                    "UDP port {port} is in use by another user's program; close it and retry"
                )));
            }
            return Err(Error::Io("bind reply port", e));
        }
    }
    debug!(
        "socket bound to {} ({}){}",
        iface.name,
        iface.addr,
        bind_port.map_or(String::new(), |p| format!(", listening on :{p}"))
    );
    Ok(UdpSocket::from(fd))
}

/// True for errors a firewall kill switch produces on send.
pub fn is_blocked(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES))
}

/// Send one datagram; logs at debug level.
pub fn send_to(sock: &UdpSocket, msg: &[u8], dst: SocketAddrV4) -> io::Result<()> {
    let r = sock.send_to(msg, dst).map(|_| ());
    debug!(
        "tx {} B to {dst}{}",
        msg.len(),
        r.as_ref()
            .err()
            .map_or(String::new(), |e| format!(" failed: {e}"))
    );
    r
}

/// poll(2) for readability on `fds` up to `timeout_ms`; returns a bitmask of ready fds.
fn poll_readable(pfds: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<u64> {
    for p in pfds.iter_mut() {
        p.revents = 0;
    }
    // SAFETY: pfds is a valid slice of pollfd.
    let r = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout_ms) };
    if r < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            return Ok(0);
        }
        return Err(e);
    }
    Ok(pfds
        .iter()
        .enumerate()
        .filter(|(_, p)| p.revents & libc::POLLIN != 0)
        .fold(0, |m, (i, _)| m | (1 << i)))
}

fn ms_until(deadline: Instant) -> i32 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .min(i32::MAX as u128) as i32
}

// ------------------------------------------------------------------ discovery

/// Where to look during a scan.
#[derive(Clone, Debug)]
pub struct ScanTarget {
    pub iface: Iface,
    /// Multicast + limited + directed broadcast on this interface.
    pub discover: bool,
    /// Extra unicast probes (known device IPs on this interface).
    pub unicast: Vec<Ipv4Addr>,
}

/// One discovered device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub mac: String,
    pub ip: Ipv4Addr,
    pub sku: String,
    pub from: SocketAddrV4,
    pub iface: String,
}

/// Discovery per PROTOCOL.md: bind the reply port on each interface, probe, collect
/// replies until `timeout_ms` after the last probe, dedupe by device id.
/// `sweep` also unicasts to every host of each discovery interface's /24 on the scan
/// and control ports, paced at one datagram per millisecond.
pub fn scan(
    targets: &[ScanTarget],
    ports: Ports,
    timeout_ms: u64,
    sweep: bool,
) -> Result<Vec<Found>, Error> {
    let mut socks = Vec::with_capacity(targets.len());
    for t in targets {
        let s = bound_socket(&t.iface, Some(ports.reply))?;
        s.set_nonblocking(true)
            .map_err(|e| Error::Io("set_nonblocking", e))?;
        socks.push(s);
    }
    let mut blocked = false;
    let mut sent = 0usize;
    let mut fire = |sock: &UdpSocket, dst: SocketAddrV4| match send_to(sock, proto::SCAN_MSG, dst) {
        Ok(()) => sent += 1,
        Err(e) if is_blocked(&e) => blocked = true,
        Err(_) => {}
    };
    for (t, s) in targets.iter().zip(&socks) {
        if t.discover {
            fire(
                s,
                SocketAddrV4::new(Ipv4Addr::from(proto::MULTICAST), ports.scan),
            );
            fire(s, SocketAddrV4::new(Ipv4Addr::BROADCAST, ports.scan));
            if t.iface.prefix <= 30 {
                fire(s, SocketAddrV4::new(t.iface.broadcast(), ports.scan));
            }
        }
        for &ip in &t.unicast {
            fire(s, SocketAddrV4::new(ip, ports.scan));
        }
    }

    // Sweep queue: (socket index, host, port), generated lazily.
    let sweep_list: Vec<(usize, u32)> = if sweep {
        targets
            .iter()
            .enumerate()
            .filter(|(_, t)| t.discover)
            .flat_map(|(si, t)| {
                let own = u32::from(t.iface.addr);
                let base = own & 0xFFFF_FF00;
                (1..255u32)
                    .map(move |h| base | h)
                    .filter(move |&h| h != own)
                    .map(move |h| (si, h))
            })
            .collect()
    } else {
        Vec::new()
    };
    let mut sweep_i = 0usize; // index into sweep_list × 2 ports
    let sweep_total = sweep_list.len() * 2;

    let mut pfds: Vec<libc::pollfd> = socks
        .iter()
        .map(|s| libc::pollfd {
            fd: s.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let mut found: Vec<Found> = Vec::new();
    let mut buf = [0u8; 2048];
    let timeout = Duration::from_millis(timeout_ms);
    let mut deadline = Instant::now() + timeout;
    loop {
        let sweeping = sweep_i < sweep_total;
        let wait = if sweeping { 1 } else { ms_until(deadline) };
        if !sweeping && wait == 0 {
            break;
        }
        let ready = poll_readable(&mut pfds, wait).map_err(|e| Error::Io("poll", e))?;
        if sweeping {
            let (si, host) = sweep_list[sweep_i / 2];
            let port = if sweep_i.is_multiple_of(2) {
                ports.scan
            } else {
                ports.control
            };
            fire(&socks[si], SocketAddrV4::new(Ipv4Addr::from(host), port));
            sweep_i += 1;
            if sweep_i == sweep_total {
                deadline = Instant::now() + timeout;
            }
        }
        for (si, s) in socks.iter().enumerate() {
            if ready & (1 << si) == 0 {
                continue;
            }
            while let Ok((n, from)) = s.recv_from(&mut buf) {
                let std::net::SocketAddr::V4(from) = from else {
                    continue;
                };
                debug!("rx {n} B from {from} on {}", targets[si].iface.name);
                let iface = &targets[si].iface;
                if !iface.contains(*from.ip()) {
                    continue; // not from this LAN: ignore
                }
                let Some(r) = proto::parse_scan(&buf[..n]) else {
                    continue;
                };
                let mac = String::from_utf8_lossy(r.device).to_ascii_uppercase();
                if found.iter().any(|f| f.mac == mac) {
                    continue;
                }
                // The payload's "ip" is only a hint; a LAN host could claim any
                // address. Never cache an address outside this interface's subnet.
                let ip =
                    r.ip.and_then(|b| std::str::from_utf8(b).ok()?.parse().ok())
                        .filter(|ip| iface.contains(*ip))
                        .unwrap_or(*from.ip());
                found.push(Found {
                    mac,
                    ip,
                    sku: String::from_utf8_lossy(r.sku).into_owned(),
                    from,
                    iface: targets[si].iface.name.clone(),
                });
            }
        }
    }
    debug!(
        "scan: sent {sent} probes, {} device(s) answered",
        found.len()
    );
    if blocked && sent == 0 {
        return Err(Error::Runtime(
            "every scan probe was refused (EPERM): a VPN kill switch blocks LAN traffic; allow LAN access in the VPN (gowl will not bypass it)".into(),
        ));
    }
    Ok(found)
}

// ------------------------------------------------------------------ status

/// A status reply and where it came from.
#[derive(Clone, Copy, Debug)]
pub struct StatusReply {
    pub status: proto::Status,
    pub from: SocketAddrV4,
    pub elapsed_ms: u64,
    /// Which query got the answer.
    pub query: &'static str,
}

/// devStatus query (falls back to `status` halfway through `timeout_ms`), from the
/// reply port, accepting only datagrams from `ip`. `Ok(None)` = no reply.
pub fn status(
    iface: &Iface,
    ports: Ports,
    ip: Ipv4Addr,
    control_port: u16,
    timeout_ms: u64,
) -> Result<Option<StatusReply>, Error> {
    let sock = bound_socket(iface, Some(ports.reply))?;
    sock.set_nonblocking(true)
        .map_err(|e| Error::Io("set_nonblocking", e))?;
    let dst = SocketAddrV4::new(ip, control_port);
    let start = Instant::now();
    let half = start + Duration::from_millis(timeout_ms / 2);
    let end = start + Duration::from_millis(timeout_ms);
    let mut msg = [0u8; 64];
    let n = proto::status_query(true, &mut msg);
    send_to(&sock, &msg[..n], dst).map_err(|e| Error::Io("send devStatus", e))?;
    let mut fell_back = false;
    let mut pfd = [libc::pollfd {
        fd: sock.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    let mut buf = [0u8; 2048];
    loop {
        let deadline = if fell_back { end } else { half };
        let wait = ms_until(deadline);
        if wait == 0 {
            if fell_back {
                return Ok(None);
            }
            fell_back = true;
            let n = proto::status_query(false, &mut msg);
            send_to(&sock, &msg[..n], dst).map_err(|e| Error::Io("send status", e))?;
            continue;
        }
        if poll_readable(&mut pfd, wait).map_err(|e| Error::Io("poll", e))? == 0 {
            continue;
        }
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            let std::net::SocketAddr::V4(from) = from else {
                continue;
            };
            debug!("rx {n} B from {from}");
            if *from.ip() != ip {
                continue;
            }
            if let Some(status) = proto::parse_status(&buf[..n]) {
                return Ok(Some(StatusReply {
                    status,
                    from,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    query: if fell_back { "status" } else { "devStatus" },
                }));
            }
        }
    }
}

/// Kernel counters `Icmp: OutDestUnreachs − InDestUnreachs` (/proc/net/snmp,
/// unprivileged). firewalld zones answer unsolicited inbound datagrams with an ICMP
/// unreachable, so a rise while we wait for a reply that never arrives means the
/// reply likely reached this machine and was rejected by the firewall. Unreachables
/// the kernel sends to itself (ARP failure for an offline host) count as both sent
/// and received, so subtracting `In` keeps "light offline" from looking like
/// "firewall". The counters are system-wide, so this is a hint, not proof.
/// Read only on the no-reply path.
pub fn icmp_unreach_sent() -> Option<i64> {
    let text = std::fs::read_to_string("/proc/net/snmp").ok()?;
    let mut lines = text.lines().filter(|l| l.starts_with("Icmp:"));
    let (head, vals) = (lines.next()?, lines.next()?);
    let col = |key: &str| -> Option<i64> {
        let i = head.split_whitespace().position(|k| k == key)?;
        vals.split_whitespace().nth(i)?.parse().ok()
    };
    Some(col("OutDestUnreachs")? - col("InDestUnreachs")?)
}

/// Replies likely rejected since `before` (see [`icmp_unreach_sent`]).
pub fn rejected_since(before: Option<i64>) -> u64 {
    match (before, icmp_unreach_sent()) {
        (Some(a), Some(b)) => (b - a).max(0) as u64,
        _ => 0,
    }
}

/// Source address the kernel's routing table would use for `ip` (unbound socket,
/// `connect` sends nothing). Doctor's route-hijack check only.
pub fn route_source(ip: Ipv4Addr, port: u16) -> io::Result<Ipv4Addr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    s.connect(SocketAddrV4::new(ip, port))?;
    match s.local_addr()? {
        std::net::SocketAddr::V4(a) => Ok(*a.ip()),
        std::net::SocketAddr::V6(_) => Err(io::Error::other("unexpected IPv6 source")),
    }
}

// ------------------------------------------------------------------ streaming

/// Per-device preallocated razer datagram. The JSON prefix/suffix are written once;
/// each frame rewrites only the raw bytes, XOR and the base64 slot (fixed length
/// for a given segment count). No allocation.
pub struct Sender {
    pub dst: SocketAddrV4,
    n: usize,
    raw: [u8; MAX_FRAME],
    dgram: [u8; MSG_MAX],
    len: usize,
}

impl Sender {
    /// `segments` in 1..=255.
    pub fn new(dst: SocketAddrV4, segments: usize) -> Sender {
        let n = segments.clamp(1, proto::MAX_SEGMENTS);
        let mut s = Sender {
            dst,
            n,
            raw: [0; MAX_FRAME],
            dgram: [0; MSG_MAX],
            len: 0,
        };
        let black = [[0u8; 3]; proto::MAX_SEGMENTS];
        let fl = proto::razer_color_frame(&black[..n], &mut s.raw);
        s.len = proto::razer_msg(&s.raw[..fl], &mut s.dgram);
        s
    }

    pub fn segments(&self) -> usize {
        self.n
    }

    /// Encode `colors` (`len == segments()`) into the datagram and send it.
    #[inline]
    pub fn send_colors(&mut self, sock: &UdpSocket, colors: &[Rgb]) -> io::Result<()> {
        debug_assert_eq!(colors.len(), self.n);
        let fl = proto::razer_color_frame(&colors[..self.n], &mut self.raw);
        let p = proto::RAZER_PREFIX.len();
        proto::b64_encode(&self.raw[..fl], &mut self.dgram[p..]);
        sock.send_to(&self.dgram[..self.len], self.dst).map(|_| ())
    }

    /// The current datagram (for tests).
    pub fn datagram(&self) -> &[u8] {
        &self.dgram[..self.len]
    }

    /// Razer mode ON/OFF.
    pub fn razer_mode(&self, sock: &UdpSocket, on: bool) -> io::Result<()> {
        let mut m = [0u8; 64];
        let n = proto::razer_msg(&proto::razer_mode_frame(on), &mut m);
        send_to(sock, &m[..n], self.dst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(
        name: &str,
        addr: [u8; 4],
        prefix: u8,
        flags: i32,
        hw: Option<u16>,
        index: u32,
    ) -> Iface {
        Iface {
            name: name.into(),
            index,
            addr: Ipv4Addr::from(addr),
            prefix,
            flags: (flags | libc::IFF_UP) as u32,
            hw_type: hw,
        }
    }

    fn machine() -> Vec<Iface> {
        vec![
            iface("lo", [127, 0, 0, 1], 8, libc::IFF_LOOPBACK, Some(772), 1),
            iface(
                "enp5s0",
                [192, 168, 1, 179],
                24,
                libc::IFF_BROADCAST,
                Some(1),
                2,
            ),
            iface(
                "tailscale0",
                [100, 101, 1, 2],
                32,
                libc::IFF_POINTOPOINT,
                Some(65534),
                5,
            ),
            // Tailscale --accept-routes doesn't add an address; a WireGuard with an
            // overlapping /24 is name-excluded.
            iface(
                "wg0",
                [192, 168, 1, 250],
                24,
                libc::IFF_POINTOPOINT,
                Some(65534),
                6,
            ),
            iface(
                "proton0",
                [10, 2, 0, 2],
                32,
                libc::IFF_POINTOPOINT,
                Some(65534),
                7,
            ),
        ]
    }

    #[test]
    fn picks_lan_not_vpn() {
        let s = select_for(&machine(), Ipv4Addr::new(192, 168, 1, 3), None).unwrap();
        assert_eq!(s.iface.name, "enp5s0");
        let s = select_for(&machine(), Ipv4Addr::LOCALHOST, None).unwrap();
        assert_eq!(s.iface.name, "lo");
        assert!(matches!(
            select_for(&machine(), Ipv4Addr::new(10, 9, 9, 9), None),
            Err(Error::NoLanIf(_))
        ));
        let s = select_for(&machine(), Ipv4Addr::new(192, 168, 1, 3), Some("enp5s0")).unwrap();
        assert_eq!(s.iface.name, "enp5s0");
        assert!(matches!(
            select_for(&machine(), Ipv4Addr::new(10, 9, 9, 9), Some("enp5s0")),
            Err(Error::NoLanIf(_))
        ));
        let d = discovery_ifaces(&machine(), None);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "enp5s0");
        assert_eq!(tunnel_names(&machine()), ["tailscale0", "wg0", "proton0"]);
    }

    #[test]
    fn tun_lan_fallback_like_netns_test() {
        let v = vec![
            iface(
                "lan0",
                [192, 168, 50, 1],
                24,
                libc::IFF_POINTOPOINT,
                Some(65534),
                2,
            ),
            iface(
                "vpn0",
                [10, 2, 0, 2],
                32,
                libc::IFF_POINTOPOINT,
                Some(65534),
                3,
            ),
        ];
        assert_eq!(
            select_for(&v, Ipv4Addr::new(192, 168, 50, 23), None)
                .unwrap()
                .iface
                .name,
            "lan0"
        );
        let d = discovery_ifaces(&v, None);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "lan0");
        assert_eq!(d[0].broadcast(), Ipv4Addr::new(192, 168, 50, 255));
    }
}
