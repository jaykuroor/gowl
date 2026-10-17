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

## D-015 — Phase 1 review fixes: LAN-only addresses, ICMP heuristic (2026-10-17, phase 1 gate, orchestrator)
Context: code review found two ways gowl could address a non-LAN IP (rule 7): `lan_if` returned the
forced interface even when its subnet didn't contain the device; `scan` cached the reply's `ip` field
unchecked. Also the firewall hint (ΔOutDestUnreachs) counted ICMP the kernel sends to itself when ARP
fails, so an offline light could be reported as "firewall".
Decision: `lan_if` must contain the device IP (else exit 5); scan ignores replies from off-subnet senders
and uses the payload `ip` only if it is on the receiving interface's subnet; the hint uses
Δ(OutDestUnreachs − InDestUnreachs) and messages say "likely". Also: zero-size frame guard in
`zone_bounds`, alloc test asserts frames were actually sent.
Evidence: tests 30/30, netns `--bin` PASS, binary 529,960 B.
Revisit-if: devices on a different subnet behind a router are ever needed (they are out of scope).

## D-016 — Hand-written Wayland wire client instead of wayland-client (2026-10-17, phase 2, capture agent)
Context: CLAUDE.md whitelists `wayland-client` + `wayland-protocols(-wlr)`. gowl speaks 8 interfaces
(wl_display/registry/callback/shm/shm_pool/buffer/output, zwlr_screencopy_manager/frame_v1) and
creates one new object per frame (screencopy frames are single-use).
Decision: `src/capture/wire.rs` (~450 lines incl. test): Unix socket, 16 KB receive / 4 KB send
buffers allocated once, SCM_RIGHTS for `create_pool`, id reuse via `delete_id`, events parsed in
place. No dependency added; `cargo tree` stays gowl → libc.
Evidence (measured): scratch crate with wayland-client 0.31.15 + wayland-protocols-wlr 0.3.12 that only
lists globals: 448,032 B stripped, 20 crates incl. rustix (rule 3 says rustix *or* libc), cc,
pkg-config, wayland-sys, a proc-macro scanner (quick-xml), 10 s clean build, max RSS 2.0 MB.
gowl with the full capture path: 529,960 → 607,784 B (+78 KB), clean build of the whole crate 6.5 s.
By construction wayland-client allocates for every new proxy (Arc'd object data) → ≥ 1 heap
allocation per frame, against rule 4; wire.rs allocates nothing per frame (tests/capture_alloc.rs:
200 frames, 0 allocations; live niri with `--features count-alloc`: 0 per second after warm-up).
Revisit-if: gowl needs many more protocols (e.g. DMA-BUF feedback tables, ext-image-copy-capture
sessions) and the hand-written surface becomes a maintenance burden.
Orchestrator sign-off 2026-10-17 (Phase 2 review): approved.

## D-017 — Smoothing on a static screen: timer steps until the EMA is settled (2026-10-17, phase 2, capture agent)
Context: legacy captures and steps the EMA every frame forever; gowl captures only on damage, so
after the last change no frames arrive and the light would freeze mid-fade.
Decision: after each frame-driven step, if `Smoother::settled(target)` (another update would change
nothing — works for any α, including α < 0.5 where the u16 EMA never reaches the target exactly) is
false, a timer step follows. Steps are spaced ≥ max(¾·interval, 25 ms) so frames, whose arrival
jitters by the compositor's render phase, step directly; the fallback timer step waits 1.5 intervals
after a frame-driven step. Keepalive resends the current smoothed colors (legacy `frame_needs_send`).
Evidence: busy screen 30 fps: timer wakeups 52/s → 32/s (30 capture + 2 keepalive); mock e2e test:
static phase sends keepalive only.
Revisit-if: the human sees a visible difference in fade timing vs legacy.

## D-018 — Wakeups: 3 per frame is the screencopy minimum we accept for now (2026-10-17, phase 2, capture agent)
Context: BUDGETS S1 asks ≤ 2·fps wakeups. Each screencopy frame costs a timer wakeup (send
`capture_output`), a Wayland wakeup for `buffer`…`buffer_done` (send `copy_with_damage`) and one for
`ready`. Measured 92/s at 30 fps (3/frame + 2 keepalive), down from 122 (D-017 fix).
Options: speculatively send `copy_with_damage` together with `capture_output` (reusing the previous
offer) → 2/frame. Rejected for now: if the output mode changes in between, niri/wlroots answer with
the fatal protocol error `invalid_buffer` and gowl would die; doing it safely needs reconnect logic.
Decision: keep 3/frame; Phase 5 may add speculation + reconnect if the wakeup budget matters.
Revisit-if: Phase 5 perf pass.

## D-019 — niri pays ~14 % of a core for 30 fps SHM screencopy at 3440x1440 (2026-10-17, phase 2, capture agent)
Context: BUDGETS target "niri CPU delta ≤ 3 % at 1440p30".
Evidence: S0 niri 5.7–6.4 %; S1 (30 fps) niri 19.4–20.1 % → Δ ≈ 14 %; `--fps 15` niri 12.8 % → Δ ≈ 6.5 %.
gowl itself 0.27 % (30 fps) / 0.10 % (15 fps). niri reports full-output damage on every frame (even
byte-identical ones), so it reads back all 19.8 MB per frame.
Decision: not fixed in Phase 2 (functional phase); flagged for Phase 5: DMA-BUF variant
(CAPTURE_NIRI.md), `capture_output_region` per edge band, and/or a lower default fps. Default fps
unchanged (30, D-004) until the human weighs responsiveness vs niri CPU.
Revisit-if: Phase 5.

## D-020 — Screen fps is 1-40, rejected above the device cap (2026-10-17, phase 2 review, capture agent)
Context: `--fps`/`fps =` accepted 1-240 but the pipeline silently capped at 40 Hz (legacy
`max_update_hz`), so `--fps 60` looked accepted and did nothing.
Decision: reject instead of silently capping: `--fps` outside 1-40 is a usage error (exit 2) whose
message names the 40 Hz device cap; config `fps` outside 1-40 warns and keeps the default (like every
other bad config value). Help text, SPEC and config.example say 1-40. `music_fps` is unchanged.
Revisit-if: a device with a higher safe update rate shows up (make the cap per device).

## D-021 — Phase 2 review fixes: flipped transforms, unplug silence, stuck sends (2026-10-17, phase 2 review, capture agent)
Context: code review of the Phase 2 branch.
Decision:
- `Geometry::to_buffer` is now an explicit 8-entry table, the inverse of wlroots `wlr_box_transform` /
  smithay `transform_point_in`; FLIPPED_90 (pure transpose) and FLIPPED_270 (anti-transpose) were
  swapped. The test checks fixed corner values and an independent buffer→logical reference table.
  The earlier "matches grim" claim was wrong for the flipped rotations; the vector test was
  self-consistent and could not catch it.
- Output unplugged = one razer OFF (same as pause; the double OFF is only for exit), then no
  keepalives and no fade steps until it is back (both were still running); replug = razer ON + resend
  + capture. Covered by tests/screen_mock.rs `output_unplug_and_replug` (fails on the old code).
- A send to a compositor that stops reading now waits at most 2 s and aborts as soon as the signalfd
  is readable (Ctrl+C still sends razer OFF and exits 0).
- After `failed` and after a cancel (pause), the next frame counts as fully damaged; a y-inverted frame
  is always fully damaged (the protocol doesn't say whether damage rows are stored or displayed order;
  niri never sets y_invert). "More than 32 rects → everything" is decided by the backend's overflow
  flag; `Sampler::mark` no longer also treats exactly 32 as overflow.
Revisit-if: hardware rotation test (human) disagrees with the table.

## D-030 — Music capture: libpulse's non-threaded mainloop on our thread, not pa_simple (2026-10-17, phase 3, audio agent)
Context: AUDIO.md/CLAUDE.md name `libpulse-simple` (4 symbols). Measured with it (commit 6ef38ca):
`tools/measure.py music 20 --fake` → 100.5 ctx switches/s, 2 threads — pa_simple runs its own
`pa_threaded_mainloop`, so each 21.3 ms fragment wakes two threads (budget S3: ≤ 50/s). Its read also
waits on a condition variable, which a signal cannot interrupt (only the next fragment ends it).
Decision: `dlopen("libpulse.so.0")` (same `pulseaudio-libs` package; libpulse-simple is a thin wrapper
around it and loads it anyway) and drive a non-threaded `pa_mainloop` with `pa_mainloop_iterate(block)`
from the main thread: 18 hand-declared symbols, no callbacks (state polled after each iteration, audio
via `pa_stream_peek`/`pa_stream_drop`). Stream flags: ADJUST_LATENCY only (no auto timing updates).
A signal makes the mainloop's `poll()` return EINTR → loop sees the stop flag. A 1 s `alarm` watchdog
(sendto razer OFF + `_exit` from the handler) covers the race of a signal landing just before `poll()`
while no audio flows. Still `dlopen` only in music mode; no crate added.
Evidence: same scenario → 52.0 ctx/s of which 47 voluntary (= 48000/1024 windows/s, the floor for N=1024
at 48 kHz) + ~4 involuntary preemptions; 1 thread; CPU 0.50 → 0.40 %; RSS 6.05 → 5.95 MB; SIGINT → exit
in 11–21 ms, also while blocked in connect. CLAUDE.md's whitelist says "libpulse-simple"; this is
libpulse proper (no new crate, same package). **Orchestrator sign-off 2026-10-17** (full libpulse via
dlopen, single-threaded mainloop approved against the whitelist).
Revisit-if: wakeups must go below 47/s (option: record at 40960 Hz → exactly 40 windows/s = render cap,
at the cost of 40 Hz bins instead of 46.875 Hz), or integrating netlink into the music loop (then use
`pa_mainloop_set_poll_func` to add our fds to its poll).

## D-031 — FFT: N=1024 real via packed 512-point complex radix-2 (2026-10-17, phase 3, audio agent)
Context: prompt asked to pick packed-real vs plain complex-1024 by measured speed.
Decision: pack `z[n] = x[2n] + i·x[2n+1]`, 512-point iterative radix-2 DIT (bit-reversal and twiddles
tabled once; Hann multiply fused into the bit-reversed load), split to bins 0..512 on demand. Only bins
1..341 are evaluated (bands need ≤ 16 kHz); band RMS uses Σ|X|² (one sqrt per band, not per bin). f32.
Evidence: `cargo test --release -- --ignored bench_real_vs_complex`: window+FFT+341 bins 3.28 µs packed vs
6.13 µs complex-1024. vs naive DFT (f64) on random [-1,1) input: max abs error 6.9e-6 (limit 1e-4).
audio.json spectral_bands (n=1024): worst relative error 4.1e-7. The 1104-sample vectors are not used.
Revisit-if: profiles ever show the FFT (they won't: ~0.015 % of a core at 47 windows/s).

## D-032 — Music pacing: render at min(music_fps, 40 Hz) with a deadline accumulator (2026-10-17, phase 3, audio agent)
Context: windows arrive at 46.875/s; the device cap is 40 Hz. A plain min-gap cap (≥ 25 ms since last
send) would yield only ~23 Hz with 21.3 ms windows. Legacy's loop ran at 1/min(music_fps, 40) = 25 ms
per frame, with renderer/AutoGain per-frame constants tuned for that (AutoGain dt = 1/music_fps, as legacy).
Decision: every window is read (keeps the stream flowing); a window is analysed and rendered only when
`now ≥ next`, then `next += interval` (or `now + interval` after a stall > 1 interval: no bursts). Average
render/send rate = exactly min(music_fps, 40); renderer animation speed and smoothing per frame match legacy's
40 Hz. Dropped windows (~1 in 7) skip FFT work entirely. Legacy music sent every frame; gowl adds the delta
gate + keepalive (rule 7) → silence costs 2 datagrams/s instead of 40.
Evidence: debug counter "47 windows, 40 renders, 40 datagrams"/s with music; measure.py 39.77 dgrams/s;
silent: 1.97 dgrams/s.
Revisit-if: the human sees missed beats (then analyse every window and render the max of the bands seen
since the last render).

## D-033 — Renderer port: f64 + float32 emulation, all reactions and palettes (2026-10-17, phase 3, audio agent)
Context: AUDIO.md suggested f32; rule 8 wants ±1/channel; legacy mixes Python floats (f64) with numpy
float32 inside `bands_to_color`/`_spectral_complexity` (NumPy 2.4, NEP 50 weak scalars, left-to-right sum).
Decision: scalar f64 where legacy uses Python floats, f32 exactly where numpy uses float32, Python `round`
= `round_ties_even`. Ported the required set plus the cheap optional ones — reactions wave, bass_bounce,
chase, twinkle, auto (Auto Director, inner renderer boxed once and reset in place) and the `auto` palette —
since they cost nothing unless selected. `album_art` rejected with a usage error; unknown names are usage
errors (legacy silently fell back to flow).
Evidence: tests/vectors/render.json (87 cases: 13 palettes × flow, 9 reactions × {rgb, rainbow, fire}@10 +
{rgb, mono}@14, 1–3 segment strips, mid-stream reaction switches, 420-frame Auto Director): 89,928 channels,
worst error 0. AutoGain sequence: bit-exact. Music hot path: 0 allocations over 130 reaction×palette
combinations × 400 windows (tests/alloc_music.rs).
Revisit-if: legacy changes its renderer (regenerate vectors).

## D-034 — Music idle: keep the monitor stream active; reopen after 30 s of digital silence (2026-10-17, phase 3, audio agent)
Context: with nothing playing, could the stream sleep? Tested gowl-side only (temporary, uncommitted env
override of the source name/flags; no system audio settings touched) on the idle S/PDIF sink's monitor.
Decision: default stream (inhibits auto-suspend of the monitored sink): silence keeps arriving as zeros, the
renderer fades to black, the gate cuts sends to the keepalive (2/s). PA_STREAM_DONT_INHIBIT_AUTO_SUSPEND
(passive) is NOT used: then the stream does not even become ready while the sink is suspended — zero
wakeups, but also zero keepalives, and the device's razer-starvation timeout is unknown (PROTOCOL.md; Phase 4
measures it), so the light could fall back to its pre-stream state mid-silence. After 1406 consecutive
all-zero windows (30 s) the stream is reopened, so a changed default sink is picked up.
Evidence: idle monitor, default flags: 47 voluntary wakeups/s, 2 dgrams/s, ~0 CPU ticks in 5 s;
measure.py with playback paused: 0.167 % CPU, 48.2 ctx/s, 5.87 MB, 1.97 dgrams/s. Passive flag: 0 wakeups,
stream stuck in CREATING; SIGINT still exits in 11 ms. Could not test passive resume (no playback tool
installed: no pw-play/paplay/aplay).
Revisit-if: Phase 4 knows the razer timeout — then passive + a keepalive deadline in the mainloop poll could
make idle music mode cost only keepalive wakeups.

## D-035 — Two acknowledged rule deviations in music mode (2026-10-17, phase 3 review, audio agent)
Context: the Phase 3 review asked to record where music mode bends CLAUDE.md rules 5 and 6.
Decision / evidence:
(a) Rule 5 (no sleeps in loops): while the sound server is unreachable, `open_stream` retries every 1 s
    with `nanosleep`, which a stop signal interrupts (EINTR; no SA_RESTART), so shutdown stays prompt (measured
    14 ms with pulse unreachable). Costs 1 wakeup/s only during an outage. Gap: no keepalives go out
    while pulse is down, so the device may drop out of razer mode (timeout unknown). Phase 4 should revisit:
    replace the sleep with a timed `poll()` that also drives the keepalive deadline (and netlink).
(b) Rule 6 (integer/fixed-point in hot paths): the renderer and AutoGain use scalar f64 with `sin` (wave),
    `sqrt`, `exp` (setup only), `ln` (f32, complexity), mirroring legacy's Python floats/numpy float32 for
    bit-exact parity (render.json worst error 0 over 89,928 channels; AutoGain exact). Measured
    (`cargo test --release -- --ignored bench_render`): render+AutoGain per frame at 10 segments — flow/rgb
    31 ns, wave/rainbow (sin) 182 ns, auto/auto 132 ns; at ≤ 40 frames/s that is ≤ 7.3 µs/s ≈ 0.0007 % of a
    core, against 0.40 % for the whole process. A fixed-point port would save nothing measurable and lose
    exactness. The FFT is f32 (float is explicitly allowed there).
Revisit-if: (a) Phase 4 integrates timers/netlink into the music loop; (b) never, unless profiles show it.

## D-040 — Calibration records a position per segment (`layout`), not reverse/offset (2026-10-17, orchestrator)
**Context.** First hardware screen test (H6609, HDMI-A-1 3440x1440): sync works but the
picture lands in the wrong places. The three patterns are consistent with a closed loop of
10 segments running clockwise from the left side — 2 left, 2 top, top-right corner, 2
right, 2 bottom, bottom-left corner — while the legacy edge walk at 2.39:1 puts 4 zones on
top, 1 left, 4 bottom, 1 right. No `reverse`/`offset` maps one onto the other (the
per-edge counts differ), so the walk planned for Phase 4 (count + reverse) can't fix it.
The human asked for a calibration mode rather than a hand-made fix.
**Decision.** `gowl calibrate [DEV]` lights one segment red at a time (others dim white),
the human types its position as a numpad cell (7 8 9 / 4 6 / 1 2 3, 5 = whole screen,
0 = off; `n`/`p` change the count, `b` back, `q` quit). At the end the light previews the
result in edge colors; `y` writes `segments` and `layout = …` into the device's section
(byte-preserving; a new `[device ALIAS]` keyed by ip/mac if there is none; new file also
gets `device =`). `color::layout_rects` turns a layout into one rect per segment: corners
are depth-sized squares, segments sharing an edge split it evenly in the order the strip
runs (winding from the sum of turns between consecutive cells; runs that wrap past the
last index stay together), sides leave room for used top/bottom rows. With a layout,
`reverse`/`offset` are ignored; a layout whose length disagrees with `segments` is
ignored with a warning. Music mode is unaffected (renders in segment order).
**Alternatives.** Per-segment free rectangles (too fiddly by keyboard); clock positions
(1-12, more typing, same information for 4 edges + corners); auto-detect with the camera
(out of scope). **Cost.** Setup-only code; the sampler hot path is unchanged (off segments
are zero-sample zones). Binary +36,864 B (665,160 → 702,024 B, mostly the walk UI text and float layout math). Same poll loop shape as screen mode (stdin +
timerfd keepalive + signalfd), razer OFF ×2 on every exit.
