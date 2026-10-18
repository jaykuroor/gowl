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
Result (human, 2026-10-17): 30 vs 40 fps on the H6609 "not too distinguishable" → keep 30.

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

## D-041 — Calibration preview = what the test images will give (2026-10-17, orchestrator)
**Context.** First real calibration (3 segments on top, 2 on the bottom, corner segments):
the D-040 preview painted every corner cell white, so the human read "left is white, should
be red" and didn't save. **Decision.** The review cycles (Enter) through three previews that
reproduce `left_red_right_yellow`, `top_blue_bottom_green` and `quadrants` exactly as screen
sync will: each segment's `layout_rects` rect is area-weighted against the pattern's
quadrants, averaged in linear light. Corners show their half's color, a segment across the
middle shows the blend (orange/teal). The legend now says corner keys are only for a
segment that wraps a corner, and that several segments on one side are split evenly in
strip order (direction detected). Aspect ratio needs no input: rects are fractions of the
captured output, whose size gowl reads from the compositor.

## D-042 — Music mode on one poll(): our pa_poll_func + signalfd, no sigaction/alarm (2026-10-17, phase 4, runtime agent)
**Context.** Phase 3 left music mode outside the runtime contract: SIGINT/TERM/HUP set an
`AtomicBool` (sigaction, no SA_RESTART) with a 1 s `alarm` watchdog for the race before
`poll()`, no SIGUSR1/2, no pidfile, no netlink, and a 1 s `nanosleep` back-off while the
sound server is down (D-035 (a)). **Decision.** libpulse lets the owner of a `pa_mainloop`
replace its poll function (`pa_mainloop_set_poll_func`). Ours (`pulse::hook_poll`) copies
libpulse's pollfds behind up to 4 of ours (signalfd, timerfd, netlink, verify socket) into
one stack array, calls `poll()` once, copies revents back and returns libpulse's own count;
if one of ours fired, `Capture::read` returns `Interrupted` and the loop handles it. Signals
are blocked + signalfd (shared `runtime::Ctl`), so none can be missed and the watchdog and
its static emergency socket table are gone. Pause closes the stream (disconnects from
PipeWire, 0 wakeups), razer OFF; resume = razer ON + reopen. Server down: retry every 1 s on
the timerfd, and keepalives hold the last frame meanwhile (the D-035 gap). **Evidence.**
Live PipeWire, loopback receiver: pause → 1 razer OFF, **0 context switches in 5 s**, resume
→ razer ON + frames, `gowl stop` → razer OFF ×2, exit 0 in 12 ms, pidfile removed; second
`gowl music` → exit 1 "already running"; server unreachable → retries + keepalives, stop in
14 ms. No allocation added to the window path (alloc_music still 0). **Alternatives.**
`pa_mainloop_api->io_new` per fd (callbacks, re-registered on every reopen); a second
thread (rule 5). **Revisit-if.** libpulse ever needs > 28 pollfds (then the hook falls back
to polling libpulse's fds alone, losing ours until the next window — never seen).
**Corrected by D-048:** the evidence above was wrong — the stream never opened (an expired
timerfd aborted every connect; the "frames" were black keepalives). Fixed and re-measured there.

## D-043 — Pidfile + control commands (2026-10-17, phase 4, runtime agent)
`$XDG_RUNTIME_DIR/gowl.pid`, created `O_EXCL`, content `PID\n`. Stale = pid dead
(`kill(pid,0)` → ESRCH) or `/proc/PID/comm` not `gowl` (or this binary's own comm, so a
renamed build works); a stale file is removed only if it still holds the pid just read; an
empty file younger than 1 s is "another gowl starting" (the create→write window). Held by
`screen`, `music` and `calibrate` (both walks): one session drives the lights at a time,
and `gowl stop`/`toggle` end a calibration too. Signals go through a pidfd
(`pidfd_open` + `pidfd_send_signal`), so a pid recycled between the check and the signal is
never hit; `stop` (≤ 1 s) and `toggle` (≤ 500 ms) wait by polling the pidfd — no sleep loop
(fallback to `kill` + 10 ms checks only on kernels < 5.3). `pause|resume|stop` with nothing
running exit 1 ("no gowl sync is running"). `toggle [screen|music] [ARGS]` passes ARGS to
the mode it starts (`gowl toggle music --reaction pulse` works as a keybind). Signals are
blocked (signalfd) *before* the pidfile is claimed, so a `stop` racing startup is queued.
Without XDG_RUNTIME_DIR a sync runs unclaimed with a warning.
**Superseded in part by D-048:** the claim is now an OFD lock on the pidfile, not O_EXCL +
kill/comm guessing.

## D-044 — netwatch: rebind only on interface change or send errors; non-blocking re-verify (2026-10-17, phase 4, runtime agent)
**Decision.** rtnetlink groups `RTMGRP_LINK | RTMGRP_IPV4_IFADDR | RTMGRP_IPV4_ROUTE`; content
ignored; drained into an 8 KB stack buffer (ENOBUFS = "changed"). Debounce: 500 ms after the
last event, capped at 2 s after the first (a flapping link still gets re-checked). Then one
`getifaddrs` + `select_for` per device; a device is rebound only if its interface name,
index or address changed (VPN up/down normally changes neither — the bound socket keeps
working, nothing is sent), it had no LAN interface before, or its last send failed with
ENETUNREACH/ENODEV/EADDRNOTAVAIL/ENETDOWN/EHOSTUNREACH (re-selection then retried at 0.25 →
1 → 5 s, reset on the first good send). Rebind while live: new bound socket, razer ON,
current colors resent; while paused/output-gone: socket only, razer waits for resume. No LAN
interface: keep the old socket, one warning, recover when it is back. Device-IP re-verify
(NETWORK.md): only after a rebind that changed the interface and only for devices with a
known MAC: devStatus to the known IP (reply port bound on the LAN interface, socket in the
poll set), 300 ms, then one scan (multicast + limited + directed broadcast; unicast only on
loopback), 1 s, matched by MAC; a move is followed (razer ON) and written to the device
cache, never to a config `ip =`. On the human's machine firewalld rejects replies, so this
gives up silently after 1.3 s and ≤ 4 datagrams per rebind. **Evidence.**
`tools/netns_vpn_test.py --bin` (new rebind check): LAN address moved lan0 → lan1 mid-stream,
then a /25 VPN route over half the LAN: first lan1 datagram (razer ON) **0.511 s** after the
move, keepalives continue on lan1 after the VPN route, razer OFF ×2 on lan1, **0 packets on
vpn0**. Stable network: `GOWL_LOG=debug gowl screen` prints `netlink 0` every second (11 s
shown), and `measure.py paused 20` (netlink fd in the poll set) = **0.0 context switches/s**.
**Alternatives.** Parsing messages to filter (more code, the debounce + one getifaddrs per
burst is already ~free); `SO_MARK`/fwmark (rule 11). **Revisit-if.** Wi-Fi drivers that
spam RTM_NEWLINK (wireless events) show up as periodic debounce wakeups in the debug line.

## D-045 — One `runtime::Streams` for screen, music and calibrate (2026-10-17, phase 4, runtime agent)
Music's own `Lane` (socket + Sender + gate + reorder + send-error log) duplicated
`runtime::Stream`; rebind, back-off and verify would have had to be written twice. Music now
keeps only per-device render state (`Voice`: renderer, smoother) and pushes through
`Streams::push`, exactly as screen does. `Streams::open(.., use_layout)`: screen honours a
calibrated `layout` (zones already in segment order); music passes false and keeps
`reverse`/`offset` as before (D-040: music renders in segment order). Behaviour changes:
a failed razer ON in music is a warning (like screen), not a fatal error — music now keeps
running and recovers when a kill switch is lifted; send errors are logged once per failing
streak instead of every 5 s. Multi-device: `devices::resolve_many` — no DEV = default
device; the same light named twice (alias + IP) is streamed to once (warning); screen
groups devices by (segment count, layout) for one zone set each (unchanged), music renders
per device from one analysis. Tested: tests/multi_device.rs (two fake_govee.py on 127.0.0.2
/ .3, 10 and 6 segments, one reversed) and tests/alloc_music.rs (two devices, 0 allocs).

## D-046 — Keepalive tuning walk (2026-10-17, phase 4, runtime agent)
`gowl calibrate --keepalive [DEV]`: razer ON, a fixed two-tone frame (first half cyan, rest
magenta — unlike any built-in scene, so a drop-out is obvious), repeated every 0.5, 1, 2, 3,
5, 8 s; after each step "did it stay cyan + magenta the whole time?" (watch ≥ max(10 s, 3
intervals)); razer ON again at each step so every step starts from stream mode; the first
"n" ends it. Recommendation = 70 % of the longest interval held, rounded down to 10 ms
(8 s → 5600, 0.5 s → 350); dropped out at 0.5 s → nothing saved, advice printed. `y` writes
the global `keepalive_ms` with `config::set_global_key` (last global occurrence replaced in
place, else inserted after the last global line, before the first section; every other byte
kept; tests in config.rs and tests/calibrate_cli.rs). Runs on the same `line_loop` as the
segment walk (stdin + timerfd + signalfd + netlink + verify). **Pending hardware:** the
H6609's value goes here once the human has run it (it also unblocks D-034's passive stream).
**Result (human, 2026-10-17):** the H6609 held stream mode at every step up to 8 s (no
drop seen), so `keepalive_ms = 5600` was saved. 8 s is only the longest step tested; the true
limit may be higher. A keepalive every 5.6 s on a static scene is already ≈ 0.18 packets/s,
so longer steps were not added.

