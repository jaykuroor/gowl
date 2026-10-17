# CAPTURE on niri

## What niri offers (verify in Phase 0 with `wayland-info` and `niri --version`)
- `zwlr_screencopy_manager_v1` — supported for years; niri 26.04 (Apr 2026) fixed
  cursor exclusion for damage copies and concurrent damage requests.
- `ext_image_copy_capture_manager_v1` + `ext_output_image_capture_source_manager_v1` —
  the modern replacement; was an open PR for niri as of mid-2026. **If present, prefer it.**
- Portal (`xdg-desktop-portal-gnome`) + PipeWire DMA-BUF streams — not used by gowl.

Window rules matter: windows with `block-out-from "screen-capture"` render black in any
capture → lights go dark while they're visible. That's correct behaviour; document it in
the README. (`block-out-from "screencast"` only affects PipeWire casts, so it doesn't apply.)

## Globals to bind
`wl_shm`, every `wl_output` (version ≥ 4 for the `name` event), `zwlr_screencopy_manager_v1`
(v3), optionally `ext_image_copy_capture_manager_v1` + `ext_output_image_capture_source_manager_v1`,
optionally `zwp_linux_dmabuf_v1` (v4, DMA-BUF phase only). Track `wl_output` `name`,
`mode` (current), `scale`, `geometry.transform`; hot-plug via registry `global`/`global_remove`.

Output selection: `--output` / config `output` by name (`DP-1`, `HDMI-A-1`, … — same
names as `niri msg outputs`). Default: the first output announced. If the chosen output
disappears: razer OFF, wait for it to reappear (no busy loop), resume.

## wlr-screencopy frame cycle (per frame)
```
manager.capture_output(overlay_cursor = 0, output) → frame
  frame.buffer(format, width, height, stride)        ← SHM option (one per supported shm format)
  frame.linux_dmabuf(fourcc, width, height)          ← v3, DMA-BUF option
  frame.buffer_done                                  ← v3, all options sent
→ pick SHM option (format we support, see below); reuse existing wl_buffer if
  (format, width, height, stride) unchanged, else (re)allocate
frame.copy_with_damage(buffer)                       ← completes only after the output changes
  frame.flags(y_invert)
  frame.damage(x, y, w, h)  × n                      ← buffer coordinates
  frame.ready(tv_sec_hi, tv_sec_lo, tv_nsec)  | frame.failed
frame.destroy()
```
- **Pull pacing:** issue the next `capture_output` only when the timerfd says the frame
  budget elapsed (`1/fps` after the previous request). One frame in flight, max.
- First `copy_with_damage` after (re)allocation reports full damage — treat as such.
- `failed` → back off (100 ms, then 1 s) and retry; log once, not per failure.
- `copy` (non-damage) is only for `gowl calibrate`-style one-shots; never in sync loops.

## ext-image-copy-capture cycle (when available)
```
source = output_source_manager.create_source(output)
session = manager.create_session(source, options = 0)     ← 0 = don't paint cursors
  session.buffer_size(w, h); session.shm_format(fmt)×n; session.dmabuf_device/format…; session.done
frame = session.create_frame(); frame.attach_buffer(buf); frame.damage_buffer(0,0,w,h) (first only)
frame.capture()
  frame.transform(t); frame.damage(x,y,w,h)×n; frame.presentation_time(…); frame.ready | frame.failed
```
Same SHM buffer code, same damage→zone logic. The session persists (no per-frame
re-negotiation), which is cheaper — this is why it's preferred when present. Keep both
backends behind `trait Capture`; select at runtime from advertised globals; config/env
`GOWL_CAPTURE=screencopy|imagecopy` to force for benchmarking.

## SHM buffer
`memfd_create("gowl", MFD_CLOEXEC | MFD_ALLOW_SEALING)` → `ftruncate(stride*height)` →
`mmap(PROT_READ | PROT_WRITE, MAP_SHARED)` once → `wl_shm.create_pool` → `create_buffer`.
Reallocate only on size/format change; `munmap` the old one after the compositor
releases it. Optional: `madvise(MADV_HUGEPAGE)` — measure, don't assume.

## Pixel formats to support (pick from what the frame offers, in this order)
| wl_shm format | bytes in memory (LE) | read |
|---|---|---|
| XRGB8888 / ARGB8888 | B G R X | r=p[2] g=p[1] b=p[0] |
| XBGR8888 / ABGR8888 | R G B X | r=p[0] g=p[1] b=p[2] |
| XRGB2101010 / ARGB2101010 | u32: x2 r10 g10 b10 | r = (v>>20 & 0x3ff) >> 2, … |
| XBGR2101010 / ABGR2101010 | u32: x2 b10 g10 r10 | r = (v & 0x3ff) >> 2, … |
Anything else → error exit 4 with the offered list printed. Select the reader once per
format change (a generic `fn sample<F: PixelFormat>` so the inner loop is monomorphized).
10-bit → 8-bit by `>> 2` is fine for ambient light (legacy works in 8-bit).

## Geometry: zones → buffer offsets
1. Zones come from `color::generate_screen_mapping(N_zones, aspect, depth)` in **logical
   (as-seen) screen space**, aspect = logical width / height (after transform).
2. Map each logical rect into **buffer coordinates** by inverting the output transform
   (`wl_output` transform for screencopy; `frame.transform` for image-copy) and applying
   `y_invert`. Reference: how `grim` handles `wl_output_transform`. Verify on hardware
   by asking the human to rotate the output (`niri msg output <NAME> transform 90`) and
   confirm the top edge of the screen still drives the top LEDs.
3. Per zone, replicate legacy `processing.average_zone_colors` striding exactly:
   zone bounds via `_zone_bounds` (int truncation of x·W etc.), then
   `step = max(1, size // 24)` per axis starting at the zone's first pixel, taking every
   `step`-th row/column → `ceil(size/step)` samples per axis (24–47, fewer for tiny zones).
   Store byte offsets (`u32`) in one flat `Vec` with per-zone ranges (≤ 2209 per zone;
   ~22k offsets ≈ 88 KB for 10 zones at 1440p — fine, but note it for the cache budget).
   Phase 5 may test a coarser grid if it stays within ±1 of the vectors.
4. Per zone, also store its buffer-space bounding rect for the damage test.

## Damage → dirty zones
Each `damage` rect is tested against each zone's bounding rect (≤ 255 zones, a handful of
rects — trivial). Only dirty zones are re-summed; clean zones keep last frame's linear
mean. If the compositor sends > 32 rects or one rect covers > 50% of the output,
just mark all zones dirty.

