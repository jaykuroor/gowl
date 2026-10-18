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
Allocation count: `cargo test --release --test capture_alloc -- --ignored alloc_steady_state`
(the test has its own counting allocator; `--features count-alloc` is only for the live debug line).
Frame → sendto latency: `cargo test --release --test latency -- --ignored --nocapture`.
S2 on the gowl side (mock compositor that stops completing copies):
`cargo test --release --test screen_mock -- --ignored s2 --nocapture`.
Precise CPU without tick rounding: `perf stat -e task-clock,context-switches -p $(pgrep -x gowl) -- sleep 30`
(own processes are allowed at `perf_event_paranoid = 2`).
Profiling: `perf record -g ./target/release/gowl screen` (needs `perf`, ask first);
build with `CARGO_PROFILE_RELEASE_DEBUG=true CARGO_PROFILE_RELEASE_STRIP=false` for symbols.

## Phase 5 review + audit round (2026-10-17, branch head after D-054..D-058; fake device, temp config with
keepalive_ms = 5600 + the calibrated layout, isolated pidfile because the human's own `gowl screen` (main build)
was running the whole time — so niri's numbers here include *its* capture too; CPU now from
/proc/PID/task/*/schedstat, exact)

| Scenario | gowl CPU % | Wakeups/s | RSS MB | Shmem | niri | Notes |
|---|---|---|---|---|---|---|
| S1 screen 30 fps, edges static (desktop calm at the time) | 0.008 | 1.3 | 8.61 | — | 22.8 vs S0 17.6–17.9 (Δ ≈ 5: the pending damage copy, D-050) | copies block; keepalive 0.17 dgrams/s |
| S1, desktop animated, idle 5 fps vs off (2 paired rounds, 30 s) | 0.175 / 0.118 vs 0.217 / 0.289 | 39.4 / 28.8 vs 44.7 / 61.8 | — | — | 46.5 / 44.2 vs 46.7 / 52.8 | desktop load changed between runs: inconclusive for niri; human to re-run (commands in the report) |
| S1 later, edges static, idle 5 fps vs off (8 paired rounds, 12 s) | ≈ 0.009 vs ≈ 0.008 | 1.0–2.1 vs 0.7–1.6 | — | — | 22.3–25.4 vs 21.4–23.3 | idle rate can't help when copies block anyway |
| S4 paused 30 s | 0.000 | **0.0** | **3.05** (was 8.4) | capture memfd freed: **−19,352 kB** on pause, +19,416 on resume (screen_pause probe) | = S0 | D-055 |
| S3 music idle (nothing playing), 40 s | 0.002 | **0.2** | 6.08 | — | — | black + keepalive 0.12 dgrams/s; quiet restarts back off (D-054) |
| mock compositor, idle rate | — | — | — | — | — | copies/s 30 → 5 → 30, first changed frame < 0.3 s (`idle_capture_rate_drops_and_recovers`) |
| smoothing deferral (D-058, re-review fix) | — | 5,654 timer wakeups saved over 36 simulated scripts | — | — | — | wire identical in those and in 12,348 down/up sweep runs (`smoothing_wire`) |
| binary (stripped) | — | — | — | — | — | **652,904 B** (+12 KB: idle rate, netlink filter, Quiet, release) |

Corrections to the summary below (audit 16):
- **User vs kernel:** user code ≈ 54 % of gowl's CPU on a busy desktop (auditor, utime/stime), not 20 %.
- **niri's extra cost depends on the desktop:** 13.5–21 points at 30 fps on an animated desktop (auditor and
  me), ≈ 5 while copies are pending on a calm one, 0 paused. The "≈ 21" in the summary is the top of the range.
- **Precise CPU:** `tools/measure.py` now reads /proc/PID/task/*/schedstat (ns) instead of 10 ms ticks, and
  prints system Shmem, where the capture memfd is charged (gowl's RSS/PSS hide most of it; niri's PSS
  carries it). gowl's true memory cost while capturing is RSS + ≈ 19.35 MB of shared capture buffer.
- **libpulse (music):** ≈ 94 calloc+free pairs per second inside libpulse's mainloop/stream code (not gowl
  code; rule 4 covers gowl's hot path, which stays at 0 — tests/alloc_music.rs). Besides libpulse itself, 15 more
  libraries come with it in music mode (libpulsecommon, which pulls in libsndfile and its codecs libFLAC,
  libogg, libvorbis, libvorbisenc, libopus, libmpg123, libmp3lame, libgsm, plus libasyncns, libdbus-1,
  libsystemd, libxcb, libXau) ≈ 2.4 MB RSS / 0.75 MB PSS. Unavoidable with libpulse (they are its NEEDED
  libs); screen mode never maps them (dlopen in music only).
