# Changelog

## 0.1.0 — 2026-10-18

First release.

- **Screen sync** (`gowl screen`): lights follow the colors at the edges of your screen, via wlr-screencopy
  (niri, sway, Hyprland, …). Drops to a low capture rate on a still screen (`idle_fps`).
- **Music sync** (`gowl music`): lights react to whatever is playing (PipeWire / PulseAudio monitor), with
  LumiSync's reactions and palettes, and fade out gently when the music stops (`music_fade_ms`).
- **Calibration** (`gowl calibrate`): map each light segment to its place around the screen; `--keepalive`
  finds how long your light holds a frame.
- **Control**: `on`, `off`, `brightness`, `color`, `temp`; `pause`, `resume`, `stop`, and
  `toggle screen|music` (starts, stops or switches, with a desktop notification).
- **Desktop integration**: `gowl state` / `gowl watch` JSON status feed, a ready-made quickshell service and
  tile, niri key snippet (Mod+M / Mod+Shift+M), systemd user unit.
- **Setup**: `make install` / `make uninstall`, and a guided first run with `gowl setup`.
- **Network**: talks only to lights on your LAN, bound to the LAN interface so VPNs (ProtonVPN verified;
  Tailscale/WireGuard/OpenVPN covered by a simulated test) don't break it; `gowl doctor` explains problems.
- **Small**: one ~790 KB binary, libc as its only dependency, ~0.4 % of a CPU core and ~8 MB while syncing.

Credits: a from-scratch Rust rewrite of the Govee parts of [LumiSync](https://github.com/Minlor/LumiSync)
by Minlor (MIT).