## D-047 — Phase 1 review leftovers fixed in Phase 4 (2026-10-17, phase 4, runtime agent)
- `GOWL_PORT_BASE` now only applies to loopback targets (`Ports::for_ip`/`for_iface`): a
  stray variable can never send real LAN traffic to odd ports; tests and the fake device
  are loopback, so nothing else changes.
- Labelled secondary addresses (`enp5s0:1`) map to the device name before
  `SO_BINDTODEVICE`/`if_nametoindex` (`net::base_name`).
- `scan --sweep` pacing: one datagram per millisecond by deadline, no longer early when
  replies arrive; the sweep covers the interface's own subnet clipped to its /24 (a /28
  no longer sweeps off-subnet hosts).
- `doctor` hijack check accepts any address of the LAN interface as "no hijack".
- Fake-device tests fail without python3 unless `GOWL_TEST_NO_PYTHON=1` opts out.
- New tests: status source-IP filter and scan end to end (tests/net_e2e.rs, loopback).
Still open (Phase 5): per-device port override (legacy `Device_Port`).

## D-048 — Phase 4 review fixes (2026-10-17, phase 4 review, runtime agent)
1. **Blocking: `gowl music` never opened its stream on a live PipeWire.** While the stream was
   down the timerfd was armed to `retry_at`, which had always passed by the time the open
   ran; inside `pa_context_connect` the first mainloop iteration hit our poll hook, saw the
   expired timer, flagged it, and the open returned `Interrupted` → retry now → forever (the
   reviewer counted ~880k connections in 0.7 s). My D-042 check missed it: it never looked
   for the "capturing" line and counted keepalive frames as renders. **Fix:** `pulse::Hook`
   has an active-slot mask; `run::open_capture` polls *only the signalfd* during an open
   (timer/netlink are level-triggered and handled right after the open returns), the timer
   is no longer armed to the retry time while opening, and `Interrupted` from an open now
   always means "signal pending". **Test:** tests/music_open.rs drives the real libpulse open
   through the hook against a silent fake server (PULSE_SERVER) with always-ready timer and
   netlink stand-ins: the open waits for the server (fails at ~400 ms when it hangs up) and is
   interrupted only by the signal stand-in; with the mask removed it fails (Interrupted after
   0.27 ms). Plus a unit test of the hook's mask. **Evidence (fixed build, live PipeWire,
   loopback receiver):** "capturing @DEFAULT_MONITOR@" logged at start and after resume;
   streaming idle 0.20–0.40 % CPU, 49–50 context switches/s (= one per 21.3 ms window, as S3);
   pause → razer OFF ×2, **0 context switches in 5 s**; resume → razer ON + frames, stream
   reopened; `gowl stop` → razer OFF ×2, exit in 14 ms; second instance → exit 1.
