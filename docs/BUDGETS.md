# BUDGETS — targets, how to measure, results

Targets are for **gowl's own process** unless stated. "Ceiling" = must not exceed;
"target" = what we're aiming for. If a target is impossible on this machine, record the
evidence and the best achieved number in `DECISIONS.md` — don't silently lower it.

## Scenarios
- **S1 screen-busy:** `gowl screen`, 1 device, 10 zones, human's main output, fps 30,
  a 60 fps video playing fullscreen (e.g. `mpv --loop` of any clip).
- **S2 screen-static:** same, idle desktop, nothing animating, cursor still.
- **S3 music:** `gowl music`, music playing, defaults.
- **S4 paused:** `gowl screen` after `gowl pause`.
- **S0 baseline:** nothing running (for niri's CPU delta).

## Targets
| Metric | S1 | S2 | S3 | S4 |
|---|---|---|---|---|
| CPU, % of one core (avg over 60 s) | target ≤ 0.5, ceiling 1.5 | ≤ 0.05 | target ≤ 0.7, ceiling 1.5 | 0 |
| Wakeups/s (ctx switches, `tools/measure.py`) | ≤ 2·fps | ≤ 1 + 1000/keepalive_ms | ≤ 50 | 0 |
| RSS (VmRSS) | target ≤ 6 MB, ceiling 10 MB | same | target ≤ 8 MB, ceiling 12 MB | same as S1 |
| Heap allocations after warm-up | 0 | 0 | 0 | 0 |
| UDP datagrams/s per device | ≤ min(fps, 40) | = 1000/keepalive_ms | ≤ min(music_fps, 40) | 0 |
| Bytes per datagram (10 seg) | 92 | 92 | 92 | — |
| Wakeups from netlink on a stable network | 0 | 0 | 0 | 0 |
| niri CPU delta vs S0 | record; target ≤ 3% of a core at 1440p30 | ≈ 0 | — | 0 |

Other:
- Stripped release binary ≤ 1.5 MB (target ≤ 800 KB).
- `gowl screen` start → first datagram ≤ 150 ms. One-shot commands ≤ 20 ms wall.
- Frame ready → `sendto` returns: p99 ≤ 1 ms (debug feature timestamps; off in release).
- Network: zero datagrams to non-RFC1918/non-link-local addresses; zero DNS lookups; zero datagrams on any
  VPN/tunnel interface (`tools/netns_vpn_test.py --bin`).
- Recovery after VPN up/down or Wi-Fi roam: sync resumes ≤ 1 s, no restart.
- `cargo build --release` from clean ≤ 60 s on the human's machine (dependency diet check).

## How to measure
`python3 tools/measure.py <screen|music|paused|baseline> <seconds> [--fake] [-- gowl args]`
starts gowl, waits out a warm-up, and reads `/proc` directly (no sysstat, no root):
CPU % of one core for gowl **and niri**, context switches/s, VmRSS/VmHWM, thread count,
mapped shared libraries (warns if libpulse is mapped outside music mode), binary size,
and — with `--fake` — datagrams/s and bytes/frame as received by `tools/fake_govee.py`.
It prints a ready-to-paste row for the results log below.
For real devices, count datagrams with `sudo tcpdump -i any -n udp dst port 4003` (ask
first), or run with `GOWL_LOG=debug`, which prints a once-per-second send counter.
Allocation count: `cargo test --release --features count-alloc -- --ignored alloc_steady_state`.
Profiling: `perf record -g ./target/release/gowl screen` (needs `perf`, ask first);
build with `CARGO_PROFILE_RELEASE_DEBUG=true CARGO_PROFILE_RELEASE_STRIP=false` for symbols.

## Results log (append; newest first)
| Date | Commit | Scenario | CPU % | Wakeups/s | RSS MB | Pkts/s | niri Δ CPU % | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-17 | merge 2+3 | screen (busy desktop, 30 fps) | 0.300 | 92.0 | 8.63 | 2.00 | 19.40 | fake=True; merged binary 665,160 B; screen mode maps only ld/libc/libgcc_s/libm (no libpulse) |
| 2026-10-17 | calibrate (D-040) | binary size | | | | | | 702,024 B (+36,864 B: `gowl calibrate` + `layout` key); hot path unchanged |
| 2026-10-17 | 886a006 | S1 screen (busy desktop, 30 fps, 3440x1440) | 0.267 | 92.2 | 8.63 | 2.00 | 19.43 (Δ ≈ 14 vs S0) | fake=True. Desktop never static (animated background/browser; niri full-output damage every frame) → 30 frames/s captured, edge colors barely moved so 2 pkts/s (keepalive). Wakeups 3/frame + keepalive (D-018, over the 2·fps target). RSS = 0.2 anon + 2.9 file + 5.9 shmem fault-around (CAPTURE_NIRI.md). 0 heap allocations/s after warm-up (`--features count-alloc`, live niri). Binary 607,784 B |
| 2026-10-17 | 886a006 | S1 screen `--fps 15` | 0.100 | 47.0 | 8.55 | 2.00 | 12.80 (Δ ≈ 6.5) | fake=True, same busy desktop |
| 2026-10-17 | 886a006 | S0 baseline | — | — | — | — | 5.65 / 6.35 | two runs, same busy desktop, nothing running |
| 2026-10-17 | f9c481d | S1 screen (before D-017 wakeup fix) | 0.300 | 122.5 | 8.61 | 2.00 | 20.10 | fake=True |
| 2026-10-17 | 886a006 | S4-like (SIGUSR1 pause; `gowl pause` is Phase 4) | 0.000 | 0.0 | 9.02 | 0 | — | 10 s window: 0 CPU ticks, 0 context switches |
| 2026-10-17 | — | S2 screen-static | not measured | | | | | the agent could not make the human's desktop static; human to run `python3 tools/measure.py screen 30 --fake` on an idle desktop |
| 2026-10-17 | phase3 review | music allocations | — | — | — | — | — | tests/alloc_music.rs: 0 allocations from the first window (render→send, 130 reaction×palette combos × 400 windows). Not covered: `Capture::read`/`stream_loop` (libpulse peek + memcpy into a fixed buffer; Strings only on error paths) — needs a live sound server |
| 2026-10-17 | 544d0b5 | music, playback paused (silence) | 0.167 | 48.2 | 5.87 | 1.97 | 7.63 abs | fake; keepalive only (gate); 1 thread; binary 587,336 B |
| 2026-10-17 | 35f59eb | **S3** music (flow/rgb, browser playing) | 0.400 | 52.0 (47 voluntary = 1/window + ~4 involuntary preemptions) | 5.95 | 39.77 | 12.33 abs | fake; libpulse non-threaded mainloop (D-030); 1 thread; 92 B/frame; libpulse mapped (music only) |
| 2026-10-17 | 6ef38ca | S3 music, pa_simple variant (superseded) | 0.500 | 100.5 | 6.05 | 40.00 | 15.80 abs | fake; 2 threads (pa_simple mainloop thread) → replaced, D-030 |
| 2026-10-17 | phase1 review fixes | — | — | — | — | — | — | release binary 529,960 B after review fixes |
| 2026-10-17 | b5f9b15 | one-shot | — | — | 2.7 (maxrss) | 1 | — | stripped release binary **538,152 B** (target ≤ 800 KB); deps: libc only; `gowl color` / `status` to fake device < 10 ms wall (`/usr/bin/time` resolution 10 ms) |
| 2026-10-17 | recon | — | — | — | — | — | — | hello-world release binary 296,480 B (release profile from ARCHITECTURE.md) |

## Legacy comparison
Not measured: `legacy/` is never executed (CLAUDE.md rule 1). If the human wants a
comparison table for the README, they can run legacy music sync themselves and paste
the numbers here.