- Syscalls on the S1 hot path (auditor's count): ≈ 360 → 240/s, recvmsg 150 → 60/s, timerfd reads 30 → 0
  (D-055). `perf trace`/syscall tracepoints are not readable here (tracefs root-only), so I could not recount.

## Phase 5 summary (2026-10-17, final code = commit f997638 + docs; fake device, temp config with the
human's keepalive_ms = 5600 and calibrated layout; same desktop for every row — it was *not* idle: niri
S0 = 27.0–28.2 % of a core, an animation kept it redrawing; numbers from `tools/measure.py` unless noted)

| Scenario | gowl CPU % | Wakeups/s | RSS MB (PSS) | Pkts/s | niri Δ % | Budget verdict |
|---|---|---|---|---|---|---|
| S0 baseline | — | — | — | — | (27.0–28.2 abs) | — |
| S1 screen 30 fps (60 s) | **0.25** (perf task-clock 0.32) | **60.6** (= 2·fps + keepalive) | **8.43** (3.6) | 0.18 | **≈ 21** | CPU ✔ · wakeups ✔ (was 90.7) · RSS target ✘ / ceiling ✔ · niri ✘ (justified, D-050) |
| S2 static (gowl side, mock compositor, `screen_mock -- --ignored s2`) | ≈ 0 | **2.00** at keepalive 500 → 0.18 at 5600 | — | 1000/keepalive | 5.5–7.9 *if niri keeps redrawing* (static-region probe, D-050); ≈ 0 on a truly idle niri (not measurable here) | gowl ✔ · real-desktop S2 → human |
| S3 music, sink running (inaudible silent playback from a 2nd client; real music → human) | **0.47** | **47.9** | **6.15** | 0.13 (silence) | — | ✔ (CPU target 0.7, ≤ 50/s) |
| S3 music, nothing playing (passive stream, D-052) | 0.000 | **0.2–0.3** (was 47–50) | 6.13 | 0.12–0.20 | — | ✔ |
| S4 paused (30 s) | 0.000 | **0.0** | 8.41 | 0.00 | 0 (27.27 abs = S0) | ✔ |
| one-shot `gowl color` (20 runs) | — | — | — | 1 | — | median **1.5 ms**, max 2.0 (≤ 20 ✔) |
| `gowl screen` start → razer ON / first frame (5 runs) | — | — | — | — | — | 1.7–5.8 ms / **47–79 ms** (≤ 150 ✔) |
| frame ready → sendto (`latency -- --ignored`, 3440x1440, all zones dirty) | — | — | — | — | — | p50 23.6 µs, **p99 39.2 µs** (≤ 1 ms ✔) |
| heap allocations after warm-up | — | — | — | — | — | **0**: `alloc_steady_state` (300 frames, counted from 10); live niri `--features count-alloc` 0/s; music 0 (alloc_music) ✔ |
| binary (stripped) | — | — | — | — | — | **640,616 B** (≤ 800 KB target ✔; was 743,016 at `opt-level = 3`) |

RSS: 8.4–8.8 MB = 5.7 MB of the compositor-shared capture memfd (fault-around around ~6.4k sampled
pixels; PSS 2.85 MB, shared with niri) + 2.4 MB shared libraries (libc 1.6, libm 0.5, ld 0.2) + 0.56 MB
binary text + **0.2 MB private dirty**. PSS 3.6 MB. The 6 MB target counts pages gowl does not own;
static linking would drop the library pages but breaks `dlopen` for music; MADV_RANDOM does not stop
shmem fault-around (Phase 2); MADV_HUGEPAGE has no effect (`shmem_enabled = never`). Ceiling met.

### Profile (prompt step 3) — `perf record -e cycles:u --call-graph dwarf` on gowl, S1, 60 s (515 samples)
`perf stat`: task-clock 194 ms / 60 s (0.32 %), user cycles 163M. **Correction (audit):** I divided user
cycles by an assumed clock; the auditor's utime/stime split on a busy desktop gives **user code ≈ 54 % of
gowl's CPU**, the rest kernel time (poll / recvmsg / sendmsg / timerfd, 2 wakeups per frame). Top user symbols:
1. `Sampler::run::<Xrgb8888>` (inlined into `run_screen`) — 63–75 % of user samples (≈ 6.4k scattered
   pixel loads per fully dirty frame; 23.6 µs p50 in the latency bench)
