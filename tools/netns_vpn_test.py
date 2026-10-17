#!/usr/bin/env python3
"""Prove that gowl's socket strategy survives a VPN hijacking the routes.

No root, no hardware, no real VPN: re-executes itself inside an unprivileged user+net
namespace (`unshare -rn`), creates two TUN interfaces and reproduces what VPNs do:

  lan0  192.168.50.1/24          the real LAN with the light at 192.168.50.23
  vpn0  10.2.0.2/32              the VPN tunnel
  routes: 0.0.0.0/1 + 128.0.0.0/1 dev vpn0          (OpenVPN/Proton-style full tunnel)
          policy rule 5270 -> table 52:
            192.168.50.0/24 dev vpn0                (Tailscale --accept-routes overlap)
            default dev vpn0                        (Tailscale exit node, no LAN access)
          239.0.0.0/8 + 255.255.255.255 dev vpn0    (multicast/broadcast pulled into the tunnel)

Then it sends every kind of datagram gowl sends (unicast control, multicast scan,
limited + directed broadcast scan) twice: once from a plain socket ("naive") and once
with gowl's strategy (SO_BINDTODEVICE + IP_MULTICAST_IF + IP_MULTICAST_TTL 1, sent by
an unprivileged uid with no capabilities), and reads both TUN fds to see which
interface each packet actually left on.

    python3 tools/netns_vpn_test.py                              # reference sockets
    python3 tools/netns_vpn_test.py --bin target/release/gowl    # also the real binary
    exit 0 = every datagram stayed on the LAN interface

Needs: util-linux `unshare`, iproute2 `ip`, /dev/net/tun, unprivileged user namespaces
(Fedora default). This does NOT emulate firewall-level kill switches (nftables drop
rules) — no socket option can or should bypass those; see docs/NETWORK.md.
"""

from __future__ import annotations

import fcntl
import os
import select
import socket
import struct
import subprocess
import sys

TUNSETIFF = 0x400454CA
IFF_TUN, IFF_NO_PI = 0x0001, 0x1000
SO_BINDTODEVICE = 25
LAN_IF, VPN_IF = "lan0", "vpn0"
LIGHT = "192.168.50.23"

SETUP = f"""
set -e
ip link set lo up
ip tuntap add {LAN_IF} mode tun
ip tuntap add {VPN_IF} mode tun
ip addr add 192.168.50.1/24 dev {LAN_IF}
ip addr add 10.2.0.2/32 dev {VPN_IF}
ip link set {LAN_IF} up multicast on
ip link set {VPN_IF} up multicast on
ip route add 0.0.0.0/1 dev {VPN_IF}
ip route add 128.0.0.0/1 dev {VPN_IF}
ip route add 239.0.0.0/8 dev {VPN_IF}
ip route add 255.255.255.255/32 dev {VPN_IF} table 52
ip route add 192.168.50.0/24 dev {VPN_IF} table 52
ip route add default dev {VPN_IF} table 52
ip rule add pref 5270 lookup 52
"""


def open_tun(name: str) -> int:
    fd = os.open("/dev/net/tun", os.O_RDWR | os.O_NONBLOCK)
    fcntl.ioctl(fd, TUNSETIFF, struct.pack("16sH", name.encode(), IFF_TUN | IFF_NO_PI))
    return fd


def drain(fds: dict[str, int]) -> list[tuple[str, str, int, bytes]]:
    """Return (iface, dst, dport, payload) for every UDP/IPv4 packet seen."""
    seen = []
    while True:
        ready, _, _ = select.select(list(fds.values()), [], [], 0.2)
        if not ready:
            return seen
        for name, fd in fds.items():
            if fd not in ready:
                continue
            while True:
                try:
                    pkt = os.read(fd, 65535)
                except BlockingIOError:
                    break
                if len(pkt) < 28 or pkt[0] >> 4 != 4 or pkt[9] != 17:
                    continue  # skip IPv6 / IGMP / router solicitations
                ihl = (pkt[0] & 0x0F) * 4
                dst = socket.inet_ntoa(pkt[16:20])
                dport = struct.unpack("!H", pkt[ihl + 2: ihl + 4])[0]
                seen.append((name, dst, dport, pkt[ihl + 8:]))


def gowl_socket(lan_if: str, lan_ip: str) -> socket.socket:
    """The strategy docs/NETWORK.md prescribes."""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, SO_BINDTODEVICE, lan_if.encode())
    s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(lan_ip))
    s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)
    return s


def drop_privileges() -> None:
    """Become an unprivileged uid with no capabilities in this namespace, so the test
    proves SO_BINDTODEVICE works without CAP_NET_RAW (Linux >= 5.7)."""
    os.setgroups([])
    os.setgid(65534)
    os.setuid(65534)  # dropping euid 0 clears effective/permitted capabilities
    with open("/proc/self/status") as f:
        caps = next(line for line in f if line.startswith("CapEff")).split()[1]
    assert int(caps, 16) == 0, f"still have capabilities: {caps}"