2. **Pidfile claim race** (two starts both removing a stale file and both winning): the claim
   is now an exclusive open-file-description lock (`F_OFD_SETLK`) held for the session; the
   file only carries the pid. Starts open/create, lock (held → "already running (pid N)",
   exit 1), check the locked inode is still the one at the path (a holder that just exited
   unlinks it → retry), then write the pid. `pause|resume|stop` believe the pid only while
   the file is locked (`F_OFD_GETLK`) and it isn't their own pid, so a stale file — even one
   naming our own pid — is never signalled and `stop` can't SIGTERM itself. Test: 20 rounds of
   8 threads racing `acquire_at` → exactly one winner each round.
3. **Exits without razer OFF:** SIGQUIT joins the signalfd (→ stop). Pause and output-lost now
   send razer OFF ×2 (10 ms apart) like stop, in screen and music. `panic = "abort"` skips
   `shutdown()`, so every `Streams` registers its sockets/addresses in a static table and a
   chained panic hook sends razer OFF ×2 from it before the report (unit-tested on a private
   slot).
4. **No black keepalive before the first frame** (rule 7): `Streams::keepalive` skips devices
   with nothing sent since razer ON, so music no longer sends black while the stream opens.
5. **Config rewrites follow `parse`:** keys after a non-device section are global, a
   malformed header doesn't end a section, CRLF line endings are kept on replaced and
   inserted lines; `write_atomic` writes through a symlinked config (the link stays a link)
   and keeps the file mode. Tests for each.
