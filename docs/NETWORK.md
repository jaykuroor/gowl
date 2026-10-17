# NETWORK — VPN-proof LAN path (Tailscale, ProtonVPN, WireGuard, OpenVPN, …)

**Requirement (from the human):** sync must keep working while any VPN is up —
Tailscale (incl. exit node / `--accept-routes`), ProtonVPN, plain WireGuard/OpenVPN.
The lights are on the LAN; gowl's traffic must never depend on the routing table.

## Why VPNs break LAN lights
gowl doesn't care *how* a VPN is configured; it only cares that one of these happens:
| VPN behaviour | Effect on a naive socket |
|---|---|
| Full tunnel: `0.0.0.0/1` + `128.0.0.0/1` (OpenVPN/Proton style) or default route in a policy table | LAN /24 route usually still wins (more specific) — **but** multicast `239.255.255.250` and limited broadcast follow the tunnel → discovery dies |
| Policy rule before `main` (Tailscale table 52, prio 5270; WireGuard fwmark tables) with a route covering the LAN subnet (`--accept-routes` overlap, exit node without LAN access) | unicast to the light goes **into the tunnel** → control and sync die |
| Firewall kill switch (nftables/iptables drop of non-tunnel traffic) | dropped at the firewall — no socket option can fix this, by design |
| Strict reverse-path filter (`rp_filter=1`) + hijacked LAN route | the light's *replies* (scan/status) are dropped on input; sending still works |

`tools/netns_vpn_test.py` reproduces rows 1–2 in an unprivileged network namespace
(two TUN interfaces, Tailscale-style table 52, Proton-style split default, multicast
and broadcast routes pulled into the tunnel). Result on the scaffold author's machine:

```
packet                                           naive socket   gowl socket
unicast control → light:4003                     vpn0           lan0           OK
multicast scan → 239.255.255.250:4001            vpn0           lan0           OK
limited broadcast scan → 255.255.255.255:4001    vpn0           lan0           OK
directed broadcast scan → 192.168.50.255:4001    lan0           lan0           OK
```

