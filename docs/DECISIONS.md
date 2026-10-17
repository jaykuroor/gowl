# DECISIONS — append-only

Format: `## D-NNN — title (date, phase, who)` then Context / Decision / Evidence / Revisit-if.

## D-001 — Rust, single crate, single thread (scaffold)
Context: goal is minimum CPU/RAM/network on niri/Fedora; legacy is Python+Qt.
Decision: Rust stable, no async runtime, `poll()` loop, hand-written small utilities.
Revisit-if: never for v1.

## D-002 — Capture via wlr-screencopy, not portal/PipeWire (scaffold)
Context: niri supports wlr-screencopy (cursor exclusion with damage fixed in 26.04) and
portal+PipeWire DMA-BUF casts. ext-image-copy-capture was an open PR as of mid-2026.
Decision: screencopy SHM first (pull-paced, damage-blocking, no D-Bus/portal dialog);
ext-image-copy-capture preferred automatically when advertised; DMA-BUF variant only if
Phase 5 shows niri's readback cost is over budget.
Revisit-if: niri drops screencopy, or measurements show PipeWire is cheaper end-to-end.

## D-003 — Music capture via dlopen'd libpulse-simple (scaffold)
Context: Fedora runs pipewire-pulse; pa_simple is a tiny blocking API; avoids bindgen.
Decision: `@DEFAULT_MONITOR@`, float32 mono 48 kHz, 1024-sample windows, dlopen on demand.
Revisit-if: profiles show pulse overhead > 0.2% CPU.

## D-004 — Default screen fps 30 (legacy 45) (scaffold)
Context: EMA smoothing makes 30 fps visually equivalent for ambient light; fewer copies,
fewer packets. Device cap is ~40 Hz anyway.
Decision: default 30, configurable. Phase 4 asks the human to compare 30 vs 45 by eye.

## D-005 — legacy/ path and git handling (2026-10-17, phase 0, orchestrator)
Context: the LumiSync clone sits directly in `legacy/` (has its own `.git`), not `legacy/LumiSync/`.
Decision: doc references rewritten to `legacy/`; `legacy/` is git-ignored (default from
`prompts/00_recon.md`; it is a separate repo at upstream commit 92e1fce).
Evidence: `legacy/lumisync/connection.py` exists; regenerated vectors are byte-identical to the committed ones.
Revisit-if: the human wants legacy vendored into this repo.

## D-006 — niri 26.04 on this machine has no ext-image-copy-capture (2026-10-17, phase 0, orchestrator)
Context: CAPTURE_NIRI.md prefers ext-image-copy-capture when advertised.
Decision: Phase 2 implements screencopy v3 only; keep the `Capture` trait seam for imagecopy.
Evidence: `wayland-info` lists zwlr_screencopy_manager_v1 v3, no ext_image_copy_capture_manager_v1.
Revisit-if: a niri update advertises it.

## D-007 — VPN-proof LAN path via interface binding (UPDATE_001)
Context: human uses Tailscale and ProtonVPN; on Windows, Tailscale broke Govee LAN control.
VPNs reroute unicast (policy tables, exit nodes, overlapping subnet routes) and pull
multicast/broadcast discovery into the tunnel.
Decision: choose the LAN interface by the light's subnet; SO_BINDTODEVICE (unprivileged
since Linux 5.7) on every socket, IP_MULTICAST_IF, directed broadcast; rtnetlink-driven
rebind. Never bypass kill switches or touch VPN/firewall state; diagnose via `gowl doctor`.
Evidence: tools/netns_vpn_test.py: naive sockets leak unicast, multicast and limited
broadcast into the fake VPN; bound sockets keep all four datagram kinds on the LAN.
Revisit-if: the Phase 4 VPN matrix shows a VPN that still breaks it.
(Numbered D-005 in UPDATE_001; renumbered because D-005/D-006 were taken in Phase 0.)

## D-008 — Firewall: no unscoped port opening (UPDATE_001)
Context: Fedora `public` zone drops inbound UDP 4002 (scan/status replies only).
Decision: sync and one-shots need no inbound. Test conntrack for unicast replies first;
if a rule is needed, suggest a LAN-subnet-scoped rich rule, runtime before permanent.
(Numbered D-006 in UPDATE_001.)

## D-009 — netns_vpn_test.py: fall back when unshare itself fails (2026-10-17, UPDATE_001, orchestrator)
Context: on this machine newuidmap refuses `--map-users=65534:65534:1` (no subuid range); `unshare`
then exits 1, which the script treated as a test FAIL and never tried its `-r` fallback.
Decision: if the child's stderr starts with `newuidmap:`/`newgidmap:`/`unshare:`, try the next variant.
Evidence: before: exit 1 with only the newuidmap error; after: fallback runs, RESULT PASS.
Revisit-if: the human adds a subuid range (then the unprivileged variant runs and proves no caps).