6. Small: hook poll guards `nfds == 0`; `gowl pause/resume` during calibrate prints that they
   don't apply (q or `gowl stop` ends it); the sweep never falls more than one slot behind, so
   a stall doesn't cause a burst.
**Question from the review — keepalive while the music stream is open.** Before this fix:
nothing kept the light in stream mode if the monitor stopped delivering. Not intended: D-034
keeps the stream *active* precisely so PipeWire keeps delivering (measured 47 windows/s on an
idle monitor), but a suspended/removed sink would stall it. Now the timerfd carries a stall
guard at `last send + keepalive + keepalive/4` while streaming: when windows flow the gate
sends the keepalive first and moves the deadline (timerfd_settime only on a send, no extra
wakeup — idle streaming stayed at 49 ctx/s), when they stop the timer resends the last
rendered frame every 1.25 keepalive intervals (≤ 87.5 % of the hold time once keepalive_ms
is 70 % of it).

## D-049 — Per-device control port override (2026-10-17, phase 5, perf agent)
Last Phase 1 leftover (legacy `Device_Port`/`devicePort`/manual `port`, `connection.get_device_port`).
**Decision.** `port = N` (1-65535) in a `[device ALIAS]` section; `Device::control_port` = the override,
else 4003 (or the loopback `GOWL_PORT_BASE` test port — the explicit override wins over the hook). Used
by every control path: one-shots, `status`, `doctor` (route check + devStatus), sync `Streams` and the
netwatch re-verify devStatus (`Pending.port`). Discovery stays on 4001/4002 (legacy only overrides the
control port). Legacy's "repair a stale auto-discovered 4001" quirk is not needed: gowl never stores a
port from discovery. **Exception:** `scan --sweep` probes *unknown* hosts on 4001 and 4003; it
has no device to take a port from. A light on a non-standard port needs `ip =` in its section (no
discovery needed then). Documented in PROTOCOL.md. **Tests:** config parse (valid / 0 / 70000 → warning), tests/net_e2e.rs
`device_port_override_is_used_by_one_shots` (gowl binary, temp config, receiver on 127.0.0.4).

## D-050 — Capture path for D-019: stay on wlr-screencopy SHM; DMA-BUF needs libgbm sign-off (2026-10-17, phase 5, perf agent)
**Context.** D-019: niri pays ≈ 14 % of a core (Phase 2) for 30 fps SHM screencopy of 3440x1440;
budget ≤ 3 %. Today's desktop was busier (niri S0 27.0–28.2 %) and the Δ reads **≈ 21 %** at 30 fps.
Options from PROGRESS: DMA-BUF, region capture, lower fps, ext-image-copy-capture.
**Evidence** (fake device, 20–60 s windows, S0 re-measured between runs; probes were temporary knobs,
reverted):
- *What niri's cost scales with.* Capture area (`capture_output_region`): full output Δ 19.7–21,
  3440x400 (28 % of the pixels) Δ 20.0, 3440x100 (7 %) Δ 17.4. Readback *volume* explains only ≈ 3.5
  of the ≈ 21 points; the rest is per capture. Capture rate: 1 fps Δ 2.7, 5 → 12.7, 10 → 16, 15 → 17,
  20 → 20, 30 → 21 — strongly saturating, and niri's own context switches fall from 400/s to 285/s as
  the rate rises (it sleeps less: a synchronous render + download per capture). A copy that is
  pending but never completes (static 64x64 / 1000x400 regions on this animated desktop) still costs
  Δ 5.5–7.9 (niri checks damage for the pending copy on every redraw), with gowl at 1.1–1.4 wakeups/s.