## The strategy (all unprivileged; Fedora kernels ≥ 5.7)
1. **Pick the LAN interface by the light, not by routes.** From `getifaddrs`: the
   interface that is UP, not `IFF_LOOPBACK`, not `IFF_POINTOPOINT` (tun/wg/tailscale/
   proton all are), whose IPv4 subnet **contains the device IP**. Skip `100.64.0.0/10`
   (Tailscale CGNAT) and link names starting `tailscale`, `wg`, `tun`, `proton`, `pvpn`,
   `ipv6leak` as a belt-and-braces check. Tie-break: ARPHRD_ETHER (Wi-Fi/Ethernet) over
   anything else, then lowest ifindex. Config `lan_if = NAME` overrides everything.
   Two narrow fallbacks (D-011): a loopback device IP (the fake device, `127.0.0.1`)
   uses `lo`; and if *only* a point-to-point/TUN link that is not VPN-named and not
   a /32 has the device in its subnet, that link is used (this is also what
   `tools/netns_vpn_test.py`'s TUN-based `lan0` looks like).
2. **`SO_BINDTODEVICE` that interface on every gowl socket** (send and receive). The
   route lookup is then forced through that interface's routes, so policy tables and
   tunnel default routes are skipped. Unprivileged since Linux 5.7 as long as the socket
   isn't already bound to a device → bind once at creation; to change interface, create
   a new socket.
3. **Multicast:** `IP_MULTICAST_IF` = LAN interface address, `IP_MULTICAST_TTL` = 1,
   `IP_MULTICAST_LOOP` = 0.
4. **Broadcast:** send the scan to the **subnet-directed broadcast** (e.g.
   `192.168.1.255`) in addition to `255.255.255.255`; both on the bound socket.
5. **Discovery with no known device:** run steps 2–4 on *every* candidate LAN interface
   (usually one). Never send discovery into a tunnel.
6. **Never touch VPN state.** No `SO_MARK`, no fwmark tricks, no root, no editing routes,
   rules, nftables or VPN settings. Bypassing a kill switch would also be a security
   regression — the VPN owner decides. gowl *diagnoses* instead (see `doctor`).

## React to network changes without polling
Add an `AF_NETLINK`/`NETLINK_ROUTE` socket subscribed to `RTMGRP_LINK |
RTMGRP_IPV4_IFADDR | RTMGRP_IPV4_ROUTE` to the main `poll()` set. Ignore message
content beyond "something changed"; on readable: drain it, arm a 500 ms debounce on the
timerfd, then re-run interface selection. If the chosen interface/address changed (Wi-Fi
roam, VPN up/down, suspend/resume): create new bound sockets, send razer ON again, carry
on. Zero cost while nothing changes (no wakeups). Hand-rolled: just `socket`, `bind`
with `nl_groups`, `recv` into a 8 KB stack buffer.

Send errors (`ENETUNREACH`, `ENODEV`, `EADDRNOTAVAIL`, `ENETDOWN`) → same re-selection,
with back-off 0.25 s → 1 s → 5 s, logged once per state change, not per packet.

**Device IP changed (DHCP):** sync can't notice silently (UDP). Re-verify only on network
change events and on explicit `gowl scan`: unicast `devStatus` to the cached IP first (1
datagram); if no reply in 300 ms, run a scan on the LAN interface and match by MAC.
Recommend a DHCP reservation for the light in the README — it removes this whole class.

## Inbound replies and the firewall (Fedora `public` zone)
Only `scan` and `status` need inbound UDP 4002. Sync, `on/off/color/...` are send-only and
need **no firewall change**. Phase 1 must test, with the port closed, whether *unicast*
status/scan replies pass via conntrack (gowl sends from local port 4002 to the light; if
the light replies from the port we sent to, conntrack treats it as a reply and
firewalld's `public` zone accepts it). Record the result in PROGRESS.md:
- passes → document "no firewall change needed once the light's IP is known"; only the
  very first multicast discovery needs a one-off temporary rule (or the IP from the
  router's client list in config).
- blocked → recommend the **scoped** rule, LAN subnet only, runtime first:
  ```
  sudo firewall-cmd --zone=public --add-rich-rule='rule family="ipv4" source address="192.168.1.0/24" port port="4002" protocol="udp" accept'
  # works? make it survive reboot:
  sudo firewall-cmd --permanent --zone=public --add-rich-rule='rule family="ipv4" source address="192.168.1.0/24" port port="4002" protocol="udp" accept'
  ```
  (substitute the real LAN subnet). Never suggest the unscoped `--add-port=4002/udp`
  as the first option. Never run either command yourself.

### Result on the human's machine (Phase 1, 2026-10-17): **blocked**
Firewall unchanged (zone `public` on enp5s0), light H6609 at 192.168.1.3 (ARP REACHABLE):
- `gowl status` (devStatus → status, sent from :4002 to :4003): **no reply reached gowl**.
- `gowl doctor`'s unicast scan (from :4002 to :4001): **no reply reached gowl**.
- `gowl scan` (multicast + limited + directed broadcast): **nothing found**.
- But the light *does* answer: while gowl waited, the kernel's `Icmp OutDestUnreachs`
  (`/proc/net/snmp`, readable without root) rose by exactly one per query (2 for
  status, 3 for doctor, 2 for the multicast scan; 0 in 1-s idle baselines), and
  `Ip InReceives − InDelivers` rose by the same amount with `Udp NoPorts` = 0, so the
  replies were rejected by netfilter (firewalld rejects with ICMP admin-prohibited)
  before reaching UDP.
- **Reply source port:** not 4003 (else conntrack would have matched the status flow),
  not 4001 (the unicast-scan flow didn't match either), not 4002 (a priming datagram
  from :4002 to light:4002 before the query didn't help). Most likely an ephemeral
  port per reply, which conntrack can never match. It can only be read with a capture
  (`sudo tcpdump -ni enp5s0 udp and host 192.168.1.3`, ask first) or once the rule is in.

So the "passes" case does not apply: **status and scan need the scoped rule above**
(`192.168.1.0/24`, UDP 4002, runtime first). Sync and one-shot control still need
nothing. `gowl doctor` detects this case by itself (ICMP counter delta) and prints
the rule with the real zone and subnet; `gowl status`/`scan` say "the firewall
rejected the reply" instead of a bare "no reply".

## `gowl doctor [DEV]`
One screen of facts, no fixes applied:
- chosen LAN interface, its address/subnet, why it was chosen; other candidates; detected
  tunnel interfaces (names only)
- **route hijack check:** unbound UDP socket `connect()`ed to the device IP →
  `getsockname()`; if the source address isn't on the LAN interface, print
  "your VPN routes the light's IP into <iface>; gowl bypasses this by binding to <lan_if>"
- bound unicast `devStatus` → reply within 500 ms? (yes = LAN path + firewall OK)
- if no reply: distinguish (a) light offline / LAN Control off, (b) inbound blocked
  (firewall zone + `rp_filter` value from `/proc/sys/net/ipv4/conf/<if>/rp_filter`),
  (c) outbound blocked by a kill switch (`sendto` returned `EPERM`/`EACCES`) — print the
  likely VPN setting to enable ("allow LAN connections" / "local network sharing") and
  that gowl will not bypass it.
- firewall zone of the LAN interface: read-only `firewall-cmd --get-zone-of-interface=<if>`
  if the binary exists (spawned once, doctor only — never in sync paths).

## Tests
- `tools/netns_vpn_test.py` must pass on the human's machine (runs unprivileged; uses
  uid 65534 inside the namespace when `newuidmap` exists, proving no capability needed).
- Once the binary exists: `python3 tools/netns_vpn_test.py --bin target/release/gowl`
  runs `gowl color`, `gowl on` and `gowl scan` inside the hijacked namespace and fails if
  any gowl datagram leaves on the VPN interface. Run it in every phase gate from Phase 1 on.
- Human VPN matrix (Phase 4), each with `gowl screen` running + `gowl doctor`:
  1. no VPN  2. ProtonVPN connected (standard kill switch)  3. ProtonVPN with advanced
  kill switch  4. Tailscale up  5. Tailscale with exit node  6. Tailscale `--accept-routes`
  if a peer advertises the LAN subnet  7. VPN connect/disconnect *while* syncing
  (must recover ≤ 1 s, no restart). Record pass/fail + doctor output per row in PROGRESS.md.