## D-010 — libc, not rustix (2026-10-17, phase 1, core agent)
Context: CLAUDE.md allows `rustix` or `libc`, not both. Phase 1 needs getifaddrs,
SO_BINDTODEVICE, ip_mreqn/IP_MULTICAST_IF, poll; later phases need memfd, mmap, timerfd, signalfd.
Decision: `libc` with `default-features = false`. It is declarations only (no runtime code),
covers getifaddrs (rustix has no getifaddrs), and matches the hand-written FFI style planned for
libpulse-simple/libgbm. Lib + bin targets in the one crate so integration tests can reach modules.
Evidence: `cargo tree` = gowl → libc 0.2.191 only; stripped release binary 538,152 B.
Revisit-if: unsafe surface grows enough that rustix's safe wrappers would remove real risk.

## D-011 — Interface-selection fallbacks + test port hook (2026-10-17, phase 1, core agent)
Context: NETWORK.md excludes loopback and point-to-point links. But the fake device lives on
127.0.0.1, and `tools/netns_vpn_test.py` models the LAN as a TUN (`lan0`, POINTOPOINT, ARPHRD_NONE).
Decision: (1) loopback device IP → `lo`. (2) If no non-tunnel interface contains the device IP,
accept a P2P link that contains it, is not VPN-named (tailscale/wg/tun/proton/pvpn/ipv6leak), not
CGNAT and not a /32; discovery likewise falls back to such links only when no normal LAN exists.
Real VPNs are still never used: their addresses are /32 or CGNAT, or their names match.
(3) `GOWL_PORT_BASE=N` (test hook, undocumented in --help) shifts ports to N+1..N+3 to match
`fake_govee.py --port-base N`.
Evidence: unit tests `picks_lan_not_vpn`, `tun_lan_fallback_like_netns_test`; netns `--bin` PASS.
Revisit-if: a VPN shows up as a non-/32, oddly named P2P link containing the LAN subnet.

## D-012 — Smoother starts black, as legacy actually does (2026-10-17, phase 1, core agent)
Context: SPEC/ARCHITECTURE said "first frame snaps". Every legacy sync loop constructs
`ColorSmoother(alpha, segment_count)`, which pre-sizes the state to zeros, so the first frames
fade in from black; only a count change snaps. `processing.json` (frame 0 → 140 for 255 at α .55) agrees.
Decision: match legacy + vectors (rule 8): `Smoother::new(alpha, count)` starts at black; count
change snaps. SPEC and ARCHITECTURE corrected.
Revisit-if: the human prefers an instant first frame (one-line change: start empty).

## D-013 — Fixed-point formats: Q16 saturation/brightness/EMA alpha (2026-10-17, phase 1, core agent)
Context: ARCHITECTURE proposed Rec.709 ×256 luma and Q8 factors for saturation and EMA.
Decision: Q16 (×65536) weights and factors with i64 intermediates and round-to-nearest. Same
instruction count on x86-64, and the error vs legacy is 0, not "within 1". The zone average
uses LIN16 (u16) sums → Q8 mean → 255 midpoint thresholds (branchless 8-step search).
Evidence: tests/vectors.rs: worst channel error 0 for all 12 zone cases, saturation and
brightness; smoother and resample within ±1.
Revisit-if: Phase 5 profiling shows these per-zone ops matter (they run 10×/frame).

## D-014 — Firewall: unicast replies do NOT pass conntrack; detect with ICMP counters (2026-10-17, phase 1, core agent)
Context: NETWORK.md asked whether bound unicast status replies pass the `public` zone via
conntrack. Rule: no firewall changes, no root.
Decision: result is "blocked": the H6609 replies to status *and* unicast scan from a port that
doesn't match the flow (not 4001/4002/4003, probably ephemeral), so conntrack can't help. Recommend
the LAN-scoped rich rule (runtime, then permanent) for `status`/`scan` only. To make this
self-diagnosing without root, doctor/status/scan compare `/proc/net/snmp` `Icmp OutDestUnreachs`
before/after waiting: a rise means the light answered and the firewall rejected it.
Evidence: +2 ICMP unreachables per `gowl status` (3 runs), +3 per `gowl doctor` (3 runs), +2 for
one multicast `gowl scan`, 0 in 1-s idle baselines; `Udp NoPorts` 0; priming :4001/:4002 didn't help.
Also: `gowl scan` additionally unicasts the scan to known IPs (cache/config/GOWL_DEVICE; one
datagram each) and merges results into the cache instead of overwriting it (a scan that misses a
device keeps its entry). Compact JSON on the wire (PROTOCOL.md allows it): legacy strings minus
json.dumps spaces, byte-equal.
Revisit-if: the human captures the reply port (tcpdump) and it turns out to be fixed.
