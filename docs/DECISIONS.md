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