- *Region capture:* rejected. The zones at the default depth 0.28 cover ≈ 80 % of the screen (their
  bounding box is the whole output), and the cost barely depends on area anyway.
- *Lower fps:* only ≤ 2 fps gets near the target; 30 → 15 fps saves ≈ 4 points. The human chose 30
  (D-004), so the default stays; it is a human trade-off, listed in the Phase 5 report.
- *ext-image-copy-capture:* not advertised by niri 26.04 (D-006); nothing to measure.
- *DMA-BUF without libgbm (tried):* sealed memfd → `/dev/udmabuf` (the user has an ACL on it) →
  `zwp_linux_dmabuf_v1` v3 (`create`, not `create_immed`, so a refusal is an event) as XR24 LINEAR
  (niri lists LINEAR for XR24/AR24), stride 256-aligned and tight. niri **imported** the buffer
  (`created`) but every copy failed; niri's journal: `[GL] GL_INVALID_OPERATION … EGLImage not
  supported` → `error binding dmabuf: Failed to bind Framebuffer`. NVIDIA 615.71.09's EGL cannot render
  into a foreign system-memory dma-buf. After 3 failed frames gowl fell back to SHM, so the code was
  safe, but it can never help on this machine; reverted (≈ 300 lines not worth carrying untested on
  Mesa). It would likely work on AMD/Intel.
- *DMA-BUF via GBM (not tried, needs libgbm → orchestrator sign-off):* NVIDIA's GBM backend
  (`/usr/lib64/gbm/nvidia-drm_gbm.so`) would allocate a LINEAR BO in VRAM that niri can render into.
  The catch is reading it: 6,423 scattered samples per frame through an uncached BAR mapping (~µs each
  → milliseconds per frame in gowl), or `gbm_bo_map`, which on NVIDIA blits the whole BO to a staging
  copy (the readback moves into gowl). Expected niri saving is the readback + synchronous-wait share,
  which the area probe bounds from below at ≈ 3.5 points and which only a niri profile can size.
**Decision.** Keep wlr-screencopy SHM at 30 fps, now 2 wakeups per frame (D-051). The S1 niri target
(≤ 3 %) is **missed and justified** by the curves above: on this NVIDIA machine even 1 fps costs 2.7 %.
No libgbm added. Before a GBM experiment: the human profiles niri user space (own process,
`perf_event_paranoid = 2`, `ptrace_scope = 0`) with and without `gowl screen` running —
`perf record -e cycles:u -g --call-graph dwarf -p $(pgrep -x niri) -o /tmp/niri-gowl.perf -- sleep 20` —
and `perf report -i /tmp/niri-gowl.perf --no-children --sort dso,sym --stdio | head -60`. If
`glReadPixels`/`map_texture`/memcpy/driver spin-waits dominate, GBM is worth a sign-off request
(measure: niri Δ, gowl CPU and RSS with the BO mapped); if scene rendering dominates, no buffer type
will help and only a lower or adaptive capture rate does.
**Revisit-if.** niri ships ext-image-copy-capture, the GPU/driver changes (Mesa: retry udmabuf — the
reverted patch is described above), or the human accepts a slower idle capture rate.

## D-051 — Screen capture: two wakeups per frame (prepare + held fd) (2026-10-17, phase 5, perf agent)
Supersedes D-018. **Context.** S1 measured 90.7 wakeups/s at 30 fps (3/frame: timer → `capture_output`;
offers → `copy_with_damage`; `ready`), budget ≤ 2·fps. D-018 rejected sending the copy speculatively
(a buffer that no longer matches is the fatal `invalid_buffer` error). **Decision.** On `ready`, if the
next slot is in the future, gowl sends `capture_output` for the *next* frame at once (`Capture::prepare`)
but holds the copy; while a prepared frame waits for its slot the Wayland fd is left out of the poll set
(`Capture::held`), so niri's offers sit in the socket instead of waking us. The timer wakeup then
dispatches everything pending (offers, output events) and sends `copy_with_damage` immediately — the copy
still follows its own `buffer_done`, so no protocol risk. The last frame's damage/flags are reset only
when the next copy is sent, so it stays fully readable while the next frame is negotiated. Output
events are seen up to one frame interval (33 ms) later while held; pause cancels the held frame; a
`failed`/lost output for it is handled at the timer like before. If the offers are not in yet at the
timer (slow compositor), the copy goes out on `buffer_done` as before (3 wakeups that frame).
**Evidence** (same desktop, fake device, 30 s, `measure.py screen 30 --fake`): wakeups 90.7 → **60.5/s**
(debug line: wayland 30 + timer 30 per second); `perf stat` task-clock 107.2 → **90.2 ms per 30 s**
(0.357 → 0.301 % of a core); niri Δ unchanged (≈ 21 %, see D-050). Test: tests/capture_alloc.rs
`prepare_holds_the_copy_and_keeps_the_last_frame` (offers arrive, no copy until `request_next`, last
frame's pixels and damage intact, cancel while held).
**Revisit-if.** A compositor sends something time-critical on the Wayland socket (none of the
interfaces gowl binds has a ping).

## D-052 — Passive music stream (auto-suspend friendly) (2026-10-17, phase 5, perf agent)
Supersedes D-034's "keep the monitor stream active". **Context.** D-034 kept the stream active
because the light's razer timeout was unknown; D-046 measured it: the H6609 holds stream mode for at
least 8 s, keepalive_ms = 5600, and the D-048 stall guard resends every 1.25 × keepalive (7 s) when
windows stop. **Decision.** Record with `PA_STREAM_DONT_INHIBIT_AUTO_SUSPEND`. On a suspended sink
the stream stays CREATING (D-034's observation), so `Pulse::open` returns once the stream is
requested and `Capture::read` waits in the hooked mainloop (signals, timer, netlink stay live; no
`pa_stream_peek` while CREATING — it returns "Bad state"). libpulse times the pending create out
after ~30 s → `ReadError::Expired` → quiet reopen (not a warning, no razer re-ON). No window within
1 s of opening → one black frame (what silence renders to: the light is in stream mode and dark, as
in legacy's idle music mode); keepalives repeat it. No window for 30 s → reopen, so a changed default
sink is still picked up although a suspended sink delivers no zero windows to count (deadline
quantised to whole seconds so windows don't re-arm the timerfd).
**Evidence** (live PipeWire 1.6.9, loopback receiver, nothing playing): `measure.py music 30 --fake`
**0.3 ctx switches/s** (was 47–50), CPU 0.000 %, RSS 6.13 MB, 0.12–0.20 dgrams/s (black + keepalive);
65 s run: reopen every ~30 s, no warnings. Playing 5–6 s of digital silence to the default sink
from a separate client (inaudible) woke the stream: 47–48 windows/s, 40 renders/s; it went quiet
again when that client stopped. Sink running (silent playback) S3-like: 47.6 ctx/s, 0.44 % CPU,
6.26 MB. Pause → stream closed, razer OFF ×2; resume → reopen; stop → exit in 14 ms.
System-wide: the USB DAC and the PipeWire graph can now suspend while `gowl music` idles.
**Corrected by D-054:** the evidence above used a digital-silence client, which hid that a loud
frame stayed frozen for ~31 s when audio stopped abruptly (the passive stream then delivers nothing).
**Not verified here:** reaction to real music (the human: `gowl music`, start/stop a song).
**Revisit-if.** A PipeWire update keeps passive streams from resuming (music would stay dark);
the fix is to drop the flag (one constant in pulse.rs).

## D-053 — Release profile opt-level "s" (2026-10-17, phase 5, perf agent)
ARCHITECTURE.md asked to try `"s"` against `3` and keep whichever measures better on CPU.
**Evidence** (same desktop, fake device, `perf stat` over 20 s of S1, two alternating runs each):
task-clock 62.5 / 62.8 ms (`3`) vs **58.2 / 59.4 ms** (`"s"`), user cycles 53.0M / 53.8M vs 51.1M / 50.7M.
Music benches (`cargo test --release --lib -- --ignored bench`): render+AutoGain 41–43 vs 49 ns/frame
(flow), 191–196 vs 184 ns (wave), 130 vs 171 ns (auto); window+FFT 3.55–3.84 vs 3.24 µs — all noise
at ≤ 47 windows/s (≤ 0.02 % of a core). All 86 tests (vectors included) pass at `"s"`. Binary
**743,016 → 640,616 B**; S1 RSS 8.84 → 8.55 MB (the binary's mapped text 600 → 560 KB).
**Decision.** `opt-level = "s"`. **Revisit-if.** A profile shows a hot loop losing vectorization.
Not tried, with reasons: `target-cpu=native` (user code is ~20 % of gowl's CPU, the rest is kernel time
in poll/recvmsg/sendmsg; and the RPM must stay portable); `MADV_HUGEPAGE` on the SHM buffer
(`shmem_enabled = never` on this kernel, and a huge page would map 2 MB per touched sample → more
RSS); `MAP_POPULATE` (maps all 19.8 MB → RSS up, nothing saved per frame).

## D-054 — Music: black ~1 s after the last window; quiet restarts back off (2026-10-17, phase 5 review, perf agent)
**Bug (review, blocking).** With the passive stream (D-052), PipeWire suspends the sink when a song is
paused or the default sink changes, so windows simply stop. `black_at` was cleared by the first window
and only armed again at a reopen, so the D-048 stall guard kept resending the last *loud* frame until
the 30 s quiet reopen — the light froze for ~31 s. My D-052 check played digital silence, which renders
to black by itself, so it could not show this.
**Fix.** `audio::run::Quiet` (pure, unit-tested) tracks the last window and the last window with sound.
Black is due 1.0–1.5 s after the *last* window (deadline on a 0.5 s grid, so windows arriving 47×/s
don't re-arm the timerfd; while music plays this also replaces the per-send re-arm of the stall
guard as the earliest deadline); `Music::blank` sends black once (the gate skips devices already black)
and resets the smoothers so playback fades in from black like the light. After a rebind / moved
device while quiet and black, black is sent again at once (review item 3: razer ON had reset the gate
and left the light without a frame).
**Quiet restarts (review question c).** A stream that hears no *sound* (no windows, or only digital
silence) for 30 s is restarted to follow a changed default sink (`@DEFAULT_MONITOR@` is resolved when
the stream is created); the interval now doubles 30 → 60 → 120 → 240 → 300 s while it stays quiet and
resets on sound. Restarts — and libpulse's ~30 s expiry of a never-started passive stream, which can't
be lengthened — reuse the existing connection (`Capture::restart`: new stream, same context), so idle
music no longer makes a new pulse connection twice a minute. A sound server that is down is retried
after 1 s, doubling to 30 s (review item 15), reset when connected.
**Evidence.** tests/music_idle.rs: a loud 80 Hz tone through the real `Music` pipeline to a loopback
receiver, then no windows: exactly one black frame 1.0–1.6 s after the last window (fails on the old
code: nothing for 30 s); back-off sequence 30/60/120/240/300 s and reset on sound. Live (PipeWire 1.6.9,
isolated pidfile, fake device): a −50 dBFS 60 Hz tone from a second client lit the light (13 frames),
stopped abruptly → black datagram **1.06 s** later, then only keepalives.

## D-055 — Screen review/audit fixes: memfd freed on pause, syscall trims, held-poll guard (2026-10-17, phase 5 review, perf agent)
- **Capture memfd freed on pause and output lost** (audit 11): `Capture::release` = cancel + destroy
  `wl_buffer`/pool + munmap; the next frame reallocates. gowl's RSS/PSS hide this buffer — niri's PSS
  carries most of it and /proc/meminfo `Shmem` rises by its size. Live: Shmem −19,352 kB on pause,
  +19,416 kB on resume, −19,356 kB on exit; gowl RSS 8.75 → 3.18 MB paused.
- **Syscall trims** (audit 12): `Conn::fill` marks the socket drained after a short read without fds
  (a stream socket returns everything queued) and the dispatch loop stops there instead of reading to
  EAGAIN; the post-`prepare` dispatch is gone (it always found nothing — the offers are read at the
  timer wakeup while the fd is held, so nothing is left unread); `Timer::drain` no longer reads the
  timerfd: every loop re-arms before polling, and the forced `timerfd_settime` (even to "disarmed")
  clears the expiry. Not done: "arm to now + interval at the timer wake so the `ready` re-arm is a
  no-op" — on a static screen (S2) the copy blocks, so the capture slot must not be armed while a copy
  is pending (it would wake every interval for nothing); with the drain change the timer wake has to
  call settime anyway, so the saving would be nil.
- **Held fd only while a capture deadline is armed** (review 2): `held = cap_dl.is_some() &&
  cap.held()`, so paused / output-gone / no-slot states always poll the Wayland fd (errors, hang-up,
  output events never wait).
- **y_invert (review question a).** `flags` belongs to a frame object and is sent after the copy in
  practice (wlroots, niri), but nothing forbids sending it earlier. It is now recorded per frame object
  (`next_y_invert`, reset when a new frame is negotiated) and becomes the frame's `y_invert` at `ready`,
  so the last frame keeps its own flag while the next one is prepared and an early `flags` is never lost.
  Damage stays reset at copy time (damage events only follow a copy).
- **`failed` in the widened gap (review question b).** tests/screen_mock.rs
  `failed_while_held_backs_off_and_recovers`: the mock sends `failed` right after `buffer_done` of the
  prepared frame (before any copy); gowl reads it at the timer wakeup, logs "capture failed; retrying",
  backs off 100 ms, and capture resumes (≥ 20 frames in the next second, "works again after 1 failure").
- **Allocation test drives the production path** (review 4): `capture_alloc` / `alloc_steady_state`
  now go through `prepare` → held → offers read at the "timer" → `request_next`.

## D-056 — Netlink: link changes of unused interfaces are filtered in the kernel (2026-10-17, phase 5 audit, perf agent)
**Context (audit 13).** The idle Wi-Fi `wlp4s0` randomises its MAC; every RTM_NEWLINK woke gowl and
started a debounce + re-selection, even while paused. **Decision.** A classic BPF program on the netlink
socket (`SO_ATTACH_FILTER`) drops RTM_NEWLINK/DELLINK whose `ifi_index` is not one of the interfaces the
devices are bound to; address and route messages always pass (a new LAN address elsewhere may be the
new path). The same check runs in user space (`netwatch::relevant`) before the debounce. The filter is
rebuilt only when a binding changes (`Streams::links_gen`, checked in `Ctl::net_deadline`); while a
device has no LAN interface every link change counts. **Tests:** `relevant` classification; the BPF
program run in the kernel on a unix datagram socket with crafted messages (foreign link change dropped,
bound link / address / route pass). netns `--bin` PASS (rebind 0.51 s). S4 stays at 0 wakeups.

## D-057 — Idle capture rate: 5 fps once the edges stop changing (2026-10-17, phase 5, perf agent)
**Context.** D-050: niri's cost is per capture and the human accepted "up to ~200 ms more latency on the
first reaction after a calm spell" (option 5, yes). **Decision.** `idle_fps` (default 5; 0 or ≥ `fps` =
off) and `idle_after_ms` (default 1000). Each group keeps an *anchor* target; a frame whose target moves
more than `threshold` on any channel from the anchor is a change (slow drifts add up and count too).
After `idle_after_ms` without a change the next capture slot is `1/idle_fps` after the last request;
the first changed frame switches back to `1/fps` for the very next slot. Only the slot length changes —
no extra wakeups, no allocation, keepalive and smoothing untouched. **Tests:** tests/screen_mock.rs
`idle_capture_rate_drops_and_recovers` (mock keeps completing copies with unchanged pixels): 30 → 5 →
30 copies/s, first changed frame within 0.3 s. **Evidence (niri):** see BUDGETS Phase 5 summary.

## D-058 — Smoothing steps that can't send are deferred, not timed (2026-10-17, phase 5 audit, perf agent)
**Context (audit 14).** After a color change the EMA keeps stepping on the timer (D-017) until it is
exactly settled, ~8 wakeups per change whose output the gate never sends. **Decision.** After each step,
if every unsettled group's target is within the threshold of what each of its devices last sent, the
remaining steps can't send: the output moves monotonically from the current output (within the
threshold of the last sent frame, by the gate) toward the target. Then `step_at` is not armed; the
steps run later at their scheduled times (`Pipeline::catch_up`) before the next frame, timer
(keepalive) or live resend, so the smoother state, every pushed frame and every keepalive are the same
as before. Pause applies the steps due before it; resume resends first and lets an overdue step follow
(as the old timer did). **Proof:** tests/smoothing_wire.rs runs a scripted screen (changes, a sub-
threshold change, a change mid-fade, static spells with keepalives, pause/resume, a slow drift)
through the real pipeline with timer steps (`lazy_ok = false`) and with deferral, on a simulated clock,
for smoothing 0.05/0.3/0.55/1.0 × threshold 1/3/8 × keepalive 0/500/5600: every datagram (bytes and
time) identical in all 36 runs, 5,546 timer wakeups saved; forcing deferral unconditionally makes the
test fail. Vectors unchanged (the EMA math is untouched).