2. `screen_loop` body — 5 %
3. `__libc_recvmsg` — 3.6 %
4. `Ctl::service_net` — 2.7 %
5. `Streams::keepalive_due_ms` — 2.7 % (then `Ctl::net_deadline` 2.0 %, `memmove` 2.0 %)
No candidate from prompt step 5 that targets user code (coarser grid, direct inv-LUT, target-cpu=native)
is justified: the whole sampler is ≈ 0.07 % of a core at 30 fps. Kernel time scales with wakeups → D-051.

### Attempts (one change at a time; kept or reverted)
| Change | Before → after | Kept? |
|---|---|---|
| D-051 prepare next frame on `ready` + leave the Wayland fd out of poll while the frame is held | wakeups 90.7 → 60.5/s; task-clock 107.2 → 90.2 ms/30 s | **kept** |
| prepare only (fd still polled) | wayland wakeups 60/s → no change (offers arrive after we sleep) | superseded by the row above |
| D-052 passive music stream | idle music 47–50 → 0.2–0.3 wakeups/s, CPU → 0.000 | **kept** |
| D-053 `opt-level = "s"` | task-clock 62.5/62.8 → 58.2/59.4 ms/20 s; binary 743,016 → 640,616 B | **kept** |
| DMA-BUF via `/dev/udmabuf` (no libgbm) | niri imports the buffer but every copy fails (NVIDIA EGL: "EGLImage not supported" as a framebuffer) → falls back to SHM | **reverted** (D-050) |
| `capture_output_region` (probe knob) | niri Δ: full 21, 3440x400 20, 3440x100 17.4 | **reverted** (D-050) |
| lower fps (probe, `--fps`) | niri Δ: 30 → 21, 20 → 20, 15 → 17, 10 → 16, 5 → 12.7, 1 → 2.7 | not changed (human: 30 fps, D-004) |