## DMA-BUF variant (Phase 5, only if measurements demand it)
Goal: replace niri's GPU→CPU readback with a GPU→GPU blit into a LINEAR buffer we mmap.
- Get the render node from `zwp_linux_dmabuf_v1` default feedback `main_device` (dev_t → `/dev/dri/renderD*`).
- `gbm_create_device` → `gbm_bo_create_with_modifiers2(…, [DRM_FORMAT_MOD_LINEAR], GBM_BO_USE_LINEAR)`
  → export fd/stride/offset → `zwp_linux_buffer_params_v1.create_immed`.
- Read via `gbm_bo_map(GBM_BO_TRANSFER_READ)` or mmap the dmabuf fd with `DMA_BUF_IOCTL_SYNC`
  begin/end around the read. Our access is sparse (≤ 5–6k pixels) so uncached reads are OK
  — measure on the human's GPU (NVIDIA proprietary + LINEAR may fail; fall back to SHM).
- Ship only if niri's CPU drops measurably and gowl's CPU/RSS stays in budget.

## Things to verify on the machine (write results into this file)
- [x] screencopy version advertised; ext-image-copy-capture present? — **screencopy v3; ext-image-copy-capture and ext-output-image-capture-source absent on niri 26.04** (2026-10-17, wayland-info). The imagecopy backend stays a seam only.
- [ ] which SHM formats niri offers on this output (8-bit? 10-bit?) — partial: `wl_shm` advertises only XRGB8888, ARGB8888, XBGR8888, ABGR8888 (no 10-bit). The per-frame `buffer` offer still needs checking in Phase 2.
- [ ] does `copy_with_damage` really block on a static screen (gowl CPU ≈ 0)?
- [ ] damage rect granularity niri reports (full-output vs. tight rects)
- [ ] y_invert ever set?
- [ ] behaviour on output power-off (`niri msg action power-off-monitors`) and on lock screen