def check_binary(fds: dict[str, int], binary: str) -> bool:
    """Run real gowl commands inside the namespace; every datagram must leave on lan0."""
    import tempfile
    tmp = tempfile.mkdtemp(prefix="gowl-netns-")
    env = dict(os.environ, GOWL_DEVICE=LIGHT, GOWL_LOG="debug",
               XDG_CONFIG_HOME=tmp, XDG_STATE_HOME=tmp, XDG_RUNTIME_DIR=tmp, HOME=tmp)
    ok = True
    print(f"\nreal binary: {binary}")
    for args in (["color", "ff0000"], ["on"], ["brightness", "40"], ["scan", "--timeout", "300"]):
        drain(fds)
        proc = subprocess.run([binary, *args], env=env, capture_output=True, text=True, timeout=10)
        seen = drain(fds)
        on_vpn = [(d, port) for n, d, port, _ in seen if n == VPN_IF]
        on_lan = [(d, port) for n, d, port, _ in seen if n == LAN_IF]
        good = not on_vpn and bool(on_lan)
        ok &= good
        print(f"  gowl {' '.join(args):24s} exit {proc.returncode}  lan0 {len(on_lan):2d} pkts  "
              f"vpn0 {len(on_vpn):2d} pkts  {'OK' if good else 'FAIL'}")
        if not good:
            print("    stderr:", proc.stderr.strip()[-400:])
    return ok


def child() -> int:
    subprocess.run(["sh", "-c", SETUP], check=True)
    # uid 65534 must be mapped for setuid to succeed; with -r only root is mapped, so
    # run the unprivileged half via a fork that keeps tun fds open.
    fds = {LAN_IF: open_tun(LAN_IF), VPN_IF: open_tun(VPN_IF)}
    cases = [
        ("unicast control → light:4003", (LIGHT, 4003)),
        ("multicast scan → 239.255.255.250:4001", ("239.255.255.250", 4001)),
        ("limited broadcast scan → 255.255.255.255:4001", ("255.255.255.255", 4001)),
        ("directed broadcast scan → 192.168.50.255:4001", ("192.168.50.255", 4001)),
    ]

    results = {}
    for mode in ("naive", "gowl"):
        drain(fds)
        r, w = os.pipe()
        pid = os.fork()
        if pid == 0:
            os.close(r)
            try:
                unpriv = True
                try:
                    drop_privileges()
                except (PermissionError, OSError, AssertionError):
                    unpriv = False  # uid 65534 unmapped: fall back, report it
                if mode == "naive":
                    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
                    s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
                else:
                    s = gowl_socket(LAN_IF, "192.168.50.1")
                for i, (_, addr) in enumerate(cases):
                    try:
                        s.sendto(f"{mode}:{i}".encode(), addr)
                    except OSError as exc:
                        os.write(w, f"ERR {i} {exc}\n".encode())
                os.write(w, f"UNPRIV {int(unpriv)}\n".encode())
            finally:
                os._exit(0)
        os.close(w)
        os.waitpid(pid, 0)
        notes = os.read(r, 4096).decode()
        os.close(r)
        seen = drain(fds)
        for i, (label, _) in enumerate(cases):
            ifaces = sorted({n for n, _, _, p in seen if p == f"{mode}:{i}".encode()})
            results[(mode, label)] = ",".join(ifaces) or "dropped"
        results[(mode, "_notes")] = notes.strip()

    ok = True
    print(f"{'packet':48s} {'naive socket':14s} {'gowl socket':14s}")
    for label, _ in cases:
        naive, gowl = results[("naive", label)], results[("gowl", label)]
        good = gowl == LAN_IF
        ok &= good
        print(f"{label:48s} {naive:14s} {gowl:14s} {'OK' if good else 'FAIL'}")
    for mode in ("naive", "gowl"):
        if results[(mode, "_notes")]:
            print(f"[{mode}] {results[(mode, '_notes')]}")
    binary = os.environ.get("GOWL_NETNS_BIN")
    if binary:
        ok &= check_binary(fds, binary)
    print("RESULT:", "PASS — gowl traffic stays on the LAN interface" if ok else "FAIL")
    return 0 if ok else 1


def main() -> int:
    if os.environ.get("GOWL_NETNS_CHILD") == "1":
        return child()
    env = dict(os.environ, GOWL_NETNS_CHILD="1")
    if "--bin" in sys.argv:
        binary = os.path.abspath(sys.argv[sys.argv.index("--bin") + 1])
        if not os.access(binary, os.X_OK):
            print(f"not executable: {binary}", file=sys.stderr)
            return 2
        env["GOWL_NETNS_BIN"] = binary
    # --map-users/--map-groups give us uid/gid 65534 inside the namespace when the
    # host has subuid ranges; plain -r is the fallback (then the test runs as ns-root).
    for cmd in (
        ["unshare", "--user", "--map-root-user", "--map-users=65534:65534:1",
         "--map-groups=65534:65534:1", "--net", sys.executable, "-I", __file__],
        ["unshare", "--user", "--map-root-user", "--net", sys.executable, "-I", __file__],
    ):
        proc = subprocess.run(cmd, env=env, stderr=subprocess.PIPE, text=True)
        # unshare itself exits 1 when the namespace can't be set up (e.g. newuidmap
        # refusing uid 65534); that is "try the next variant", not a test failure.
        setup_failed = proc.stderr.startswith(("newuidmap:", "newgidmap:", "unshare:"))
        if proc.returncode in (0, 1) and not setup_failed:
            sys.stderr.write(proc.stderr)
            return proc.returncode
    print("could not create a user+net namespace (unshare unavailable?)", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