## Results log (append; newest first)
| Date | Commit | Scenario | CPU % | Wakeups/s | RSS MB | Pkts/s | niri Δ CPU % | Notes |
|---|---|---|---|---|---|---|---|---|
| 2026-10-18 | cad72e6+D-064 | S3 music after the D-064 clock fix (tone from a 2nd client, −50 dBFS 60 Hz, 30 s) | 0.306 | 50.2 | 6.10 | 39.30 | 49.67 abs (busy desktop) | fake, isolated runtime dir, keepalive_ms 5600; live duplicate check: 0 identical datagrams < 100 ms apart (was 15 of 52 frames in 6 s), min gap 21.2 ms. **Real-light S3 to be re-run by the human.** |
| 2026-10-18 | phase 7 | binary size | | | | | | **771,744 B** stripped (+77,824 B over Phase 6's 693,920: `gowl setup`, `gowl keys`, session pidfd, unit markers); ≤ 800 KB target ✔; libc only. Hot paths unchanged except `Streams::off` (one bool test) and one more idle hook slot in music (D-069: an fd, 0 wakeups) |
| 2026-10-18 | 7f33e73 | **S1 screen, real light** (human: `measure.py screen 60`, fullscreen video) | 0.374 | 69.5 | 8.42 | — | 44.19 abs (no same-session S0; paused run below had niri 12.55 abs without capture) | real H6609 via the human's config; Shmem +53 MB in the window is system-wide (video player), not gowl; screen maps only ld/libc/libgcc_s/libm; binary 638 KB. Wakeups 69.5 vs 60 at 30 fps: ≈ +9/s on a changing video, not investigated further (gowl CPU stays 0.37 %) |
| 2026-10-18 | 7f33e73 | **S3 music, real light** (human: `measure.py music 60`, music playing) — *superseded: every sent frame was resent ~60 µs later by the keepalive (D-064); re-measure* | 0.675 | 48.8 | 6.13 | — | 14.83 abs | real light; ≈ 47 windows/s + sends; 15 libsndfile codec libs mapped via libpulsecommon (documented, unavoidable) |
| 2026-10-18 | 7f33e73 | **S4 paused, real light** (human: `measure.py paused 30`) | 0.000 | 0.0 | 3.11 (HWM 8.16) | 0.00 | 12.55 abs | real light; capture memfd freed on pause (D-055): RSS 8.4 → 3.1 MB |
| 2026-10-17 | f997638 | S1 screen 30 fps, 60 s (final) | 0.250 | 60.6 | 8.43 | 0.18 | 20.9 (48.08 vs S0 27.03/27.40) | fake; perf task-clock 0.32 %; binary 640,616 B |
| 2026-10-17 | f997638 | S4 paused 30 s (final) | 0.000 | 0.0 | 8.41 | 0.00 | 0 (27.27 abs) | fake |
| 2026-10-17 | 196acf7+ | S3 music, sink running (silent playback), 30 s | 0.467 | 47.9 | 6.15 | 0.13 | — | fake; passive stream |
| 2026-10-17 | 196acf7 | music idle, nothing playing (passive, D-052), 30–40 s | 0.000 | 0.2–0.3 | 6.13–6.30 | 0.12–0.20 | — | fake; was 47–50 wakeups/s |
| 2026-10-17 | 48ee7f0 | opt-level "s" vs 3 (perf stat S1 20 s ×2) | task-clock 58.2/59.4 vs 62.5/62.8 ms | | 8.55 vs 8.84 | | | binary 640,616 vs 743,016 B (D-053) |
| 2026-10-17 | 975cfda | S1 30 fps after D-051 (×2) | 0.267 / 0.300 | 60.5 / 60.7 | 8.64 / 8.57 | 0.17 | 20.4 / 21.2 | fake; perf task-clock 90.2 ms/30 s |
| 2026-10-17 | 121aa8d | S1 30 fps baseline (×2) | 0.333 / 0.333 | 90.7 / 90.7 | 8.70 / 8.87 | 0.17 | 20.9 / 20.8 | fake; perf task-clock 107.2 ms/30 s |
| 2026-10-17 | 121aa8d | S0 baseline (this session, animated desktop) | — | — | — | — | 27.0–28.2 abs | earlier sessions read 5.7–6.4 (different desktop state) |
| 2026-10-17 | probe (reverted) | niri Δ vs capture region (S1, 20 s each) | | | | | full 19.7, 3440x400 20.0, 3440x100 17.4, static regions 5.5–7.9 | D-050 |
| 2026-10-17 | probe | niri Δ vs fps (S1, 20 s each) | | 2.1 / 10.3 / 20.2 / 30.5 / 40.6 / 60.7 | | | 1 fps 2.7, 5 fps 12.7, 10 fps 16, 15 fps 17, 20 fps 20, 30 fps 21 | D-050 |
| 2026-10-17 | phase 4 review fixes | binary size | | | | | | **743,016 B** stripped (+8,256 B: OFD-lock pidfile, hook mask, panic hook table, parse-faithful config rewrite); libc only |
| 2026-10-17 | phase 4 review fixes | S4 paused (`measure.py paused 20 --fake`) | 0.000 | 0.0 | 8.74 | 0.00 | 1.25 abs | a first run read 0.2/s (4 switches in 20 s, most likely compositor events on the Wayland fd, which stays open while paused); rerun 0.0, and a 30 s `GOWL_LOG=debug` paused run had 0 context switches |
| 2026-10-17 | phase 4 review fixes | music on live PipeWire (D-048) | 0.20–0.40 | 49–50 | — | keepalive ~2 (silence) | — | stream really opens now ("capturing" logged); paused 0 switches in 5 s; stop 14 ms. Supersedes the "music paused" row below, which measured a stream that never opened |
| 2026-10-17 | phase 4 (7947711+) | binary size | | | | | | **734,760 B** stripped (+32,736 B vs 702,024: pidfile/control, netwatch + verify, music poll hook, keepalive walk); `cargo tree` = libc only |
| 2026-10-17 | 7947711 | **S4 paused** (`measure.py paused 20 --fake`, netlink fd in the poll set) | 0.000 | 0.0 | 8.81 | 0.00 | 26.70 abs | fake=True; 20 s window: 0 CPU ticks, 0 context switches, 0 datagrams → S4 met, and netlink adds 0 wakeups on a stable network. niri absolute was high from the human's desktop (gowl captures nothing while paused) |
| 2026-10-17 | 7947711 | S1 screen 30 fps (regression check after Phase 4) | 0.250 | 70.3 | 8.63 | 4.35 | 36.00 abs | fake=True; desktop partly static (~12 frames/s captured). `GOWL_LOG=debug` per-second line: `netlink 0 verify 0` in all 11 s |
| 2026-10-17 | 7947711 | music paused (live PipeWire, `gowl pause`) | — | 0 in 5 s | — | 0 | — | loopback receiver; stream closed while paused; resume → razer ON + frames; `gowl stop` exit in 12 ms |
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
